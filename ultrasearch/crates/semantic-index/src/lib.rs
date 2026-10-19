//! Semantic / vector search index and deterministic text embedder.

pub mod ann;
pub mod embedding;

pub use ann::hnsw::SemanticIndex;
use core_types::DocKey;
pub use embedding::{DEFAULT_EMBEDDING_DIM, TextEmbedder, cosine_similarity, embed_text};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone)]
pub struct VectorEmbedding(pub Vec<f32>);

static GLOBAL_INDEX: OnceLock<Mutex<SemanticIndex>> = OnceLock::new();

fn global_index() -> &'static Mutex<SemanticIndex> {
    GLOBAL_INDEX.get_or_init(|| Mutex::new(SemanticIndex::in_memory()))
}

pub fn add_embedding(key: DocKey, embedding: VectorEmbedding) {
    if let Ok(mut guard) = global_index().lock() {
        let _ = guard.insert(key, embedding.0);
    }
}

pub fn remove_embedding(key: DocKey) -> bool {
    global_index()
        .lock()
        .map(|mut guard| guard.remove(key))
        .unwrap_or(false)
}

pub fn search_embeddings(query: &[f32], k: usize) -> Vec<(DocKey, f32)> {
    global_index()
        .lock()
        .ok()
        .and_then(|guard| guard.search(query, k).ok())
        .unwrap_or_default()
}
