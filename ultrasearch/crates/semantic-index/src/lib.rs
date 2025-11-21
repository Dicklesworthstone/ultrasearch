//! Semantic / vector search scaffolding (stub).

pub mod ann;
pub mod embedding;

use core_types::DocKey;

#[derive(Debug)]
pub struct VectorEmbedding(pub Vec<f32>);

pub fn add_embedding(_key: DocKey, _embedding: VectorEmbedding) {
    // TODO: wire HNSW / ANN backend.
}
