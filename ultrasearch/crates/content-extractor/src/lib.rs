//! Content extraction facade.
//!
//! Source text and optional rich-document extraction share bounded inputs and
//! explicit omission/failure semantics. Verified handle and snapshot entry
//! points never fall back to reopening a mutable source pathname.

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

    /// Extract from an immutable, bounded snapshot captured by the caller.
    /// Implementations must not reopen `ctx.path` to read document contents.
    fn extract_bytes(
        &self,
        _ctx: &ExtractContext,
        _key: DocKey,
        _bytes: &[u8],
    ) -> Result<ExtractedContent, ExtractError> {
        Err(ExtractError::Unsupported(format!(
            "{} does not support extraction from verified snapshot bytes",
            self.name()
        )))
    }
}

/// Ordered stack of extractors with first-success semantics. Only an explicit
/// Unsupported result permits fallback; read, parse, and size failures remain
/// failures so ingestion cannot acknowledge a silently empty replacement.
pub struct ExtractorStack {
    backends: Vec<Box<dyn Extractor + Send + Sync>>,
}

impl ExtractorStack {
    /// Simple default stack. Unknown formats are explicit content omissions.
    pub fn with_defaults() -> Self {
        Self::simple_only()
    }

    /// Simple-only stack (no external dependencies).
    pub fn simple_only() -> Self {
        Self::new(vec![Box::new(SimpleTextExtractor)])
    }

    /// Build a stack optionally including Extractous when the feature is enabled.
    pub fn with_extractous_enabled(enable: bool) -> Self {
        if enable {
            #[cfg(feature = "extractous_backend")]
            {
                return Self::new(vec![
                    Box::new(SimpleTextExtractor),
                    Box::new(ExtractousExtractor::new()),
                ]);
            }
        }
        Self::simple_only()
    }

    pub fn new(backends: Vec<Box<dyn Extractor + Send + Sync>>) -> Self {
        Self { backends }
    }

    /// Run supporting backends until one succeeds or reports a real failure.
    #[instrument(skip(self, ctx))]
    pub fn extract(&self, key: DocKey, ctx: &ExtractContext) -> Result<ExtractedContent> {
        self.extract_with(ctx, |backend| backend.extract(ctx, key))
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
        self.extract_with(ctx, |backend| {
            // A declining backend may have consumed a header. Its successor
            // must see the same verified file from the beginning, not a suffix
            // and never a reopened (possibly replaced) pathname.
            let mut source = file;
            source
                .rewind()
                .map_err(|error| ExtractError::Failed(error.to_string()))?;
            backend.extract_file(ctx, key, file)
        })
    }

    /// Process a verified snapshot after filesystem protection has been released.
    /// Path-only backends must explicitly decline instead of rereading a name.
    #[instrument(skip(self, ctx, bytes))]
    pub fn extract_bytes(
        &self,
        key: DocKey,
        ctx: &ExtractContext,
        bytes: &[u8],
    ) -> Result<ExtractedContent> {
        if bytes.len() > ctx.max_bytes {
            return Err(ExtractError::FileTooLarge {
                bytes: bytes.len() as u64,
                max_bytes: ctx.max_bytes as u64,
            }
            .into());
        }
        self.extract_with(ctx, |backend| backend.extract_bytes(ctx, key, bytes))
    }

    fn extract_with(
        &self,
        ctx: &ExtractContext,
        mut extract: impl FnMut(&dyn Extractor) -> Result<ExtractedContent, ExtractError>,
    ) -> Result<ExtractedContent> {
        let mut unsupported = None;
        for backend in &self.backends {
            if !backend.supports(ctx) {
                continue;
            }
            match extract(backend.as_ref()) {
                Err(error @ ExtractError::Unsupported(_)) => unsupported = Some(error),
                result => return result.map_err(Into::into),
            }
        }
        Err(unsupported
            .unwrap_or_else(|| {
                ExtractError::Unsupported(resolve_ext(ctx).unwrap_or_else(|| "unknown".into()))
            })
            .into())
    }
}

/// Explicit no-content extractor for caller-supplied stacks and test fixtures.
/// Production stacks omit this backend so unsupported formats remain visible.
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

    fn extract_bytes(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        _bytes: &[u8],
    ) -> Result<ExtractedContent, ExtractError> {
        self.extract(ctx, key)
    }
}

/// Plain-text extractor for source code, configuration, and lightweight markup.
pub struct SimpleTextExtractor;

impl Extractor for SimpleTextExtractor {
    fn name(&self) -> &'static str {
        "simple-text"
    }

    fn supports(&self, ctx: &ExtractContext) -> bool {
        if let Some(ext) = resolve_ext(ctx) {
            matches!(
                ext.as_str(),
                "txt"
                    | "log"
                    | "md"
                    | "markdown"
                    | "json"
                    | "jsonl"
                    | "toml"
                    | "csv"
                    | "tsv"
                    | "rs"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "mjs"
                    | "cjs"
                    | "py"
                    | "pyi"
                    | "go"
                    | "java"
                    | "c"
                    | "h"
                    | "cc"
                    | "cpp"
                    | "cxx"
                    | "hh"
                    | "hpp"
                    | "hxx"
                    | "cs"
                    | "swift"
                    | "kt"
                    | "kts"
                    | "rb"
                    | "sh"
                    | "bash"
                    | "zsh"
                    | "fish"
                    | "ps1"
                    | "psm1"
                    | "psd1"
                    | "bat"
                    | "cmd"
                    | "yaml"
                    | "yml"
                    | "ini"
                    | "cfg"
                    | "conf"
                    | "xml"
                    | "html"
                    | "htm"
                    | "css"
                    | "scss"
                    | "sql"
                    | "tex"
                    | "rst"
                    | "adoc"
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
        self.extract_bytes(ctx, key, &data)
    }

    fn extract_bytes(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        bytes: &[u8],
    ) -> Result<ExtractedContent, ExtractError> {
        if bytes.len() > ctx.max_bytes {
            return Err(ExtractError::FileTooLarge {
                bytes: bytes.len() as u64,
                max_bytes: ctx.max_bytes as u64,
            });
        }
        if is_probably_binary(bytes) {
            return Err(ExtractError::Unsupported("binary".into()));
        }

        let text_raw = String::from_utf8_lossy(bytes);
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
        extractous::Extractor::new().set_extract_string_max_length(extractous_text_limit(ctx))
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

    fn supports(&self, _ctx: &ExtractContext) -> bool {
        // Extractous detects formats from the supplied bytes. Probing a live
        // path here would discard a valid snapshot after rename/deletion, or
        // let a replacement file decide which backend reads the original.
        true
    }

    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        let file = fs::File::open(ctx.path).map_err(|e| ExtractError::Failed(e.to_string()))?;
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

        let mut source = file;
        source
            .rewind()
            .map_err(|e| ExtractError::Failed(e.to_string()))?;
        // Read no more than one byte past the input policy, even after growth.
        // Parse only these bytes, never the possibly replaced ctx.path.
        let bytes = read_bounded(source, ctx.max_bytes)?;
        self.extract_bytes(ctx, key, &bytes)
    }

    fn extract_bytes(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        bytes: &[u8],
    ) -> Result<ExtractedContent, ExtractError> {
        if bytes.len() > ctx.max_bytes {
            return Err(ExtractError::FileTooLarge {
                bytes: bytes.len() as u64,
                max_bytes: ctx.max_bytes as u64,
            });
        }
        if bytes.is_empty() {
            // An empty observed file has no text. Avoid turning a newly
            // created zero-byte rich document into a perpetual parser retry.
            return Ok(bounded_extractous_output(key, "", ctx));
        }
        let engine = self.build_engine(ctx);
        let (text, _metadata) = engine
            .extract_bytes_to_string(bytes)
            .map_err(|e| ExtractError::Failed(e.to_string()))?;
        Ok(bounded_extractous_output(key, &text, ctx))
    }
}

/// Bound the native string and leave room to observe truncation. UTF-16 may
/// use two units per Rust char; UTF-8 uses at least as many bytes as UTF-16
/// units. Always request a positive limit, including a zero-output policy.
#[cfg(any(feature = "extractous_backend", test))]
fn extractous_text_limit(ctx: &ExtractContext) -> i32 {
    ctx.max_bytes
        .min(ctx.max_chars.saturating_mul(2))
        .saturating_add(1)
        .min(i32::MAX as usize) as i32
}

#[cfg(any(feature = "extractous_backend", test))]
fn bounded_extractous_output(key: DocKey, raw: &str, ctx: &ExtractContext) -> ExtractedContent {
    // A string at the engine ceiling may be incomplete, even if it happens
    // to fit our UTF-8/char limits. Never describe that as complete extraction.
    let engine_limited = raw.encode_utf16().count() >= extractous_text_limit(ctx) as usize;
    let (text, truncated, bytes_processed) = enforce_limits_str(raw, ctx);
    ExtractedContent {
        key,
        text,
        lang: None,
        truncated: truncated || engine_limited,
        content_lang: None,
        bytes_processed,
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
        let stack = ExtractorStack::new(vec![Box::new(PathOnly)]);
        assert_eq!(stack.extract(key, &ctx).unwrap().key, key);
        let error = stack.extract_file(key, &ctx, &file).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ExtractError>(),
            Some(ExtractError::Unsupported(reason)) if reason.contains("verified file handle")
        ));
        let error = stack.extract_bytes(key, &ctx, b"verified").unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ExtractError>(),
            Some(ExtractError::Unsupported(reason)) if reason.contains("verified snapshot bytes")
        ));
    }

    #[test]
    fn snapshot_bytes_survive_path_replacement_and_disappearance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.txt");
        fs::write(&path, "verified original").unwrap();
        let captured = fs::read(&path).unwrap();
        let ctx = ExtractContext {
            path: path.to_str().unwrap(),
            max_bytes: 1024,
            max_chars: 1024,
            ext_hint: Some("txt"),
            mime_hint: None,
        };
        let key = DocKey::from_parts(4, 52);
        let stack = ExtractorStack::simple_only();
        fs::write(&path, "wrong replacement").unwrap();
        let out = stack.extract_bytes(key, &ctx, &captured).unwrap();
        assert_eq!(out.key, key);
        assert_eq!(out.text, "verified original");
        assert_eq!(out.bytes_processed, captured.len());
        fs::rename(&path, dir.path().join("retained.txt")).unwrap();
        assert_eq!(
            stack.extract_bytes(key, &ctx, &captured).unwrap().text,
            "verified original"
        );
    }

    #[test]
    fn snapshot_bytes_apply_size_character_and_binary_policy() {
        let key = DocKey::from_parts(4, 52);
        let mut ctx = ExtractContext {
            path: "missing.txt",
            max_bytes: 6,
            max_chars: 2,
            ext_hint: Some("txt"),
            mime_hint: None,
        };
        let out = SimpleTextExtractor
            .extract_bytes(&ctx, key, "ééé".as_bytes())
            .unwrap();
        assert_eq!(out.text, "éé");
        assert_eq!(out.bytes_processed, 4);
        assert!(out.truncated);

        ctx.max_bytes = 5;
        assert!(matches!(
            SimpleTextExtractor.extract_bytes(&ctx, key, "ééé".as_bytes()),
            Err(ExtractError::FileTooLarge {
                bytes: 6,
                max_bytes: 5
            })
        ));
        assert!(matches!(
            SimpleTextExtractor.extract_bytes(&ctx, key, b"a\0b"),
            Err(ExtractError::Unsupported(reason)) if reason == "binary"
        ));
        ctx.max_bytes = 0;
        assert_eq!(
            SimpleTextExtractor
                .extract_bytes(&ctx, key, b"")
                .unwrap()
                .text,
            ""
        );
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
        assert_eq!(stack.backends.len(), 1);
        assert_eq!(stack.backends[0].name(), "simple-text");
    }

    #[test]
    fn common_code_and_configuration_formats_extract_real_content_in_every_mode() {
        let dir = tempfile::tempdir().unwrap();
        let stack = ExtractorStack::simple_only();
        let key = DocKey::from_parts(42, 0xabcd_0000_0000_0042);
        let source = "searchable source λ\n";
        for ext in [
            "py", "pyi", "go", "java", "c", "h", "cc", "cpp", "cxx", "hh", "hpp", "hxx",
            "cs", "swift", "kt", "kts", "rb", "js", "jsx", "mjs", "cjs", "sh", "bash",
            "zsh", "fish", "ps1", "psm1", "psd1", "bat", "cmd", "yaml", "yml", "ini",
            "cfg", "conf", "xml", "html", "htm", "css", "scss", "sql", "tex", "rst",
            "adoc", "markdown", "tsv",
        ] {
            let path = dir.path().join(format!("input.{}", ext.to_ascii_uppercase()));
            fs::write(&path, source).unwrap();
            let file = fs::File::open(&path).unwrap();
            let ctx = ExtractContext {
                path: path.to_str().unwrap(),
                max_bytes: 1024,
                max_chars: 1024,
                ext_hint: None,
                mime_hint: None,
            };
            for output in [
                stack.extract(key, &ctx).unwrap(),
                stack.extract_file(key, &ctx, &file).unwrap(),
                stack.extract_bytes(key, &ctx, source.as_bytes()).unwrap(),
            ] {
                assert_eq!(output.key, key);
                assert_eq!(output.text, source, "extension={ext}");
                assert_eq!(output.bytes_processed, source.len());
                assert!(!output.truncated);
            }
        }
    }

    #[test]
    fn production_stacks_do_not_claim_unsupported_or_binary_files_are_empty_documents() {
        let dir = tempfile::tempdir().unwrap();
        let key = DocKey::from_parts(2, 7);
        for (name, bytes) in [("unknown.weird", &b"data"[..]), ("binary.py", &b"a\0b"[..])] {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            let file = fs::File::open(&path).unwrap();
            let ctx = ExtractContext {
                path: path.to_str().unwrap(),
                max_bytes: 1024,
                max_chars: 1024,
                ext_hint: None,
                mime_hint: None,
            };
            for stack in [
                ExtractorStack::with_defaults(),
                ExtractorStack::with_extractous_enabled(false),
            ] {
                for result in [
                    stack.extract(key, &ctx),
                    stack.extract_file(key, &ctx, &file),
                    stack.extract_bytes(key, &ctx, bytes),
                ] {
                    assert!(matches!(
                        result.unwrap_err().downcast_ref::<ExtractError>(),
                        Some(ExtractError::Unsupported(_))
                    ));
                }
            }
        }
    }

    #[derive(Clone, Copy)]
    enum BackendFailure {
        Unsupported,
        Failed,
        TooLarge,
    }

    struct RejectingBackend(BackendFailure);

    impl Extractor for RejectingBackend {
        fn name(&self) -> &'static str {
            "rejecting-fixture"
        }

        fn supports(&self, _: &ExtractContext) -> bool {
            true
        }

        fn extract(&self, _: &ExtractContext, _: DocKey) -> Result<ExtractedContent, ExtractError> {
            Err(match self.0 {
                BackendFailure::Unsupported => {
                    ExtractError::Unsupported("try the next backend".into())
                }
                BackendFailure::Failed => {
                    ExtractError::Failed("do not acknowledge this failure".into())
                }
                BackendFailure::TooLarge => ExtractError::FileTooLarge {
                    bytes: 2048,
                    max_bytes: 1024,
                },
            })
        }

        fn extract_file(
            &self,
            ctx: &ExtractContext,
            key: DocKey,
            file: &fs::File,
        ) -> Result<ExtractedContent, ExtractError> {
            // Consume a real header before declining. The next backend must
            // still get the entire original file, not just the unread suffix.
            let mut source = file;
            let mut header = [0; 2];
            source.read_exact(&mut header).unwrap();
            self.extract(ctx, key)
        }

        fn extract_bytes(
            &self,
            ctx: &ExtractContext,
            key: DocKey,
            _: &[u8],
        ) -> Result<ExtractedContent, ExtractError> {
            self.extract(ctx, key)
        }
    }

    #[test]
    fn only_unsupported_permits_fallback_in_all_three_entry_points() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fallback.txt");
        fs::write(&path, "full original").unwrap();
        let file = fs::File::open(&path).unwrap();
        let key = DocKey::from_parts(2, 8);
        let ctx = ExtractContext {
            path: path.to_str().unwrap(),
            max_bytes: 1024,
            max_chars: 1024,
            ext_hint: Some("txt"),
            mime_hint: None,
        };
        for failure in [
            BackendFailure::Unsupported,
            BackendFailure::Failed,
            BackendFailure::TooLarge,
        ] {
            let stack = ExtractorStack::new(vec![
                Box::new(RejectingBackend(failure)),
                Box::new(SimpleTextExtractor),
            ]);
            for result in [
                stack.extract(key, &ctx),
                stack.extract_file(key, &ctx, &file),
                stack.extract_bytes(key, &ctx, b"full original"),
            ] {
                match failure {
                    BackendFailure::Unsupported => {
                        let output = result.unwrap();
                        assert_eq!(output.text, "full original");
                        assert_eq!(output.key, key);
                    }
                    BackendFailure::Failed => assert!(matches!(
                        result.unwrap_err().downcast_ref::<ExtractError>(),
                        Some(ExtractError::Failed(_))
                    )),
                    BackendFailure::TooLarge => assert!(matches!(
                        result.unwrap_err().downcast_ref::<ExtractError>(),
                        Some(ExtractError::FileTooLarge { .. })
                    )),
                }
            }
        }
    }

    #[test]
    fn verified_fallback_never_calls_a_path_only_backend_or_reads_a_replacement() {
        struct FromPosition;
        impl Extractor for FromPosition {
            fn name(&self) -> &'static str {
                "verified-only"
            }
            fn supports(&self, _: &ExtractContext) -> bool {
                true
            }
            fn extract(
                &self,
                _: &ExtractContext,
                _: DocKey,
            ) -> Result<ExtractedContent, ExtractError> {
                panic!("verified fallback must not reopen any pathname")
            }
            fn extract_file(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
                file: &fs::File,
            ) -> Result<ExtractedContent, ExtractError> {
                // Intentionally do not rewind: the stack owns fallback cursor reset.
                let bytes = read_bounded(file, ctx.max_bytes)?;
                SimpleTextExtractor.extract_bytes(ctx, key, &bytes)
            }
            fn extract_bytes(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
                bytes: &[u8],
            ) -> Result<ExtractedContent, ExtractError> {
                SimpleTextExtractor.extract_bytes(ctx, key, bytes)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("observed.txt");
        fs::write(&path, "verified original").unwrap();
        let file = fs::File::open(&path).unwrap();
        fs::rename(&path, dir.path().join("retained-original.txt")).unwrap();
        fs::write(&path, "wrong replacement").unwrap();
        let ctx = ExtractContext {
            path: path.to_str().unwrap(),
            max_bytes: 1024,
            max_chars: 1024,
            ext_hint: Some("txt"),
            mime_hint: None,
        };
        let key = DocKey::from_parts(42, 0xfedc_0000_0000_0042);
        let stack = ExtractorStack::new(vec![
            Box::new(RejectingBackend(BackendFailure::Unsupported)),
            Box::new(FromPosition),
        ]);
        for output in [
            stack.extract_file(key, &ctx, &file).unwrap(),
            stack.extract_bytes(key, &ctx, b"verified original").unwrap(),
        ] {
            assert_eq!(output.key, key);
            assert_eq!(output.text, "verified original");
        }
    }

    #[test]
    fn snapshot_size_rejection_precedes_backend_selection() {
        struct NeverSelected;
        impl Extractor for NeverSelected {
            fn name(&self) -> &'static str {
                "never"
            }
            fn supports(&self, _: &ExtractContext) -> bool {
                panic!("over-budget input reached selection")
            }
            fn extract(
                &self,
                _: &ExtractContext,
                _: DocKey,
            ) -> Result<ExtractedContent, ExtractError> {
                unreachable!()
            }
        }
        let ctx = ExtractContext {
            path: "missing.txt",
            max_bytes: 3,
            max_chars: 1024,
            ext_hint: None,
            mime_hint: None,
        };
        let stack = ExtractorStack::new(vec![Box::new(NeverSelected)]);
        assert!(matches!(
            stack
                .extract_bytes(DocKey::from_parts(1, 2), &ctx, b"long")
                .unwrap_err()
                .downcast_ref::<ExtractError>(),
            Some(ExtractError::FileTooLarge {
                bytes: 4,
                max_bytes: 3
            })
        ));
    }

    #[test]
    fn rich_output_limits_preserve_utf8_and_report_native_truncation() {
        let key = DocKey::from_parts(42, 0xabcd_0000_0000_0042);
        let mut ctx = ExtractContext {
            path: "missing.pdf",
            max_bytes: 7,
            max_chars: 1024,
            ext_hint: Some("pdf"),
            mime_hint: None,
        };
        let output = bounded_extractous_output(key, "é🥁é", &ctx);
        assert_eq!(output.text, "é🥁");
        assert_eq!(output.bytes_processed, 6);
        assert!(output.truncated);
        assert_eq!(output.key, key);
        ctx.max_bytes = 1024;
        ctx.max_chars = 1;
        let output = bounded_extractous_output(key, "🥁🥁", &ctx);
        assert_eq!(output.text, "🥁");
        assert_eq!(output.bytes_processed, 4);
        assert!(output.truncated);
        let output = bounded_extractous_output(key, "🥁", &ctx);
        assert_eq!(output.text, "🥁");
        assert!(!output.truncated);
        ctx.max_chars = 0;
        assert_eq!(extractous_text_limit(&ctx), 1);
        assert!(bounded_extractous_output(key, "nonempty", &ctx).truncated);
        assert!(!bounded_extractous_output(key, "", &ctx).truncated);
        ctx.max_bytes = usize::MAX;
        ctx.max_chars = usize::MAX;
        assert_eq!(extractous_text_limit(&ctx), i32::MAX);
        ctx.max_bytes = 0;
        assert_eq!(extractous_text_limit(&ctx), 1);
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
    fn extractous_reads_verified_bytes_even_when_the_source_path_is_missing() {
        // Enabling the feature opts into its real native parser prerequisites.
        // Do not pass this test by checking environment variables or support()
        // without ever executing the extraction engine.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.rtf");
        let ctx = ExtractContext {
            path: path.to_str().unwrap(),
            max_bytes: 1024,
            max_chars: 2048,
            ext_hint: Some("rtf"),
            mime_hint: None,
        };
        let extractor = ExtractousExtractor::new();
        assert!(extractor.supports(&ctx));
        let key = DocKey::from_parts(42, 0xffff_0000_0000_0042);
        let snapshot = br"{\rtf1\ansi verified rich snapshot}";
        let output = extractor.extract_bytes(&ctx, key, snapshot).unwrap();
        assert!(output.text.contains("verified rich snapshot"));
        assert_eq!(output.key, key);
        assert!(!path.exists());
        let empty = extractor.extract_bytes(&ctx, key, b"").unwrap();
        assert_eq!(empty.key, key);
        assert!(empty.text.is_empty());
        assert!(!empty.truncated);
        let small = ExtractContext {
            max_bytes: 3,
            ..ctx
        };
        assert!(matches!(
            extractor.extract_bytes(&small, key, snapshot),
            Err(ExtractError::FileTooLarge { .. })
        ));
    }

    #[cfg(feature = "extractous_backend")]
    #[test]
    fn rich_pdf_and_office_snapshots_survive_replacement_and_obey_limits() {
        use std::io::{Cursor, Write};
        use zip::write::SimpleFileOptions;

        // Build a complete minimal PDF with real object offsets and a text
        // stream. The backend must parse the document, not return PDF syntax.
        let text = "BT /F1 12 Tf 72 720 Td (verifiedpdfpayload) Tj ET\n";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
            format!("<< /Length {} >>\nstream\n{text}endstream", text.len()),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            writeln!(pdf, "{} 0 obj\n{object}\nendobj", index + 1).unwrap();
        }
        let xref = pdf.len();
        pdf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
        for offset in offsets {
            writeln!(pdf, "{offset:010} 00000 n ").unwrap();
        }
        writeln!(pdf, "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF").unwrap();

        let mut office = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, xml) in [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#,
            ),
            (
                "word/document.xml",
                r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>verifiedofficepayload</w:t></w:r></w:p></w:body></w:document>"#,
            ),
        ] {
            office.start_file(name, SimpleFileOptions::default()).unwrap();
            office.write_all(xml.as_bytes()).unwrap();
        }
        let docx = office.finish().unwrap().into_inner();
        let stack = ExtractorStack::with_extractous_enabled(true);
        let root = tempfile::tempdir().unwrap();
        let key = DocKey::from_parts(42, 0xabcd_0000_0000_0042);
        for (ext, bytes, expected) in [
            ("pdf", pdf, "verifiedpdfpayload"),
            ("docx", docx, "verifiedofficepayload"),
        ] {
            let path = root.path().join(format!("source.{ext}"));
            fs::write(&path, &bytes).unwrap();
            let file = fs::File::open(&path).unwrap();
            fs::rename(&path, root.path().join(format!("retained.{ext}"))).unwrap();
            fs::write(&path, "wrong replacement payload").unwrap();
            let ctx = ExtractContext {
                path: path.to_str().unwrap(),
                max_bytes: 65536,
                max_chars: 1024,
                ext_hint: Some(ext),
                mime_hint: None,
            };
            for output in [
                stack.extract_bytes(key, &ctx, &bytes).unwrap(),
                stack.extract_file(key, &ctx, &file).unwrap(),
            ] {
                assert!(output.text.contains(expected), "{ext}: {:?}", output.text);
                assert!(!output.text.contains("wrong replacement"));
                assert_eq!(output.key, key);
                assert!(output.text.len() <= ctx.max_bytes);
                assert!(output.text.chars().count() <= ctx.max_chars);
                assert!(!output.truncated);
            }
            let limited = ExtractContext {
                max_chars: 4,
                ..ctx.clone()
            };
            let output = stack.extract_bytes(key, &limited, &bytes).unwrap();
            assert!(output.text.chars().count() <= 4);
            assert!(output.truncated);
            if ext == "docx" {
                // A misleading text extension sends binary bytes to the simple
                // backend first. Its explicit Unsupported must reach the rich
                // byte backend, never a path reread or a successful empty noop.
                let misleading = ExtractContext {
                    ext_hint: Some("txt"),
                    ..ctx
                };
                let output = stack.extract_bytes(key, &misleading, &bytes).unwrap();
                assert!(output.text.contains(expected));
            }
        }
    }
}
