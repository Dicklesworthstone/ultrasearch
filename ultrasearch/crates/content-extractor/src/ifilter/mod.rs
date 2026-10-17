#![cfg(windows)]

use crate::{ExtractContext, ExtractError, ExtractedContent, Extractor, enforce_limits_str};
use anyhow::Result;
use core_types::DocKey;
use std::ffi::c_void;
use std::path::Path;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Storage::IndexServer::{
    CHUNK_TEXT, FILTER_E_END_OF_CHUNKS, FILTER_E_NO_MORE_TEXT, IFilter, LoadIFilter, STAT_CHUNK,
};
use windows::Win32::System::Com::{CoInitialize, CoUninitialize};
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR};

// TODO: Properly manage COM initialization. CoInitialize is thread-local.
// A robust solution might use a dedicated STA thread pool for IFilters.
// For this shim, we attempt to init and ignore if already inited (RPC_E_CHANGED_MODE).

pub struct IFilterExtractor;

impl IFilterExtractor {
    pub fn new() -> Self {
        Self
    }

    fn extract_from_path(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        path: &Path,
    ) -> Result<ExtractedContent, ExtractError> {
        let meta = std::fs::metadata(path).map_err(|e| ExtractError::Failed(e.to_string()))?;
        let max_bytes = ctx.max_bytes as u64;
        if meta.len() > max_bytes {
            return Err(ExtractError::FileTooLarge {
                bytes: meta.len(),
                max_bytes,
            });
        }
        let path_hstring = HSTRING::from(path.as_os_str());

        unsafe {
            let coinit_hr = CoInitialize(None);
            let should_uninit = coinit_hr.is_ok();

            struct CoGuard(bool);
            impl Drop for CoGuard {
                fn drop(&mut self) {
                    if self.0 {
                        unsafe {
                            CoUninitialize();
                        }
                    }
                }
            }
            let _guard = CoGuard(should_uninit);

            let mut raw_filter: *mut c_void = std::ptr::null_mut();
            LoadIFilter(
                PCWSTR(path_hstring.as_ptr()),
                None,
                &mut raw_filter as *mut *mut _,
            )
            .map_err(|e| ExtractError::Failed(format!("LoadIFilter failed: {e}")))?;

            if raw_filter.is_null() {
                return Err(ExtractError::Failed(
                    "LoadIFilter returned null filter".into(),
                ));
            }

            // SAFETY: LoadIFilter populated raw_filter on success.
            let filter: IFilter = IFilter::from_raw(raw_filter.cast());

            // Initialize filter (canonicalize whitespace and paragraphs, index attributes).
            let init_flags: u32 =
                (windows::Win32::Storage::IndexServer::IFILTER_INIT_CANON_PARAGRAPHS.0
                    | windows::Win32::Storage::IndexServer::IFILTER_INIT_CANON_SPACES.0
                    | windows::Win32::Storage::IndexServer::IFILTER_INIT_APPLY_INDEX_ATTRIBUTES.0
                    | windows::Win32::Storage::IndexServer::IFILTER_INIT_INDEXING_ONLY.0
                    | windows::Win32::Storage::IndexServer::IFILTER_INIT_SEARCH_LINKS.0)
                    as u32;
            let mut init_flags_out: u32 = 0;
            let init_hr = filter.Init(init_flags, &[], &mut init_flags_out);
            if init_hr != S_OK.0 {
                return Err(ExtractError::Failed(format!(
                    "IFilter::Init failed with 0x{init_hr:08x}"
                )));
            }

            let mut raw_text = String::new();
            let mut char_count = 0usize;
            let mut truncated_early = false;

            loop {
                let mut stat = STAT_CHUNK::default();
                let hr = filter.GetChunk(&mut stat);
                if hr == FILTER_E_END_OF_CHUNKS.0 {
                    break;
                }
                if hr != S_OK.0 {
                    return Err(ExtractError::Failed(format!(
                        "GetChunk failed with 0x{hr:08x}"
                    )));
                }

                if stat.flags.0 & CHUNK_TEXT.0 == CHUNK_TEXT.0 {
                    loop {
                        let mut buf = [0u16; 4096];
                        let mut count = buf.len() as u32;
                        let hr = filter.GetText(&mut count, PWSTR(buf.as_mut_ptr()));

                        if hr == FILTER_E_NO_MORE_TEXT.0 || hr != S_OK.0 || count == 0 {
                            break;
                        }

                        let chunk = String::from_utf16_lossy(&buf[..count as usize]);
                        char_count = char_count.saturating_add(chunk.chars().count());
                        raw_text.push_str(&chunk);
                        if raw_text.len() > ctx.max_bytes || char_count > ctx.max_chars {
                            truncated_early = true;
                            break;
                        }
                    }
                }

                if truncated_early {
                    break;
                }
            }

            let (text, truncated_by_limits, bytes_processed) = enforce_limits_str(&raw_text, ctx);
            Ok(ExtractedContent {
                key,
                text,
                lang: None,
                truncated: truncated_early || truncated_by_limits,
                content_lang: None,
                bytes_processed,
            })
        }
    }
}

impl Default for IFilterExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor for IFilterExtractor {
    fn name(&self) -> &'static str {
        "ifilter"
    }

    fn supports(&self, ctx: &ExtractContext) -> bool {
        if let Some(ext) = super::resolve_ext(ctx) {
            matches!(ext.as_str(), "rtf" | "odt" | "msg" | "doc" | "xls" | "ppt")
        } else {
            false
        }
    }

    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        self.extract_from_path(ctx, key, Path::new(ctx.path))
    }

    fn extract_file(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        file: &std::fs::File,
    ) -> Result<ExtractedContent, ExtractError> {
        let bytes = crate::read_file_snapshot(file, ctx.max_bytes)?;
        self.extract_bytes(ctx, key, &bytes)
    }

    fn extract_bytes(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        bytes: &[u8],
    ) -> Result<ExtractedContent, ExtractError> {
        crate::with_snapshot_tempfile(ctx, bytes, |temp_path| {
            self.extract_from_path(ctx, key, temp_path)
        })
    }
}
