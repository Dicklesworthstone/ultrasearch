use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, TextAnalyzer, TokenizerManager};

pub const LOG_ANALYZER: &str = "log_analyzer";

pub fn register_log_analyzers(manager: &TokenizerManager) {
    // Log analyzer: tailored for machine logs (timestamps, error codes, paths)
    // Splits on common delimiters but preserves sequence tokens.
    // Ideally we want to keep UUIDs/IPs intact or split them predictably.
    // Simple version:
    // 1. Standard tokenizer (whitespace + punctuation)
    // 2. Lowercase
    // 3. Ngrams? Or just simple tokenization.
    // Let's use a regex-based one if possible, or standard for now.
    // Standard is okay for logs if we don't care about special symbols too much.
    // But often logs have `error_code=500`. Standard splits on `=`. That's good.

    let log_analyzer = TextAnalyzer::builder(tantivy::tokenizer::SimpleTokenizer::default())
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(255)) // Cap extremely long tokens
        .build();

    manager.register(LOG_ANALYZER, log_analyzer);
}
