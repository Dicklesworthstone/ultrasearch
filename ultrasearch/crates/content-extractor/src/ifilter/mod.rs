#![cfg(windows)]

use crate::{ExtractContext, ExtractError, ExtractedContent, Extractor, enforce_limits_str};
use anyhow::Result;
use core_types::DocKey;
use std::path::Path;
use windows::Win32::System::Com::{CoInitialize, CoUninitialize, IPersistFile};
use windows::Win32::Storage::IndexServer::{IFilter, LoadIFilter, FILTER_TEXT};
use windows::core::{PCWSTR, HSTRING};

// TODO: Properly manage COM initialization. CoInitialize is thread-local.
// A robust solution might use a dedicated STA thread pool for IFilters.
// For this shim, we attempt to init and ignore if already inited (RPC_E_CHANGED_MODE).

pub struct IFilterExtractor;

impl IFilterExtractor {
    pub fn new() -> Self {
        Self
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
            // Attempt init; ignore error (e.g. already init)
            let _ = CoInitialize(None);
            // Defer uninit? In a thread pool, we might init once per thread.
            // Here we are likely in a rayon thread. 
            // Ideally we should use a scope guard or just assume the thread is initialized by the runtime wrapper.
            // But rayon threads are generic.
            // Let's defer uninit for correctness in this scope.
            // Actually, excessive init/uninit is slow.
            
            // Scope guard for CoUninitialize
            struct CoGuard;
            impl Drop for CoGuard {
                fn drop(&mut self) {
                    unsafe { CoUninitialize(); }
                }
            }
            let _guard = CoGuard;

            let mut filter: Option<IFilter> = None;
            // LoadIFilter takes path, returns IFilter interface.
            // The signature in windows crate might differ slightly.
            // LoadIFilter(path, null, &mut filter as *mut _ as *mut _)
            // Actually, LoadIFilter returns Result<IFilter> wrapper in windows crate?
            // Let's check docs or generated code signature.
            // windows 0.52 function signature:
            // pub unsafe fn LoadIFilter<P0>(pwcspath: P0, punknownouter: Option<IUnknown>, pvreserved: *mut c_void) -> Result<IFilter>
            
            let filter_res = LoadIFilter(PCWSTR(path_hstring.as_ptr()), None, std::ptr::null_mut());
            
            match filter_res {
                Ok(f) => filter = Some(f),
                Err(e) => return Err(ExtractError::Failed(format!("LoadIFilter failed: {e}"))),
            }
            
            let filter = filter.unwrap();
            
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
                let mut stat = windows::Win32::Storage::IndexServer::STAT_CHUNK::default();
                if let Err(e) = filter.GetChunk(&mut stat) {
                    // filter exhausted or error?
                    // "FILTER_E_END_OF_CHUNKS" (0x80041700)
                    // windows crate maps HRESULTs.
                    // We check specific error code.
                    if e.code().0 == -2147215616 { // FILTER_E_END_OF_CHUNKS
                        break;
                    }
                    // Other error
                    return Err(ExtractError::Failed(format!("GetChunk failed: {e}")));
                }
                
                if stat.flags & windows::Win32::Storage::IndexServer::CHUNK_TEXT != 0 {
                    // Read text
                    loop {
                        let mut buf = [0u16; 4096];
                        // GetText(buf_len, buf_ptr) -> Result<()>
                        // Returns chunks.
                        // In 0.52 it might take slice.
                        
                        // Signature: unsafe fn GetText(&self, pcwcbuffer: *mut u32, awcbuffer: *mut u16) -> Result<()>
                        // pcwcbuffer is in/out.
                        
                        let mut count = buf.len() as u32;
                        let res = filter.GetText(&mut count, buf.as_mut_ptr());
                        
                        match res {
                            Ok(_) => {
                                if count == 0 {
                                    break;
                                }
                                let chunk = String::from_utf16_lossy(&buf[..count as usize]);
                                let (trimmed, was_trunc, used) = enforce_limits_str(&chunk, ctx);
                                text.push_str(&trimmed);
                                bytes_processed += used; // approx
                                if was_trunc || text.len() >= ctx.max_chars {
                                    truncated = true;
                                    break;
                                }
                            }
                            Err(e) => {
                                // FILTER_E_NO_MORE_TEXT (0x80041701) -> break chunk loop
                                if e.code().0 == -2147215615 {
                                    break;
                                }
                                // warning log?
                                break; 
                            }
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
