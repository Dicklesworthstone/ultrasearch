use anyhow::Result;
use core_types::DocKey;
use std::path::Path;

#[cfg(feature = "hnsw_rs")]
use hnsw_rs::prelude::*;

/// A semantic index storing embeddings for document chunks.
pub struct SemanticIndex {
    #[cfg(feature = "hnsw_rs")]
    index: Hnsw<'static, f32, DistCosine>,
    #[cfg(feature = "hnsw_rs")]
    keys: Vec<DocKey>,
    #[cfg(feature = "hnsw_rs")]
    ids: std::collections::HashMap<DocKey, usize>,
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
            Ok(Self {
                index,
                keys: Vec::new(),
                ids: std::collections::HashMap::new(),
            })
        }

        #[cfg(not(feature = "hnsw_rs"))]
        Ok(Self { _stub: () })
    }

    /// Add a vector for a document.
    pub fn insert(&mut self, _key: DocKey, _vector: Vec<f32>) -> Result<()> {
        #[cfg(feature = "hnsw_rs")]
        {
            // HNSW accepts usize labels, which cannot contain a volume plus a
            // full FRN. Use a dense label and retain the lossless key mapping.
            let id = *self.ids.entry(_key).or_insert_with(|| {
                let id = self.keys.len();
                self.keys.push(_key);
                id
            });
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
                .filter_map(|n| {
                    // DistCosine returns a distance in [0,2]; convert to a similarity-ish score.
                    let score = 1.0 - n.distance;
                    self.keys.get(n.d_id).copied().map(|key| (key, score))
                })
                .collect();
            Ok(hits)
        }
        #[cfg(not(feature = "hnsw_rs"))]
        {
            Ok(Vec::new())
        }
    }
}

#[cfg(all(test, feature = "hnsw_rs"))]
mod tests {
    use super::*;

    #[test]
    fn dense_hnsw_labels_preserve_volume_and_frn_sequence() -> Result<()> {
        let mut index = SemanticIndex::open_or_create(Path::new("unused"))?;
        let first = DocKey::from_parts(1, 0x0001_0000_0000_002a);
        let reused = DocKey::from_parts(1, 0x0002_0000_0000_002a);
        let other_volume = DocKey::from_parts(2, first.file_id());
        index.insert(first, vec![1.0, 0.0, 0.0])?;
        index.insert(reused, vec![0.0, 1.0, 0.0])?;
        index.insert(other_volume, vec![0.0, 0.0, 1.0])?;
        assert_eq!(index.search(&[1.0, 0.0, 0.0], 1)?[0].0, first);
        assert_eq!(index.search(&[0.0, 1.0, 0.0], 1)?[0].0, reused);
        assert_eq!(index.search(&[0.0, 0.0, 1.0], 1)?[0].0, other_volume);
        Ok(())
    }
}
