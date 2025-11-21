use anyhow::Result;
use core_types::DocKey;
use std::path::Path;

#[cfg(feature = "hnsw_rs")]
use hnsw_rs::prelude::*;

/// A semantic index storing embeddings for document chunks.
pub struct SemanticIndex {
    #[cfg(feature = "hnsw_rs")]
    index: Hnsw<f32, DistCosine>,
    #[cfg(not(feature = "hnsw_rs"))]
    _stub: (),
}

impl SemanticIndex {
    /// Open or create a semantic index at the given path.
    pub fn open_or_create(_path: &Path) -> Result<Self> {
        // TODO: Load from disk if exists.
        // For now, create in-memory structure.

        #[cfg(feature = "hnsw_rs")]
        {
            let index = Hnsw::new(
                100, // max elements (stub)
                100, // M
                16,  // ef_construction
                10,  // ef_search
                DistCosine,
            );
            Ok(Self { index })
        }

        #[cfg(not(feature = "hnsw_rs"))]
        Ok(Self { _stub: () })
    }

    /// Add a vector for a document.
    pub fn insert(&mut self, _key: DocKey, _vector: Vec<f32>) -> Result<()> {
        #[cfg(feature = "hnsw_rs")]
        {
            // hnsw_rs uses usize or u64 IDs. DocKey is u64 compatible.
            // self.index.insert(&vector, key.0 as usize);
            // But hnsw_rs might require slice.
            // Unimplemented in stub.
        }
        Ok(())
    }

    /// Search for nearest neighbors.
    pub fn search(&self, _vector: &[f32], _k: usize) -> Result<Vec<(DocKey, f32)>> {
        Ok(Vec::new())
    }
}
