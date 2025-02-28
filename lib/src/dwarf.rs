use crate::path_mapper::PathMapper;
use crate::shared::{
    relative_address_base, AddressDebugInfo, InlineStackFrame, RangeReadRef, SymbolicationResult,
};
use crate::symbolicate::demangle;
use addr2line::{
    fallible_iterator,
    gimli::{self, EndianSlice, Reader, ReaderOffsetId, RunTimeEndian},
};
use fallible_iterator::FallibleIterator;
use gimli::SectionId;
use object::read::ReadRef;
use object::CompressionFormat;
use std::{borrow::Cow, cmp::min, marker::PhantomData, str};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AddressPair {
    /// A "relative" address, meaningful in the final linked module / library / executable
    /// which we're trying to symbolicate. These addresses are relative to that image's
    /// "relative address base" whose definition depends on the image type.
    /// See `relative_address_base` for more information.
    pub original_relative_address: u32,

    /// An address that is meaningful in the current object and in the space that
    /// symbol addresses and DWARF debug info addresses in this object are expressed in.
    pub vmaddr_in_this_object: u64,
}

pub fn make_address_pairs_for_root_object<'data: 'file, 'file, O>(
    addresses: &[u32],
    object_file: &'file O,
) -> Vec<AddressPair>
where
    O: object::Object<'data, 'file>,
{
    // Make an AddressPair for every address.
    let image_base = relative_address_base(object_file);

    addresses
        .iter()
        .map(|a| AddressPair {
            original_relative_address: *a,
            vmaddr_in_this_object: image_base + *a as u64,
        })
        .collect()
}

pub fn collect_dwarf_address_debug_data<'data: 'file, 'file, O, R>(
    data: RangeReadRef<'data, impl ReadRef<'data>>,
    object: &'file O,
    addresses: &[AddressPair],
    symbolication_result: &mut R,
    path_mapper: &mut PathMapper<()>,
) where
    O: object::Object<'data, 'file>,
    R: SymbolicationResult,
{
    if addresses.is_empty() {
        return;
    }

    let section_data = SectionDataNoCopy::from_object(data, object);
    if let Ok(context) = section_data.make_addr2line_context() {
        for AddressPair {
            original_relative_address,
            vmaddr_in_this_object,
        } in addresses
        {
            if let Ok(frame_iter) = context.find_frames(*vmaddr_in_this_object as u64) {
                let frames: std::result::Result<Vec<_>, _> = frame_iter
                    .map(|f| Ok(convert_stack_frame(f, path_mapper)))
                    .collect();
                if let Ok(frames) = frames {
                    if !frames.is_empty() {
                        symbolication_result.add_address_debug_info(
                            *original_relative_address,
                            AddressDebugInfo { frames },
                        );
                    }
                }
            }
        }
    }
}

fn convert_stack_frame<R: gimli::Reader>(
    frame: addr2line::Frame<R>,
    path_mapper: &mut PathMapper<()>,
) -> InlineStackFrame {
    let function = match frame.function {
        Some(function_name) => {
            if let Ok(name) = function_name.raw_name() {
                Some(demangle::demangle_any(&name))
            } else {
                None
            }
        }
        None => None,
    };
    let file_path = match &frame.location {
        Some(location) => location.file.map(|file| path_mapper.map_path(file)),
        None => None,
    };

    InlineStackFrame {
        function,
        file_path,
        line_number: frame.location.and_then(|l| l.line).map(|l| l as u32),
    }
}

enum SingleSectionData<'data, T: ReadRef<'data>> {
    View(RangeReadRef<'data, T>, u64),
    Owned(Cow<'data, [u8]>),
}

/// Holds on to section data so that we can create an addr2line::Context for that
/// that data. This avoids one copy compared to what addr2line::Context::new does
/// by default, saving 1.5 seconds on libxul. (For comparison, dumping all symbols
/// from libxul takes 200ms in total.)
/// See addr2line::Context::new for details.
pub struct SectionDataNoCopy<'data, T: ReadRef<'data>> {
    endian: gimli::RunTimeEndian,
    debug_abbrev_data: SingleSectionData<'data, T>,
    debug_addr_data: SingleSectionData<'data, T>,
    debug_aranges_data: SingleSectionData<'data, T>,
    debug_info_data: SingleSectionData<'data, T>,
    debug_line_data: SingleSectionData<'data, T>,
    debug_line_str_data: SingleSectionData<'data, T>,
    debug_ranges_data: SingleSectionData<'data, T>,
    debug_rnglists_data: SingleSectionData<'data, T>,
    debug_str_data: SingleSectionData<'data, T>,
    debug_str_offsets_data: SingleSectionData<'data, T>,
    default_section_data: SingleSectionData<'data, T>,
}

impl<'data, T: ReadRef<'data>> SectionDataNoCopy<'data, T> {
    pub fn from_object<'file, O>(data: RangeReadRef<'data, T>, file: &'file O) -> Self
    where
        'data: 'file,
        O: object::Object<'data, 'file>,
    {
        let endian = if file.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        fn try_get_section_data<'data, 'file, O, T>(
            data: RangeReadRef<'data, T>,
            file: &'file O,
            section_name: &'static str,
        ) -> Option<SingleSectionData<'data, T>>
        where
            'data: 'file,
            O: object::Object<'data, 'file>,
            T: ReadRef<'data>,
        {
            use object::ObjectSection;
            let (section, used_manual_zdebug_path) =
                if let Some(section) = file.section_by_name(section_name) {
                    (section, false)
                } else {
                    // Also detect old-style compressed section which start with .zdebug / __zdebug
                    // in case object did not detect them.
                    assert!(section_name.as_bytes().starts_with(b".debug_"));
                    let mut name = Vec::with_capacity(section_name.len() + 1);
                    name.extend_from_slice(b".zdebug_");
                    name.extend_from_slice(&section_name.as_bytes()[7..]);
                    let section = file.section_by_name_bytes(&name)?;
                    (section, true)
                };

            // Handle sections which are not compressed.
            if let Ok(file_range) = section.compressed_file_range() {
                if file_range.format == CompressionFormat::None && !used_manual_zdebug_path {
                    let size = file_range.uncompressed_size;
                    return Some(SingleSectionData::View(
                        data.make_subrange(file_range.offset, size),
                        size,
                    ));
                }
            }

            // This section is probably compressed. Try to uncompress the data with object's
            // built-in compressed section handling.
            let section_data = section.uncompressed_data().ok()?;

            // Make sure the data is actually decompressed.
            if used_manual_zdebug_path && section_data.starts_with(b"ZLIB\0\0\0\0") {
                // Object's built-in compressed section handling didn't detect this as a
                // compressed section. This happens on old Go binaries which use compressed
                // sections like __zdebug_ranges, which is generally uncommon on macOS, so
                // object's mach-O parser doesn't handle them.
                // But we want to handle them.
                // Go fixed this in https://github.com/golang/go/issues/50796 .
                let b = section_data.get(8..12)?;
                let uncompressed_size = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                let compressed_bytes = &section_data[12..];

                let mut decompressed = Vec::with_capacity(uncompressed_size as usize);
                let mut decompress = flate2::Decompress::new(true);
                decompress
                    .decompress_vec(
                        compressed_bytes,
                        &mut decompressed,
                        flate2::FlushDecompress::Finish,
                    )
                    .ok()?;

                return Some(SingleSectionData::Owned(decompressed.into()));
            }
            Some(SingleSectionData::Owned(section_data))
        }

        fn get_section_data<'data, 'file, O, T>(
            data: RangeReadRef<'data, T>,
            file: &'file O,
            section_name: &'static str,
        ) -> SingleSectionData<'data, T>
        where
            'data: 'file,
            O: object::Object<'data, 'file>,
            T: ReadRef<'data>,
        {
            try_get_section_data(data, file, section_name)
                .unwrap_or_else(|| SingleSectionData::View(data.make_subrange(0, 0), 0))
        }

        let debug_abbrev_data = get_section_data(data, file, SectionId::DebugAbbrev.name());
        let debug_addr_data = get_section_data(data, file, SectionId::DebugAddr.name());
        let debug_aranges_data = get_section_data(data, file, SectionId::DebugAranges.name());
        let debug_info_data = get_section_data(data, file, SectionId::DebugInfo.name());
        let debug_line_data = get_section_data(data, file, SectionId::DebugLine.name());
        let debug_line_str_data = get_section_data(data, file, SectionId::DebugLineStr.name());
        let debug_ranges_data = get_section_data(data, file, SectionId::DebugRanges.name());
        let debug_rnglists_data = get_section_data(data, file, SectionId::DebugRngLists.name());
        let debug_str_data = get_section_data(data, file, SectionId::DebugStr.name());
        let debug_str_offsets_data =
            get_section_data(data, file, SectionId::DebugStrOffsets.name());
        let default_section_data = SingleSectionData::View(data.make_subrange(0, 0), 0);

        Self {
            endian,
            debug_abbrev_data,
            debug_addr_data,
            debug_aranges_data,
            debug_info_data,
            debug_line_data,
            debug_line_str_data,
            debug_ranges_data,
            debug_rnglists_data,
            debug_str_data,
            debug_str_offsets_data,
            default_section_data,
        }
    }

    /// Create an addr2line::Context around fully-read section data buffers.
    /// The EndianSlice that wraps the section data refers to either a buffer
    /// from read_bytes_at (for uncompressed sections), or to data from a Cow
    /// in the SingleSectionData::Owned variant.
    /// Either way, this means that the entire section data has been read upfront,
    /// and nothing is being read lazily during DWARF parsing.
    pub fn make_addr2line_context<'a>(
        &'a self,
    ) -> std::result::Result<addr2line::Context<EndianSlice<'a, RunTimeEndian>>, gimli::read::Error>
    where
        'data: 'a,
    {
        let endian = self.endian;
        fn get<'a, 'data: 'a, T: ReadRef<'data>>(
            data: &'a SingleSectionData<'data, T>,
            endian: gimli::RunTimeEndian,
        ) -> EndianSlice<'a, RunTimeEndian> {
            let buffer = match data {
                SingleSectionData::View(readref, size) => readref.read_bytes_at(0, *size).unwrap(),
                SingleSectionData::Owned(v) => &v[..],
            };
            EndianSlice::new(buffer, endian)
        }

        addr2line::Context::from_sections(
            get(&self.debug_abbrev_data, endian).into(),
            get(&self.debug_addr_data, endian).into(),
            get(&self.debug_aranges_data, endian).into(),
            get(&self.debug_info_data, endian).into(),
            get(&self.debug_line_data, endian).into(),
            get(&self.debug_line_str_data, endian).into(),
            get(&self.debug_ranges_data, endian).into(),
            get(&self.debug_rnglists_data, endian).into(),
            get(&self.debug_str_data, endian).into(),
            get(&self.debug_str_offsets_data, endian).into(),
            get(&self.default_section_data, endian),
        )
    }

    /// Create an addr2line::Context where the section data is read lazily, by
    /// wrapping the original ReadRef that this SectionDataNoCopy object was
    /// created from.
    /// In theory this allows skipping parts of the section data. However, in
    /// pracice, at least in the benchmarks in this repo we end up reading most
    /// of the section data anyway, and we also read it in very small increments
    /// and we touch many parts of it multiple times. So this probably requires
    /// a bit more work before it becomes competitive with the simple
    /// implementation that reads everything upfront.
    /// Returns None if any of the sections was compressed.
    #[allow(unused)]
    pub fn make_addr2line_context_partial_reads(
        &self,
    ) -> Option<
        std::result::Result<addr2line::Context<EndianRangeReadRef<'data, T>>, gimli::read::Error>,
    > {
        let endian = self.endian;
        fn get<'a, 'data: 'a, T: ReadRef<'data>>(
            data: &'a SingleSectionData<'data, T>,
            endian: gimli::RunTimeEndian,
        ) -> Option<EndianRangeReadRef<'data, T>> {
            match data {
                SingleSectionData::View(range_data, _) => {
                    Some(EndianRangeReadRef::new(*range_data, endian))
                }
                SingleSectionData::Owned(_) => None,
            }
        }

        Some(addr2line::Context::from_sections(
            get(&self.debug_abbrev_data, endian)?.into(),
            get(&self.debug_addr_data, endian)?.into(),
            get(&self.debug_aranges_data, endian)?.into(),
            get(&self.debug_info_data, endian)?.into(),
            get(&self.debug_line_data, endian)?.into(),
            get(&self.debug_line_str_data, endian)?.into(),
            get(&self.debug_ranges_data, endian)?.into(),
            get(&self.debug_rnglists_data, endian)?.into(),
            get(&self.debug_str_data, endian)?.into(),
            get(&self.debug_str_offsets_data, endian)?.into(),
            get(&self.default_section_data, endian)?,
        ))
    }
}

#[derive(Clone, Copy)]
pub struct EndianRangeReadRef<'data, T: ReadRef<'data>> {
    original_readref: T,
    range_start: u64,
    range_size: u64,
    endian: RunTimeEndian,
    _phantom_data: PhantomData<&'data ()>,
}

impl<'data, T: ReadRef<'data>> EndianRangeReadRef<'data, T> {
    pub fn new(range: RangeReadRef<'data, T>, endian: RunTimeEndian) -> Self {
        Self {
            original_readref: range.original_readref(),
            range_start: range.range_start(),
            range_size: range.range_size(),
            endian,
            _phantom_data: PhantomData,
        }
    }
}

impl<'data, T: ReadRef<'data>> std::fmt::Debug for EndianRangeReadRef<'data, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "EndianRangeReadRef(at {}, {} bytes)",
            self.range_start, self.range_size
        )
    }
}

impl<'data, T: ReadRef<'data>> Reader for EndianRangeReadRef<'data, T> {
    type Endian = RunTimeEndian;
    type Offset = usize;

    #[inline]
    fn endian(&self) -> RunTimeEndian {
        self.endian
    }

    #[inline]
    fn len(&self) -> usize {
        self.range_size as usize
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.range_size == 0
    }

    #[inline]
    fn empty(&mut self) {
        self.range_size = 0;
    }

    #[inline]
    fn truncate(&mut self, len: usize) -> gimli::Result<()> {
        if self.range_size < len as u64 {
            Err(gimli::Error::UnexpectedEof(self.offset_id()))
        } else {
            self.range_size = len as u64;
            Ok(())
        }
    }

    #[inline]
    fn offset_from(&self, base: &Self) -> usize {
        (self.range_start - base.range_start) as usize
    }

    #[inline]
    fn offset_id(&self) -> ReaderOffsetId {
        ReaderOffsetId(self.range_start)
    }

    #[inline]
    fn lookup_offset_id(&self, id: ReaderOffsetId) -> Option<Self::Offset> {
        let id = id.0;
        let self_id = self.range_start;
        let self_len = self.range_size;
        if id >= self_id && id <= self_id + self_len {
            Some((id - self_id) as usize)
        } else {
            None
        }
    }

    #[inline]
    fn find(&self, byte: u8) -> gimli::Result<usize> {
        // Read 4096-byte slices until the value is found.
        // TODO: Maybe make sure that chunks are aligned with 4096 chunks in the
        // original space?
        let start = self.range_start;
        let end = self.range_start + self.range_size;
        let mut chunk_start = start;
        while chunk_start < end {
            let chunk_size = min(4096, end - chunk_start);
            let read_chunk = self
                .original_readref
                .read_bytes_at(chunk_start, chunk_size)
                .map_err(|_| gimli::Error::Io)?;
            if let Some(pos) = read_chunk.iter().position(|b| *b == byte) {
                return Ok((chunk_start - start) as usize + pos);
            }
            chunk_start += chunk_size;
        }
        Err(gimli::Error::UnexpectedEof(self.offset_id()))
    }

    #[inline]
    fn skip(&mut self, len: usize) -> gimli::Result<()> {
        if self.range_size < len as u64 {
            Err(gimli::Error::UnexpectedEof(self.offset_id()))
        } else {
            self.range_start += len as u64;
            self.range_size -= len as u64;
            Ok(())
        }
    }

    #[inline]
    fn split(&mut self, len: usize) -> gimli::Result<Self> {
        if self.range_size < len as u64 {
            return Err(gimli::Error::UnexpectedEof(self.offset_id()));
        }
        let mut copy = *self;
        self.range_start += len as u64;
        self.range_size -= len as u64;
        copy.range_size = len as u64;
        Ok(copy)
    }

    #[inline]
    fn to_slice(&self) -> gimli::Result<Cow<[u8]>> {
        match self
            .original_readref
            .read_bytes_at(self.range_start, self.range_size)
        {
            Ok(slice) => Ok(slice.into()),
            Err(()) => Err(gimli::Error::Io),
        }
    }

    #[inline]
    fn to_string(&self) -> gimli::Result<Cow<str>> {
        let slice = self
            .original_readref
            .read_bytes_at(self.range_start, self.range_size)
            .map_err(|_| gimli::Error::Io)?;
        match str::from_utf8(slice) {
            Ok(s) => Ok(s.into()),
            _ => Err(gimli::Error::BadUtf8),
        }
    }

    #[inline]
    fn to_string_lossy(&self) -> gimli::Result<Cow<str>> {
        let slice = self
            .original_readref
            .read_bytes_at(self.range_start, self.range_size)
            .map_err(|_| gimli::Error::Io)?;
        Ok(String::from_utf8_lossy(slice))
    }

    #[inline]
    fn read_slice(&mut self, buf: &mut [u8]) -> gimli::Result<()> {
        let size = buf.len() as u64;
        if self.range_size < size {
            return Err(gimli::Error::UnexpectedEof(self.offset_id()));
        }
        let slice = self
            .original_readref
            .read_bytes_at(self.range_start, size)
            .map_err(|_| gimli::Error::Io)?;
        buf.clone_from_slice(slice);
        self.range_start += size;
        self.range_size -= size;
        Ok(())
    }
}
