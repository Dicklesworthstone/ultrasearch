//! Durable intent and checkpoints for the single journal ingestion lane.
//!
//! A checkpoint never describes queued work: it describes both committed indices.
//! The pending batch is written first and replayed after a crash. Replacements and
//! deletions are idempotent, so a crash between the two index commits is safe.

use anyhow::{Context, Result, bail, ensure};
use core_types::{DocKey, FileMeta, VolumeId, config::AppConfig};
use ntfs_watcher::{FileEvent, JournalCursor, VolumeInfo};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

use crate::dispatcher::job_dispatch::{IndexBatch, JobOperation, JobSpec};
use crate::scheduler_runtime::content_job_from_meta;

pub(super) const BATCH_LIMIT: usize = 1024;
const STATE_VERSION: u32 = 2;
const MAX_STATE_BYTES: u64 = 32 * 1024 * 1024;

/// Policy represented by a volume's materialized content index. Missing policy
/// in an older checkpoint requires reconciliation instead of assuming coverage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ContentPolicy {
    pub enabled: bool,
    pub max_bytes_per_file: u64,
    pub max_chars_per_file: u64,
}

impl ContentPolicy {
    pub fn for_volume(volume: &VolumeInfo, cfg: &AppConfig) -> Self {
        let automatic = cfg.volumes.is_empty() && cfg.content_index_volumes.is_empty();
        let enabled = automatic
            || volume.drive_letters.iter().any(|letter| {
                cfg.content_index_volumes
                    .iter()
                    .any(|mount| mount.eq_ignore_ascii_case(&format!("{letter}:\\")))
            });
        Self {
            enabled,
            max_bytes_per_file: cfg.extract.max_bytes_per_file,
            max_chars_per_file: cfg.extract.max_chars_per_file,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct VolumeCheckpoint {
    pub guid: String,
    pub id: VolumeId,
    pub cursor: Option<JournalCursor>,
    pub needs_scan: bool,
    pub catching_up: bool,
    #[serde(default)]
    pub content_policy: Option<ContentPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) enum MetadataChange {
    Upsert(FileMeta),
    Delete(DocKey),
}

impl MetadataChange {
    pub fn key(&self) -> DocKey {
        match self {
            Self::Upsert(meta) => meta.key,
            Self::Delete(key) => *key,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PendingBatch {
    pub volume: VolumeId,
    pub worker: IndexBatch,
    pub metadata: Vec<MetadataChange>,
    pub next_cursor: Option<JournalCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Checkpoints {
    version: u32,
    generation: Uuid,
    meta_path: String,
    content_path: String,
    #[serde(default)]
    pub retired_indices: Vec<String>,
    /// Last fully acknowledged durable batch and its exact physical commits.
    /// Older state without this evidence must rebuild before trusting its USN.
    #[serde(default)]
    pub completed_batch: Option<Uuid>,
    #[serde(default)]
    pub meta_commit: Option<Uuid>,
    #[serde(default)]
    pub content_commit: Option<Uuid>,
    pub volumes: Vec<VolumeCheckpoint>,
    pub pending: Option<PendingBatch>,
}

pub(super) struct StateStore {
    path: PathBuf,
    _lock: Arc<File>,
    pub state: Checkpoints,
}

impl StateStore {
    /// Open the state and explicitly rebuild incompatible or replaced indices.
    /// Old index directories are retained, never interpreted using the new schema.
    pub fn open(cfg: &AppConfig) -> Result<Self> {
        fs::create_dir_all(&cfg.paths.state_dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(Path::new(&cfg.paths.state_dir).join("ingestion.lock"))?;
        lock.try_lock()
            .context("another service owns this ingestion state")?;
        let path = Path::new(&cfg.paths.state_dir).join("ingestion-v2.json");
        let previous = if path.exists() {
            let file = File::open(&path)?;
            ensure!(
                file.metadata()?.len() <= MAX_STATE_BYTES,
                "ingestion state exceeds size limit"
            );
            let state: Checkpoints = serde_json::from_reader(file.take(MAX_STATE_BYTES))
                .context("invalid ingestion state; refusing to guess volume identities")?;
            ensure!(
                state.version == STATE_VERSION,
                "unsupported ingestion state version"
            );
            validate_state(&state)?;
            Some(state)
        } else {
            None
        };

        let mut rebuild = previous.as_ref().is_none_or(|state| {
            state.meta_path != cfg.paths.meta_index || state.content_path != cfg.paths.content_index
        });
        let mut meta_batch = None;
        let mut content_batch = None;
        for (index_path, is_content) in [
            (&cfg.paths.meta_index, false),
            (&cfg.paths.content_index, true),
        ] {
            let index_path = Path::new(index_path);
            if index_path.join("meta.json").exists() {
                let index = tantivy::Index::open_in_dir(index_path)
                    .with_context(|| format!("cannot open index {}", index_path.display()))?;
                let schema_valid = if is_content {
                    content_index::validate_schema(&index)
                } else {
                    meta_index::validate_schema(&index)
                };
                if schema_valid.is_err() {
                    rebuild = true;
                }
                match content_index::batch_receipt(&index) {
                    Ok(batch) if is_content => content_batch = batch,
                    Ok(batch) => meta_batch = batch,
                    Err(error) => {
                        tracing::warn!(path = %index_path.display(), %error, "invalid ingestion commit payload; reconciliation required");
                        rebuild = true;
                    }
                }
            } else {
                rebuild = true;
            }
            let marker = fs::read_to_string(index_path.join("ingestion-generation"));
            if previous.as_ref().is_none_or(|state| {
                marker.as_deref().ok() != Some(state.generation.to_string().as_str())
            }) {
                rebuild = true;
            }
        }
        if previous
            .as_ref()
            .is_some_and(|state| !index_commits_match(state, meta_batch, content_batch))
        {
            tracing::warn!(
                ?meta_batch,
                ?content_batch,
                "index commits do not match durable ingestion state; reconciliation required"
            );
            rebuild = true;
        }

        let mut state = previous.unwrap_or_else(|| Checkpoints {
            version: STATE_VERSION,
            generation: Uuid::new_v4(),
            meta_path: cfg.paths.meta_index.clone(),
            content_path: cfg.paths.content_index.clone(),
            retired_indices: Vec::new(),
            completed_batch: None,
            meta_commit: None,
            content_commit: None,
            volumes: Vec::new(),
            pending: None,
        });
        if rebuild {
            state.generation = Uuid::new_v4();
            for index_path in [&cfg.paths.meta_index, &cfg.paths.content_index] {
                let index_path = Path::new(index_path);
                if index_path.join("meta.json").exists() {
                    let archive = index_path
                        .with_extension(format!("before-ingestion-v2-{}", Uuid::new_v4()));
                    fs::rename(index_path, &archive)
                        .with_context(|| format!("preserve old index at {}", archive.display()))?;
                    state
                        .retired_indices
                        .push(archive.to_string_lossy().into_owned());
                    tracing::warn!(path = %archive.display(), "preserved index; full reconciliation required");
                }
                fs::create_dir_all(index_path)?;
            }
            // Both empty indices receive complete receipts for one seed batch,
            // each with its own physical commit identity. Static generation
            // markers alone cannot detect an older same-generation backup
            // restored underneath a newer journal checkpoint.
            let seed = Uuid::new_v4();
            {
                let index = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
                let mut writer = meta_index::create_writer(
                    &index,
                    &meta_index::WriterConfig {
                        heap_size_bytes: 32 * 1024 * 1024,
                        num_threads: 1,
                    },
                )?;
                content_index::commit_batch(&mut writer, seed)?;
                state.meta_commit = Some(
                    content_index::batch_receipt(&index.index)?
                        .context("metadata seed commit has no receipt")?
                        .commit_id,
                );
            }
            {
                let index = content_index::open_or_create(Path::new(&cfg.paths.content_index))?;
                let mut writer = content_index::create_writer(
                    &index,
                    &content_index::WriterConfig {
                        heap_size_bytes: 32 * 1024 * 1024,
                        num_threads: 1,
                    },
                )?;
                content_index::commit_batch(&mut writer, seed)?;
                state.content_commit = Some(
                    content_index::batch_receipt(&index.index)?
                        .context("content seed commit has no receipt")?
                        .commit_id,
                );
            }
            state.completed_batch = Some(seed);
            state.meta_path = cfg.paths.meta_index.clone();
            state.content_path = cfg.paths.content_index.clone();
            state.pending = None;
            for volume in &mut state.volumes {
                volume.cursor = None;
                volume.needs_scan = true;
                volume.catching_up = true;
            }
            for index_path in [&cfg.paths.meta_index, &cfg.paths.content_index] {
                atomic_write(
                    &Path::new(index_path).join("ingestion-generation"),
                    state.generation.to_string().as_bytes(),
                )?;
            }
        }
        let store = Self {
            path,
            _lock: Arc::new(lock),
            state,
        };
        store.save()?;
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        validate_state(&self.state)?;
        let bytes = serde_json::to_vec(&self.state)?;
        ensure!(
            bytes.len() as u64 <= MAX_STATE_BYTES,
            "ingestion state exceeds size limit"
        );
        atomic_write(&self.path, &bytes)
    }

    /// Blocking mutations can outlive their async caller. A lease keeps the
    /// same locked file handle open until the final mutation owner exits.
    pub fn mutation_lease(&self) -> Arc<File> {
        Arc::clone(&self._lock)
    }

    /// IDs are persisted by GUID and never reassigned when drive discovery changes.
    pub fn bind_volume(&mut self, volume: &mut VolumeInfo) -> Result<()> {
        if let Some(existing) = self
            .state
            .volumes
            .iter()
            .find(|v| v.guid.eq_ignore_ascii_case(&volume.guid_path))
        {
            volume.id = existing.id;
            return Ok(());
        }
        let id = self
            .state
            .volumes
            .iter()
            .map(|v| v.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .context("volume identity space exhausted")?;
        volume.id = id;
        self.state.volumes.push(VolumeCheckpoint {
            guid: volume.guid_path.clone(),
            id,
            cursor: None,
            needs_scan: true,
            catching_up: true,
            content_policy: None,
        });
        self.save()
    }

    /// A changed policy cannot be satisfied by the journal alone: existing
    /// unchanged files may need extraction, truncation changes, or text removal.
    pub fn set_content_policy(&mut self, id: VolumeId, policy: ContentPolicy) -> Result<()> {
        let volume = self.volume_mut(id)?;
        volume.content_policy = Some(policy);
        volume.needs_scan = true;
        volume.catching_up = true;
        self.save()
    }

    pub fn volume(&self, id: VolumeId) -> Result<&VolumeCheckpoint> {
        self.state
            .volumes
            .iter()
            .find(|v| v.id == id)
            .context("unknown volume identity")
    }

    pub fn volume_mut(&mut self, id: VolumeId) -> Result<&mut VolumeCheckpoint> {
        self.state
            .volumes
            .iter_mut()
            .find(|v| v.id == id)
            .context("unknown volume identity")
    }

    pub fn begin(&mut self, pending: PendingBatch) -> Result<()> {
        ensure!(
            self.state.pending.is_none(),
            "an ingestion batch is already pending"
        );
        self.state.pending = Some(pending);
        self.save()
    }

    /// Call only after worker success and metadata commit. Failed saves leave the
    /// old on-disk intent available to replay, including its original batch ID.
    pub fn finish(&mut self) -> Result<()> {
        let pending = self.state.pending.as_ref().context("no pending batch")?;
        let mut receipts = Vec::with_capacity(2);
        for path in [&self.state.meta_path, &self.state.content_path] {
            let index = tantivy::Index::open_in_dir(path)?;
            let receipt = content_index::batch_receipt(&index)?
                .with_context(|| format!("index {path} has no ingestion commit receipt"))?;
            ensure!(
                receipt.complete && receipt.batch_id == pending.worker.id,
                "index {} has not committed pending ingestion batch {}",
                path,
                pending.worker.id
            );
            receipts.push(receipt);
        }
        let mut completed = self.state.clone();
        if let Some(cursor) = pending.next_cursor {
            let volume = completed
                .volumes
                .iter_mut()
                .find(|v| v.id == pending.volume)
                .context("unknown volume identity")?;
            volume.cursor = Some(cursor);
        }
        completed.completed_batch = Some(pending.worker.id);
        completed.meta_commit = Some(receipts[0].commit_id);
        completed.content_commit = Some(receipts[1].commit_id);
        completed.pending = None;
        let bytes = serde_json::to_vec(&completed)?;
        atomic_write(&self.path, &bytes)?;
        self.state = completed;
        Ok(())
    }
}

/// The normal split-commit states are C/C, P/C, and P/P (content/metadata).
/// Metadata P with content C cannot be produced by the ordered commit lane;
/// accepting it would conceal a restored or independently modified index.
fn index_commits_match(
    state: &Checkpoints,
    meta: Option<content_index::BatchReceipt>,
    content: Option<content_index::BatchReceipt>,
) -> bool {
    let Some(completed) = state.completed_batch.filter(|id| !id.is_nil()) else {
        return false;
    };
    let Some(meta_commit) = state.meta_commit else {
        return false;
    };
    let Some(content_commit) = state.content_commit else {
        return false;
    };
    let meta_completed = meta.is_some_and(|receipt| {
        receipt.complete && receipt.batch_id == completed && receipt.commit_id == meta_commit
    });
    let content_completed = content.is_some_and(|receipt| {
        receipt.complete && receipt.batch_id == completed && receipt.commit_id == content_commit
    });
    if meta_completed && content_completed {
        return true;
    }
    state.pending.as_ref().is_some_and(|pending| {
        content.is_some_and(|receipt| receipt.batch_id == pending.worker.id)
            && (meta_completed
                || meta.is_some_and(|receipt| {
                    receipt.complete && receipt.batch_id == pending.worker.id
                }))
    })
}

fn validate_state(state: &Checkpoints) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    let mut guids = std::collections::BTreeSet::new();
    for volume in &state.volumes {
        ensure!(
            volume.id != 0 && ids.insert(volume.id),
            "invalid or duplicate volume identity"
        );
        ensure!(
            guids.insert(volume.guid.to_ascii_lowercase()),
            "duplicate volume GUID"
        );
    }
    if let Some(batch) = &state.pending {
        ensure!(
            !batch.worker.id.is_nil(),
            "pending batch has a nil identity"
        );
        ensure!(
            ids.contains(&batch.volume),
            "pending batch has unknown volume"
        );
        ensure!(
            batch.metadata.len() <= BATCH_LIMIT && batch.worker.jobs.len() <= BATCH_LIMIT,
            "pending batch exceeds record limit"
        );
        ensure!(
            batch
                .worker
                .reset_volumes
                .iter()
                .all(|id| *id == batch.volume),
            "cross-volume reset in pending batch"
        );
        ensure!(
            batch
                .metadata
                .iter()
                .all(|change| change.key().volume() == batch.volume),
            "cross-volume metadata in pending batch"
        );
        ensure!(
            batch
                .worker
                .jobs
                .iter()
                .all(|job| job.volume_id == batch.volume),
            "cross-volume job in pending batch"
        );
        let metadata_keys: std::collections::BTreeSet<_> =
            batch.metadata.iter().map(MetadataChange::key).collect();
        let job_keys: std::collections::BTreeSet<_> = batch
            .worker
            .jobs
            .iter()
            .map(|job| DocKey::from_parts(job.volume_id, job.file_id))
            .collect();
        ensure!(
            metadata_keys.len() == batch.metadata.len()
                && job_keys.len() == batch.worker.jobs.len()
                && metadata_keys == job_keys,
            "pending metadata and worker identities must match exactly once"
        );
    }
    Ok(())
}

/// Coalesce to the last observed state per full document identity. Sorting makes
/// replay independent of hash seeds and preserves different NTFS generations.
pub(super) fn event_changes(events: &[FileEvent]) -> Result<Vec<MetadataChange>> {
    let mut changes = BTreeMap::new();
    for event in events {
        match event {
            FileEvent::Created(meta)
            | FileEvent::Modified(meta)
            | FileEvent::AttributesChanged(meta) => {
                changes.insert(meta.key, MetadataChange::Upsert(meta.clone()));
            }
            FileEvent::Renamed { from, to } => {
                if *from != to.key {
                    changes.insert(*from, MetadataChange::Delete(*from));
                }
                changes.insert(to.key, MetadataChange::Upsert(to.clone()));
            }
            FileEvent::Deleted(key) => {
                changes.insert(*key, MetadataChange::Delete(*key));
            }
            FileEvent::RescanRequired { .. } => {
                bail!("directory/link change requires volume reconciliation")
            }
            FileEvent::Excluded { .. } => {
                bail!("excluded events must be resolved against the current index")
            }
        }
    }
    ensure!(
        changes.len() <= BATCH_LIMIT,
        "journal batch exceeds record limit"
    );
    Ok(changes.into_values().collect())
}

pub(super) fn make_pending(
    volume: VolumeId,
    changes: Vec<MetadataChange>,
    next_cursor: Option<JournalCursor>,
    reset: bool,
    cfg: &AppConfig,
) -> PendingBatch {
    let jobs = changes
        .iter()
        .map(|change| {
            if let MetadataChange::Upsert(meta) = change
                && let Some(mut job) = content_job_from_meta(meta, &cfg.extract)
            {
                job.operation = JobOperation::Reconcile;
                return job;
            }
            let key = change.key();
            JobSpec {
                volume_id: key.volume(),
                file_id: key.file_id(),
                path: PathBuf::new(),
                max_bytes: None,
                max_chars: None,
                file_size: 0,
                operation: JobOperation::Delete,
            }
        })
        .collect();
    PendingBatch {
        volume,
        worker: IndexBatch {
            id: Uuid::new_v4(),
            jobs,
            reset_volumes: if reset { vec![volume] } else { Vec::new() },
        },
        metadata: changes,
        next_cursor,
    }
}

/// Production admission applies the selected volume's content policy as well as
/// file eligibility. Delete jobs purge old text while metadata upserts survive.
pub(super) fn pending_for_volume(
    volume: &VolumeInfo,
    changes: Vec<MetadataChange>,
    next_cursor: Option<JournalCursor>,
    reset: bool,
    cfg: &AppConfig,
) -> PendingBatch {
    let mut pending = make_pending(volume.id, changes, next_cursor, reset, cfg);
    if !ContentPolicy::for_volume(volume, cfg).enabled {
        for job in &mut pending.worker.jobs {
            job.operation = JobOperation::Delete;
            job.path = PathBuf::new();
            job.max_bytes = None;
            job.max_chars = None;
            job.file_size = 0;
        }
    }
    pending
}

/// Flush the new file before replacing the old checkpoint. Windows rename does
/// not replace an existing destination; MoveFileExW supplies that contract.
pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("pending-write");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        use windows::core::PCWSTR;
        let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: both buffers are NUL-terminated and live through the call.
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }?;
    }
    #[cfg(not(windows))]
    {
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::FileFlags;

    fn config(root: &Path) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.paths.meta_index = root.join("meta").to_string_lossy().into_owned();
        cfg.paths.content_index = root.join("content").to_string_lossy().into_owned();
        cfg.paths.state_dir = root.join("state").to_string_lossy().into_owned();
        cfg.paths.jobs_dir = root.join("jobs").to_string_lossy().into_owned();
        cfg
    }

    fn volume(guid: &str) -> VolumeInfo {
        VolumeInfo {
            id: 999,
            guid_path: guid.into(),
            drive_letters: vec!['X'],
        }
    }

    fn meta(key: DocKey, name: &str, size: u64) -> FileMeta {
        FileMeta::new(
            key,
            key.volume(),
            None,
            name.into(),
            Some(format!("X:\\{name}")),
            size,
            10,
            20,
            FileFlags::empty(),
        )
    }

    fn commit_marker(path: &str, batch: Uuid) -> Result<()> {
        let index = tantivy::Index::open_in_dir(path)?;
        let mut writer = index.writer_with_num_threads(1, 20_000_000)?;
        content_index::commit_batch(&mut writer, batch)?;
        Ok(())
    }

    fn copy_index_snapshot(source: &Path, target: &Path) -> Result<()> {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let destination = target.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_index_snapshot(&entry.path(), &destination)?;
            } else {
                ensure!(
                    entry.file_type()?.is_file(),
                    "unexpected snapshot file type"
                );
                fs::copy(entry.path(), destination)?;
            }
        }
        Ok(())
    }

    #[test]
    fn content_selection_distinguishes_automatic_and_explicit_metadata_only_volumes() -> Result<()>
    {
        let volume = volume("policy-volume");
        let mut cfg = AppConfig::default();
        assert!(ContentPolicy::for_volume(&volume, &cfg).enabled);
        cfg.volumes = vec!["X:\\".into()];
        assert!(!ContentPolicy::for_volume(&volume, &cfg).enabled);
        cfg.content_index_volumes = vec!["Y:\\".into()];
        assert!(!ContentPolicy::for_volume(&volume, &cfg).enabled);
        cfg.content_index_volumes = vec!["x:\\".into()];
        let enabled = ContentPolicy::for_volume(&volume, &cfg);
        assert!(enabled.enabled);
        cfg.extract.max_bytes_per_file += 1;
        assert_ne!(ContentPolicy::for_volume(&volume, &cfg), enabled);
        let different_bytes = ContentPolicy::for_volume(&volume, &cfg);
        cfg.extract.max_chars_per_file += 1;
        assert_ne!(ContentPolicy::for_volume(&volume, &cfg), different_bytes);

        let old: VolumeCheckpoint = serde_json::from_value(serde_json::json!({
            "guid": "policy-volume", "id": 1, "cursor": null,
            "needs_scan": false, "catching_up": false
        }))?;
        assert!(
            old.content_policy.is_none(),
            "old coverage must not be guessed"
        );
        Ok(())
    }

    #[test]
    fn event_replay_coalesces_all_mutations_without_losing_ntfs_generations() -> Result<()> {
        let old_key = DocKey::from_parts(1, 0x0001_0000_0000_000a);
        let new_key = DocKey::from_parts(1, 0x0002_0000_0000_000a);
        let other = DocKey::from_parts(1, 11);
        let renamed = meta(other, "renamed.txt", 40);
        let events = vec![
            FileEvent::Created(meta(old_key, "old.txt", 10)),
            FileEvent::Modified(meta(old_key, "old.txt", 30)),
            FileEvent::Deleted(old_key),
            FileEvent::Created(meta(new_key, "new.txt", 50)),
            FileEvent::Created(meta(other, "before.txt", 20)),
            FileEvent::Renamed {
                from: other,
                to: renamed.clone(),
            },
            FileEvent::AttributesChanged(renamed.clone()),
        ];
        let got = event_changes(&events)?;
        assert_eq!(got, event_changes(&[events.clone(), events].concat())?);
        assert_eq!(got.len(), 3);
        assert!(got.contains(&MetadataChange::Delete(old_key)));
        assert!(got.contains(&MetadataChange::Upsert(meta(new_key, "new.txt", 50))));
        assert!(got.contains(&MetadataChange::Upsert(renamed)));
        assert!(event_changes(&[FileEvent::RescanRequired { doc: other }]).is_err());
        Ok(())
    }

    #[test]
    fn checkpoint_waits_for_complete_transaction_and_replays_identical_intent() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut vol = volume("volume-a");
        store.bind_volume(&mut vol)?;
        let start = JournalCursor {
            journal_id: 31,
            last_usn: 100,
        };
        store.volume_mut(vol.id)?.cursor = Some(start);
        store.volume_mut(vol.id)?.needs_scan = false;
        store.save()?;
        let next = JournalCursor {
            journal_id: 31,
            last_usn: 500,
        };
        let key = DocKey::from_parts(vol.id, 0xabcd_0000_0000_002a);
        store.begin(make_pending(
            vol.id,
            vec![MetadataChange::Upsert(meta(key, "a.txt", 90))],
            Some(next),
            false,
            &cfg,
        ))?;
        let batch_id = store.state.pending.as_ref().unwrap().worker.id;
        assert_eq!(store.volume(vol.id)?.cursor, Some(start));
        // A second service cannot race the journal state or its temporary file.
        assert!(StateStore::open(&cfg).is_err());
        drop(store);

        let mut replay = StateStore::open(&cfg)?;
        assert_eq!(replay.volume(vol.id)?.cursor, Some(start));
        let pending = replay.state.pending.as_ref().unwrap();
        assert_eq!(pending.worker.id, batch_id);
        assert_eq!(pending.worker.jobs[0].file_id, key.file_id());
        assert_eq!(pending.metadata[0].key(), key);
        // An admitted job or a content-only commit is not a completed batch.
        assert!(replay.finish().is_err());
        commit_marker(&cfg.paths.content_index, batch_id)?;
        assert!(replay.finish().is_err());
        assert_eq!(replay.volume(vol.id)?.cursor, Some(start));
        commit_marker(&cfg.paths.meta_index, batch_id)?;
        replay.finish()?;
        drop(replay);
        let completed = StateStore::open(&cfg)?;
        assert!(completed.state.pending.is_none());
        assert_eq!(completed.volume(vol.id)?.cursor, Some(next));
        assert_eq!(completed.state.completed_batch, Some(batch_id));
        Ok(())
    }

    #[test]
    fn pending_commit_tokens_preserve_only_reachable_split_commit_states() -> Result<()> {
        for (content_pending, meta_pending, preserve) in [
            (false, false, true),
            (true, false, true),
            (true, true, true),
            (false, true, false),
        ] {
            let root = tempfile::tempdir()?;
            let cfg = config(root.path());
            let mut store = StateStore::open(&cfg)?;
            let mut vol = volume("split-commit-volume");
            store.bind_volume(&mut vol)?;
            let previous = JournalCursor {
                journal_id: 41,
                last_usn: 100,
            };
            store.volume_mut(vol.id)?.cursor = Some(previous);
            store.volume_mut(vol.id)?.needs_scan = false;
            let pending = make_pending(
                vol.id,
                Vec::new(),
                Some(JournalCursor {
                    last_usn: 200,
                    ..previous
                }),
                true,
                &cfg,
            );
            let pending_id = pending.worker.id;
            let generation = store.state.generation;
            store.begin(pending)?;
            if content_pending {
                commit_marker(&cfg.paths.content_index, pending_id)?;
            }
            if meta_pending {
                commit_marker(&cfg.paths.meta_index, pending_id)?;
            }
            drop(store);
            let recovered = StateStore::open(&cfg)?;
            if preserve {
                assert_eq!(recovered.state.generation, generation);
                assert_eq!(recovered.volume(vol.id)?.cursor, Some(previous));
                assert_eq!(
                    recovered.state.pending.as_ref().unwrap().worker.id,
                    pending_id
                );
                // Even P/P is replayed: its commit payload cannot stand in for
                // a successful worker acknowledgement and durable checkpoint.
            } else {
                assert_ne!(recovered.state.generation, generation);
                assert!(recovered.state.pending.is_none());
                assert!(recovered.volume(vol.id)?.cursor.is_none());
                assert!(recovered.volume(vol.id)?.needs_scan);
                assert_eq!(recovered.state.retired_indices.len(), 2);
            }
        }
        Ok(())
    }

    #[test]
    fn same_generation_index_restore_cannot_keep_a_newer_journal_checkpoint() -> Result<()> {
        for restore_content in [true, false] {
            let root = tempfile::tempdir()?;
            let cfg = config(root.path());
            let mut store = StateStore::open(&cfg)?;
            let mut vol = volume("restored-index-volume");
            store.bind_volume(&mut vol)?;
            let key = DocKey::from_parts(vol.id, 0xabcd_0000_0000_1234);
            let old = meta(key, "deleted-before-restore.txt", 10);
            let indexed = make_pending(
                vol.id,
                vec![MetadataChange::Upsert(old.clone())],
                Some(JournalCursor {
                    journal_id: 29,
                    last_usn: 100,
                }),
                false,
                &cfg,
            );
            store.begin(indexed.clone())?;
            {
                let index = content_index::open_or_create(Path::new(&cfg.paths.content_index))?;
                let mut writer = content_index::create_writer(
                    &index,
                    &content_index::WriterConfig {
                        heap_size_bytes: 20_000_000,
                        num_threads: 1,
                    },
                )?;
                content_index::add_content_doc(
                    &mut writer,
                    &index.fields,
                    &content_index::ContentDoc {
                        key,
                        volume: vol.id,
                        name: Some(old.name.clone()),
                        path: old.path.clone(),
                        ext: old.ext.clone(),
                        size: old.size,
                        created: old.created,
                        modified: old.modified,
                        flags: u64::from(old.flags.bits()),
                        content_lang: None,
                        content: "obsolete restored content".into(),
                    },
                )?;
                content_index::commit_batch(&mut writer, indexed.worker.id)?;
            }
            super::super::apply_metadata(&cfg, &indexed)?;
            store.finish()?;
            store.volume_mut(vol.id)?.needs_scan = false;
            store.save()?;
            let generation = store.state.generation;
            let restored_path = if restore_content {
                Path::new(&cfg.paths.content_index)
            } else {
                Path::new(&cfg.paths.meta_index)
            };
            let backup = root.path().join("older-index-snapshot");
            copy_index_snapshot(restored_path, &backup)?;

            let deletion = make_pending(
                vol.id,
                vec![MetadataChange::Delete(key)],
                Some(JournalCursor {
                    journal_id: 29,
                    last_usn: 200,
                }),
                false,
                &cfg,
            );
            store.begin(deletion.clone())?;
            {
                let index = content_index::open_or_create(Path::new(&cfg.paths.content_index))?;
                let mut writer = content_index::create_writer(
                    &index,
                    &content_index::WriterConfig {
                        heap_size_bytes: 20_000_000,
                        num_threads: 1,
                    },
                )?;
                content_index::delete_doc(&mut writer, &index.fields, key);
                content_index::commit_batch(&mut writer, deletion.worker.id)?;
            }
            super::super::apply_metadata(&cfg, &deletion)?;
            store.finish()?;
            drop(store);
            fs::rename(restored_path, root.path().join("newer-index-preserved"))?;
            fs::rename(&backup, restored_path)?;
            // The restored index is valid and has the same generation marker,
            // but still contains the document whose deletion was checkpointed.
            assert_eq!(
                fs::read_to_string(restored_path.join("ingestion-generation"))?,
                generation.to_string()
            );
            let restored = tantivy::Index::open_in_dir(restored_path)?;
            let reader = restored.reader()?;
            assert_eq!(reader.searcher().num_docs(), 1);
            drop(reader);
            drop(restored);

            let rebuilt = StateStore::open(&cfg)?;
            assert_ne!(rebuilt.state.generation, generation);
            assert_eq!(rebuilt.volume(vol.id)?.id, vol.id);
            assert!(rebuilt.volume(vol.id)?.cursor.is_none());
            assert!(rebuilt.volume(vol.id)?.needs_scan);
            assert_eq!(rebuilt.state.retired_indices.len(), 2);
            for path in [&cfg.paths.meta_index, &cfg.paths.content_index] {
                let index = tantivy::Index::open_in_dir(path)?;
                let reader = index.reader()?;
                assert_eq!(reader.searcher().num_docs(), 0);
                assert_eq!(
                    content_index::committed_batch(&index)?,
                    rebuilt.state.completed_batch
                );
            }
        }
        Ok(())
    }

    #[test]
    fn restored_attempt_of_the_same_batch_cannot_impersonate_its_completed_commit() -> Result<()> {
        for partial in [true, false] {
            let root = tempfile::tempdir()?;
            let cfg = config(root.path());
            let mut store = StateStore::open(&cfg)?;
            let mut vol = volume("same-batch-restored-attempt");
            store.bind_volume(&mut vol)?;
            let pending = make_pending(
                vol.id,
                Vec::new(),
                Some(JournalCursor {
                    journal_id: 51,
                    last_usn: 200,
                }),
                true,
                &cfg,
            );
            store.begin(pending.clone())?;
            {
                let index = tantivy::Index::open_in_dir(&cfg.paths.content_index)?;
                let mut writer = index.writer_with_num_threads(1, 20_000_000)?;
                if partial {
                    content_index::commit_partial_batch(&mut writer, pending.worker.id)?;
                } else {
                    content_index::commit_batch(&mut writer, pending.worker.id)?;
                }
            }
            let snapshot = root.path().join("earlier-attempt-preserved");
            copy_index_snapshot(Path::new(&cfg.paths.content_index), &snapshot)?;
            // A later successful retry has the same durable batch identity,
            // but a different physical commit identity. The checkpoint binds
            // to that exact completed attempt, not just its work description.
            commit_marker(&cfg.paths.content_index, pending.worker.id)?;
            commit_marker(&cfg.paths.meta_index, pending.worker.id)?;
            store.finish()?;
            let finished_receipt = store.state.content_commit;
            drop(store);
            fs::rename(
                &cfg.paths.content_index,
                root.path().join("finished-attempt-preserved"),
            )?;
            fs::rename(&snapshot, &cfg.paths.content_index)?;
            let old = tantivy::Index::open_in_dir(&cfg.paths.content_index)?;
            let receipt = content_index::batch_receipt(&old)?.unwrap();
            assert_eq!(receipt.batch_id, pending.worker.id);
            assert_ne!(Some(receipt.commit_id), finished_receipt);
            assert_eq!(receipt.complete, !partial);
            drop(old);
            let rebuilt = StateStore::open(&cfg)?;
            assert!(rebuilt.volume(vol.id)?.cursor.is_none());
            assert!(rebuilt.volume(vol.id)?.needs_scan);
            assert!(rebuilt.state.pending.is_none());
            assert_eq!(rebuilt.state.retired_indices.len(), 2);
        }
        Ok(())
    }

    #[test]
    fn partial_worker_receipt_is_recoverable_only_while_its_batch_remains_pending() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut vol = volume("partial-worker-replay");
        store.bind_volume(&mut vol)?;
        let pending = make_pending(vol.id, Vec::new(), None, true, &cfg);
        store.begin(pending.clone())?;
        // A replay can start after metadata committed P but before state saved.
        commit_marker(&cfg.paths.content_index, pending.worker.id)?;
        commit_marker(&cfg.paths.meta_index, pending.worker.id)?;
        {
            let index = tantivy::Index::open_in_dir(&cfg.paths.content_index)?;
            let mut writer = index.writer_with_num_threads(1, 20_000_000)?;
            content_index::commit_partial_batch(&mut writer, pending.worker.id)?;
        }
        let generation = store.state.generation;
        assert!(store.finish().is_err());
        drop(store);
        let recovered = StateStore::open(&cfg)?;
        assert_eq!(recovered.state.generation, generation);
        assert_eq!(
            recovered.state.pending.as_ref().unwrap().worker.id,
            pending.worker.id
        );
        assert!(recovered.state.retired_indices.is_empty());
        Ok(())
    }

    #[test]
    fn legacy_state_without_commit_evidence_rebuilds_instead_of_assuming_coverage() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut vol = volume("untagged-old-state");
        store.bind_volume(&mut vol)?;
        store.volume_mut(vol.id)?.cursor = Some(JournalCursor {
            journal_id: 9,
            last_usn: 500,
        });
        store.volume_mut(vol.id)?.needs_scan = false;
        let mut legacy = serde_json::to_value(&store.state)?;
        legacy.as_object_mut().unwrap().remove("completed_batch");
        atomic_write(&store.path, &serde_json::to_vec(&legacy)?)?;
        drop(store);
        let rebuilt = StateStore::open(&cfg)?;
        assert!(rebuilt.volume(vol.id)?.cursor.is_none());
        assert!(rebuilt.volume(vol.id)?.needs_scan);
        assert!(rebuilt.state.completed_batch.is_some());
        assert_eq!(rebuilt.state.retired_indices.len(), 2);
        Ok(())
    }

    #[test]
    fn volume_registry_survives_discovery_reordering_and_mount_reuse() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut a = volume("volume-a");
        let mut b = volume("volume-b");
        store.bind_volume(&mut a)?;
        store.bind_volume(&mut b)?;
        assert_ne!(a.id, b.id);
        drop(store);
        let mut store = StateStore::open(&cfg)?;
        let mut rediscovered_b = volume("VOLUME-B");
        store.bind_volume(&mut rediscovered_b)?;
        assert_eq!(rediscovered_b.id, b.id);
        let mut c = volume("volume-c");
        store.bind_volume(&mut c)?;
        assert!(c.id > b.id);
        Ok(())
    }

    #[test]
    fn missing_index_generation_invalidates_checkpoint_and_preserves_old_index() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut vol = volume("volume-a");
        store.bind_volume(&mut vol)?;
        store.volume_mut(vol.id)?.cursor = Some(JournalCursor {
            journal_id: 7,
            last_usn: 900,
        });
        store.volume_mut(vol.id)?.needs_scan = false;
        store.save()?;
        drop(store);
        // Simulate a restored/replaced index with the same schema but a different
        // generation. No journal checkpoint can be trusted against that index.
        atomic_write(
            &Path::new(&cfg.paths.content_index).join("ingestion-generation"),
            b"other-generation",
        )?;
        let rebuilt = StateStore::open(&cfg)?;
        assert!(rebuilt.volume(vol.id)?.cursor.is_none());
        assert!(rebuilt.volume(vol.id)?.needs_scan);
        assert_eq!(
            fs::read_dir(root.path())?
                .filter_map(Result::ok)
                .filter(|e| e
                    .file_name()
                    .to_string_lossy()
                    .contains("before-ingestion-v2"))
                .count(),
            2
        );
        Ok(())
    }

    #[test]
    fn atomic_checkpoint_replacement_and_bounded_input() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let store = StateStore::open(&cfg)?;
        let path = store.path.clone();
        drop(store);
        atomic_write(&path, b"not valid json")?;
        assert!(StateStore::open(&cfg).is_err());
        let file = OpenOptions::new().write(true).open(&path)?;
        file.set_len(MAX_STATE_BYTES + 1)?;
        assert!(StateStore::open(&cfg).is_err());
        Ok(())
    }
}
