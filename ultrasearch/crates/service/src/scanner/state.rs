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
pub(super) const MAX_DEFERRED_FILES: usize = 1024;
const STATE_VERSION: u32 = 3;
const MAX_STATE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RETRY_DELAY_SECS: i64 = 300;

/// Admission failed before any index mutation. Older obligations can still run
/// to release capacity even when this volume has unread journal records.
#[derive(Debug)]
pub(super) struct DeferredCapacity;

impl std::fmt::Display for DeferredCapacity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("deferred extraction capacity is reserved; retry existing files before admitting new work")
    }
}

impl std::error::Error for DeferredCapacity {}

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
    /// Retry attempts never consume a later journal position. A subsequent
    /// journal mutation supersedes the older per-file obligation.
    #[serde(default)]
    pub retry: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct DeferredFile {
    pub meta: FileMeta,
    pub attempts: u32,
    pub retry_at: i64,
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
    #[serde(default)]
    pub deferred: Vec<DeferredFile>,
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
            let mut state: Checkpoints = serde_json::from_reader(file.take(MAX_STATE_BYTES))
                .context("invalid ingestion state; refusing to guess volume identities")?;
            ensure!(
                matches!(state.version, 2 | STATE_VERSION),
                "unsupported ingestion state version"
            );
            // Upgrade before publishing any deferred outcome. An older service
            // rejects v3 rather than ignoring retry obligations and trusting its
            // journal cursor. Keep the existing path and stable volume IDs.
            state.version = STATE_VERSION;
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
            deferred: Vec::new(),
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
            state.deferred.clear();
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
        reserve_retry_capacity(&self.state, &pending)?;
        let mut admitted = self.state.clone();
        admitted.pending = Some(pending);
        validate_state(&admitted)?;
        let bytes = serde_json::to_vec(&admitted)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(DeferredCapacity.into());
        }
        // Failure leaves the old in-memory state usable for capacity recovery,
        // just as it leaves the cursor unchanged on disk. Never install an
        // oversized or invalid pending barrier before its durable write.
        atomic_write(&self.path, &bytes)?;
        self.state = admitted;
        Ok(())
    }

    /// Call only after worker success and metadata commit. Failed saves leave the
    /// old on-disk intent available to replay, including its original batch ID.
    pub fn finish(&mut self) -> Result<()> {
        self.finish_at(super::unix_timestamp_secs())
    }

    fn finish_at(&mut self, now: i64) -> Result<()> {
        let pending = self.state.pending.as_ref().context("no pending batch")?;
        let mut receipts = Vec::with_capacity(2);
        let mut deferred = Vec::new();
        for (path, is_content) in [
            (&self.state.meta_path, false),
            (&self.state.content_path, true),
        ] {
            let index = tantivy::Index::open_in_dir(path)?;
            let outcome = content_index::batch_outcome(&index)?
                .with_context(|| format!("index {path} has no ingestion commit receipt"))?;
            let receipt = outcome.receipt;
            ensure!(
                receipt.complete && receipt.batch_id == pending.worker.id,
                "index {} has not committed pending ingestion batch {}",
                path,
                pending.worker.id
            );
            if is_content {
                validate_deferred_outcome(pending, &outcome.deferred)?;
                deferred = outcome.deferred;
            } else {
                ensure!(
                    outcome.deferred.is_empty(),
                    "metadata commit must not contain deferred extraction outcomes"
                );
            }
            receipts.push(receipt);
        }
        let mut completed = self.state.clone();
        let mut retries: BTreeMap<_, _> = completed
            .deferred
            .into_iter()
            .map(|retry| (retry.meta.key, retry))
            .collect();
        for volume in &pending.worker.reset_volumes {
            retries.retain(|key, _| key.volume() != *volume);
        }
        for change in &pending.metadata {
            let previous = retries.remove(&change.key());
            if deferred.binary_search(&change.key()).is_ok() {
                let MetadataChange::Upsert(meta) = change else {
                    bail!("a deletion cannot defer extraction");
                };
                let attempts = if pending.retry {
                    previous.map_or(1, |retry| retry.attempts.saturating_add(1))
                } else {
                    1
                };
                retries.insert(
                    meta.key,
                    DeferredFile {
                        meta: meta.clone(),
                        attempts,
                        retry_at: now.saturating_add(retry_delay(attempts)),
                    },
                );
            }
        }
        completed.deferred = retries.into_values().collect();
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
        validate_state(&completed)?;
        let bytes = serde_json::to_vec(&completed)?;
        ensure!(
            bytes.len() as u64 <= MAX_STATE_BYTES,
            "deferred ingestion state exceeds size limit; batch remains pending"
        );
        atomic_write(&self.path, &bytes)?;
        self.state = completed;
        Ok(())
    }

    /// Choose the oldest due obligation from the caller's admitted volumes.
    /// Bound one attempt just like an ordinary mutation batch. Offline and
    /// deselected volumes retain their obligations without blocking others.
    pub fn due_retry(
        &self,
        volumes: &[VolumeInfo],
        cfg: &AppConfig,
        now: i64,
        limit: usize,
    ) -> Option<PendingBatch> {
        if self.state.pending.is_some() {
            return None;
        }
        let mut due: Vec<_> = self
            .state
            .deferred
            .iter()
            .filter(|retry| {
                (retry.retry_at <= now || retry.retry_at > now.saturating_add(MAX_RETRY_DELAY_SECS))
                    && volumes
                        .iter()
                        .any(|volume| volume.id == retry.meta.key.volume())
            })
            .collect();
        due.sort_by_key(|retry| (retry.retry_at, retry.meta.key));
        let volume_id = due.first()?.meta.key.volume();
        let volume = volumes.iter().find(|volume| volume.id == volume_id)?;
        let changes = due
            .into_iter()
            .filter(|retry| retry.meta.key.volume() == volume_id)
            .take(limit.min(BATCH_LIMIT))
            .map(|retry| MetadataChange::Upsert(retry.meta.clone()))
            .collect::<Vec<_>>();
        if changes.is_empty() {
            return None;
        }
        let mut pending = pending_for_volume(volume, changes, None, false, cfg);
        pending.retry = true;
        // Measure the saved state once. Add entries only while their actual
        // encoded metadata and worker jobs fit the remaining intent budget.
        // Admission reserves room for the largest single-key retry, so an old
        // long path cannot become permanently stranded behind shorter files.
        let available =
            MAX_STATE_BYTES.checked_sub(json_length(&self.state).ok()?.checked_sub(4)?)?;
        let metadata = std::mem::take(&mut pending.metadata);
        let jobs = std::mem::take(&mut pending.worker.jobs);
        let mut used = json_length(&pending).ok()?;
        for (change, job) in metadata.into_iter().zip(jobs) {
            let separators = if pending.metadata.is_empty() { 0 } else { 2 };
            let additional = json_length(&change)
                .ok()?
                .checked_add(json_length(&job).ok()?)?
                .checked_add(separators)?;
            let next = used.checked_add(additional)?;
            if next > available {
                break;
            }
            pending.metadata.push(change);
            pending.worker.jobs.push(job);
            used = next;
        }
        if pending.metadata.is_empty() {
            return None;
        }
        Some(pending)
    }
}

/// Reserve for the worst permitted worker outcome before admitting any new
/// writes. A full retry queue must not strand a new global pending transaction
/// in front of the older obligations that need to run to free that capacity.
fn reserve_retry_capacity(state: &Checkpoints, pending: &PendingBatch) -> Result<()> {
    let mut keys: std::collections::BTreeSet<_> =
        state.deferred.iter().map(|retry| retry.meta.key).collect();
    for volume in &pending.worker.reset_volumes {
        keys.retain(|key| key.volume() != *volume);
    }
    for change in &pending.metadata {
        keys.remove(&change.key());
    }
    keys.extend(
        pending
            .worker
            .jobs
            .iter()
            .filter(|job| job.operation == JobOperation::Reconcile)
            .map(|job| DocKey::from_parts(job.volume_id, job.file_id)),
    );
    if keys.len() > MAX_DEFERRED_FILES {
        return Err(DeferredCapacity.into());
    }

    // Project the largest completed ledger this batch can legitimately leave.
    // Counters/deadlines use their maximum JSON widths, so repeated failures or
    // changed extraction limits cannot gradually consume reserved retry space.
    let superseded: std::collections::BTreeSet<_> =
        pending.metadata.iter().map(MetadataChange::key).collect();
    let reconcile: std::collections::BTreeSet<_> = pending
        .worker
        .jobs
        .iter()
        .filter(|job| job.operation == JobOperation::Reconcile)
        .map(|job| DocKey::from_parts(job.volume_id, job.file_id))
        .collect();
    let mut projected = state.clone();
    projected.pending = None;
    projected.deferred.retain(|retry| {
        !pending
            .worker
            .reset_volumes
            .contains(&retry.meta.key.volume())
            && !superseded.contains(&retry.meta.key)
    });
    for change in &pending.metadata {
        if let MetadataChange::Upsert(meta) = change
            && reconcile.contains(&meta.key)
        {
            projected.deferred.push(DeferredFile {
                meta: meta.clone(),
                attempts: u32::MAX,
                retry_at: i64::MAX,
            });
        }
    }
    for retry in &mut projected.deferred {
        retry.attempts = u32::MAX;
        retry.retry_at = i64::MAX;
    }
    if let Some(cursor) = pending.next_cursor {
        projected
            .volumes
            .iter_mut()
            .find(|volume| volume.id == pending.volume)
            .context("unknown volume identity")?
            .cursor = Some(cursor);
    }
    // Completing a baseline can populate a missing cursor, and actionless
    // progress can enlarge it or turn a catch-up flag false without admitting
    // another worker batch. Reserve those maximum widths too.
    for volume in &mut projected.volumes {
        volume.cursor = Some(JournalCursor {
            journal_id: u64::MAX,
            last_usn: u64::MAX,
        });
        volume.needs_scan = false;
        volume.catching_up = false;
    }
    projected.completed_batch = Some(pending.worker.id);
    projected.meta_commit = Some(pending.worker.id);
    projected.content_commit = Some(pending.worker.id);
    let completed_bytes = json_length(&projected)?;
    let mut largest_retry = 0;
    for retry in &projected.deferred {
        largest_retry = largest_retry.max(json_length(&largest_single_retry(&retry.meta)?)?);
    }
    let required = if largest_retry == 0 {
        completed_bytes
    } else {
        // Checkpoints.pending always encodes as `null` without an intent.
        completed_bytes
            .saturating_sub(4)
            .saturating_add(largest_retry)
    };
    if required > MAX_STATE_BYTES {
        return Err(DeferredCapacity.into());
    }
    Ok(())
}

/// Maximum encoded single-file retry for this captured identity. The metadata
/// cannot grow on retry; fresh observations pass through admission again. Job
/// limits may change with configuration, so reserve their full integer widths.
fn largest_single_retry(meta: &FileMeta) -> Result<PendingBatch> {
    let path = meta
        .path
        .as_ref()
        .context("deferred file has no retry path")?;
    Ok(PendingBatch {
        volume: meta.key.volume(),
        worker: IndexBatch {
            id: Uuid::from_u128(1), // Every textual UUID occupies the same 36 bytes.
            jobs: vec![JobSpec {
                operation: JobOperation::Reconcile,
                volume_id: meta.key.volume(),
                file_id: meta.key.file_id(),
                path: PathBuf::from(path),
                max_bytes: Some(usize::MAX),
                max_chars: Some(usize::MAX),
                file_size: u64::MAX,
            }],
            reset_volumes: Vec::new(),
        },
        metadata: vec![MetadataChange::Upsert(meta.clone())],
        next_cursor: None,
        retry: true,
    })
}

/// Count JSON bytes without retaining another full copy of the bounded state.
/// Measuring the base once plus each entry once keeps retry sizing linear in
/// the total encoded path bytes instead of serializing the whole state per key.
fn json_length(value: &impl Serialize) -> Result<u64> {
    #[derive(Default)]
    struct Length(u64);
    impl Write for Length {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| std::io::Error::other("ingestion JSON length overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut length = Length::default();
    serde_json::to_writer(&mut length, value)?;
    Ok(length.0)
}

fn retry_delay(attempts: u32) -> i64 {
    (5i64 << attempts.saturating_sub(1).min(6)).min(MAX_RETRY_DELAY_SECS)
}

/// The worker may defer only a reconciliation that has a corresponding durable
/// metadata snapshot. Reject invented keys before mutating metadata or moving a
/// cursor; the receipt alone never creates an untracked retry obligation.
pub(super) fn validate_deferred_outcome(pending: &PendingBatch, deferred: &[DocKey]) -> Result<()> {
    ensure!(
        deferred.len() <= BATCH_LIMIT && deferred.windows(2).all(|keys| keys[0] < keys[1]),
        "invalid deferred extraction identity set"
    );
    for key in deferred {
        ensure!(
            pending.metadata.iter().any(|change| {
                matches!(change, MetadataChange::Upsert(meta) if meta.key == *key)
            }) && pending.worker.jobs.iter().any(|job| {
                job.operation == JobOperation::Reconcile
                    && DocKey::from_parts(job.volume_id, job.file_id) == *key
            }),
            "worker deferred a file outside the durable reconciliation batch: {key}"
        );
    }
    Ok(())
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
    ensure!(
        state.deferred.len() <= MAX_DEFERRED_FILES,
        "deferred extraction limit reached; batch remains pending"
    );
    let mut deferred_keys = std::collections::BTreeSet::new();
    for retry in &state.deferred {
        ensure!(
            ids.contains(&retry.meta.key.volume())
                && retry.meta.volume == retry.meta.key.volume()
                && deferred_keys.insert(retry.meta.key)
                && retry.attempts > 0
                && retry.retry_at >= 0
                && retry
                    .meta
                    .path
                    .as_ref()
                    .is_some_and(|path| !path.is_empty()),
            "invalid or duplicate deferred extraction obligation"
        );
    }
    if let Some(batch) = &state.pending {
        reserve_retry_capacity(state, batch)?;
        ensure!(
            !batch.retry || (batch.next_cursor.is_none() && batch.worker.reset_volumes.is_empty()),
            "retry batch cannot advance a journal cursor or reset a volume"
        );
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
        retry: false,
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

    fn commit_outcome(path: &str, batch: Uuid, deferred: &[DocKey]) -> Result<()> {
        let index = tantivy::Index::open_in_dir(path)?;
        let mut writer = index.writer_with_num_threads(1, 20_000_000)?;
        content_index::commit_batch_with_deferred(&mut writer, batch, deferred)?;
        Ok(())
    }

    fn complete_at(
        store: &mut StateStore,
        cfg: &AppConfig,
        deferred: &[DocKey],
        now: i64,
    ) -> Result<()> {
        let id = store
            .state
            .pending
            .as_ref()
            .context("missing test intent")?
            .worker
            .id;
        commit_outcome(&cfg.paths.content_index, id, deferred)?;
        commit_marker(&cfg.paths.meta_index, id)?;
        store.finish_at(now)
    }

    #[test]
    fn byte_capacity_keeps_one_retry_admissible_and_shrinks_oversized_groups() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut cfg = config(root.path());
        cfg.extract.max_bytes_per_file = u64::MAX;
        cfg.extract.max_chars_per_file = u64::MAX;
        let mut store = StateStore::open(&cfg)?;
        let mut volume = volume("byte-capacity-volume");
        store.bind_volume(&mut volume)?;
        let first_key = DocKey::from_parts(volume.id, 42);
        let mut first = meta(first_key, "long-first.txt", 1);
        first.path = Some(format!("X:\\{}", "深".repeat(20_000)));
        let mut second = first.clone();
        second.key = DocKey::from_parts(volume.id, 43);
        second.name = "long-second.txt".into();

        let fresh = pending_for_volume(
            &volume,
            vec![MetadataChange::Upsert(first.clone())],
            Some(JournalCursor {
                journal_id: u64::MAX,
                last_usn: u64::MAX,
            }),
            false,
            &cfg,
        );
        // Model accumulated non-retry state without allocating many path
        // copies. The incoming intent fits, but its possible completed ledger
        // would not leave room to retry that same file afterward.
        store.state.retired_indices.push(String::new());
        let filler = MAX_STATE_BYTES - json_length(&store.state)? - (json_length(&fresh)? - 4) - 64;
        store.state.retired_indices[0].extend(std::iter::repeat_n('a', filler as usize));
        store.save()?;
        let persisted_bytes = fs::metadata(&store.path)?.len();
        {
            let mut raw_admission = store.state.clone();
            raw_admission.pending = Some(fresh.clone());
            assert!(json_length(&raw_admission)? <= MAX_STATE_BYTES);
        }
        let error = store.begin(fresh).unwrap_err();
        assert!(error.is::<DeferredCapacity>());
        assert!(store.state.pending.is_none());
        assert_eq!(fs::metadata(&store.path)?.len(), persisted_bytes);

        // A legitimately reserved ledger admits one long-file retry even when
        // the requested two-file group exceeds the available JSON budget.
        store.state.retired_indices[0].clear();
        store.state.deferred = vec![first, second]
            .into_iter()
            .map(|meta| DeferredFile {
                meta,
                attempts: u32::MAX,
                retry_at: i64::MAX,
            })
            .collect();
        let volumes = [volume];
        let whole = store
            .due_retry(&volumes, &cfg, i64::MAX, 2)
            .context("two-file retry")?;
        assert_eq!(whole.metadata.len(), 2);
        let mut one = whole.clone();
        one.metadata.truncate(1);
        one.worker.jobs.truncate(1);
        let filler = MAX_STATE_BYTES - json_length(&store.state)? - (json_length(&one)? - 4) - 256;
        store.state.retired_indices[0].extend(std::iter::repeat_n('a', filler as usize));
        assert!(json_length(&store.state)? - 4 + json_length(&whole)? > MAX_STATE_BYTES);
        let retry = store
            .due_retry(&volumes, &cfg, i64::MAX, 2)
            .context("reserved single retry")?;
        assert_eq!(retry.metadata.len(), 1);
        assert_eq!(retry.metadata[0].key(), first_key);
        assert_eq!(retry.worker.jobs.len(), 1);
        reserve_retry_capacity(&store.state, &retry)?;
        store.begin(retry)?;
        assert_eq!(store.state.pending.as_ref().unwrap().metadata.len(), 1);
        assert!(fs::metadata(&store.path)?.len() <= MAX_STATE_BYTES);
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
    fn deferred_outcome_replays_across_both_commit_windows_and_retries_independently() -> Result<()>
    {
        for metadata_committed in [false, true] {
            let root = tempfile::tempdir()?;
            let cfg = config(root.path());
            let mut store = StateStore::open(&cfg)?;
            let mut vol = volume("deferred-crash-volume");
            store.bind_volume(&mut vol)?;
            let start = JournalCursor {
                journal_id: 31,
                last_usn: 100,
            };
            let next = JournalCursor {
                last_usn: 500,
                ..start
            };
            store.volume_mut(vol.id)?.cursor = Some(start);
            store.volume_mut(vol.id)?.needs_scan = false;
            store.volume_mut(vol.id)?.catching_up = false;
            store.save()?;
            let generation = store.state.generation;
            let key = DocKey::from_parts(vol.id, 42);
            let pending = make_pending(
                vol.id,
                vec![MetadataChange::Upsert(meta(key, "blocked.txt", 90))],
                Some(next),
                false,
                &cfg,
            );
            let batch_id = pending.worker.id;
            store.begin(pending)?;
            commit_outcome(&cfg.paths.content_index, batch_id, &[key])?;
            if metadata_committed {
                commit_marker(&cfg.paths.meta_index, batch_id)?;
            }
            drop(store);

            let mut replay = StateStore::open(&cfg)?;
            assert_eq!(replay.state.generation, generation);
            assert_eq!(replay.volume(vol.id)?.cursor, Some(start));
            assert_eq!(replay.state.pending.as_ref().unwrap().worker.id, batch_id);
            assert!(replay.state.deferred.is_empty());
            if !metadata_committed {
                assert!(replay.finish_at(1000).is_err());
            }
            // Replay republishes both physical commits, while the durable
            // batch identity and the resulting obligation remain singular.
            complete_at(&mut replay, &cfg, &[key], 1000)?;
            assert_eq!(replay.volume(vol.id)?.cursor, Some(next));
            assert!(replay.state.pending.is_none());
            assert_eq!(replay.state.deferred.len(), 1);
            assert_eq!(replay.state.deferred[0].meta.key, key);
            assert_eq!(replay.state.deferred[0].attempts, 1);
            assert_eq!(replay.state.deferred[0].retry_at, 1005);
            drop(replay);

            let mut replay = StateStore::open(&cfg)?;
            assert_eq!(replay.state.generation, generation);
            assert!(replay.state.retired_indices.is_empty());
            assert_eq!(replay.state.deferred.len(), 1);
            let later = JournalCursor {
                last_usn: 700,
                ..start
            };
            replay.begin(make_pending(
                vol.id,
                vec![MetadataChange::Upsert(meta(
                    DocKey::from_parts(vol.id, 43),
                    "healthy.txt",
                    30,
                ))],
                Some(later),
                false,
                &cfg,
            ))?;
            complete_at(&mut replay, &cfg, &[], 1001)?;
            assert_eq!(replay.volume(vol.id)?.cursor, Some(later));
            assert_eq!(replay.state.deferred.len(), 1);
            assert!(
                replay
                    .due_retry(std::slice::from_ref(&vol), &cfg, 1004, 128)
                    .is_none()
            );
            assert!(replay.due_retry(&[], &cfg, 1005, 128).is_none());
            assert!(
                replay
                    .due_retry(std::slice::from_ref(&vol), &cfg, 1005, 0)
                    .is_none()
            );
            let retry = replay
                .due_retry(std::slice::from_ref(&vol), &cfg, 1005, 128)
                .unwrap();
            assert!(retry.retry);
            assert_eq!(retry.metadata.len(), 1);
            assert!(retry.next_cursor.is_none());
            replay.begin(retry)?;
            complete_at(&mut replay, &cfg, &[key], 1005)?;
            assert_eq!(replay.state.deferred[0].attempts, 2);
            assert_eq!(replay.state.deferred[0].retry_at, 1015);
            assert_eq!(replay.volume(vol.id)?.cursor, Some(later));
            drop(replay);

            let mut replay = StateStore::open(&cfg)?;
            let retry = replay
                .due_retry(std::slice::from_ref(&vol), &cfg, 1015, 128)
                .unwrap();
            replay.begin(retry)?;
            complete_at(&mut replay, &cfg, &[], 1015)?;
            assert!(replay.state.deferred.is_empty());
            assert_eq!(replay.volume(vol.id)?.cursor, Some(later));
        }
        Ok(())
    }

    #[test]
    fn retry_capacity_rejects_new_intent_before_blocking_existing_recovery() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut vol = volume("retry-capacity-volume");
        store.bind_volume(&mut vol)?;
        let cursor = JournalCursor {
            journal_id: 31,
            last_usn: 100,
        };
        store.volume_mut(vol.id)?.cursor = Some(cursor);
        store.state.deferred = (1..=MAX_DEFERRED_FILES as u64)
            .map(|id| DeferredFile {
                meta: meta(DocKey::from_parts(vol.id, id), &format!("held{id}.txt"), 20),
                attempts: 1,
                retry_at: 1000,
            })
            .collect();
        store.save()?;
        let before = fs::read(&store.path)?;
        let new = make_pending(
            vol.id,
            vec![MetadataChange::Upsert(meta(
                DocKey::from_parts(vol.id, 10_000),
                "new.txt",
                20,
            ))],
            Some(JournalCursor {
                last_usn: 200,
                ..cursor
            }),
            false,
            &cfg,
        );
        let error = store.begin(new.clone()).unwrap_err();
        assert!(error.is::<DeferredCapacity>(), "{error:#}");
        assert!(store.state.pending.is_none());
        assert_eq!(store.volume(vol.id)?.cursor, Some(cursor));
        assert_eq!(fs::read(&store.path)?, before);
        // Incomplete scan/backlog flags cannot forbid capacity recovery. This
        // admitted retry consumes no USN and frees room for the rejected event.
        let retry = store
            .due_retry(std::slice::from_ref(&vol), &cfg, 1000, 128)
            .unwrap();
        assert_eq!(retry.worker.jobs.len(), 128);
        store.begin(retry)?;
        complete_at(&mut store, &cfg, &[], 1000)?;
        assert_eq!(store.state.deferred.len(), MAX_DEFERRED_FILES - 128);
        assert_eq!(store.volume(vol.id)?.cursor, Some(cursor));
        store.begin(new)?;
        complete_at(&mut store, &cfg, &[], 1001)?;
        assert_eq!(store.volume(vol.id)?.cursor.unwrap().last_usn, 200);
        Ok(())
    }

    #[test]
    fn version_two_upgrade_preserves_bound_volume_and_physical_checkpoints() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut vol = volume("upgrade-volume");
        store.bind_volume(&mut vol)?;
        let cursor = JournalCursor {
            journal_id: 31,
            last_usn: 700,
        };
        store.volume_mut(vol.id)?.cursor = Some(cursor);
        store.volume_mut(vol.id)?.needs_scan = false;
        store.save()?;
        let generation = store.state.generation;
        let content_commit = store.state.content_commit;
        let meta_commit = store.state.meta_commit;
        let mut old = serde_json::to_value(&store.state)?;
        old["version"] = 2.into();
        old.as_object_mut().unwrap().remove("deferred");
        atomic_write(&store.path, &serde_json::to_vec(&old)?)?;
        drop(store);
        let reopened = StateStore::open(&cfg)?;
        assert_eq!(reopened.state.version, 3);
        assert_eq!(reopened.state.generation, generation);
        assert_eq!(reopened.state.content_commit, content_commit);
        assert_eq!(reopened.state.meta_commit, meta_commit);
        assert_eq!(reopened.volume(vol.id)?.cursor, Some(cursor));
        assert!(!reopened.volume(vol.id)?.needs_scan);
        assert!(reopened.state.deferred.is_empty());
        assert!(reopened.state.retired_indices.is_empty());
        let disk: serde_json::Value = serde_json::from_slice(&fs::read(&reopened.path)?)?;
        assert_eq!(disk["version"], 3);
        Ok(())
    }

    #[test]
    fn unowned_deferred_outcome_cannot_publish_a_checkpoint() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut vol = volume("invalid-outcome-volume");
        store.bind_volume(&mut vol)?;
        let key = DocKey::from_parts(vol.id, 42);
        store.begin(make_pending(
            vol.id,
            vec![MetadataChange::Upsert(meta(key, "owned.txt", 90))],
            Some(JournalCursor {
                journal_id: 31,
                last_usn: 500,
            }),
            false,
            &cfg,
        ))?;
        let error =
            complete_at(&mut store, &cfg, &[DocKey::from_parts(vol.id, 43)], 1000).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the durable reconciliation batch")
        );
        assert!(store.state.pending.is_some());
        assert!(store.state.deferred.is_empty());
        assert!(store.volume(vol.id)?.cursor.is_none());
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
