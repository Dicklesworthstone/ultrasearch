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
            // Some legacy formats often have IFilters
            matches!(ext.as_str(), "rtf" | "odt" | "msg")
        } else {
            false
        }
    }

    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        let path = Path::new(ctx.path);
        let path_hstring = HSTRING::from(path.as_os_str());

        unsafe {
            // Attempt init; track whether we should uninitialize.
            let coinit_hr = CoInitialize(None);
            let should_uninit = coinit_hr.is_ok();
            // Defer uninit? In a thread pool, we might init once per thread.
            // Here we are likely in a rayon thread.
            // Ideally we should use a scope guard or just assume the thread is initialized by the runtime wrapper.
            // But rayon threads are generic.
            // Let's defer uninit for correctness in this scope.
            // Actually, excessive init/uninit is slow.

            // Scope guard for CoUninitialize
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

            // Extract text chunks
            let mut text = String::new();
            let mut truncated = false;
            let mut bytes_processed = 0;

            // Stat chunk
            // STAT_CHUNK struct.
            // IFilter::GetChunk(&mut stat)

            // Loop chunks
            // Reading text: IFilter::GetText(&mut buffer)
            // We need a buffer.

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
                    // Read text
                    loop {
                        let mut buf = [0u16; 4096];
                        let mut count = buf.len() as u32;
                        let hr = filter.GetText(&mut count, PWSTR(buf.as_mut_ptr()));

                        if hr == FILTER_E_NO_MORE_TEXT.0 {
                            break;
                        }
                        if hr != S_OK.0 {
                            break;
                        }
                        if count == 0 {
                            break;
                        }

                        let chunk = String::from_utf16_lossy(&buf[..count as usize]);
                        let (trimmed, was_trunc, used) = enforce_limits_str(&chunk, ctx);
                        text.push_str(&trimmed);
                        bytes_processed += used; // approximate bytes from the UTF-16 slice
                        if was_trunc || text.len() >= ctx.max_chars {
                            truncated = true;
                            break;
                        }
                    }
                }

                if truncated {
                    break;
                }
            }

            Ok(ExtractedContent {
                key,
                text,
                lang: None,
                truncated,
                content_lang: None,
                bytes_processed,
            })
        }
    }
}
