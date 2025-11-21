//! Content extraction facade.
//!
//! This module defines the traits and types for the extraction pipeline. The
//! actual adapters (Extractous, IFilter, OCR) will be wired incrementally; for
//! c00.5 we provide compile-ready scaffolding with minimal logic.

use anyhow::Result;
use core_types::DocKey;
use std::fs;
use std::path::Path;
use tracing::instrument;

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
}

/// Ordered stack of extractors with first-win semantics.
pub struct ExtractorStack {
    backends: Vec<Box<dyn Extractor + Send + Sync>>,
}

impl ExtractorStack {
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
        let meta = fs::metadata(path).map_err(|e| ExtractError::Failed(e.to_string()))?;
        let max_bytes = ctx.max_bytes as u64;
        if meta.len() > max_bytes {
            return Err(ExtractError::FileTooLarge {
                bytes: meta.len(),
                max_bytes,
            });
        }

        let data = fs::read(path).map_err(|e| ExtractError::Failed(e.to_string()))?;
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

fn resolve_ext(ctx: &ExtractContext) -> Option<String> {
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
        match err {
            ExtractError::FileTooLarge { bytes, max_bytes } => {
                assert_eq!(bytes, 10);
                assert_eq!(max_bytes, 5);
            }
            other => panic!("unexpected error: {other:?}"),
        }
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
}
