use anyhow::Result;
use core_types::DocKey;
use std::path::Path;

#[cfg(feature = "hnsw_rs")]
use hnsw_rs::prelude::*;

/// A semantic index storing embeddings for document chunks.
pub struct SemanticIndex {
    #[cfg(feature = "hnsw_rs")]
    index: Hnsw<'static, f32, DistCosine>,
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
            // Parameters chosen for balanced accuracy vs. memory; will be tuned when wiring real data.
            let max_nb_connection = 32;
            let max_elements_hint = 100_000;
            let max_layer = 16;
            let ef_construction = 50;
            let index = Hnsw::new(
                max_nb_connection,
                max_elements_hint,
                max_layer,
                ef_construction,
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
            let id = _key.0 as usize;
            self.index.insert((_vector.as_slice(), id));
        }
        Ok(())
    }

    /// Search for nearest neighbors.
    pub fn search(&self, _vector: &[f32], _k: usize) -> Result<Vec<(DocKey, f32)>> {
        #[cfg(feature = "hnsw_rs")]
        {
            let k = _k.max(1);
            let ef = (self.index.get_ef_construction()).max(k * 2);
            let res = self.index.search(_vector, k, ef);
            let hits = res
                .into_iter()
                .map(|n| {
                    // DistCosine returns a distance in [0,2]; convert to a similarity-ish score.
                    let score = 1.0 - n.distance;
                    (DocKey(n.d_id as u64), score)
                })
                .collect();
            return Ok(hits);
        }
        #[cfg(not(feature = "hnsw_rs"))]
        {
            Ok(Vec::new())
        }
    }
}
