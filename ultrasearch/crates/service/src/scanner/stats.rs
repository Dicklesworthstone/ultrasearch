//! Cache real committed metadata counts without traversing the index every tick.

use anyhow::{Context, Result};
use core_types::VolumeId;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) struct IndexStatistics {
    index: meta_index::MetaIndex,
    reader: tantivy::IndexReader,
    opstamp: Option<tantivy::Opstamp>,
    refreshed: Option<Instant>,
    volumes: BTreeMap<VolumeId, (u64, u64)>,
}

impl IndexStatistics {
    pub fn new(path: &Path) -> Result<Self> {
        let index = meta_index::open_or_create_index(path)?;
        let reader = meta_index::open_reader(&index)?;
        Ok(Self {
            index,
            reader,
            opstamp: None,
            refreshed: None,
            volumes: BTreeMap::new(),
        })
    }

    /// Counts may lag by thirty seconds; pending work and journal positions are
    /// published every tick. Idle ticks never rescan every indexed document.
    pub fn refresh(&mut self) -> Result<()> {
        if self
            .refreshed
            .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return Ok(());
        }
        let opstamp = self.index.index.load_metas()?.opstamp;
        if self.opstamp != Some(opstamp) {
            self.reader.reload()?;
            let searcher = self.reader.searcher();
            let mut totals = BTreeMap::<VolumeId, (u64, u64)>::new();
            for segment in searcher.segment_readers() {
                let volumes = segment.fast_fields().u64("volume")?;
                let sizes = segment.fast_fields().u64("size")?;
                for doc in segment.doc_ids_alive() {
                    let volume = VolumeId::try_from(
                        volumes
                            .first(doc)
                            .context("indexed document has no volume")?,
                    )?;
                    let size = sizes.first(doc).context("indexed document has no size")?;
                    let total = totals.entry(volume).or_default();
                    total.0 = total.0.saturating_add(1);
                    total.1 = total.1.saturating_add(size);
                }
            }
            self.volumes = totals;
            self.opstamp = Some(opstamp);
        }
        self.refreshed = Some(Instant::now());
        Ok(())
    }

    pub fn volume(&self, volume: VolumeId) -> (u64, u64) {
        self.volumes.get(&volume).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{DocKey, FileFlags, FileMeta};

    #[test]
    fn statistics_count_replacements_and_deletions_once() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut statistics = IndexStatistics::new(root.path())?;
        let mut writer = meta_index::create_writer(
            &statistics.index,
            &meta_index::WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let key = DocKey::from_parts(1, 0xabcd_0000_0000_0042);
        for size in [10, 80] {
            let meta = FileMeta::new(
                key,
                1,
                None,
                "report.txt".into(),
                None,
                size,
                0,
                0,
                FileFlags::empty(),
            );
            meta_index::add_file_meta_batch(&mut writer, &statistics.index.fields, [meta])?;
        }
        writer.commit()?;
        statistics.refresh()?;
        assert_eq!(statistics.volume(1), (1, 80));
        assert_eq!(statistics.volume(2), (0, 0));
        meta_index::delete_doc(&mut writer, &statistics.index.fields, key);
        writer.commit()?;
        statistics.refreshed = None;
        statistics.refresh()?;
        assert_eq!(statistics.volume(1), (0, 0));
        Ok(())
    }
}
