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
}

/// Context passed to extractors (paths, limits, hints).
#[derive(Debug, Clone)]
pub struct ExtractContext<'a> {
    pub path: &'a str,
    pub max_bytes: usize,
    pub max_chars: usize,
    pub ext_hint: Option<&'a str>,
}

/// Extraction error categories.
#[derive(thiserror::Error, Debug)]
pub enum ExtractError {
    #[error("unsupported format: {0}")]
    Unsupported(String),
    #[error("extraction failed: {0}")]
    Failed(String),
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
        for backend in &self.backends {
            if backend.supports(ctx) {
                return backend.extract(ctx, key).map_err(|e| e.into());
            }
        }
        Err(anyhow::anyhow!(ExtractError::Unsupported(
            ctx.ext_hint.unwrap_or("unknown").to_string()
        )))
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
        let truncated = false;
        Ok(ExtractedContent {
            key,
            text: String::new(),
            lang: None,
            truncated,
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
        match ctx.ext_hint.unwrap_or("").to_ascii_lowercase().as_str() {
            "txt" | "log" | "md" | "json" | "jsonl" | "toml" | "rs" | "ts" | "tsx" => true,
            _ => false,
        }
    }

    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        let path = Path::new(ctx.path);
        let meta = fs::metadata(path).map_err(|e| ExtractError::Failed(e.to_string()))?;
        if meta.len() as usize > ctx.max_bytes {
            return Err(ExtractError::Unsupported("file too large for simple extractor".into()));
        }

        let mut text = fs::read_to_string(path).map_err(|e| ExtractError::Failed(e.to_string()))?;
        let truncated = if text.len() > ctx.max_chars {
            text.truncate(ctx.max_chars);
            true
        } else {
            false
        };

        Ok(ExtractedContent {
            key,
            text,
            lang: None,
            truncated,
        })
    }
}

/// Truncate helper applied by real extractors to enforce `max_chars`.
pub fn enforce_char_limit(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() > max_chars {
        let trimmed: String = text.chars().take(max_chars).collect();
        (trimmed, true)
    } else {
        (text.to_string(), false)
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
        };
        let stack = ExtractorStack::new(vec![Box::new(NoopExtractor)]);
        let out = stack.extract(DocKey::from_parts(1, 42), &ctx).unwrap();
        assert!(out.text.is_empty());
        assert!(!out.truncated);
    }

    #[test]
    fn enforce_char_limit_truncates() {
        let s = "abcdef";
        let (trimmed, was_truncated) = enforce_char_limit(s, 3);
        assert_eq!(trimmed, "abc");
        assert!(was_truncated);
    }
}
