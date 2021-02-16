use anyhow;
use memmap::MmapOptions;
use profiler_get_symbols::{
    self, CompactSymbolTable, FileAndPathHelper, FileAndPathHelperResult, GetSymbolsError,
    OptionallySendFuture, OwnedFileData,
};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::pin::Pin;

pub async fn get_table(
    debug_name: &str,
    breakpad_id: Option<String>,
    symbol_directory: PathBuf,
) -> anyhow::Result<CompactSymbolTable> {
    let helper = Helper { symbol_directory };
    let table = get_symbols_retry_id(debug_name, breakpad_id, &helper).await?;
    Ok(table)
}

async fn get_symbols_retry_id(
    debug_name: &str,
    breakpad_id: Option<String>,
    helper: &Helper,
) -> anyhow::Result<CompactSymbolTable> {
    let breakpad_id = match breakpad_id {
        Some(breakpad_id) => breakpad_id,
        None => {
            // No breakpad ID was specified. get_compact_symbol_table always wants one, so we call it twice:
            // First, with a bogus breakpad ID ("<unspecified>"), and then again with the breakpad ID that
            // it expected.
            let result =
                profiler_get_symbols::get_compact_symbol_table(debug_name, "<unspecified>", helper)
                    .await;
            match result {
                Ok(table) => return Ok(table),
                Err(err) => match err {
                    GetSymbolsError::UnmatchedBreakpadId(expected, _) => {
                        eprintln!("Using breakpadID: {}", expected);
                        expected
                    }
                    err => return Err(err.into()),
                },
            }
        }
    };
    Ok(profiler_get_symbols::get_compact_symbol_table(debug_name, &breakpad_id, helper).await?)
}

pub fn dump_table(
    w: &mut impl std::io::Write,
    table: CompactSymbolTable,
    full: bool,
) -> anyhow::Result<()> {
    writeln!(w, "Found {} symbols.", table.addr.len())?;
    for (i, address) in table.addr.iter().enumerate() {
        if i >= 15 && !full {
            writeln!(
                w,
                "and {} more symbols. Pass --full to print the full list.",
                table.addr.len() - i
            )?;
            break;
        }

        let start_pos = table.index[i];
        let end_pos = table.index[i + 1];
        let symbol_bytes = &table.buffer[start_pos as usize..end_pos as usize];
        let symbol_string = std::str::from_utf8(symbol_bytes)?;
        writeln!(w, "{:x} {}", address, symbol_string)?;
    }
    Ok(())
}

struct MmapFileContents(memmap::Mmap);

impl OwnedFileData for MmapFileContents {
    fn get_data(&self) -> &[u8] {
        &*self.0
    }
}

struct Helper {
    symbol_directory: PathBuf,
}

impl FileAndPathHelper for Helper {
    type FileContents = MmapFileContents;

    fn get_candidate_paths_for_binary_or_pdb(
        &self,
        debug_name: &str,
        _breakpad_id: &str,
    ) -> Pin<Box<dyn OptionallySendFuture<Output = FileAndPathHelperResult<Vec<PathBuf>>>>> {
        async fn to_future(
            res: FileAndPathHelperResult<Vec<PathBuf>>,
        ) -> FileAndPathHelperResult<Vec<PathBuf>> {
            res
        }

        let mut paths = vec![];

        // Also consider .so.dbg files in the symbol directory.
        if debug_name.ends_with(".so") {
            let debug_debug_name = format!("{}.dbg", debug_name);
            paths.push(self.symbol_directory.join(debug_debug_name));
        }

        // And dSYM packages.
        if !debug_name.ends_with(".pdb") {
            paths.push(
                self.symbol_directory
                    .join(&format!("{}.dSYM", debug_name))
                    .join("Contents")
                    .join("Resources")
                    .join("DWARF")
                    .join(debug_name),
            );
        }

        // Finally, the file itself.
        paths.push(self.symbol_directory.join(debug_name));

        Box::pin(to_future(Ok(paths)))
    }

    fn read_file(
        &self,
        path: &Path,
    ) -> Pin<Box<dyn OptionallySendFuture<Output = FileAndPathHelperResult<Self::FileContents>>>>
    {
        async fn read_file_impl(path: PathBuf) -> FileAndPathHelperResult<MmapFileContents> {
            eprintln!("Reading file {:?}", &path);
            let file = File::open(&path)?;
            Ok(MmapFileContents(unsafe { MmapOptions::new().map(&file)? }))
        }

        Box::pin(read_file_impl(path.to_owned()))
    }
}

#[cfg(test)]
mod test {

    use profiler_get_symbols::GetSymbolsError;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::path::PathBuf;

    fn fixtures_dir() -> PathBuf {
        let this_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        this_dir.join("..").join("..").join("fixtures")
    }

    #[test]
    fn successful_pdb() {
        let result = futures::executor::block_on(crate::get_table(
            "firefox.pdb",
            Some(String::from("AA152DEB2D9B76084C4C44205044422E1")),
            fixtures_dir().join("win64-ci"),
        ));
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.addr.len(), 1286);
        assert_eq!(result.addr[776], 0x31fc0);
        assert_eq!(
            std::str::from_utf8(
                &result.buffer[result.index[776] as usize..result.index[777] as usize]
            ),
            Ok("sandbox::ProcessMitigationsWin32KDispatcher::EnumDisplayMonitors(sandbox::IPCInfo*, sandbox::CountedBuffer*)")
        );
    }

    #[test]
    fn successful_pdb_unspecified_id() {
        let result = futures::executor::block_on(crate::get_table(
            "firefox.pdb",
            None,
            fixtures_dir().join("win64-ci"),
        ));
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.addr.len(), 1286);
        assert_eq!(result.addr[776], 0x31fc0);
        assert_eq!(
            std::str::from_utf8(
                &result.buffer[result.index[776] as usize..result.index[777] as usize]
            ),
            Ok("sandbox::ProcessMitigationsWin32KDispatcher::EnumDisplayMonitors(sandbox::IPCInfo*, sandbox::CountedBuffer*)")
        );
    }

    #[test]
    fn unsuccessful_pdb_wrong_id() {
        let result = futures::executor::block_on(crate::get_table(
            "firefox.pdb",
            Some(String::from("AA152DEBFFFFFFFFFFFFFFFFF044422E1")),
            fixtures_dir().join("win64-ci"),
        ));
        assert!(result.is_err());
        let err = match result {
            Ok(_) => panic!("Shouldn't have succeeded with wrong breakpad ID"),
            Err(err) => err,
        };
        let err = match err.downcast::<GetSymbolsError>() {
            Ok(err) => err,
            Err(_) => panic!("wrong error type"),
        };
        match err {
            GetSymbolsError::UnmatchedBreakpadId(expected, actual) => {
                assert_eq!(expected, "AA152DEB2D9B76084C4C44205044422E1");
                assert_eq!(actual, "AA152DEBFFFFFFFFFFFFFFFFF044422E1");
            }
            _ => panic!("wrong GetSymbolsError subtype"),
        }
    }

    #[test]
    fn unspecified_id_fat_arch() {
        let result = futures::executor::block_on(crate::get_table(
            "firefox",
            None,
            fixtures_dir().join("macos-ci"),
        ));
        assert!(result.is_err());
        let err = match result {
            Ok(_) => panic!("Shouldn't have succeeded with unspecified breakpad ID"),
            Err(err) => err,
        };
        let err = match err.downcast::<GetSymbolsError>() {
            Ok(err) => err,
            Err(_) => panic!("wrong error type"),
        };
        match err {
            GetSymbolsError::NoMatchMultiArch(errors) => {
                let mut expected_ids = vec![];
                for err in errors {
                    match err {
                        GetSymbolsError::UnmatchedBreakpadId(expected, _actual) => {
                            expected_ids.push(expected);
                        }
                        _ => panic!("wrong GetSymbolsError subtype"),
                    }
                }
                assert_eq!(expected_ids.len(), 2);
                assert!(expected_ids.contains(&"B993FABD8143361AB199F7DE9DF7E4360".to_string()));
                assert!(expected_ids.contains(&"8E7B0ED0B04F3FCCA05E139E5250BA720".to_string()));
            }
            _ => panic!("wrong GetSymbolsError subtype: {:?}", err),
        }
    }

    #[test]
    fn fat_arch_1() {
        let result = futures::executor::block_on(crate::get_table(
            "firefox",
            Some("B993FABD8143361AB199F7DE9DF7E4360".to_string()),
            fixtures_dir().join("macos-ci"),
        ));
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.addr.len(), 13);
        assert_eq!(result.addr[9], 0x2730);
        assert_eq!(
            std::str::from_utf8(
                &result.buffer[result.index[9] as usize..result.index[10] as usize]
            ),
            Ok("__ZN7mozilla20ProfileChunkedBuffer17ResetChunkManagerEv")
        );
    }

    #[test]
    fn fat_arch_2() {
        let result = futures::executor::block_on(crate::get_table(
            "firefox",
            Some("8E7B0ED0B04F3FCCA05E139E5250BA720".to_string()),
            fixtures_dir().join("macos-ci"),
        ));
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.addr.len(), 13);
        assert_eq!(result.addr[9], 0x759c);
        assert_eq!(
            std::str::from_utf8(
                &result.buffer[result.index[9] as usize..result.index[10] as usize]
            ),
            Ok("__ZN7mozilla20ProfileChunkedBuffer17ResetChunkManagerEv")
        );
    }

    #[test]
    fn compare_snapshot() {
        let table = futures::executor::block_on(crate::get_table(
            "mozglue.pdb",
            Some(String::from("63C609072D3499F64C4C44205044422E1")),
            fixtures_dir().join("win64-ci"),
        ))
        .unwrap();
        let mut output: Vec<u8> = Vec::new();
        crate::dump_table(&mut output, table, true).unwrap();

        if false {
            let mut output_file = File::create(
                fixtures_dir()
                    .join("snapshots")
                    .join("output-win64-ci-mozglue.pdb.txt"),
            )
            .unwrap();
            output_file.write_all(&output).unwrap();
        }

        let mut snapshot_file = File::open(
            fixtures_dir()
                .join("snapshots")
                .join("win64-ci-mozglue.pdb.txt"),
        )
        .unwrap();
        let mut expected: Vec<u8> = Vec::new();
        snapshot_file.read_to_end(&mut expected).unwrap();
        assert_eq!(output, expected);
    }
}
