//! Semantic / vector search scaffolding (stub).

use core_types::DocKey;

#[derive(Debug)]
pub struct VectorEmbedding(pub Vec<f32>);

pub fn add_embedding(_key: DocKey, _embedding: VectorEmbedding) {
    // TODO: wire HNSW / ANN backend.
}
