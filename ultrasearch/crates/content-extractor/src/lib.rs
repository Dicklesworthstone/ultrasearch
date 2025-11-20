//! Content extraction facade (stub).

use core_types::DocKey;

#[derive(Debug)]
pub struct ExtractedContent {
    pub key: DocKey,
    pub text: String,
}

pub fn extract_placeholder(key: DocKey, _path: &str) -> ExtractedContent {
    // TODO: wire Extractous / IFilter / OCR stack.
    ExtractedContent {
        key,
        text: String::new(),
    }
}
