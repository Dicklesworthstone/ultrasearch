use crate::embedding::cosine_similarity;
use anyhow::Result;
use core_types::DocKey;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "hnsw_rs")]
use hnsw_rs::prelude::*;

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSemanticIndex {
    entries: Vec<(DocKey, Vec<f32>)>,
}

/// A semantic index storing embeddings for documents.
pub struct SemanticIndex {
    path: Option<PathBuf>,
    entries: BTreeMap<DocKey, Vec<f32>>,
    #[cfg(feature = "hnsw_rs")]
    index: Hnsw<'static, f32, DistCosine>,
    #[cfg(feature = "hnsw_rs")]
    keys: Vec<DocKey>,
    #[cfg(feature = "hnsw_rs")]
    ids: std::collections::HashMap<DocKey, usize>,
}

impl Default for SemanticIndex {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl SemanticIndex {
    /// Create an in-memory semantic index without a backing file path.
    pub fn in_memory() -> Self {
        #[cfg(feature = "hnsw_rs")]
        let index = Hnsw::new(32, 100_000, 16, 50, DistCosine);

        Self {
            path: None,
            entries: BTreeMap::new(),
            #[cfg(feature = "hnsw_rs")]
            index,
            #[cfg(feature = "hnsw_rs")]
            keys: Vec::new(),
            #[cfg(feature = "hnsw_rs")]
            ids: std::collections::HashMap::new(),
        }
    }

    fn resolve_storage_file(path: &Path) -> PathBuf {
        if path.extension().is_some() {
            path.to_path_buf()
        } else {
            path.join("semantic_index.json")
        }
    }

    /// Open or create a semantic index at the given path.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        let mut idx = Self::in_memory();
        let storage_file = Self::resolve_storage_file(path);
        idx.path = Some(storage_file.clone());

        if storage_file.is_file() {
            let bytes = fs::read(&storage_file)?;
            let persisted: PersistedSemanticIndex = serde_json::from_slice(&bytes)?;
            for (key, vec) in persisted.entries {
                idx.insert(key, vec)?;
            }
        }

        Ok(idx)
    }

    /// Persist the semantic index atomically to disk.
    pub fn save(&self, path: Option<&Path>) -> Result<()> {
        let target = match path {
            Some(p) => Self::resolve_storage_file(p),
            None => self
                .path
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no storage path configured for SemanticIndex"))?,
        };
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let persisted = PersistedSemanticIndex {
            entries: self.entries.iter().map(|(&k, v)| (k, v.clone())).collect(),
        };
        let bytes = serde_json::to_vec(&persisted)?;
        let tmp = target.with_extension("json.tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &target)?;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add or replace a vector for a document.
    pub fn insert(&mut self, key: DocKey, vector: Vec<f32>) -> Result<()> {
        #[cfg(feature = "hnsw_rs")]
        {
            let id = *self.ids.entry(key).or_insert_with(|| {
                let id = self.keys.len();
                self.keys.push(key);
                id
            });
            self.index.insert((vector.as_slice(), id));
        }
        self.entries.insert(key, vector);
        Ok(())
    }

    /// Remove a document's vector from the index.
    pub fn remove(&mut self, key: DocKey) -> bool {
        self.entries.remove(&key).is_some()
    }

    /// Search for nearest neighbors.
    pub fn search(&self, vector: &[f32], k: usize) -> Result<Vec<(DocKey, f32)>> {
        if k == 0 || vector.is_empty() || self.entries.is_empty() {
            return Ok(Vec::new());
        }

        #[cfg(feature = "hnsw_rs")]
        {
            let ef = (self.index.get_ef_construction()).max(k * 2);
            let res = self.index.search(vector, k.saturating_mul(2).max(1), ef);
            let mut hits: Vec<(DocKey, f32)> = res
                .into_iter()
                .filter_map(|n| {
                    let key = *self.keys.get(n.d_id)?;
                    let stored = self.entries.get(&key)?;
                    Some((key, cosine_similarity(vector, stored)))
                })
                .collect();
            hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            hits.dedup_by_key(|(k, _)| *k);
            hits.truncate(k);
            Ok(hits)
        }
        #[cfg(not(feature = "hnsw_rs"))]
        {
            let mut hits: Vec<(DocKey, f32)> = self
                .entries
                .iter()
                .map(|(&key, stored)| (key, cosine_similarity(vector, stored)))
                .collect();
            hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            hits.truncate(k);
            Ok(hits)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_index_insert_search_remove_and_persist_round_trip() -> Result<()> {
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

        assert!(index.remove(first));
        assert_ne!(index.search(&[1.0, 0.0, 0.0], 1)?[0].0, first);

        let temp_dir =
            std::env::temp_dir().join(format!("ultrasearch_semantic_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        index.save(Some(&temp_dir))?;

        let reloaded = SemanticIndex::open_or_create(&temp_dir)?;
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.search(&[0.0, 1.0, 0.0], 1)?[0].0, reused);
        let _ = fs::remove_dir_all(&temp_dir);
        Ok(())
    }
}
