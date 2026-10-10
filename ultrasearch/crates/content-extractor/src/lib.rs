//! Content extraction facade.
//!
//! This module defines the traits and types for the extraction pipeline. The
//! actual adapters (Extractous, IFilter, OCR) will be wired incrementally; for
//! c00.5 we provide compile-ready scaffolding with minimal logic.

use anyhow::Result;
use core_types::DocKey;
use std::fs;
use std::io::{Read, Seek};
use std::path::Path;
use tracing::instrument;

pub mod component_manager;
pub mod ifilter;
pub mod ocr;
pub mod plugins;

/// Unified extraction output.
#[derive(Debug, Clone)]
pub struct ExtractedContent {
    pub key: DocKey,
    pub text: String,
    pub lang: Option<String>,
    pub truncated: bool,
    pub content_lang: Option<String>,
    pub bytes_processed: usize,
}

/// Context passed to extractors (paths, limits, hints).
#[derive(Debug, Clone)]
pub struct ExtractContext<'a> {
    pub path: &'a str,
    pub max_bytes: usize,
    pub max_chars: usize,
    pub ext_hint: Option<&'a str>,
    pub mime_hint: Option<&'a str>,
}

/// Extraction error categories.
#[derive(thiserror::Error, Debug)]
pub enum ExtractError {
    #[error("unsupported format: {0}")]
    Unsupported(String),
    #[error("extraction failed: {0}")]
    Failed(String),
    #[error("file too large (bytes={bytes}, max={max_bytes})")]
    FileTooLarge { bytes: u64, max_bytes: u64 },
}

/// Trait implemented by concrete extractor backends.
pub trait Extractor {
    fn name(&self) -> &'static str;
    fn supports(&self, ctx: &ExtractContext) -> bool;
    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError>;

    /// Extract from an already verified file identity. Implementations must
    /// read this handle instead of reopening `ctx.path`, which may have changed.
    /// Path-only backends explicitly decline this contract.
    fn extract_file(
        &self,
        _ctx: &ExtractContext,
        _key: DocKey,
        _file: &fs::File,
    ) -> Result<ExtractedContent, ExtractError> {
        Err(ExtractError::Unsupported(format!(
            "{} does not support extraction from a verified file handle",
            self.name()
        )))
    }
}

/// Ordered stack of extractors with first-win semantics.
pub struct ExtractorStack {
    backends: Vec<Box<dyn Extractor + Send + Sync>>,
}

impl ExtractorStack {
    /// Simple default stack: SimpleText followed by Noop.
    pub fn with_defaults() -> Self {
        Self::simple_only()
    }

    /// Simple-only stack (no external dependencies).
    pub fn simple_only() -> Self {
        Self::new(vec![Box::new(SimpleTextExtractor), Box::new(NoopExtractor)])
    }

    /// Build a stack optionally including Extractous when the feature is enabled.
    pub fn with_extractous_enabled(enable: bool) -> Self {
        if enable {
            #[cfg(feature = "extractous_backend")]
            {
                return Self::new(vec![
                    Box::new(SimpleTextExtractor),
                    Box::new(ExtractousExtractor::new()),
                    Box::new(NoopExtractor),
                ]);
            }
        }
        Self::simple_only()
    }

    pub fn new(backends: Vec<Box<dyn Extractor + Send + Sync>>) -> Self {
        Self { backends }
    }

    /// Run the first extractor that claims support.
    #[instrument(skip(self, ctx))]
    pub fn extract(&self, key: DocKey, ctx: &ExtractContext) -> Result<ExtractedContent> {
        if self.backends.is_empty() {
            let ext = resolve_ext(ctx).unwrap_or_else(|| "unknown".to_string());
            return Err(anyhow::anyhow!(ExtractError::Unsupported(ext)));
        }

        for backend in &self.backends {
            if backend.supports(ctx) {
                return backend.extract(ctx, key).map_err(|e| e.into());
            }
        }
        let ext = resolve_ext(ctx).unwrap_or_else(|| "unknown".to_string());
        Err(anyhow::anyhow!(ExtractError::Unsupported(ext)))
    }

    /// Preserve the caller's file identity through backend selection and reads.
    /// A path-only backend cannot fall back to an unverified pathname here.
    #[instrument(skip(self, ctx, file))]
    pub fn extract_file(
        &self,
        key: DocKey,
        ctx: &ExtractContext,
        file: &fs::File,
    ) -> Result<ExtractedContent> {
        for backend in &self.backends {
            if backend.supports(ctx) {
                return backend.extract_file(ctx, key, file).map_err(Into::into);
            }
        }
        let ext = resolve_ext(ctx).unwrap_or_else(|| "unknown".to_string());
        Err(anyhow::anyhow!(ExtractError::Unsupported(ext)))
    }
}

/// Minimal placeholder extractor that returns empty text; used until real
/// Extractous/IFilter/OCR adapters are wired.
pub struct NoopExtractor;

impl Extractor for NoopExtractor {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn supports(&self, _ctx: &ExtractContext) -> bool {
        true
    }

    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        let (text, truncated, used) = enforce_limits_str("", ctx);
        Ok(ExtractedContent {
            key,
            text,
            lang: None,
            truncated,
            content_lang: None,
            bytes_processed: used,
        })
    }

    fn extract_file(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        _file: &fs::File,
    ) -> Result<ExtractedContent, ExtractError> {
        // This backend produces no content and does not read a pathname.
        self.extract(ctx, key)
    }
}

/// Plain-text extractor for lightweight formats (txt/log/rs/toml/json/md).
pub struct SimpleTextExtractor;

impl Extractor for SimpleTextExtractor {
    fn name(&self) -> &'static str {
        "simple-text"
    }

    fn supports(&self, ctx: &ExtractContext) -> bool {
        if let Some(ext) = resolve_ext(ctx) {
            matches!(
                ext.as_str(),
                "txt" | "log" | "md" | "json" | "jsonl" | "toml" | "rs" | "ts" | "tsx" | "csv"
            )
        } else {
            false
        }
    }

    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        let path = Path::new(ctx.path);
        let file = fs::File::open(path).map_err(|e| ExtractError::Failed(e.to_string()))?;
        self.extract_file(ctx, key, &file)
    }

    fn extract_file(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        file: &fs::File,
    ) -> Result<ExtractedContent, ExtractError> {
        let meta = file
            .metadata()
            .map_err(|e| ExtractError::Failed(e.to_string()))?;
        let max_bytes = ctx.max_bytes as u64;
        if meta.len() > max_bytes {
            return Err(ExtractError::FileTooLarge {
                bytes: meta.len(),
                max_bytes,
            });
        }

        // The file can grow after metadata() (notably an active log). Bound the
        // actual read as well, rather than allowing fs::read to allocate until
        // a moving EOF before the worker can reject its changed snapshot.
        let mut source = file;
        source
            .rewind()
            .map_err(|e| ExtractError::Failed(e.to_string()))?;
        let data = read_bounded(source, ctx.max_bytes)?;
        if is_probably_binary(&data) {
            return Err(ExtractError::Unsupported("binary".into()));
        }

        let text_raw = String::from_utf8_lossy(&data);
        let (text, truncated, used_bytes) = enforce_limits_str(&text_raw, ctx);

        Ok(ExtractedContent {
            key,
            text,
            lang: None,
            truncated,
            content_lang: None,
            bytes_processed: used_bytes,
        })
    }
}

fn read_bounded(source: impl Read, max_bytes: usize) -> Result<Vec<u8>, ExtractError> {
    let mut data = Vec::with_capacity(max_bytes.min(64 * 1024));
    source
        .take((max_bytes as u64).saturating_add(1))
        .read_to_end(&mut data)
        .map_err(|e| ExtractError::Failed(e.to_string()))?;
    if data.len() > max_bytes {
        return Err(ExtractError::FileTooLarge {
            bytes: data.len() as u64,
            max_bytes: max_bytes as u64,
        });
    }
    Ok(data)
}

/// Enforce both byte and char limits on an in-memory string.
pub fn enforce_limits_str(text: &str, ctx: &ExtractContext) -> (String, bool, usize) {
    let mut bytes = 0usize;
    let mut truncated = false;
    let mut out = String::with_capacity(text.len().min(ctx.max_bytes));

    for (idx, ch) in text.chars().enumerate() {
        let ch_len = ch.len_utf8();
        if bytes + ch_len > ctx.max_bytes || idx + 1 > ctx.max_chars {
            truncated = true;
            break;
        }
        out.push(ch);
        bytes += ch_len;
    }

    (out, truncated, bytes)
}

pub(crate) fn resolve_ext(ctx: &ExtractContext) -> Option<String> {
    if let Some(ext) = ctx.ext_hint.filter(|e| !e.is_empty()) {
        return Some(ext.to_ascii_lowercase());
    }
    std::path::Path::new(ctx.path)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
}

/// Heuristic to detect likely-binary content: look for NULs or >5% control bytes in first 4 KiB.
fn is_probably_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(4096)];
    if sample.contains(&0) {
        return true;
    }
    let ctrl = sample
        .iter()
        .filter(|&&b| (b < 0x09) || (b > 0x0D && b < 0x20))
        .count();
    ctrl * 20 > sample.len()
}

#[cfg(feature = "extractous_backend")]
/// Extractor that delegates to the Extractous engine for rich document types.
pub struct ExtractousExtractor;

#[cfg(feature = "extractous_backend")]
impl ExtractousExtractor {
    pub fn new() -> Self {
        Self
    }

    fn build_engine(&self, ctx: &ExtractContext) -> extractous::Extractor {
        let max_chars = ctx.max_chars.min(i32::MAX as usize) as i32;
        extractous::Extractor::new().set_extract_string_max_length(max_chars)
    }
}

#[cfg(feature = "extractous_backend")]
impl Default for ExtractousExtractor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "extractous_backend")]
impl Extractor for ExtractousExtractor {
    fn name(&self) -> &'static str {
        "extractous"
    }

    fn supports(&self, ctx: &ExtractContext) -> bool {
        fs::metadata(ctx.path)
            .map(|meta| meta.is_file())
            .unwrap_or(false)
    }

    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        let path = Path::new(ctx.path);
        let meta = fs::metadata(path).map_err(|e| ExtractError::Failed(e.to_string()))?;

        let max_bytes = ctx.max_bytes as u64;
        if meta.len() > max_bytes {
            return Err(ExtractError::FileTooLarge {
                bytes: meta.len(),
                max_bytes,
            });
        }

        let engine = self.build_engine(ctx);
        let (text, _metadata) = engine
            .extract_file_to_string(ctx.path)
            .map_err(|e| ExtractError::Failed(e.to_string()))?;

        let byte_len = text.len();
        let char_len = text.chars().count();
        let truncated = byte_len > ctx.max_bytes || char_len > ctx.max_chars;

        Ok(ExtractedContent {
            key,
            lang: None,
            content_lang: None,
            truncated,
            bytes_processed: byte_len.min(ctx.max_bytes),
            text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_always_supports() {
        let ctx = ExtractContext {
            path: "dummy",
            max_bytes: 1024,
            max_chars: 1024,
            ext_hint: Some("txt"),
            mime_hint: None,
        };
        let stack = ExtractorStack::new(vec![Box::new(NoopExtractor)]);
        let out = stack.extract(DocKey::from_parts(1, 42), &ctx).unwrap();
        assert!(out.text.is_empty());
        assert!(!out.truncated);
        assert_eq!(out.bytes_processed, 0);
    }

    #[test]
    fn verified_handle_extraction_explicitly_rejects_path_only_backends() {
        struct PathOnly;
        impl Extractor for PathOnly {
            fn name(&self) -> &'static str {
                "path-only"
            }
            fn supports(&self, _: &ExtractContext) -> bool {
                true
            }
            fn extract(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
            ) -> Result<ExtractedContent, ExtractError> {
                NoopExtractor.extract(ctx, key)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.txt");
        fs::write(&path, "input").unwrap();
        let file = fs::File::open(&path).unwrap();
        let ctx = ExtractContext {
            path: path.to_str().unwrap(),
            max_bytes: 1024,
            max_chars: 1024,
            ext_hint: Some("txt"),
            mime_hint: None,
        };
        let key = DocKey::from_parts(1, 42);
        let stack = ExtractorStack::new(vec![Box::new(PathOnly), Box::new(NoopExtractor)]);
        assert_eq!(stack.extract(key, &ctx).unwrap().key, key);
        let error = stack.extract_file(key, &ctx, &file).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ExtractError>(),
            Some(ExtractError::Unsupported(reason)) if reason.contains("verified file handle")
        ));
    }

    #[test]
    fn enforce_limits_truncates_on_chars() {
        let s = "abcdef";
        let ctx = ExtractContext {
            path: "dummy",
            max_bytes: 1024,
            max_chars: 3,
            ext_hint: None,
            mime_hint: None,
        };
        let (trimmed, was_truncated, used) = enforce_limits_str(s, &ctx);
        assert_eq!(trimmed, "abc");
        assert!(was_truncated);
        assert_eq!(used, 3);
    }

    #[test]
    fn enforce_limits_truncates_on_bytes_with_utf8() {
        let s = "ééé"; // 6 bytes, 3 chars
        let ctx = ExtractContext {
            path: "dummy",
            max_bytes: 3, // allow only one char (2 bytes)
            max_chars: 10,
            ext_hint: None,
            mime_hint: None,
        };
        let (trimmed, truncated, used) = enforce_limits_str(s, &ctx);
        assert_eq!(trimmed, "é");
        assert!(truncated);
        assert_eq!(used, 2);
    }

    #[test]
    fn enforce_limits_truncates_on_bytes_plain_ascii() {
        let s = "0123456789"; // 10 bytes
        let ctx = ExtractContext {
            path: "dummy",
            max_bytes: 5,
            max_chars: 10,
            ext_hint: None,
            mime_hint: None,
        };
        let (trimmed, truncated, used) = enforce_limits_str(s, &ctx);
        assert_eq!(trimmed, "01234");
        assert!(truncated);
        assert_eq!(used, 5);
    }

    #[test]
    fn file_too_large_error_formats() {
        let err = ExtractError::FileTooLarge {
            bytes: 2,
            max_bytes: 1,
        };
        let msg = format!("{err}");
        assert!(msg.contains("file too large"));
        assert!(msg.contains("2"));
    }

    #[test]
    fn simple_extractor_bytes_processed_matches_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.txt");
        std::fs::write(&path, b"abc").unwrap();

        let ctx = ExtractContext {
            path: path.to_str().unwrap(),
            max_bytes: 10,
            max_chars: 10,
            ext_hint: Some("txt"),
            mime_hint: None,
        };
        let simple = SimpleTextExtractor;
        let out = simple.extract(&ctx, DocKey::from_parts(1, 1)).unwrap();
        assert_eq!(out.bytes_processed, 3);
        assert!(!out.truncated);
        assert_eq!(out.text, "abc");
    }

    #[test]
    fn simple_extractor_rejects_large_file() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"0123456789").unwrap(); // 10 bytes

        let ctx = ExtractContext {
            path: path.to_str().unwrap(),
            max_bytes: 5,
            max_chars: 20,
            ext_hint: Some("txt"),
            mime_hint: None,
        };
        let simple = SimpleTextExtractor;
        let err = simple.extract(&ctx, DocKey::from_parts(1, 1)).unwrap_err();
        assert!(matches!(
            err,
            ExtractError::FileTooLarge {
                bytes: 10,
                max_bytes: 5
            }
        ));
    }

    #[test]
    fn bounded_read_rejects_growth_after_metadata_without_reading_the_rest() {
        use std::io::{Seek, Write};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("growing.log");
        fs::write(&path, b"small").unwrap();
        let mut reader = fs::File::open(&path).unwrap();
        assert!(reader.metadata().unwrap().len() <= 8);

        // Deterministically place the growth between the successful initial
        // size check and the read, without relying on a racing writer thread.
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[b'x'; 4096])
            .unwrap();
        let error = read_bounded(&mut reader, 8).unwrap_err();
        assert!(matches!(
            error,
            ExtractError::FileTooLarge {
                bytes: 9,
                max_bytes: 8
            }
        ));
        assert_eq!(reader.stream_position().unwrap(), 9);
        assert_eq!(reader.metadata().unwrap().len(), 4101);
    }

    #[test]
    fn bounded_read_accepts_exact_limit_and_handles_zero_budget() {
        assert_eq!(read_bounded(&b"exact"[..], 5).unwrap(), b"exact");
        assert!(read_bounded(&b""[..], 0).unwrap().is_empty());
        assert!(matches!(
            read_bounded(&b"x"[..], 0),
            Err(ExtractError::FileTooLarge {
                bytes: 1,
                max_bytes: 0
            })
        ));
    }

    #[test]
    fn supports_falls_back_to_path_extension() {
        let ctx = ExtractContext {
            path: "/tmp/file.TXT",
            max_bytes: 1024,
            max_chars: 1024,
            ext_hint: None,
            mime_hint: None,
        };
        let simple = SimpleTextExtractor;
        assert!(simple.supports(&ctx));
    }

    #[test]
    fn resolve_ext_prefers_hint_over_path() {
        let ctx = ExtractContext {
            path: "/tmp/file.md",
            max_bytes: 10,
            max_chars: 10,
            ext_hint: Some("txt"),
            mime_hint: None,
        };
        assert_eq!(resolve_ext(&ctx).as_deref(), Some("txt"));
    }

    #[test]
    fn empty_stack_returns_unsupported() {
        let ctx = ExtractContext {
            path: "/tmp/file.unknown",
            max_bytes: 10,
            max_chars: 10,
            ext_hint: None,
            mime_hint: None,
        };
        let stack = ExtractorStack::new(vec![]);
        let err = stack.extract(DocKey::from_parts(1, 1), &ctx).unwrap_err();
        assert!(err.to_string().contains("unsupported"));
    }

    #[test]
    fn with_extractous_disabled_uses_simple_only() {
        let stack = ExtractorStack::with_extractous_enabled(false);
        assert_eq!(stack.backends.len(), 2);
    }

    #[cfg(feature = "extractous_backend")]
    #[test]
    fn extractous_stack_includes_backend() {
        // This does not require GraalVM at runtime unless extract() is invoked.
        let stack = ExtractorStack::with_extractous_enabled(true);
        assert!(
            stack.backends.iter().any(|b| b.name() == "extractous"),
            "extractous backend should be present when feature is enabled"
        );
    }

    #[cfg(feature = "extractous_backend")]
    #[test]
    fn extractous_rejects_large_file() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.docx");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[0u8; 128]).unwrap();

        let ctx = ExtractContext {
            path: path.to_str().unwrap(),
            max_bytes: 64,
            max_chars: 1024,
            ext_hint: Some("docx"),
            mime_hint: None,
        };

        let extractor = ExtractousExtractor::new();
        assert!(extractor.supports(&ctx));
        let err = extractor
            .extract(&ctx, DocKey::from_parts(9, 9))
            .unwrap_err();
        assert!(matches!(
            err,
            ExtractError::FileTooLarge {
                bytes: 128,
                max_bytes: 64
            }
        ));
    }

    #[cfg(feature = "extractous_backend")]
    #[test]
    fn extractous_smoke_if_graal_present() {
        // Only attempt when env is set to avoid failing in CI without GraalVM.
        if std::env::var("GRAALVM_HOME").is_err() && std::env::var("JAVA_HOME").is_err() {
            eprintln!("skipping extractous smoke: GRAALVM_HOME/JAVA_HOME not set");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        std::fs::write(&path, b"hello extractous").unwrap();

        let ctx = ExtractContext {
            path: path.to_str().unwrap(),
            max_bytes: 1024,
            max_chars: 2048,
            ext_hint: Some("txt"),
            mime_hint: None,
        };

        let extractor = ExtractousExtractor::new();
        assert!(extractor.supports(&ctx));
        // We don't call extract() to avoid requiring the full Graal runtime during tests.
    }
}
