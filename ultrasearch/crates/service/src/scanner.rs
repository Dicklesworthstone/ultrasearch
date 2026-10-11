//! Serialized, durable MFT reconciliation and USN ingestion.

mod state;
mod stats;

use crate::scheduler_runtime::{content_admission_available, submit_index_batch};
use crate::status_provider::{
    update_status_ingestion_state, update_status_last_commit, update_status_volumes,
};
use anyhow::{Context, Result, ensure};
use core_types::config::AppConfig;
use core_types::{DocKey, FileFlags, FileMeta, VolumeId, index_path_matches, normalize_index_path};
use ipc::VolumeStatus;
use ntfs_watcher::{
    FileEvent, JournalBatch, JournalCursor, NtfsError, ReaderConfig, VolumeInfo, begin_mft_scan,
    canonical_path, discover_volumes, resolve_file_ids, tail_usn_batch_with_config,
};
use state::{
    BATCH_LIMIT, ContentPolicy, DeferredFile, DirectoryRepairCheckpoint, MAX_DEFERRED_FILES,
    MetadataChange, PendingBatch, StateStore, event_changes, pending_for_volume,
    validate_deferred_outcome,
};
use stats::IndexStatistics;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock, RwLockReadGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, MissedTickBehavior, interval};

static RESCAN_GENERATION: AtomicU64 = AtomicU64::new(0);
const MUTATION_BATCH_LIMIT: usize = 128;

type MftPages = Box<dyn Iterator<Item = std::result::Result<Vec<FileMeta>, NtfsError>> + Send>;
type MftScans = BTreeMap<VolumeId, MftProgress<MftPages>>;
type DirectoryRepairs = BTreeMap<VolumeId, DirectoryProgress>;

/// A frozen metadata snapshot and at most one unadmitted mutation page. Old
/// prefixes are durable; the readers are recreated after restart without ever
/// resetting unrelated documents or advancing past unfinished descendants.
struct DirectoryProgress {
    checkpoint: DirectoryRepairCheckpoint,
    index: Option<meta_index::PathScan>,
    index_done: bool,
    deferred: VecDeque<FileMeta>,
    discovery: Option<MftPages>,
    discovery_done: bool,
    ordinary: VecDeque<MetadataChange>,
    roots: VecDeque<DocKey>,
    page: Option<Vec<MetadataChange>>,
    final_page: bool,
}

/// One bounded reader and, at most, one not-yet-admitted page per volume.
/// Readers are deliberately volatile: a service restart repeats the reset and
/// baseline, while durable pending work must finish before any reader resumes.
struct MftProgress<I> {
    start: JournalCursor,
    batches: Option<I>,
    reset_admitted: bool,
    page: Option<Vec<FileMeta>>,
}

impl<I> MftProgress<I> {
    fn new(start: JournalCursor, batches: I) -> Self {
        Self {
            start,
            batches: Some(batches),
            reset_admitted: false,
            page: None,
        }
    }
}

#[derive(Debug, Default)]
struct VolumeProgress {
    journal_read: bool,
    work_remaining: bool,
}

/// Request reconciliation on the same serialized lane as journal changes.
pub fn request_rescan() {
    RESCAN_GENERATION.fetch_add(1, Ordering::Relaxed);
    update_status_ingestion_state("reconciliation requested");
}

/// Entries being replaced are hidden from both indices until their transaction
/// commits. Directory repair masks only affected path prefixes; journal gaps
/// and incomplete baselines still hide the complete volume.
#[derive(Default)]
pub(crate) struct Visibility {
    pub volumes: BTreeSet<VolumeId>,
    pub documents: BTreeSet<DocKey>,
    pub directories: BTreeMap<VolumeId, Vec<String>>,
}

static VISIBILITY: OnceLock<RwLock<Visibility>> = OnceLock::new();

pub(crate) fn read_visibility() -> RwLockReadGuard<'static, Visibility> {
    VISIBILITY
        .get_or_init(RwLock::default)
        .read()
        .unwrap_or_else(|e| e.into_inner())
}

fn hide_volume(id: VolumeId) {
    VISIBILITY
        .get_or_init(RwLock::default)
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .volumes
        .insert(id);
}

fn show_volume(id: VolumeId) {
    let mut visibility = VISIBILITY
        .get_or_init(RwLock::default)
        .write()
        .unwrap_or_else(|e| e.into_inner());
    visibility.volumes.remove(&id);
    visibility.directories.remove(&id);
}

fn hide_directories(id: VolumeId, paths: &[String]) {
    VISIBILITY
        .get_or_init(RwLock::default)
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .directories
        .insert(id, paths.to_vec());
}

fn show_directories(id: VolumeId) {
    VISIBILITY
        .get_or_init(RwLock::default)
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .directories
        .remove(&id);
}

fn hide_pending(batch: &PendingBatch) {
    let mut visibility = VISIBILITY
        .get_or_init(RwLock::default)
        .write()
        .unwrap_or_else(|e| e.into_inner());
    visibility
        .documents
        .extend(batch.metadata.iter().map(MetadataChange::key));
    visibility
        .volumes
        .extend(batch.worker.reset_volumes.iter().copied());
}

fn show_pending(batch: &PendingBatch) {
    let mut visibility = VISIBILITY
        .get_or_init(RwLock::default)
        .write()
        .unwrap_or_else(|e| e.into_inner());
    for change in &batch.metadata {
        visibility.documents.remove(&change.key());
    }
}

/// Called before installing the search handler. Startup never serves an old
/// snapshot before validating the journal and replaying durable pending work.
pub struct IngestionSession {
    store: StateStore,
}

impl Drop for IngestionSession {
    fn drop(&mut self) {
        // Shutdown, cancellation, or a fatal watcher error removes the claim
        // that an old snapshot is current, including in same-process restarts.
        for volume in &self.store.state.volumes {
            hide_volume(volume.id);
        }
    }
}

pub fn initialize_indexes(cfg: &AppConfig) -> Result<IngestionSession> {
    let store = StateStore::open(cfg)?;
    // Reinitialization in the same process must not retain masks belonging to
    // a previous generation whose pending intent was invalidated by a rebuild.
    *VISIBILITY
        .get_or_init(RwLock::default)
        .write()
        .unwrap_or_else(|e| e.into_inner()) = Visibility::default();
    for volume in &store.state.volumes {
        hide_volume(volume.id);
    }
    if let Some(batch) = &store.state.pending {
        hide_pending(batch);
    }
    update_status_ingestion_state("initializing; journal validation pending");
    Ok(IngestionSession { store })
}

pub async fn watch_changes(mut cfg: AppConfig, mut session: IngestionSession) -> Result<()> {
    #[cfg(windows)]
    ntfs_watcher::enable_backup_privilege()
        .context("USN indexing requires SeBackupPrivilege in the service token")?;
    let store = &mut session.store;
    let mut generation = RESCAN_GENERATION.load(Ordering::Relaxed);
    let mut last_idle_checkpoint = Instant::now();
    let mut statistics = IndexStatistics::new(Path::new(&cfg.paths.meta_index))?;
    let mut scans = MftScans::new();
    let mut repairs = DirectoryRepairs::new();
    let mut work_remaining = true;
    let mut ticker = interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        if work_remaining {
            // Give every selected volume one bounded turn before continuing
            // active scans/backlogs. The one-second idle poll must not throttle
            // a large baseline to one 128-record page per second.
            tokio::task::yield_now().await;
        } else {
            ticker.tick().await;
        }
        work_remaining = false;
        let current = core_types::config::get_current_config();
        // Changing index destinations requires a service restart, since readers
        // and the dispatcher already own the original paths.
        ensure!(
            current.paths.meta_index == cfg.paths.meta_index
                && current.paths.content_index == cfg.paths.content_index
                && current.paths.state_dir == cfg.paths.state_dir
                && current.paths.jobs_dir == cfg.paths.jobs_dir
                && current.logging.file == cfg.logging.file
                && current.semantic.index_dir == cfg.semantic.index_dir,
            "runtime output paths changed; restart the service to apply them"
        );
        cfg = current;
        let requested = RESCAN_GENERATION.load(Ordering::Relaxed);
        if requested != generation {
            scans.clear();
            repairs.clear();
            for volume in &mut store.state.volumes {
                volume.needs_scan = true;
                volume.catching_up = true;
                volume.directory_repair = None;
                volume.unsettled_directories.clear();
                hide_volume(volume.id);
            }
            store.save()?;
            generation = requested;
        }

        let discovered = tokio::task::spawn_blocking(discover_volumes).await?;
        let volumes = match discovered {
            Ok(volumes) => filter_volumes(&cfg, volumes),
            Err(error) => {
                scans.clear();
                repairs.clear();
                for volume in &store.state.volumes {
                    hide_volume(volume.id);
                }
                update_status_ingestion_state(format!(
                    "unavailable: {error}; no polling fallback is active"
                ));
                tracing::error!(%error, "volume discovery failed; checkpoint unchanged");
                continue;
            }
        };
        let mut volumes = volumes;
        for volume in &mut volumes {
            store.bind_volume(volume)?;
            if refresh_content_policy(store, volume, &cfg)? {
                scans.remove(&volume.id);
                repairs.remove(&volume.id);
            }
        }
        let selected: BTreeSet<_> = volumes.iter().map(|v| v.id).collect();
        scans.retain(|id, _| selected.contains(id));
        repairs.retain(|id, _| selected.contains(id));
        for volume in &store.state.volumes {
            if !selected.contains(&volume.id) {
                hide_volume(volume.id);
            }
        }
        // A failed batch remains durable and masks stale results. Replay it
        // before reading any later journal record, preserving worker ordering.
        // Recorded jobs are GUID-bound and retain their admission policy. An
        // unavailable source becomes a durable worker deferral; deselection or
        // unmounting cannot strand every other volume behind the global intent.
        if let Err(error) = replay_pending(store, &cfg, &selected, commit_pending).await {
            update_status_ingestion_state(format!("retrying durable batch: {error:#}"));
            tracing::error!(%error, "pending batch failed; cursor retained");
            publish_status(store, &mut statistics)?;
            continue;
        }
        let reclaimed = store.reclaim_unavailable_retries(&selected)?;
        if reclaimed > 0 {
            tracing::info!(
                files = reclaimed,
                "unavailable-volume retries retained as full reconciliation; capacity released"
            );
        }
        if volumes.is_empty() {
            update_status_ingestion_state("unavailable: no configured NTFS volume is mounted");
            publish_status(store, &mut statistics)?;
            continue;
        }

        let mut retried = false;
        if store
            .state
            .deferred
            .len()
            .saturating_add(MUTATION_BATCH_LIMIT)
            > MAX_DEFERRED_FILES
            && content_admission_available()
            && let Some(retry) =
                store.due_retry(&volumes, &cfg, unix_timestamp_secs(), MUTATION_BATCH_LIMIT)
        {
            // Reserved capacity leaves the global pending slot available for
            // old obligations. Under pressure, service these before attempting
            // new extraction; retries cannot advance the journal cursor.
            retried = true;
            let result = match store.begin(retry) {
                Ok(()) => commit_pending(store, &cfg).await,
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                update_status_ingestion_state(format!("deferred capacity recovery: {error:#}"));
                publish_status(store, &mut statistics)?;
                continue;
            }
        }

        let mut failures: Vec<_> = cfg
            .volumes
            .iter()
            .filter(|mount| {
                !volumes.iter().any(|volume| {
                    volume
                        .drive_letters
                        .iter()
                        .any(|letter| mount.eq_ignore_ascii_case(&format!("{letter}:\\")))
                })
            })
            .map(|mount| format!("configured volume {mount} is unavailable or not NTFS"))
            .collect();
        let mut retry_volumes = Vec::new();
        for volume in &volumes {
            if store.volume(volume.id)?.needs_scan
                && !scans.contains_key(&volume.id)
                && store
                    .state
                    .deferred
                    .len()
                    .saturating_add(MUTATION_BATCH_LIMIT)
                    > MAX_DEFERRED_FILES
                && ContentPolicy::for_volume(volume, &cfg).enabled
            {
                // Preserve incomplete-baseline obligations while capacity is
                // exhausted; restarting/resetting the MFT here would discard
                // their backoff and repeat the same failed baseline forever.
                failures.push(format!(
                    "volume {} reconciliation waiting for deferred extraction capacity",
                    volume.id
                ));
                continue;
            }
            match process_volume(store, volume, &cfg, &mut scans, &mut repairs).await {
                Ok(progress) => {
                    work_remaining |= progress.work_remaining;
                    if progress.journal_read {
                        retry_volumes.push(volume.clone());
                    }
                }
                Err(error) => {
                    if error.is::<state::DeferredCapacity>() {
                        // Admission stopped before installing new intent. Old
                        // obligations must be able to free capacity even while
                        // this volume still has unread journal records.
                        retry_volumes.push(volume.clone());
                    }
                    hide_volume(volume.id);
                    failures.push(format!("volume {}: {error:#}", volume.id));
                    tracing::error!(volume = volume.id, %error, "ingestion deferred; checkpoint retained");
                    if store.state.pending.is_some() {
                        break;
                    }
                }
            }
        }
        // Fresh journal events supersede old retry obligations before retries
        // are considered. Admit at most one bounded retry batch after a valid
        // read or capacity rejection, while extraction's idle policy permits it.
        // Waiting for full catch-up would starve retries under continuous churn.
        if store.state.pending.is_none()
            && !retried
            && content_admission_available()
            && let Some(retry) = store.due_retry(
                &retry_volumes,
                &cfg,
                unix_timestamp_secs(),
                MUTATION_BATCH_LIMIT,
            )
        {
            let result = match store.begin(retry) {
                Ok(()) => commit_pending(store, &cfg).await,
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                failures.push(format!("deferred extraction retry: {error:#}"));
                tracing::error!(%error, "retry batch remains durable; no journal cursor consumed");
            }
        }
        let deferred = store
            .state
            .deferred
            .iter()
            .filter(|retry| selected.contains(&retry.meta.key.volume()))
            .count();
        if failures.is_empty() {
            let pending = store
                .state
                .volumes
                .iter()
                .any(|v| selected.contains(&v.id) && (v.needs_scan || v.catching_up));
            update_status_ingestion_state(if deferred > 0 {
                format!(
                    "degraded: {deferred} files deferred for extraction retry; journal {}",
                    if pending {
                        "catching up"
                    } else {
                        "reads continuing"
                    }
                )
            } else if pending {
                "catching up with journal".into()
            } else {
                "watching; last bounded journal read succeeded".into()
            });
        } else {
            update_status_ingestion_state(format!("degraded: {}", failures.join("; ")));
        }
        publish_status(store, &mut statistics)?;
        // Advancing across ignored records requires no index mutation. Flush
        // that cursor occasionally rather than journaling a checkpoint write
        // for every checkpoint write that NTFS reports back to us.
        if last_idle_checkpoint.elapsed() >= Duration::from_secs(60) {
            store.save()?;
            last_idle_checkpoint = Instant::now();
        }
    }
}

fn filter_volumes(cfg: &AppConfig, volumes: Vec<VolumeInfo>) -> Vec<VolumeInfo> {
    volumes
        .into_iter()
        .filter(|v| {
            cfg.volumes.is_empty()
                || v.drive_letters.iter().any(|letter| {
                    cfg.volumes
                        .iter()
                        .any(|mount| mount.eq_ignore_ascii_case(&format!("{letter}:\\")))
                })
        })
        .collect()
}

/// Availability controls new reads and retries, never already durable intent.
/// Completing an unavailable volume's GUID-bound batch can defer its files and
/// release the global lane while keeping that volume hidden from every search.
async fn replay_pending<C>(
    store: &mut StateStore,
    cfg: &AppConfig,
    selected: &BTreeSet<VolumeId>,
    mut commit: C,
) -> Result<()>
where
    C: AsyncFnMut(&mut StateStore, &AppConfig) -> Result<()>,
{
    if let Some(batch) = &store.state.pending {
        if !selected.contains(&batch.volume) {
            hide_volume(batch.volume);
        }
        commit(store, cfg).await?;
    }
    Ok(())
}

fn refresh_content_policy(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
) -> Result<bool> {
    let policy = ContentPolicy::for_volume(volume, cfg);
    if store.volume(volume.id)?.content_policy.as_ref() == Some(&policy) {
        return Ok(false);
    }
    // Mask old coverage before persisting a policy change. Previously admitted
    // intent keeps its operations; after replay a full reset applies this policy.
    hide_volume(volume.id);
    store.set_content_policy(volume.id, policy)?;
    Ok(true)
}

/// A volume receives one bounded baseline turn or one bounded journal read.
/// Retry fairness must not depend on reaching the journal head.
async fn process_volume(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
    scans: &mut MftScans,
    repairs: &mut DirectoryRepairs,
) -> Result<VolumeProgress> {
    if store.volume(volume.id)?.needs_scan || store.volume(volume.id)?.cursor.is_none() {
        repairs.remove(&volume.id);
        if store
            .volume_mut(volume.id)?
            .directory_repair
            .take()
            .is_some()
        {
            store.save()?;
        }
        reconcile_volume(store, volume, cfg, scans).await?;
        return Ok(VolumeProgress {
            journal_read: false,
            work_remaining: true,
        });
    }
    let cursor = store
        .volume(volume.id)?
        .cursor
        .context("volume has no journal checkpoint")?;
    if let Some(repair) = repairs.get_mut(&volume.id) {
        // Pending replay may have completed the final page after the original
        // caller was cancelled. Never replay the volatile reader past its USN.
        if cursor == repair.checkpoint.through
            && store.volume(volume.id)?.directory_repair.is_none()
        {
            repairs.remove(&volume.id);
            show_directories(volume.id);
            return Ok(VolumeProgress {
                journal_read: true,
                work_remaining: true,
            });
        }
        let result = directory_turn(store, volume, cfg, repair).await;
        match result {
            Ok(done) => {
                if done {
                    repairs.remove(&volume.id);
                    show_directories(volume.id);
                }
                return Ok(VolumeProgress {
                    journal_read: done,
                    work_remaining: true,
                });
            }
            Err(error) => {
                if error
                    .downcast_ref::<NtfsError>()
                    .is_some_and(|error| matches!(error, NtfsError::GapDetected))
                    || (error.is::<state::DeferredCapacity>() && store.state.deferred.is_empty())
                {
                    // With no retry obligation left, capacity cannot recover
                    // while the repair's history occupies the state envelope.
                    // A full scan can replace those anchors without losing work.
                    repairs.remove(&volume.id);
                    require_full_scan(store, volume.id)?;
                    return Ok(VolumeProgress {
                        journal_read: false,
                        work_remaining: true,
                    });
                }
                if store.state.pending.is_none() && !error.is::<state::DeferredCapacity>() {
                    repairs.remove(&volume.id);
                }
                return Err(error);
            }
        }
    }
    let read_volume = volume.clone();
    let read_config = reader_config(cfg, &store.state.retired_indices)?;
    let result = tokio::task::spawn_blocking(move || {
        tail_usn_batch_with_config(&read_volume, cursor, &read_config)
    })
    .await?;
    match result {
        Err(NtfsError::GapDetected) => {
            require_full_scan(store, volume.id)?;
            update_status_ingestion_state(format!("volume {} journal gap; reconciling", volume.id));
            Ok(VolumeProgress {
                journal_read: false,
                work_remaining: true,
            })
        }
        Err(error) => Err(error.into()),
        Ok(batch) => {
            if batch
                .events
                .iter()
                .any(|event| matches!(event, FileEvent::DirectoryRenamed { .. }))
            {
                let config = reader_config(cfg, &store.state.retired_indices)?;
                let mut parent_config = config.clone();
                parent_config.exclude_paths.clear();
                let parent_keys: Vec<_> = batch
                    .events
                    .iter()
                    .filter_map(|event| {
                        if let FileEvent::DirectoryRenamed { parent, .. } = event {
                            Some(*parent)
                        } else {
                            None
                        }
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let read_volume = volume.clone();
                let parents = tokio::task::spawn_blocking(move || {
                    resolve_file_ids(&read_volume, cursor, &parent_keys, &parent_config)
                })
                .await??;
                match prepare_directory_repair(store, volume, cfg, &batch, &config, &parents)? {
                    Some(repair) => {
                        repairs.insert(volume.id, repair);
                    }
                    None => require_full_scan(store, volume.id)?,
                }
                return Ok(VolumeProgress {
                    journal_read: false,
                    work_remaining: true,
                });
            }
            ensure!(
                store.volume(volume.id)?.directory_repair.is_none(),
                "journal replay lost an unfinished directory range"
            );
            apply_journal_batch(store, volume, cfg, batch, commit_pending).await
        }
    }
}

fn require_full_scan(store: &mut StateStore, volume: VolumeId) -> Result<()> {
    hide_volume(volume);
    let state = store.volume_mut(volume)?;
    state.needs_scan = true;
    state.catching_up = true;
    state.directory_repair = None;
    state.unsettled_directories.clear();
    store.save()
}

fn path_selected(path: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|prefix| index_path_matches(path, prefix))
}

/// Combine the indexed ancestry with every recorded rename component. Neither
/// an OLD_NAME bit nor a freshly resolved parent proves a historical full path:
/// records accumulate reasons, and a parent may itself have moved meanwhile.
fn prepare_directory_repair(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
    batch: &JournalBatch,
    reader: &ReaderConfig,
    current_parents: &[Option<FileMeta>],
) -> Result<Option<DirectoryProgress>> {
    let from = store
        .volume(volume.id)?
        .cursor
        .context("missing journal cursor")?;
    let index = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
    let mut roots = BTreeSet::new();
    let mut records = Vec::new();
    let mut anchors: BTreeMap<DocKey, BTreeSet<String>> = BTreeMap::new();
    let mut indexed_roots = BTreeSet::new();
    let mut current_roots = BTreeMap::new();
    let mut ordinary = Vec::new();
    for event in &batch.events {
        if let FileEvent::DirectoryRenamed {
            doc,
            parent,
            name,
            current,
        } = event
        {
            ensure!(
                doc.volume() == volume.id && parent.volume() == volume.id,
                "cross-volume directory history"
            );
            if doc == parent
                || name.is_empty()
                || name == "."
                || name == ".."
                || name.contains(['\\', '/'])
            {
                return Ok(None);
            }
            roots.insert(*doc);
            records.push((*doc, *parent, name));
            if let Some(meta) = current {
                ensure!(
                    meta.key == *doc && meta.flags.contains(FileFlags::IS_DIR),
                    "invalid directory snapshot"
                );
                if let Some(path) = &meta.path {
                    current_roots.insert(*doc, normalize_index_path(path));
                    anchors
                        .entry(*doc)
                        .or_default()
                        .insert(normalize_index_path(path));
                }
            }
        } else {
            ordinary.push(event.clone());
        }
    }
    ensure!(!roots.is_empty(), "directory repair has no root");
    let Some(mut ordinary) = resolve_events(&ordinary, cfg, &store.state.deferred)? else {
        return Ok(None);
    };
    ordinary.retain(|change| !roots.contains(&change.key()));
    let identities: BTreeSet<_> = records
        .iter()
        .flat_map(|(doc, parent, _)| [*doc, *parent])
        .collect();
    for key in identities {
        if let Some(meta) = meta_index::file_meta(&index, key)? {
            ensure!(
                meta.flags.contains(FileFlags::IS_DIR),
                "indexed directory ancestry is not a directory"
            );
            if let Some(path) = meta.path {
                anchors
                    .entry(key)
                    .or_default()
                    .insert(normalize_index_path(&path));
                if roots.contains(&key) {
                    indexed_roots.insert(key);
                }
            }
        } else if let Some(meta) = current_parents
            .iter()
            .flatten()
            .find(|meta| meta.key == key)
            && let Some(path) = &meta.path
        {
            // A volume root cannot be renamed. A known excluded parent has no
            // indexed descendants to preserve. Other missing historical parents
            // are ambiguous and must be covered by another recorded anchor.
            if meta.flags.contains(FileFlags::IS_DIR)
                && (normalize_index_path(path) == normalize_index_path(&volume.guid_path)
                    || path_selected(path, &reader.exclude_paths))
            {
                anchors
                    .entry(key)
                    .or_default()
                    .insert(normalize_index_path(path));
            }
        }
    }
    // Propagate parent history through nested moves in the same raw batch.
    // Temporal ancestry can contain cycles; bound that ambiguity and retain
    // conservative full reconciliation instead of expanding paths indefinitely.
    loop {
        let mut changed = false;
        for (doc, parent, name) in &records {
            let parents = anchors.get(parent).cloned().unwrap_or_default();
            for parent_path in parents {
                let path = normalize_index_path(&format!("{parent_path}\\{name}"));
                changed |= anchors.entry(*doc).or_default().insert(path);
                if anchors.values().map(BTreeSet::len).sum::<usize>() > BATCH_LIMIT {
                    return Ok(None);
                }
            }
        }
        if !changed {
            break;
        }
    }
    // An unanchored historical parent could conceal an intermediate subtree
    // during baseline enumeration. Current paths alone cannot certify coverage.
    if records
        .iter()
        .any(|(_, parent, _)| !anchors.contains_key(parent))
    {
        return Ok(None);
    }
    let mut paths: BTreeSet<String> = roots
        .iter()
        .flat_map(|root| anchors.get(root).into_iter().flatten().cloned())
        .collect();
    let mut discovery_roots = store
        .volume(volume.id)?
        .unsettled_directories
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    // While directory coverage is unsettled, an indexed ancestor may move the
    // subtree, or a partially indexed child may move out of it. No persisted
    // parent graph can prove either unaffected: discover every renamed root
    // until a fresh successful journal catch-up settles this coverage.
    // Baseline, edit, or retry jobs can also lose old pathnames to a directory
    // move before any rename repair exists. Their admission retains catch-up
    // until a journal read AFTER those workers, covering vanished index rows.
    let discovery_unsettled = !discovery_roots.is_empty() || store.volume(volume.id)?.catching_up;
    let mut discover_paths = BTreeSet::new();
    for (key, path) in current_roots {
        // A mixed-time baseline can index the root after it left an exclusion
        // while having skipped its unchanged children beforehand. Historical
        // excluded ancestry therefore still requires discovery for known roots.
        let excluded_history = anchors.get(&key).is_some_and(|paths| {
            paths
                .iter()
                .any(|path| path_selected(path, &reader.exclude_paths))
        });
        if (!indexed_roots.contains(&key) || discovery_unsettled || excluded_history)
            && !path_selected(&path, &reader.exclude_paths)
        {
            discovery_roots.insert(key);
            discover_paths.insert(path);
        }
    }
    // A queued descendant pathname can disappear after another directory move.
    // The worker then correctly tombstones that obsolete path, erasing the only
    // indexed child identity. Retain every repaired root until fresh catch-up so
    // the later rename discovers such unchanged children even for known roots.
    // Decide this turn's discovery above, using only the prior obligations.
    discovery_roots.extend(roots.iter().copied());
    if discovery_roots.len() > BATCH_LIMIT {
        return Ok(None);
    }
    if let Some(previous) = &store.volume(volume.id)?.directory_repair {
        paths.extend(previous.paths.iter().cloned());
        discover_paths.extend(previous.discover_paths.iter().cloned());
    }
    paths.extend(discover_paths.iter().cloned());
    if paths.is_empty() || paths.len() > BATCH_LIMIT {
        return Ok(None);
    }
    let checkpoint = DirectoryRepairCheckpoint {
        from,
        through: batch.cursor,
        paths: paths.into_iter().collect(),
        discover_paths: discover_paths.into_iter().collect(),
    };
    // Compile the mask before publishing intent; malformed/oversized query
    // state may never result in an unprotected descendant mutation.
    meta_index::path_prefix_query(index.fields.path_exact, &checkpoint.paths)?;
    let scan = meta_index::PathScan::new(&index, volume.id, &checkpoint.paths)?;
    let deferred = store
        .state
        .deferred
        .iter()
        .filter(|retry| {
            retry.meta.volume == volume.id
                && retry
                    .meta
                    .path
                    .as_ref()
                    .is_some_and(|path| path_selected(path, &checkpoint.paths))
        })
        .map(|retry| retry.meta.clone())
        .collect();
    if let Err(error) = store.begin_directory_repair(
        volume.id,
        checkpoint.clone(),
        &discovery_roots.into_iter().collect::<Vec<_>>(),
    ) {
        if error.is::<state::DeferredCapacity>() && store.state.deferred.is_empty() {
            return Ok(None);
        }
        return Err(error);
    }
    hide_directories(volume.id, &checkpoint.paths);
    Ok(Some(DirectoryProgress {
        discovery_done: checkpoint.discover_paths.is_empty(),
        checkpoint,
        index: Some(scan),
        index_done: false,
        deferred,
        discovery: None,
        ordinary: ordinary.into(),
        roots: roots.into_iter().collect(),
        page: None,
        final_page: false,
    }))
}

async fn directory_turn(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
    repair: &mut DirectoryProgress,
) -> Result<bool> {
    let mut config = reader_config(cfg, &store.state.retired_indices)?;
    config.max_records_per_tick = MUTATION_BATCH_LIMIT;
    if repair.index_done
        && repair.deferred.is_empty()
        && !repair.discovery_done
        && repair.discovery.is_none()
    {
        // A directory entering indexed coverage may contain unchanged files
        // with no individual USN event. Enumerate bounded MFT pages, select this
        // subtree, and keep every unrelated index entry intact and searchable.
        let read_volume = volume.clone();
        let scan_config = config.clone();
        let mut scan =
            tokio::task::spawn_blocking(move || begin_mft_scan(&read_volume, &scan_config))
                .await??;
        repair.discovery = Some(Box::new(std::iter::from_fn(move || {
            scan.next_batch().transpose()
        })));
        return Ok(false);
    }
    let from = repair.checkpoint.from;
    apply_directory_turn(
        store,
        volume,
        cfg,
        repair,
        |keys| {
            let read_volume = volume.clone();
            let config = config.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    resolve_file_ids(&read_volume, from, &keys, &config)
                })
                .await?
                .map_err(Into::into)
            }
        },
        commit_pending,
    )
    .await
}

fn same_snapshot(left: &FileMeta, right: &FileMeta) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.parent = None;
    right.parent = None;
    left == right
}

/// Resolve and commit one bounded page, including empty-page/EOF turns. A page
/// rejected before durable admission remains attached to this reader. Once
/// admitted, the existing global pending lane owns it across failure/restart.
async fn apply_directory_turn<R, F, C>(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
    repair: &mut DirectoryProgress,
    mut resolve: R,
    mut commit: C,
) -> Result<bool>
where
    R: FnMut(Vec<DocKey>) -> F,
    F: std::future::Future<Output = Result<Vec<Option<FileMeta>>>> + Send,
    C: AsyncFnMut(&mut StateStore, &AppConfig) -> Result<()>,
{
    ensure!(
        store.state.pending.is_none(),
        "pending work must replay before directory repair"
    );
    ensure!(
        store.volume(volume.id)?.directory_repair.as_ref() == Some(&repair.checkpoint),
        "directory repair lost its durable anchors"
    );
    if repair.page.is_none() {
        let (observed, force) = if !repair.index_done {
            let mut index = repair
                .index
                .take()
                .context("directory snapshot task was cancelled")?;
            let (returned, page) = tokio::task::spawn_blocking(move || {
                let page = index.next_batch(MUTATION_BATCH_LIMIT);
                (index, page)
            })
            .await?;
            repair.index = Some(returned);
            let Some(page) = page? else {
                repair.index_done = true;
                return Ok(false);
            };
            (
                page.into_iter()
                    .filter(|meta| !repair.roots.contains(&meta.key))
                    .collect::<Vec<_>>(),
                false,
            )
        } else if !repair.deferred.is_empty() {
            let count = repair.deferred.len().min(MUTATION_BATCH_LIMIT);
            (repair.deferred.drain(..count).collect(), true)
        } else if !repair.discovery_done {
            let mut reader = repair
                .discovery
                .take()
                .context("directory discovery reader is not open")?;
            let (returned, page) = tokio::task::spawn_blocking(move || {
                let page = reader.next();
                (reader, page)
            })
            .await?;
            repair.discovery = Some(returned);
            let Some(page) = page else {
                repair.discovery_done = true;
                return Ok(false);
            };
            let page = page?;
            ensure!(
                page.len() <= MUTATION_BATCH_LIMIT,
                "directory discovery page exceeded limit"
            );
            let index = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
            let mut changes = Vec::new();
            for meta in page {
                if !repair.roots.contains(&meta.key)
                    && meta
                        .path
                        .as_ref()
                        .is_some_and(|path| path_selected(path, &repair.checkpoint.discover_paths))
                    && meta_index::file_meta(&index, meta.key)?
                        .is_none_or(|old| !same_snapshot(&old, &meta))
                {
                    changes.push(MetadataChange::Upsert(meta));
                }
            }
            repair.page = Some(changes);
            (Vec::new(), false)
        } else if !repair.ordinary.is_empty() {
            let count = repair.ordinary.len().min(MUTATION_BATCH_LIMIT);
            let original: Vec<_> = repair.ordinary.iter().take(count).cloned().collect();
            let keys: Vec<_> = original
                .iter()
                .filter_map(|change| {
                    if let MetadataChange::Upsert(meta) = change {
                        Some(meta.key)
                    } else {
                        None
                    }
                })
                .collect();
            let current = resolve(keys.clone()).await?;
            ensure!(
                current.len() == keys.len(),
                "ordinary resolver omitted results"
            );
            let mut refreshed = BTreeMap::new();
            for (key, meta) in keys.into_iter().zip(current) {
                if let Some(meta) = &meta {
                    ensure!(
                        meta.key == key && meta.volume == volume.id,
                        "ordinary resolver changed identity"
                    );
                }
                refreshed.insert(
                    key,
                    meta.map_or(MetadataChange::Delete(key), MetadataChange::Upsert),
                );
            }
            repair.ordinary.drain(..count);
            repair.page = Some(
                original
                    .into_iter()
                    .map(|change| refreshed.remove(&change.key()).unwrap_or(change))
                    .collect(),
            );
            (Vec::new(), false)
        } else {
            let count = repair.roots.len().min(MUTATION_BATCH_LIMIT);
            let keys: Vec<_> = repair.roots.iter().take(count).copied().collect();
            // The final identity probe also revalidates the ORIGINAL journal
            // position after any long pause or subtree discovery.
            let current = resolve(keys.clone()).await?;
            ensure!(
                current.len() == keys.len(),
                "directory resolver omitted results"
            );
            let mut changes = Vec::new();
            for (key, meta) in keys.iter().zip(current) {
                if let Some(meta) = &meta {
                    ensure!(meta.key == *key, "directory resolver changed identity");
                }
                changes.push(meta.map_or(MetadataChange::Delete(*key), MetadataChange::Upsert));
            }
            repair.roots.drain(..count);
            repair.final_page = repair.roots.is_empty();
            repair.page = Some(changes);
            (Vec::new(), false)
        };
        if repair.page.is_none() {
            if observed.is_empty() {
                return Ok(false);
            }
            let keys: Vec<_> = observed.iter().map(|meta| meta.key).collect();
            let current = resolve(keys.clone()).await?;
            ensure!(
                current.len() == keys.len(),
                "descendant resolver omitted results"
            );
            let mut changes = BTreeMap::new();
            for (old, meta) in observed.into_iter().zip(current) {
                if let Some(meta) = &meta {
                    ensure!(
                        meta.key == old.key && meta.volume == volume.id,
                        "descendant resolver changed identity"
                    );
                    if !force && same_snapshot(&old, meta) {
                        continue;
                    }
                }
                changes.insert(
                    old.key,
                    meta.map_or(MetadataChange::Delete(old.key), MetadataChange::Upsert),
                );
            }
            repair.page = Some(changes.into_values().collect());
        }
    }
    let page = repair
        .page
        .as_ref()
        .context("directory mutation page missing")?;
    if page.is_empty() && !repair.final_page {
        repair.page = None;
        return Ok(false);
    }
    store.begin(pending_for_volume(
        volume,
        page.clone(),
        repair.final_page.then_some(repair.checkpoint.through),
        false,
        cfg,
    ))?;
    repair.page = None;
    commit(store, cfg).await?;
    Ok(repair.final_page)
}

async fn apply_journal_batch<C>(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
    batch: JournalBatch,
    mut commit: C,
) -> Result<VolumeProgress>
where
    C: AsyncFnMut(&mut StateStore, &AppConfig) -> Result<()>,
{
    ensure!(
        store.state.pending.is_none() && !store.volume(volume.id)?.needs_scan,
        "journal work requires a completed baseline and no pending index work"
    );
    let next = batch.cursor;
    let Some(changes) = resolve_events(&batch.events, cfg, &store.state.deferred)? else {
        require_full_scan(store, volume.id)?;
        return Ok(VolumeProgress {
            journal_read: false,
            work_remaining: true,
        });
    };
    // A previously healthy volume can acquire a backlog. Record that transition
    // before admission, so a pause or failure cannot retain an old healthy state.
    if !batch.caught_up && !store.volume(volume.id)?.catching_up {
        store.volume_mut(volume.id)?.catching_up = true;
        store.save()?;
    }
    let mut reconciled_after_read = false;
    if !changes.is_empty() {
        // The raw read's cursor belongs only to its final mutation batch;
        // earlier chunks can safely replay after an interrupted volume turn.
        let count = changes.chunks(MUTATION_BATCH_LIMIT).len();
        for (number, chunk) in changes.chunks(MUTATION_BATCH_LIMIT).enumerate() {
            let checkpoint = (number + 1 == count).then_some(next);
            let pending = pending_for_volume(volume, chunk.to_vec(), checkpoint, false, cfg);
            let reconciles = pending.worker.jobs.iter().any(|job| {
                job.operation == crate::dispatcher::job_dispatch::JobOperation::Reconcile
            });
            store.begin(pending)?;
            reconciled_after_read |= reconciles;
            commit(store, cfg).await?;
        }
    } else {
        store.volume_mut(volume.id)?.cursor = Some(next);
    }
    // A pre-worker read cannot certify moves that invalidate queued pathnames.
    // Require a fresh read after Reconcile admission before retiring coverage
    // debt. Our excluded checkpoint/log writes still count as bounded progress.
    let coverage_verified = batch.caught_up && !reconciled_after_read;
    if coverage_verified {
        if store.volume(volume.id)?.catching_up {
            let volume = store.volume_mut(volume.id)?;
            volume.catching_up = false;
            volume.unsettled_directories.clear();
            store.save()?;
        }
        show_volume(volume.id);
    }
    Ok(VolumeProgress {
        journal_read: true,
        work_remaining: !coverage_verified,
    })
}

async fn reconcile_volume(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
    scans: &mut MftScans,
) -> Result<()> {
    if let std::collections::btree_map::Entry::Vacant(entry) = scans.entry(volume.id) {
        hide_volume(volume.id);
        let state = store.volume_mut(volume.id)?;
        state.needs_scan = true;
        state.catching_up = true;
        store.save()?;
        update_status_ingestion_state(format!("reconciling volume {}", volume.id));
        let scan_volume = volume.clone();
        let mut reader_config = reader_config(cfg, &store.state.retired_indices)?;
        reader_config.max_records_per_tick = MUTATION_BATCH_LIMIT;
        let mut scan =
            tokio::task::spawn_blocking(move || begin_mft_scan(&scan_volume, &reader_config))
                .await??;
        // Opening the scan captures the journal head before reset or MFT pulls.
        // The same bounded native reader is resumed on later volume turns.
        let start = scan.journal_cursor();
        let batches: MftPages = Box::new(std::iter::from_fn(move || scan.next_batch().transpose()));
        entry.insert(MftProgress::new(start, batches));
    }
    let scan = scans
        .get_mut(&volume.id)
        .context("MFT reader disappeared before its volume turn")?;
    match apply_mft_scan(store, volume, cfg, scan, commit_pending).await {
        Ok(true) => {
            scans.remove(&volume.id);
            Ok(())
        }
        Ok(false) => Ok(()),
        Err(error) => {
            // Rejected capacity keeps the one unadmitted page for retry. Once
            // admitted, durable pending work owns that page and must replay
            // before any later pull. Reader errors restart with a fresh reset.
            if store.state.pending.is_none() && !error.is::<state::DeferredCapacity>() {
                scans.remove(&volume.id);
            }
            Err(error)
        }
    }
}

/// Apply one reset, one bounded raw page (including an empty page), or EOF, then
/// yield to other volumes. Return true only after a journal-validated native EOF.
/// There is no producer running ahead of admission, and a page whose admission
/// failed stays in memory until it is durably owned or the scan is restarted.
async fn apply_mft_scan<I, C>(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
    scan: &mut MftProgress<I>,
    mut commit: C,
) -> Result<bool>
where
    I: Iterator<Item = std::result::Result<Vec<FileMeta>, NtfsError>> + Send + 'static,
    C: AsyncFnMut(&mut StateStore, &AppConfig) -> Result<()>,
{
    ensure!(
        store.volume(volume.id)?.needs_scan,
        "MFT reconciliation must be marked incomplete before it starts"
    );
    ensure!(
        store.state.pending.is_none(),
        "durable pending work must finish before the next MFT turn"
    );
    // Reset through the worker lane as well, so old extraction jobs cannot
    // resurrect files after deletion. A crash restarts this complete scan.
    if !scan.reset_admitted {
        store.begin(pending_for_volume(volume, Vec::new(), None, true, cfg))?;
        scan.reset_admitted = true;
        commit(store, cfg).await?;
        return Ok(false);
    }
    if scan.page.is_none() {
        let mut batches = scan
            .batches
            .take()
            .context("MFT reader task was cancelled")?;
        let (returned, batch) = tokio::task::spawn_blocking(move || {
            let batch = batches.next();
            (batches, batch)
        })
        .await
        .context("MFT reader task failed")?;
        scan.batches = Some(returned);
        let Some(metas) = batch else {
            // Until this succeeds the previous durable cursor and incomplete
            // marker remain authoritative. Catch-up starts at the head captured
            // before the reset, including changes made between volume turns.
            store.complete_scan(volume.id, scan.start)?;
            return Ok(true);
        };
        let metas = match metas {
            Ok(metas) if metas.len() <= MUTATION_BATCH_LIMIT => metas,
            result => {
                // A failed page cannot later turn into a successful EOF if a
                // caller accidentally retains this progress object. Its prefix
                // stays hidden and a new scan must reset/replay the baseline.
                scan.batches = None;
                let metas = result?;
                anyhow::bail!(
                    "MFT reader exceeded the mutation batch limit: {}",
                    metas.len()
                );
            }
        };
        scan.page = Some(metas);
    }
    let metas = scan.page.as_ref().context("MFT page was not retained")?;
    // An empty page is bounded progress through excluded/missing records. It
    // still ends this turn, so excluded-heavy volumes cannot monopolize reads.
    if metas.is_empty() {
        scan.page = None;
        return Ok(false);
    }
    let changes = metas.iter().cloned().map(MetadataChange::Upsert).collect();
    store.begin(pending_for_volume(volume, changes, None, false, cfg))?;
    // From here the global intent owns these records, including on cancellation.
    // A later volume turn is forbidden until that intent has been completed.
    scan.page = None;
    commit(store, cfg).await?;
    Ok(false)
}

/// Unknown tombstones and our own output need no index commits. If an indexed
/// document moves into an excluded root, remove it from both search views.
fn resolve_events(
    events: &[FileEvent],
    cfg: &AppConfig,
    deferred: &[DeferredFile],
) -> Result<Option<Vec<MetadataChange>>> {
    if events.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let meta = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
    let meta_reader = meta_index::open_reader(&meta)?;
    let content = content_index::open_or_create(Path::new(&cfg.paths.content_index))?;
    let content_reader = content_index::open_reader(&content)?;
    // A deferred file has already been removed from both indices. Its delete
    // or exclusion event must still cancel the durable retry; searching only
    // the live indices would incorrectly discard that event as an unknown key.
    let mut known: std::collections::BTreeMap<_, _> = deferred
        .iter()
        .map(|retry| (retry.meta.key, true))
        .collect();
    let mut indexed = |key: DocKey| -> Result<bool> {
        if let Some(found) = known.get(&key) {
            return Ok(*found);
        }
        let query = |field| {
            tantivy::query::TermQuery::new(
                tantivy::Term::from_field_text(field, &key.to_string()),
                tantivy::schema::IndexRecordOption::Basic,
            )
        };
        let found = meta_reader
            .searcher()
            .search(&query(meta.fields.doc_key), &tantivy::collector::Count)?
            > 0
            || content_reader
                .searcher()
                .search(&query(content.fields.doc_key), &tantivy::collector::Count)?
                > 0;
        known.insert(key, found);
        Ok(found)
    };
    let mut normalized = Vec::new();
    let mut seen = BTreeSet::new();
    for event in events {
        match event {
            FileEvent::RescanRequired { .. } | FileEvent::DirectoryRenamed { .. } => {
                return Ok(None);
            }
            FileEvent::Excluded { doc, is_dir } => {
                if indexed(*doc)? || seen.contains(doc) {
                    if *is_dir {
                        return Ok(None);
                    }
                    normalized.push(FileEvent::Deleted(*doc));
                }
            }
            FileEvent::Created(meta)
            | FileEvent::Modified(meta)
            | FileEvent::AttributesChanged(meta) => {
                seen.insert(meta.key);
                normalized.push(event.clone());
            }
            FileEvent::Renamed { to, .. } => {
                seen.insert(to.key);
                normalized.push(event.clone());
            }
            FileEvent::Deleted(_) => normalized.push(event.clone()),
        }
    }
    let mut changes = Vec::new();
    for change in event_changes(&normalized)? {
        if !matches!(change, MetadataChange::Delete(_)) || indexed(change.key())? {
            changes.push(change);
        }
    }
    Ok(Some(changes))
}

fn reader_config(cfg: &AppConfig, retired_indices: &[String]) -> Result<ReaderConfig> {
    let mut paths = vec![
        Path::new(&cfg.paths.meta_index).to_path_buf(),
        Path::new(&cfg.paths.content_index).to_path_buf(),
        Path::new(&cfg.paths.state_dir).to_path_buf(),
        Path::new(&cfg.paths.jobs_dir).to_path_buf(),
    ];
    let log_parent = Path::new(&cfg.logging.file)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    paths.push(log_parent.to_path_buf());
    let semantic = Path::new(&cfg.semantic.index_dir);
    if semantic.exists() {
        paths.push(semantic.to_path_buf());
    }
    paths.extend(
        retired_indices
            .iter()
            .map(std::path::PathBuf::from)
            .filter(|path| path.exists()),
    );
    // A crash can occur after preserving an old index but before its new state
    // file commits. Discover these siblings as well as persisted archive paths.
    for index_path in [&cfg.paths.meta_index, &cfg.paths.content_index] {
        let index_path = Path::new(index_path);
        let parent = index_path.parent().context("index path has no parent")?;
        let prefix_path = index_path.with_extension("before-ingestion-v2-");
        let prefix = prefix_path
            .file_name()
            .context("index path has no filename")?
            .to_string_lossy();
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(prefix.as_ref())
                && entry.file_type()?.is_dir()
            {
                paths.push(entry.path());
            }
        }
    }
    paths.sort();
    paths.dedup();
    let mut exclude_paths = Vec::new();
    for path in paths {
        // All mandatory paths are created before ingestion starts. Refuse to
        // proceed if they cannot be anchored to a volume; otherwise our own
        // output could produce an unbounded indexing feedback loop.
        let canonical = canonical_path(&path)
            .with_context(|| format!("resolve indexing exclusion {}", path.display()))?;
        ensure!(
            !canonical
                .split_once("}\\")
                .is_some_and(|(_, suffix)| suffix.is_empty()),
            "service output directory cannot be a volume root: {}",
            path.display()
        );
        exclude_paths.push(canonical);
    }
    Ok(ReaderConfig {
        chunk_size: 256 * 1024,
        max_records_per_tick: BATCH_LIMIT,
        exclude_paths,
    })
}

async fn commit_pending(store: &mut StateStore, cfg: &AppConfig) -> Result<()> {
    // A previous save may have failed after changing the in-memory intent.
    // Re-establish durability on every attempt before any index mutation.
    store.save()?;
    let batch = store
        .state
        .pending
        .clone()
        .context("no pending ingestion batch")?;
    hide_pending(&batch);
    update_status_ingestion_state(format!(
        "volume {} waiting for admitted worker commit",
        batch.volume
    ));
    // Awaiting admission supplies backpressure. It does not advance any cursor.
    submit_index_batch(batch.worker.clone(), Some(store.mutation_lease())).await?;
    let apply_cfg = cfg.clone();
    let apply_batch = batch.clone();
    run_index_mutation(store.mutation_lease(), move || {
        apply_metadata(&apply_cfg, &apply_batch)
    })
    .await?;
    store.finish()?;
    show_pending(&batch);
    update_status_last_commit(Some(unix_timestamp_secs()));
    Ok(())
}

async fn run_index_mutation<T, F>(lease: std::sync::Arc<std::fs::File>, mutation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        // Dropping a JoinHandle does not stop a blocking task. Keep the sole
        // ingestion lease here so a new service session cannot overtake it.
        let _lease = lease;
        mutation()
    })
    .await?
}

fn apply_metadata(cfg: &AppConfig, batch: &PendingBatch) -> Result<()> {
    let index = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
    let content = content_index::open_or_create(Path::new(&cfg.paths.content_index))?;
    let outcome = content_index::batch_outcome(&content.index)?
        .context("worker content commit has no outcome receipt")?;
    ensure!(
        outcome.receipt.complete && outcome.receipt.batch_id == batch.worker.id,
        "worker did not commit the expected ingestion batch {}",
        batch.worker.id
    );
    validate_deferred_outcome(batch, &outcome.deferred)?;
    let content_reader = content_index::open_reader(&content)?;
    let mut writer = meta_index::create_writer(
        &index,
        &meta_index::WriterConfig {
            heap_size_bytes: 32 * 1024 * 1024,
            num_threads: 1,
        },
    )?;
    for volume in &batch.worker.reset_volumes {
        meta_index::delete_volume(&mut writer, &index.fields, *volume);
    }
    for change in &batch.metadata {
        match change {
            MetadataChange::Upsert(meta) => {
                if outcome.deferred.binary_search(&meta.key).is_ok() {
                    ensure!(
                        content_index::read_file_meta(&content, &content_reader, meta.key)?
                            .is_none(),
                        "deferred content outcome retained a stale document: {}",
                        meta.key
                    );
                    meta_index::delete_doc(&mut writer, &index.fields, meta.key);
                    continue;
                }
                let operation = batch
                    .worker
                    .jobs
                    .iter()
                    .find(|job| {
                        job.volume_id == meta.key.volume() && job.file_id == meta.key.file_id()
                    })
                    .context("metadata change has no matching durable worker job")?
                    .operation;
                // Eligibility belongs to the persisted intent. Configuration
                // may have changed while admission was blocked or after a crash.
                if operation == crate::dispatcher::job_dispatch::JobOperation::Delete {
                    meta_index::add_file_meta_batch(&mut writer, &index.fields, [meta.clone()])?;
                } else if let Some(mut committed) =
                    content_index::read_file_meta(&content, &content_reader, meta.key)?
                {
                    // Content extraction may have waited behind admission. Use
                    // the metadata from that exact worker snapshot, not the
                    // earlier journal/MFT observation.
                    committed.parent = meta.parent;
                    meta_index::add_file_meta_batch(&mut writer, &index.fields, [committed])?;
                } else {
                    // Reconcile jobs that became obsolete remove both views.
                    meta_index::delete_doc(&mut writer, &index.fields, meta.key);
                }
            }
            MetadataChange::Delete(key) => meta_index::delete_doc(&mut writer, &index.fields, *key),
        }
    }
    content_index::commit_batch(&mut writer, batch.worker.id)?;
    Ok(())
}

fn publish_status(store: &StateStore, statistics: &mut IndexStatistics) -> Result<()> {
    statistics.refresh()?;
    let mut volumes = Vec::new();
    for volume in &store.state.volumes {
        let (count, bytes) = statistics.volume(volume.id);
        let pending = store
            .state
            .pending
            .as_ref()
            .filter(|batch| batch.volume == volume.id);
        let deferred: Vec<_> = store
            .state
            .deferred
            .iter()
            .filter(|retry| retry.meta.key.volume() == volume.id)
            .filter(|retry| {
                pending.is_none_or(|batch| {
                    !batch
                        .metadata
                        .iter()
                        .any(|change| change.key() == retry.meta.key)
                })
            })
            .collect();
        volumes.push(VolumeStatus {
            volume: volume.id,
            indexed_files: count,
            indexed_bytes: bytes,
            pending_files: pending.map_or(0, |batch| batch.worker.jobs.len() as u64)
                + deferred.len() as u64,
            pending_bytes: deferred
                .iter()
                .map(|retry| retry.meta.size)
                .chain(
                    pending
                        .into_iter()
                        .flat_map(|batch| batch.worker.jobs.iter().map(|job| job.file_size)),
                )
                .fold(0, u64::saturating_add),
            last_usn: volume.cursor.map(|cursor| cursor.last_usn),
            journal_id: volume.cursor.map(|cursor| cursor.journal_id),
        });
    }
    update_status_volumes(volumes);
    Ok(())
}

fn unix_timestamp_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::state::make_pending;
    use super::*;
    use crate::dispatcher::job_dispatch::JobOperation;
    use crate::search_handler::{SearchHandler, UnifiedSearchHandler};
    use core_types::{FileFlags, FileMeta};
    use ipc::{QueryExpr, SearchMode, SearchRequest, SearchResponse, TermExpr, TermModifier};
    use ntfs_watcher::JournalCursor;
    use uuid::Uuid;

    fn config(root: &Path) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.paths.meta_index = root.join("meta").to_string_lossy().into_owned();
        cfg.paths.content_index = root.join("content").to_string_lossy().into_owned();
        cfg.paths.state_dir = root.join("state").to_string_lossy().into_owned();
        cfg.paths.jobs_dir = root.join("jobs").to_string_lossy().into_owned();
        cfg
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
            size as i64,
            FileFlags::empty(),
        )
    }

    fn search(handler: &UnifiedSearchHandler, term: &str, mode: SearchMode) -> SearchResponse {
        handler.search(SearchRequest {
            id: Uuid::new_v4(),
            query: QueryExpr::Term(TermExpr {
                field: None,
                value: term.into(),
                modifier: TermModifier::Term,
            }),
            limit: 20,
            offset: 0,
            mode,
            timeout: None,
        })
    }

    /// Model the worker's committed output with real Tantivy replacements; this
    /// deliberately separates content commit from metadata/checkpoint commit so
    /// the crash window can be tested without a timing-dependent child process.
    fn worker_commit(
        cfg: &AppConfig,
        batch: &PendingBatch,
        snapshot: Option<(&FileMeta, &str)>,
    ) -> Result<()> {
        worker_outcome(cfg, batch, &snapshot.into_iter().collect::<Vec<_>>(), &[])
    }

    fn worker_outcome(
        cfg: &AppConfig,
        batch: &PendingBatch,
        snapshots: &[(&FileMeta, &str)],
        deferred: &[DocKey],
    ) -> Result<()> {
        let index = content_index::open_or_create(Path::new(&cfg.paths.content_index))?;
        let mut writer = content_index::create_writer(
            &index,
            &content_index::WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        for volume in &batch.worker.reset_volumes {
            content_index::delete_volume(&mut writer, &index.fields, *volume);
        }
        for job in &batch.worker.jobs {
            let key = DocKey::from_parts(job.volume_id, job.file_id);
            content_index::delete_doc(&mut writer, &index.fields, key);
            if job.operation != JobOperation::Delete
                && let Some((meta, text)) =
                    snapshots.iter().copied().find(|(meta, _)| meta.key == key)
            {
                content_index::add_content_doc(
                    &mut writer,
                    &index.fields,
                    &content_index::ContentDoc {
                        key,
                        volume: meta.volume,
                        name: Some(meta.name.clone()),
                        path: meta.path.clone(),
                        ext: meta.ext.clone(),
                        size: meta.size,
                        created: meta.created,
                        modified: meta.modified,
                        flags: u64::from(meta.flags.bits()),
                        content_lang: None,
                        content: text.into(),
                    },
                )?;
            }
        }
        content_index::commit_batch_with_deferred(&mut writer, batch.worker.id, deferred)?;
        Ok(())
    }

    fn finish(store: &mut StateStore, cfg: &AppConfig, batch: &PendingBatch) -> Result<()> {
        apply_metadata(cfg, batch)?;
        store.finish()?;
        show_pending(batch);
        Ok(())
    }

    fn directory_meta(key: DocKey, parent: DocKey, path: &str) -> FileMeta {
        let mut meta = meta(key, path.rsplit('\\').next().unwrap_or("root"), 0);
        meta.parent = Some(parent);
        meta.path = Some(path.into());
        meta.flags = FileFlags::IS_DIR;
        meta
    }

    async fn commit_directory_fixture(store: &mut StateStore, cfg: &AppConfig) -> Result<()> {
        let batch = store
            .state
            .pending
            .clone()
            .context("missing fixture batch")?;
        ensure!(
            batch.worker.reset_volumes.is_empty(),
            "directory repair reset unrelated documents"
        );
        ensure!(
            batch.metadata.len() <= MUTATION_BATCH_LIMIT,
            "unbounded directory mutation"
        );
        let snapshots: Vec<_> = batch
            .metadata
            .iter()
            .filter_map(|change| {
                if let MetadataChange::Upsert(meta) = change {
                    Some((meta, "directorypayload"))
                } else {
                    None
                }
            })
            .collect();
        hide_pending(&batch);
        worker_outcome(cfg, &batch, &snapshots, &[])?;
        finish(store, cfg, &batch)
    }

    async fn seed_directory_fixture(
        store: &mut StateStore,
        volume: &VolumeInfo,
        cfg: &AppConfig,
        metas: &[FileMeta],
    ) -> Result<()> {
        for chunk in metas.chunks(MUTATION_BATCH_LIMIT) {
            store.begin(pending_for_volume(
                volume,
                chunk.iter().cloned().map(MetadataChange::Upsert).collect(),
                None,
                false,
                cfg,
            ))?;
            commit_directory_fixture(store, cfg).await?;
        }
        let checkpoint = store.volume_mut(volume.id)?;
        checkpoint.needs_scan = false;
        checkpoint.catching_up = false;
        checkpoint.content_policy = Some(ContentPolicy::for_volume(volume, cfg));
        store.save()?;
        show_volume(volume.id);
        Ok(())
    }

    fn search_directory_path(
        handler: &UnifiedSearchHandler,
        path: &str,
        mode: SearchMode,
    ) -> SearchResponse {
        handler.search(SearchRequest {
            id: Uuid::new_v4(),
            query: QueryExpr::Term(TermExpr {
                field: Some(ipc::FieldKind::Path),
                value: path.into(),
                modifier: TermModifier::Term,
            }),
            limit: 1000,
            offset: 0,
            mode,
            timeout: None,
        })
    }

    #[tokio::test]
    async fn directory_repair_replays_bounded_pages_and_preserves_unrelated_search() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (mut cfg, mut store, volume, before) = baseline_store(root.path(), 60030)?;
        cfg.content_index_volumes = vec!["X:\\".into()];
        let parent = DocKey::from_parts(volume.id, 5);
        let folder = DocKey::from_parts(volume.id, 10);
        let directory = directory_meta(folder, parent, r"C:\oldroot");
        let mut renamed = directory.clone();
        renamed.name = "newroot".into();
        renamed.path = Some(r"C:\newroot".into());
        let mut originals = vec![directory_meta(parent, parent, r"C:\"), directory];
        let mut current = BTreeMap::from([
            (parent, Some(originals[0].clone())),
            (folder, Some(renamed.clone())),
        ]);
        for file in 100..(100 + MUTATION_BATCH_LIMIT as u64 + 5) {
            let mut original = meta(
                DocKey::from_parts(volume.id, file),
                &format!("proof{file}.txt"),
                20,
            );
            original.path = Some(format!(r"C:\oldroot\{}", original.name));
            let mut moved = original.clone();
            moved.path = Some(format!(r"C:\newroot\{}", moved.name));
            current.insert(original.key, (file != 103).then_some(moved));
            originals.push(original);
        }
        let mut unrelated = meta(DocKey::from_parts(volume.id, 999), "unrelated.txt", 10);
        unrelated.path = Some(r"C:\oldrootish\unrelated.txt".into());
        originals.push(unrelated.clone());
        seed_directory_fixture(&mut store, &volume, &cfg, &originals).await?;
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        let retry_key = DocKey::from_parts(volume.id, 888);
        let mut retry = meta(retry_key, "deferred.txt", 10);
        retry.path = Some(r"C:\oldroot\deferred.txt".into());
        store.state.deferred.push(DeferredFile {
            meta: retry.clone(),
            attempts: 2,
            retry_at: 999,
        });
        retry.path = Some(r"C:\newroot\deferred.txt".into());
        current.insert(retry_key, Some(retry));
        store.save()?;
        let through = JournalCursor {
            journal_id: before.journal_id,
            last_usn: before.last_usn + 100,
        };
        let batch = JournalBatch {
            events: vec![
                FileEvent::DirectoryRenamed {
                    doc: folder,
                    parent,
                    name: "oldroot".into(),
                    current: Some(renamed.clone()),
                },
                FileEvent::DirectoryRenamed {
                    doc: folder,
                    parent,
                    name: "newroot".into(),
                    current: Some(renamed),
                },
            ],
            cursor: through,
            caught_up: true,
        };
        let config = ReaderConfig::default();
        let mut repair = prepare_directory_repair(&mut store, &volume, &cfg, &batch, &config, &[])?
            .context("ordinary rename requested reset")?;
        assert!(
            repair.discovery_done,
            "known subtree should not enumerate the MFT"
        );
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Hybrid,
        ] {
            assert_eq!(search_directory_path(&handler, "oldroot", mode).total, 0);
            assert_eq!(
                search(&handler, "unrelated", mode).hits[0].key,
                unrelated.key
            );
        }
        // Crash after the content half of a real descendant commit. The old
        // raw cursor and all subtree masks must survive the partial transaction.
        let mut failed = false;
        for _ in 0..100 {
            let result = apply_directory_turn(
                &mut store,
                &volume,
                &cfg,
                &mut repair,
                async |keys| {
                    Ok(keys
                        .into_iter()
                        .map(|key| current.get(&key).cloned().flatten())
                        .collect())
                },
                async |store, cfg| {
                    let pending = store
                        .state
                        .pending
                        .clone()
                        .context("pending page missing")?;
                    assert!(
                        !pending
                            .metadata
                            .iter()
                            .any(|change| change.key() == unrelated.key)
                    );
                    let snapshots: Vec<_> = pending
                        .metadata
                        .iter()
                        .filter_map(|change| {
                            if let MetadataChange::Upsert(meta) = change {
                                Some((meta, "directorypayload"))
                            } else {
                                None
                            }
                        })
                        .collect();
                    hide_pending(&pending);
                    worker_outcome(cfg, &pending, &snapshots, &[])?;
                    anyhow::bail!("simulated loss before metadata acknowledgement")
                },
            )
            .await;
            if result.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert_eq!(store.volume(volume.id)?.cursor, Some(before));
        assert!(store.state.pending.is_some());
        let anchors = store.volume(volume.id)?.directory_repair.clone().unwrap();
        drop(repair);
        drop(store);
        let mut store = StateStore::open(&cfg)?;
        assert_eq!(store.volume(volume.id)?.directory_repair, Some(anchors));
        replay_pending(
            &mut store,
            &cfg,
            &BTreeSet::from([volume.id]),
            commit_directory_fixture,
        )
        .await?;
        let mut repair =
            prepare_directory_repair(&mut store, &volume, &cfg, &batch, &config, &[])?.unwrap();
        assert!(!repair.discovery_done);
        let snapshot: Vec<_> = current.values().flatten().cloned().collect();
        let pages: Vec<_> = snapshot
            .chunks(MUTATION_BATCH_LIMIT)
            .map(|page| Ok(page.to_vec()))
            .collect();
        repair.discovery = Some(Box::new(pages.into_iter()));
        let mut done = false;
        for _ in 0..200 {
            done = apply_directory_turn(
                &mut store,
                &volume,
                &cfg,
                &mut repair,
                async |keys| {
                    Ok(keys
                        .into_iter()
                        .map(|key| current.get(&key).cloned().flatten())
                        .collect())
                },
                async |store, cfg| {
                    assert!(
                        !store
                            .state
                            .pending
                            .as_ref()
                            .unwrap()
                            .metadata
                            .iter()
                            .any(|change| change.key() == unrelated.key)
                    );
                    commit_directory_fixture(store, cfg).await
                },
            )
            .await?;
            if done {
                break;
            }
            assert_eq!(store.volume(volume.id)?.cursor, Some(before));
            assert_eq!(
                search(&handler, "unrelated", SearchMode::Content).hits[0].key,
                unrelated.key
            );
        }
        assert!(done);
        assert_eq!(store.volume(volume.id)?.cursor, Some(through));
        assert!(!store.volume(volume.id)?.needs_scan);
        assert!(store.volume(volume.id)?.directory_repair.is_none());
        assert!(store.state.deferred.is_empty());
        show_directories(volume.id);
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Hybrid,
        ] {
            assert_eq!(search_directory_path(&handler, "oldroot", mode).total, 0);
            let hits = search_directory_path(&handler, "newroot", mode).hits;
            let keys: BTreeSet<_> = hits.iter().map(|hit| hit.key).collect();
            assert_eq!(keys.len(), hits.len());
            assert!(keys.contains(&retry_key));
            assert!(!keys.contains(&DocKey::from_parts(volume.id, 103)));
            assert!(keys.contains(&DocKey::from_parts(volume.id, 100)));
            assert_eq!(
                search(&handler, "unrelated", mode).hits[0].key,
                unrelated.key
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn directory_move_after_admission_recovers_children_tombstoned_at_obsolete_paths()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let (mut cfg, mut store, volume, before) = baseline_store(root.path(), 60034)?;
        cfg.content_index_volumes = vec!["X:\\".into()];
        let parent = DocKey::from_parts(volume.id, 5);
        let folder = DocKey::from_parts(volume.id, 10);
        let child = DocKey::from_parts(volume.id, 20);
        let first = directory_meta(folder, parent, r"C:\first");
        let middle = directory_meta(folder, parent, r"C:\middle");
        let final_root = directory_meta(folder, parent, r"C:\final");
        let mut first_child = meta(child, "racechild.txt", 10);
        first_child.path = Some(r"C:\first\racechild.txt".into());
        let mut middle_child = first_child.clone();
        middle_child.path = Some(r"C:\middle\racechild.txt".into());
        let mut final_child = first_child.clone();
        final_child.path = Some(r"C:\final\racechild.txt".into());
        let sentinel = meta(DocKey::from_parts(volume.id, 30), "sentinel.txt", 10);
        seed_directory_fixture(
            &mut store,
            &volume,
            &cfg,
            &[
                directory_meta(parent, parent, r"C:\"),
                first,
                first_child,
                sentinel.clone(),
            ],
        )
        .await?;
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        let batch = JournalBatch {
            events: vec![FileEvent::DirectoryRenamed {
                doc: folder,
                parent,
                name: "first".into(),
                current: Some(middle.clone()),
            }],
            cursor: JournalCursor {
                last_usn: before.last_usn + 100,
                ..before
            },
            caught_up: true,
        };
        let reader = ReaderConfig::default();
        let mut repair =
            prepare_directory_repair(&mut store, &volume, &cfg, &batch, &reader, &[])?.unwrap();
        assert!(repair.discovery_done, "known first move need not scan MFT");
        let moved_again = std::sync::atomic::AtomicBool::new(false);
        let mut done = false;
        for _ in 0..50 {
            done = apply_directory_turn(
                &mut store,
                &volume,
                &cfg,
                &mut repair,
                async |keys| {
                    Ok(keys
                        .into_iter()
                        .map(|key| {
                            let moved = moved_again.load(Ordering::SeqCst);
                            Some(if key == folder {
                                if moved {
                                    final_root.clone()
                                } else {
                                    middle.clone()
                                }
                            } else {
                                assert_eq!(key, child);
                                if moved {
                                    final_child.clone()
                                } else {
                                    middle_child.clone()
                                }
                            })
                        })
                        .collect())
                },
                async |store, cfg| {
                    let pending = store.state.pending.clone().unwrap();
                    assert!(pending.worker.reset_volumes.is_empty());
                    assert!(
                        !pending
                            .metadata
                            .iter()
                            .any(|change| change.key() == sentinel.key)
                    );
                    if pending.metadata.iter().any(|change| change.key() == child) {
                        let job = pending
                            .worker
                            .jobs
                            .iter()
                            .find(|job| {
                                job.volume_id == child.volume() && job.file_id == child.file_id()
                            })
                            .unwrap();
                        assert_eq!(job.operation, JobOperation::Reconcile);
                        assert_eq!(job.path, Path::new(r"C:\middle\racechild.txt"));
                        assert!(!moved_again.swap(true, Ordering::SeqCst));
                        // The second move occurs after durable admission and
                        // before the worker opens the obsolete middle pathname.
                        // Its successful tombstone has no deferred-file debt.
                        hide_pending(&pending);
                        worker_outcome(cfg, &pending, &[], &[])?;
                        finish(store, cfg, &pending)
                    } else {
                        commit_directory_fixture(store, cfg).await
                    }
                },
            )
            .await?;
            assert_eq!(
                search(&handler, "sentinel", SearchMode::Hybrid).hits[0].key,
                sentinel.key
            );
            if done {
                break;
            }
        }
        assert!(done && moved_again.load(Ordering::SeqCst));
        assert!(store.state.deferred.is_empty());
        assert_eq!(store.volume(volume.id)?.cursor, Some(batch.cursor));
        let index = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
        assert!(meta_index::file_meta(&index, child)?.is_none());
        assert_eq!(
            meta_index::file_meta(&index, folder)?.unwrap().path,
            final_root.path
        );
        assert!(
            search(&handler, "racechild", SearchMode::Content)
                .hits
                .is_empty()
        );
        show_directories(volume.id);
        drop(repair);
        drop(store);
        let mut store = StateStore::open(&cfg)?;
        assert!(
            store
                .volume(volume.id)?
                .unsettled_directories
                .contains(&folder)
        );

        // Only the directory has another journal event. The unchanged child is
        // absent from the index, so a second prefix-only scan cannot recover it.
        let later = JournalBatch {
            events: vec![FileEvent::DirectoryRenamed {
                doc: folder,
                parent,
                name: "middle".into(),
                current: Some(final_root.clone()),
            }],
            cursor: JournalCursor {
                last_usn: batch.cursor.last_usn + 100,
                ..batch.cursor
            },
            caught_up: true,
        };
        let mut repair =
            prepare_directory_repair(&mut store, &volume, &cfg, &later, &reader, &[])?.unwrap();
        assert!(!repair.discovery_done);
        repair.discovery = Some(Box::new(vec![Ok(vec![final_child.clone()])].into_iter()));
        done = false;
        for _ in 0..50 {
            done = apply_directory_turn(
                &mut store,
                &volume,
                &cfg,
                &mut repair,
                async |keys| {
                    Ok(keys
                        .into_iter()
                        .map(|key| {
                            assert_eq!(key, folder);
                            Some(final_root.clone())
                        })
                        .collect())
                },
                commit_directory_fixture,
            )
            .await?;
            assert_eq!(
                search(&handler, "sentinel", SearchMode::Hybrid).hits[0].key,
                sentinel.key
            );
            if done {
                break;
            }
        }
        assert!(done);
        show_directories(volume.id);
        apply_journal_batch(
            &mut store,
            &volume,
            &cfg,
            JournalBatch {
                events: Vec::new(),
                cursor: later.cursor,
                caught_up: true,
            },
            commit_directory_fixture,
        )
        .await?;
        assert!(store.volume(volume.id)?.unsettled_directories.is_empty());
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Hybrid,
        ] {
            let found = search(&handler, "racechild", mode);
            assert_eq!(found.total, 1);
            assert_eq!(found.hits[0].key, child);
            assert_eq!(
                found.hits[0].path.as_deref(),
                Some(r"C:\final\racechild.txt")
            );
            assert_eq!(search_directory_path(&handler, "first", mode).total, 0);
            assert_eq!(search_directory_path(&handler, "middle", mode).total, 0);
        }
        Ok(())
    }

    #[tokio::test]
    async fn first_directory_rename_recovers_child_lost_during_baseline_or_ordinary_edit()
    -> Result<()> {
        async fn obsolete_path(store: &mut StateStore, cfg: &AppConfig) -> Result<()> {
            let pending = store.state.pending.clone().unwrap();
            assert!(pending.worker.reset_volumes.is_empty());
            assert_eq!(pending.worker.jobs.len(), 1);
            assert_eq!(pending.worker.jobs[0].operation, JobOperation::Reconcile);
            assert_eq!(
                pending.worker.jobs[0].path,
                Path::new(r"C:\source\lostchild.txt")
            );
            assert!(store.volume(pending.volume)?.catching_up);
            // The parent moves after this old pathname was durably queued.
            // A real worker returns this successful obsolete/tombstoned outcome.
            hide_pending(&pending);
            worker_outcome(cfg, &pending, &[], &[])?;
            finish(store, cfg, &pending)
        }

        for baseline in [true, false] {
            let root = tempfile::tempdir()?;
            let (mut cfg, mut store, volume, before) =
                baseline_store(root.path(), if baseline { 60035 } else { 60036 })?;
            cfg.content_index_volumes = vec!["X:\\".into()];
            let parent = DocKey::from_parts(volume.id, 5);
            let folder = DocKey::from_parts(volume.id, 10);
            let child = DocKey::from_parts(volume.id, 20);
            let source = directory_meta(folder, parent, r"C:\source");
            let target = directory_meta(folder, parent, r"C:\destination");
            let mut original_child = meta(child, "lostchild.txt", 10);
            original_child.path = Some(r"C:\source\lostchild.txt".into());
            let mut current_child = original_child.clone();
            current_child.path = Some(r"C:\destination\lostchild.txt".into());
            let sentinel = meta(DocKey::from_parts(volume.id, 30), "survivor.txt", 10);
            let mut seeded = vec![
                directory_meta(parent, parent, r"C:\"),
                source,
                sentinel.clone(),
            ];
            if !baseline {
                seeded.push(original_child.clone());
            }
            seed_directory_fixture(&mut store, &volume, &cfg, &seeded).await?;
            if baseline {
                // Prior baseline pages already committed the root and sentinel.
                // This later MFT page loses the child before validated EOF.
                store.volume_mut(volume.id)?.needs_scan = true;
                store.save()?;
                let mut scan = MftProgress::new(before, vec![Ok(vec![original_child])].into_iter());
                scan.reset_admitted = true;
                assert!(
                    !apply_mft_scan(&mut store, &volume, &cfg, &mut scan, obsolete_path).await?
                );
                assert!(apply_mft_scan(&mut store, &volume, &cfg, &mut scan, obsolete_path).await?);
            } else {
                let progress = apply_journal_batch(
                    &mut store,
                    &volume,
                    &cfg,
                    JournalBatch {
                        events: vec![FileEvent::Modified(original_child)],
                        cursor: JournalCursor {
                            last_usn: before.last_usn + 50,
                            ..before
                        },
                        caught_up: true,
                    },
                    obsolete_path,
                )
                .await?;
                assert!(
                    progress.work_remaining,
                    "pre-worker head incorrectly certified coverage"
                );
            }
            assert!(!store.volume(volume.id)?.needs_scan);
            assert!(store.volume(volume.id)?.catching_up);
            assert!(store.volume(volume.id)?.unsettled_directories.is_empty());
            assert!(store.state.deferred.is_empty());
            let index = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
            assert!(meta_index::file_meta(&index, child)?.is_none());
            assert!(meta_index::file_meta(&index, folder)?.is_some());
            drop(store);
            let mut store = StateStore::open(&cfg)?;
            assert!(store.volume(volume.id)?.catching_up);
            let cursor = store.volume(volume.id)?.cursor.unwrap();
            let batch = JournalBatch {
                events: vec![FileEvent::DirectoryRenamed {
                    doc: folder,
                    parent,
                    name: "source".into(),
                    current: Some(target.clone()),
                }],
                cursor: JournalCursor {
                    last_usn: cursor.last_usn + 100,
                    ..cursor
                },
                caught_up: true,
            };
            let mut repair = prepare_directory_repair(
                &mut store,
                &volume,
                &cfg,
                &batch,
                &ReaderConfig::default(),
                &[],
            )?
            .unwrap();
            assert!(
                !repair.discovery_done,
                "first rename forgot obsolete child coverage"
            );
            repair.discovery = Some(Box::new(vec![Ok(vec![current_child.clone()])].into_iter()));
            let handler = UnifiedSearchHandler::try_new(
                Path::new(&cfg.paths.meta_index),
                Path::new(&cfg.paths.content_index),
            )?;
            let mut done = false;
            for _ in 0..50 {
                done = apply_directory_turn(
                    &mut store,
                    &volume,
                    &cfg,
                    &mut repair,
                    async |keys| {
                        Ok(keys
                            .into_iter()
                            .map(|key| {
                                assert_eq!(key, folder);
                                Some(target.clone())
                            })
                            .collect())
                    },
                    commit_directory_fixture,
                )
                .await?;
                assert_eq!(
                    search(&handler, "survivor", SearchMode::Hybrid).hits[0].key,
                    sentinel.key
                );
                if done {
                    break;
                }
            }
            assert!(done);
            assert!(store.volume(volume.id)?.catching_up);
            apply_journal_batch(
                &mut store,
                &volume,
                &cfg,
                JournalBatch {
                    events: Vec::new(),
                    cursor: batch.cursor,
                    caught_up: true,
                },
                commit_directory_fixture,
            )
            .await?;
            assert!(!store.volume(volume.id)?.catching_up);
            for mode in [
                SearchMode::NameOnly,
                SearchMode::Content,
                SearchMode::Hybrid,
            ] {
                let found = search(&handler, "lostchild", mode);
                assert_eq!(found.total, 1);
                assert_eq!(found.hits[0].key, child);
                assert_eq!(
                    found.hits[0].path.as_deref(),
                    Some(r"C:\destination\lostchild.txt")
                );
                assert_eq!(search_directory_path(&handler, "source", mode).total, 0);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn directory_anchors_cover_intermediate_names_and_excluded_descendants() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (mut cfg, mut store, volume, before) = baseline_store(root.path(), 60031)?;
        cfg.content_index_volumes = vec!["X:\\".into()];
        let parent = DocKey::from_parts(volume.id, 5);
        let folder = DocKey::from_parts(volume.id, 10);
        let old = directory_meta(folder, parent, r"C:\first");
        let final_meta = directory_meta(folder, parent, r"C:\excluded\final");
        let mut child = meta(DocKey::from_parts(volume.id, 50), "child.txt", 10);
        child.path = Some(r"C:\middle\child.txt".into());
        seed_directory_fixture(
            &mut store,
            &volume,
            &cfg,
            &[directory_meta(parent, parent, r"C:\"), old, child.clone()],
        )
        .await?;
        let excluded_parent = DocKey::from_parts(volume.id, 11);
        let reader = ReaderConfig {
            exclude_paths: vec![r"C:\excluded".into()],
            ..ReaderConfig::default()
        };
        let batch = JournalBatch {
            events: [
                (parent, "first"),
                (parent, "middle"),
                (parent, "middle"),
                (excluded_parent, "final"),
            ]
            .into_iter()
            .map(|(parent, name)| FileEvent::DirectoryRenamed {
                doc: folder,
                parent,
                name: name.into(),
                current: Some(final_meta.clone()),
            })
            .collect(),
            cursor: JournalCursor {
                last_usn: before.last_usn + 100,
                ..before
            },
            caught_up: true,
        };
        let mut repair = prepare_directory_repair(
            &mut store,
            &volume,
            &cfg,
            &batch,
            &reader,
            &[Some(directory_meta(
                excluded_parent,
                parent,
                r"C:\excluded",
            ))],
        )?
        .context("anchored move into exclusion requested reset")?;
        assert!(
            repair
                .checkpoint
                .paths
                .contains(&normalize_index_path(r"C:\middle"))
        );
        assert!(repair.discovery_done);
        let mut done = false;
        for _ in 0..50 {
            done = apply_directory_turn(
                &mut store,
                &volume,
                &cfg,
                &mut repair,
                async |keys| Ok(vec![None; keys.len()]),
                commit_directory_fixture,
            )
            .await?;
            if done {
                break;
            }
        }
        assert!(done);
        show_directories(volume.id);
        let index = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
        assert!(meta_index::file_meta(&index, child.key)?.is_none());
        assert!(meta_index::file_meta(&index, folder)?.is_none());
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        assert!(
            search(&handler, "child", SearchMode::Content)
                .hits
                .is_empty()
        );
        assert!(!store.volume(volume.id)?.needs_scan);
        Ok(())
    }

    #[tokio::test]
    async fn discovery_obligations_survive_root_commit_ancestor_moves_and_child_moves_out()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let (mut cfg, mut store, volume, before) = baseline_store(root.path(), 60032)?;
        cfg.content_index_volumes = vec!["X:\\".into()];
        let root_key = DocKey::from_parts(volume.id, 5);
        let ancestor = DocKey::from_parts(volume.id, 10);
        let entered = DocKey::from_parts(volume.id, 20);
        let child = DocKey::from_parts(volume.id, 30);
        let first_leaf = DocKey::from_parts(volume.id, 40);
        let escaped_leaf = DocKey::from_parts(volume.id, 50);
        let excluded_parent = DocKey::from_parts(volume.id, 60);
        let root_meta = directory_meta(root_key, root_key, r"C:\");
        let ancestor_meta = directory_meta(ancestor, root_key, r"C:\parent");
        // The baseline saw this root after the move, but skipped its unchanged
        // children while they were still excluded. An indexed root alone does
        // not certify that those children were discovered.
        let entered_meta = directory_meta(entered, ancestor, r"C:\parent\entry");
        seed_directory_fixture(
            &mut store,
            &volume,
            &cfg,
            &[root_meta, ancestor_meta, entered_meta],
        )
        .await?;
        let reader = ReaderConfig {
            exclude_paths: vec![r"C:\excluded".into()],
            ..ReaderConfig::default()
        };
        let resolved_parent = Some(directory_meta(excluded_parent, root_key, r"C:\excluded"));
        let mut cursor = before;
        let mut current_files = BTreeMap::new();
        for (key, parent, old_name, path) in [
            (entered, excluded_parent, "entry", r"C:\parent\entry"),
            (ancestor, root_key, "parent", r"C:\movedparent"),
            (child, entered, "child", r"C:\escapedchild"),
        ] {
            let current_parent = if key == entered { ancestor } else { parent };
            let current = directory_meta(key, current_parent, path);
            current_files.insert(key, current.clone());
            let page = if key == entered {
                let nested = directory_meta(child, entered, r"C:\parent\entry\child");
                current_files.insert(child, nested.clone());
                vec![nested]
            } else if key == ancestor {
                current_files.insert(
                    entered,
                    directory_meta(entered, ancestor, r"C:\movedparent\entry"),
                );
                current_files.insert(
                    child,
                    directory_meta(child, entered, r"C:\movedparent\entry\child"),
                );
                let mut leaf = meta(first_leaf, "latearrival.txt", 10);
                leaf.path = Some(r"C:\movedparent\entry\latearrival.txt".into());
                current_files.insert(first_leaf, leaf.clone());
                vec![leaf]
            } else {
                let mut leaf = meta(escaped_leaf, "escapedarrival.txt", 10);
                leaf.path = Some(r"C:\escapedchild\escapedarrival.txt".into());
                current_files.insert(escaped_leaf, leaf.clone());
                vec![leaf]
            };
            let batch = JournalBatch {
                events: vec![FileEvent::DirectoryRenamed {
                    doc: key,
                    parent,
                    name: old_name.into(),
                    current: Some(current.clone()),
                }],
                cursor: JournalCursor {
                    last_usn: cursor.last_usn + 100,
                    ..cursor
                },
                caught_up: true,
            };
            let mut repair = prepare_directory_repair(
                &mut store,
                &volume,
                &cfg,
                &batch,
                &reader,
                std::slice::from_ref(&resolved_parent),
            )?
            .unwrap();
            assert!(
                !repair.discovery_done,
                "partially discovered ancestry was incorrectly considered complete"
            );
            // Simulate one bounded native MFT page. The first scan observes a
            // child directory but misses its later descendants after a move.
            repair.discovery = Some(Box::new(vec![Ok(page)].into_iter()));
            let mut done = false;
            for _ in 0..50 {
                done = apply_directory_turn(
                    &mut store,
                    &volume,
                    &cfg,
                    &mut repair,
                    async |keys| {
                        Ok(keys
                            .into_iter()
                            .map(|candidate| current_files.get(&candidate).cloned())
                            .collect())
                    },
                    commit_directory_fixture,
                )
                .await?;
                if done {
                    break;
                }
            }
            assert!(done);
            cursor = batch.cursor;
            assert!(
                store
                    .volume(volume.id)?
                    .unsettled_directories
                    .contains(&entered)
            );
            assert!(
                store
                    .volume(volume.id)?
                    .unsettled_directories
                    .contains(&key)
            );
            assert!(store.volume(volume.id)?.directory_repair.is_none());
            let index = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
            assert!(meta_index::file_meta(&index, child)?.is_some());
            if key == entered {
                assert!(meta_index::file_meta(&index, first_leaf)?.is_none());
                assert!(meta_index::file_meta(&index, escaped_leaf)?.is_none());
            } else if key == ancestor {
                assert_eq!(
                    meta_index::file_meta(&index, first_leaf)?
                        .unwrap()
                        .path
                        .as_deref(),
                    Some(r"C:\movedparent\entry\latearrival.txt")
                );
                assert!(meta_index::file_meta(&index, escaped_leaf)?.is_none());
            } else {
                assert_eq!(
                    meta_index::file_meta(&index, escaped_leaf)?
                        .unwrap()
                        .path
                        .as_deref(),
                    Some(r"C:\escapedchild\escapedarrival.txt")
                );
            }
            show_directories(volume.id);
            drop(repair);
            drop(store);
            store = StateStore::open(&cfg)?;
            assert!(
                store
                    .volume(volume.id)?
                    .unsettled_directories
                    .contains(&entered)
            );
        }
        apply_journal_batch(
            &mut store,
            &volume,
            &cfg,
            JournalBatch {
                events: Vec::new(),
                cursor,
                caught_up: true,
            },
            commit_directory_fixture,
        )
        .await?;
        assert!(store.volume(volume.id)?.unsettled_directories.is_empty());
        assert!(!store.volume(volume.id)?.catching_up);
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        assert_eq!(
            search(&handler, "latearrival", SearchMode::Content).hits[0].key,
            first_leaf
        );
        assert_eq!(
            search(&handler, "escapedarrival", SearchMode::Content).hits[0].key,
            escaped_leaf
        );
        Ok(())
    }

    #[tokio::test]
    async fn directory_page_restarts_after_probe_failure_and_waits_for_retry_capacity() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let (mut cfg, mut store, volume, before) = baseline_store(root.path(), 60033)?;
        cfg.content_index_volumes = vec!["X:\\".into()];
        let parent = DocKey::from_parts(volume.id, 5);
        let folder = DocKey::from_parts(volume.id, 10);
        let old = directory_meta(folder, parent, r"C:\before");
        let renamed = directory_meta(folder, parent, r"C:\after");
        let mut child = meta(DocKey::from_parts(volume.id, 50), "bounded.txt", 10);
        child.path = Some(r"C:\before\bounded.txt".into());
        seed_directory_fixture(
            &mut store,
            &volume,
            &cfg,
            &[directory_meta(parent, parent, r"C:\"), old, child.clone()],
        )
        .await?;
        child.path = Some(r"C:\after\bounded.txt".into());
        let current = BTreeMap::from([(folder, renamed.clone()), (child.key, child.clone())]);
        for file in 1000..1000 + MAX_DEFERRED_FILES as u64 {
            store.state.deferred.push(DeferredFile {
                meta: meta(DocKey::from_parts(volume.id, file), "blocked.txt", 1),
                attempts: 1,
                retry_at: 999,
            });
        }
        store.save()?;
        let batch = JournalBatch {
            events: vec![FileEvent::DirectoryRenamed {
                doc: folder,
                parent,
                name: "before".into(),
                current: Some(renamed),
            }],
            cursor: JournalCursor {
                last_usn: before.last_usn + 100,
                ..before
            },
            caught_up: true,
        };
        let config = ReaderConfig::default();
        let mut repair = prepare_directory_repair(&mut store, &volume, &cfg, &batch, &config, &[])?
            .context("bounded repair requested full scan")?;
        let mut failed = false;
        for _ in 0..50 {
            let result = apply_directory_turn(
                &mut store,
                &volume,
                &cfg,
                &mut repair,
                async |_| Err(NtfsError::Journal("metadata access denied".into()).into()),
                commit_directory_fixture,
            )
            .await;
            if result.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert!(store.state.pending.is_none());
        assert_eq!(store.volume(volume.id)?.cursor, Some(before));
        // The production error branch discards the advanced volatile reader.
        // Its durable anchors recreate a scan containing the unresolved child.
        drop(repair);
        let mut repair =
            prepare_directory_repair(&mut store, &volume, &cfg, &batch, &config, &[])?.unwrap();
        assert!(!repair.discovery_done);
        repair.discovery = Some(Box::new(
            vec![Ok(current.values().cloned().collect())].into_iter(),
        ));
        let mut rejected = false;
        for _ in 0..50 {
            let result = apply_directory_turn(
                &mut store,
                &volume,
                &cfg,
                &mut repair,
                async |keys| {
                    Ok(keys
                        .into_iter()
                        .map(|key| current.get(&key).cloned())
                        .collect())
                },
                commit_directory_fixture,
            )
            .await;
            if let Err(error) = result {
                assert!(error.is::<state::DeferredCapacity>());
                rejected = true;
                break;
            }
        }
        assert!(rejected);
        assert_eq!(
            repair.page.as_ref().unwrap(),
            &vec![MetadataChange::Upsert(child.clone())]
        );
        assert!(store.state.pending.is_none());
        assert_eq!(store.volume(volume.id)?.cursor, Some(before));
        let error = apply_directory_turn(
            &mut store,
            &volume,
            &cfg,
            &mut repair,
            async |_| panic!("retained page must not read or resolve ahead of admission"),
            commit_directory_fixture,
        )
        .await
        .unwrap_err();
        assert!(error.is::<state::DeferredCapacity>());

        // A normal deletion cancels one old retry and releases exactly one
        // reserved slot. The retained child page can now commit unchanged.
        let removed = store.state.deferred[0].meta.key;
        store.begin(pending_for_volume(
            &volume,
            vec![MetadataChange::Delete(removed)],
            None,
            false,
            &cfg,
        ))?;
        commit_directory_fixture(&mut store, &cfg).await?;
        assert!(
            !apply_directory_turn(
                &mut store,
                &volume,
                &cfg,
                &mut repair,
                async |_| panic!("admitting retained page must not repeat its native read"),
                commit_directory_fixture,
            )
            .await?
        );
        let mut done = false;
        for _ in 0..50 {
            done = apply_directory_turn(
                &mut store,
                &volume,
                &cfg,
                &mut repair,
                async |keys| {
                    Ok(keys
                        .into_iter()
                        .map(|key| current.get(&key).cloned())
                        .collect())
                },
                commit_directory_fixture,
            )
            .await?;
            if done {
                break;
            }
        }
        assert!(done);
        show_directories(volume.id);
        let index = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
        assert_eq!(
            meta_index::file_meta(&index, child.key)?.unwrap().path,
            child.path
        );
        assert_eq!(store.volume(volume.id)?.cursor, Some(batch.cursor));
        Ok(())
    }

    #[test]
    fn deferred_files_leave_no_stale_results_while_other_changes_and_retries_progress() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut volume = VolumeInfo {
            id: 0,
            guid_path: "deferred-search-volume".into(),
            drive_letters: vec!['X'],
        };
        store.bind_volume(&mut volume)?;
        store.volume_mut(volume.id)?.id = 60_010;
        volume.id = 60_010;
        let start = JournalCursor {
            journal_id: 7,
            last_usn: 100,
        };
        store.volume_mut(volume.id)?.cursor = Some(start);
        store.volume_mut(volume.id)?.needs_scan = false;
        store.volume_mut(volume.id)?.catching_up = false;
        store.save()?;
        show_volume(volume.id);
        let key = DocKey::from_parts(volume.id, 1);
        let other_key = DocKey::from_parts(volume.id, 2);
        let original = meta(key, "blockedreport.txt", 10);
        let other = meta(other_key, "independentreport.txt", 20);
        let seed = make_pending(
            volume.id,
            vec![
                MetadataChange::Upsert(original.clone()),
                MetadataChange::Upsert(other.clone()),
            ],
            None,
            false,
            &cfg,
        );
        store.begin(seed.clone())?;
        worker_outcome(
            &cfg,
            &seed,
            &[(&original, "staleheldtoken"), (&other, "oldothertoken")],
            &[],
        )?;
        finish(&mut store, &cfg, &seed)?;
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        assert_eq!(
            search(&handler, "staleheldtoken", SearchMode::Content).total,
            1
        );

        let changed = meta(key, "blockedreport.txt", 30);
        let other_changed = meta(other_key, "independentreport.txt", 40);
        let mixed = make_pending(
            volume.id,
            vec![
                MetadataChange::Upsert(changed.clone()),
                MetadataChange::Upsert(other_changed.clone()),
            ],
            Some(JournalCursor {
                last_usn: 200,
                ..start
            }),
            false,
            &cfg,
        );
        store.begin(mixed.clone())?;
        hide_pending(&mixed);
        worker_outcome(&cfg, &mixed, &[(&other_changed, "freshothertoken")], &[key])?;
        assert_eq!(store.volume(volume.id)?.cursor, Some(start));
        drop(store);
        let mut store = StateStore::open(&cfg)?;
        assert_eq!(
            store.state.pending.as_ref().unwrap().worker.id,
            mixed.worker.id
        );
        worker_outcome(&cfg, &mixed, &[(&other_changed, "freshothertoken")], &[key])?;
        finish(&mut store, &cfg, &mixed)?;
        assert_eq!(store.volume(volume.id)?.cursor.unwrap().last_usn, 200);
        assert_eq!(store.state.deferred.len(), 1);
        assert_eq!(store.state.deferred[0].meta, changed);
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Hybrid,
        ] {
            assert_eq!(search(&handler, "blockedreport", mode).total, 0);
            assert_eq!(search(&handler, "staleheldtoken", mode).total, 0);
            let result = search(&handler, "independentreport", mode);
            assert_eq!(result.total, 1);
            assert_eq!(result.hits[0].key, other_key);
            assert_eq!(result.hits[0].size, Some(40));
        }
        assert_eq!(
            search(&handler, "freshothertoken", SearchMode::Content).total,
            1
        );
        assert_eq!(
            search(&handler, "oldothertoken", SearchMode::Content).total,
            0
        );

        drop(store);
        let mut store = StateStore::open(&cfg)?;
        assert!(store.state.pending.is_none());
        assert!(store.state.retired_indices.is_empty());
        let retry = store
            .due_retry(
                std::slice::from_ref(&volume),
                &cfg,
                store.state.deferred[0].retry_at,
                128,
            )
            .unwrap();
        store.begin(retry.clone())?;
        hide_pending(&retry);
        // The worker's later snapshot is authoritative in both indices, even
        // if this retry's durable observation predates a rename or another edit.
        let recovered = meta(key, "recoveredreport.txt", 90);
        worker_outcome(&cfg, &retry, &[(&recovered, "recoveredbodytoken")], &[])?;
        finish(&mut store, &cfg, &retry)?;
        assert!(store.state.deferred.is_empty());
        assert_eq!(store.volume(volume.id)?.cursor.unwrap().last_usn, 200);
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Hybrid,
        ] {
            let result = search(&handler, "recoveredreport", mode);
            assert_eq!(result.total, 1);
            assert_eq!(result.hits[0].key, key);
            assert_eq!(result.hits[0].size, Some(90));
            assert_eq!(result.hits[0].path, recovered.path);
            assert_eq!(search(&handler, "blockedreport", mode).total, 0);
        }
        assert_eq!(
            search(&handler, "recoveredbodytoken", SearchMode::Content).total,
            1
        );

        let blocked = make_pending(
            volume.id,
            vec![
                MetadataChange::Upsert(recovered),
                MetadataChange::Upsert(other_changed),
            ],
            Some(JournalCursor {
                last_usn: 300,
                ..start
            }),
            false,
            &cfg,
        );
        store.begin(blocked.clone())?;
        worker_outcome(&cfg, &blocked, &[], &[key, other_key])?;
        finish(&mut store, &cfg, &blocked)?;
        // Both keys have already been tombstoned. Their subsequent deletion or
        // exclusion must still cancel the saved obligations, not look unknown.
        let changes = resolve_events(
            &[
                FileEvent::Deleted(key),
                FileEvent::Excluded {
                    doc: other_key,
                    is_dir: false,
                },
            ],
            &cfg,
            &store.state.deferred,
        )?
        .unwrap();
        assert_eq!(
            changes,
            vec![
                MetadataChange::Delete(key),
                MetadataChange::Delete(other_key)
            ]
        );
        let deleted = make_pending(
            volume.id,
            changes,
            Some(JournalCursor {
                last_usn: 400,
                ..start
            }),
            false,
            &cfg,
        );
        assert!(
            deleted
                .worker
                .jobs
                .iter()
                .all(|job| job.operation == JobOperation::Delete)
        );
        store.begin(deleted.clone())?;
        worker_commit(&cfg, &deleted, None)?;
        finish(&mut store, &cfg, &deleted)?;
        assert!(store.state.deferred.is_empty());
        assert!(store.due_retry(&[volume], &cfg, i64::MAX, 128).is_none());
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Hybrid,
        ] {
            assert_eq!(search(&handler, "recoveredreport", mode).total, 0);
            assert_eq!(search(&handler, "independentreport", mode).total, 0);
        }
        Ok(())
    }

    #[test]
    fn deferred_receipt_cannot_acknowledge_content_that_was_not_tombstoned() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut volume = VolumeInfo {
            id: 0,
            guid_path: "invalid-deferred-view".into(),
            drive_letters: vec!['X'],
        };
        store.bind_volume(&mut volume)?;
        let key = DocKey::from_parts(volume.id, 91);
        let file = meta(key, "staleview.txt", 90);
        let batch = make_pending(
            volume.id,
            vec![MetadataChange::Upsert(file.clone())],
            Some(JournalCursor {
                journal_id: 31,
                last_usn: 500,
            }),
            false,
            &cfg,
        );
        store.begin(batch.clone())?;
        worker_outcome(&cfg, &batch, &[(&file, "forbiddenstalebody")], &[key])?;
        let error = apply_metadata(&cfg, &batch).unwrap_err();
        assert!(error.to_string().contains("retained a stale document"));
        assert!(store.finish().is_err());
        assert!(store.state.pending.is_some());
        assert!(store.volume(volume.id)?.cursor.is_none());
        Ok(())
    }

    fn baseline_store(
        root: &Path,
        id: VolumeId,
    ) -> Result<(AppConfig, StateStore, VolumeInfo, JournalCursor)> {
        let mut cfg = config(root);
        // Exercise real metadata commits without pretending a test fixture was
        // extracted by a native worker. Content is disabled for this volume.
        cfg.volumes = vec!["X:\\".into()];
        let mut store = StateStore::open(&cfg)?;
        let mut volume = VolumeInfo {
            id: 0,
            guid_path: format!("bounded-baseline-{id}"),
            drive_letters: vec!['X'],
        };
        store.bind_volume(&mut volume)?;
        store.volume_mut(volume.id)?.id = id;
        volume.id = id;
        let previous = JournalCursor {
            journal_id: 17,
            last_usn: 100,
        };
        store.volume_mut(id)?.cursor = Some(previous);
        store.save()?;
        hide_volume(id);
        Ok((cfg, store, volume, previous))
    }

    async fn commit_metadata_batch(store: &mut StateStore, cfg: &AppConfig) -> Result<()> {
        let batch = store
            .state
            .pending
            .clone()
            .context("missing metadata intent")?;
        ensure!(
            batch
                .worker
                .jobs
                .iter()
                .all(|job| job.operation == JobOperation::Delete),
            "metadata fixture unexpectedly requires native extraction"
        );
        hide_pending(&batch);
        worker_commit(cfg, &batch, None)?;
        finish(store, cfg, &batch)
    }

    async fn metadata_turn<I>(
        store: &mut StateStore,
        volume: &VolumeInfo,
        cfg: &AppConfig,
        scan: &mut MftProgress<I>,
    ) -> Result<bool>
    where
        I: Iterator<Item = std::result::Result<Vec<FileMeta>, NtfsError>> + Send + 'static,
    {
        apply_mft_scan(store, volume, cfg, scan, commit_metadata_batch).await
    }

    #[tokio::test]
    async fn mft_scan_waits_for_commit_and_continues_after_empty_pages() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (cfg, mut store, volume, previous) = baseline_store(root.path(), 60_001)?;
        let first = meta(DocKey::from_parts(volume.id, 10), "firstbaseline.txt", 10);
        let last = meta(DocKey::from_parts(volume.id, 20), "lastbaseline.txt", 20);
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = reads.clone();
        let mut pages = vec![Ok(vec![first]), Ok(Vec::new()), Ok(vec![last])].into_iter();
        let batches = std::iter::from_fn(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            pages.next()
        });
        let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut waiting_tx = Some(waiting_tx);
        let mut release_rx = Some(release_rx);
        let start = JournalCursor {
            last_usn: 200,
            ..previous
        };
        let mut progress = MftProgress::new(start, batches);
        assert!(!metadata_turn(&mut store, &volume, &cfg, &mut progress).await?);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "reset must precede every MFT pull"
        );
        let commit = async |store: &mut StateStore, cfg: &AppConfig| {
            let batch = store.state.pending.clone().context("missing MFT intent")?;
            hide_pending(&batch);
            if !batch.metadata.is_empty()
                && let Some(waiting) = waiting_tx.take()
            {
                let _ = waiting.send(());
                release_rx.take().context("missing worker release")?.await?;
            }
            worker_commit(cfg, &batch, None)?;
            finish(store, cfg, &batch)
        };
        let mut operation = Box::pin(apply_mft_scan(
            &mut store,
            &volume,
            &cfg,
            &mut progress,
            commit,
        ));
        tokio::select! {
            result = &mut operation => panic!("scan completed before worker acknowledgement: {result:?}"),
            result = waiting_rx => result?,
        }
        // The live first-page intent is durable, the old cursor is retained,
        // and neither the empty page nor the later file has been read ahead.
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        let disk: serde_json::Value = serde_json::from_slice(&std::fs::read(
            Path::new(&cfg.paths.state_dir).join("ingestion-v2.json"),
        )?)?;
        assert_eq!(disk["volumes"][0]["cursor"]["last_usn"], 100);
        assert_eq!(disk["volumes"][0]["needs_scan"], true);
        assert_eq!(disk["pending"]["metadata"].as_array().unwrap().len(), 1);
        assert!(read_visibility().volumes.contains(&volume.id));

        release_tx.send(()).expect("scan is waiting for its worker");
        assert!(!operation.await?);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(store.volume(volume.id)?.cursor, Some(previous));
        // An excluded-only page still consumes exactly one volume turn.
        assert!(!metadata_turn(&mut store, &volume, &cfg, &mut progress).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert!(store.volume(volume.id)?.needs_scan);
        assert!(!metadata_turn(&mut store, &volume, &cfg, &mut progress).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 3);
        assert_eq!(store.volume(volume.id)?.cursor, Some(previous));
        assert!(metadata_turn(&mut store, &volume, &cfg, &mut progress).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 4);
        assert_eq!(store.volume(volume.id)?.cursor, Some(start));
        assert!(!store.volume(volume.id)?.needs_scan);
        assert!(store.volume(volume.id)?.catching_up);
        assert!(store.state.pending.is_none());
        // EOF only completes the baseline; the volume remains hidden until a
        // real journal read confirms catch-up from the pre-enumeration cursor.
        assert!(read_visibility().volumes.contains(&volume.id));
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        assert_eq!(
            search(&handler, "lastbaseline", SearchMode::NameOnly).total,
            0
        );
        show_volume(volume.id);
        for term in ["firstbaseline", "lastbaseline"] {
            assert_eq!(search(&handler, term, SearchMode::NameOnly).total, 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn mft_reader_failure_keeps_partial_baseline_hidden_and_checkpoint_unchanged()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let (cfg, mut store, volume, previous) = baseline_store(root.path(), 60_002)?;
        let file = meta(DocKey::from_parts(volume.id, 10), "partialbaseline.txt", 10);
        let batches = vec![
            Ok(vec![file]),
            Err(NtfsError::Mft("injected truncated MFT page".into())),
        ]
        .into_iter();
        let mut progress = MftProgress::new(
            JournalCursor {
                last_usn: 200,
                ..previous
            },
            batches,
        );
        for _ in 0..2 {
            assert!(!metadata_turn(&mut store, &volume, &cfg, &mut progress).await?);
        }
        let result = metadata_turn(&mut store, &volume, &cfg, &mut progress).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("truncated MFT page"));
        assert_eq!(store.volume(volume.id)?.cursor, Some(previous));
        assert!(store.volume(volume.id)?.needs_scan);
        assert!(store.state.pending.is_none());
        drop(store);
        let store = StateStore::open(&cfg)?;
        assert_eq!(store.volume(volume.id)?.cursor, Some(previous));
        assert!(store.volume(volume.id)?.needs_scan);
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        assert_eq!(
            search(&handler, "partialbaseline", SearchMode::NameOnly).total,
            0
        );
        show_volume(volume.id);
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_mft_scan_retains_pending_batch_for_restart() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (cfg, mut store, volume, previous) = baseline_store(root.path(), 60_003)?;
        let file = meta(
            DocKey::from_parts(volume.id, 10),
            "cancelledbaseline.txt",
            10,
        );
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = reads.clone();
        let batches = std::iter::from_fn(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Some(Ok(vec![file.clone()]))
        });
        let mut progress = MftProgress::new(
            JournalCursor {
                last_usn: 200,
                ..previous
            },
            batches,
        );
        assert!(!metadata_turn(&mut store, &volume, &cfg, &mut progress).await?);
        let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
        let mut waiting_tx = Some(waiting_tx);
        let mut operation = Box::pin(apply_mft_scan(
            &mut store,
            &volume,
            &cfg,
            &mut progress,
            async |store: &mut StateStore, cfg: &AppConfig| {
                let batch = store.state.pending.clone().context("missing MFT intent")?;
                hide_pending(&batch);
                if !batch.metadata.is_empty() {
                    let _ = waiting_tx
                        .take()
                        .context("worker already waiting")?
                        .send(());
                    std::future::pending::<()>().await;
                }
                worker_commit(cfg, &batch, None)?;
                finish(store, cfg, &batch)
            },
        ));
        tokio::select! {
            result = &mut operation => panic!("scan completed while worker was paused: {result:?}"),
            result = waiting_rx => result?,
        }
        drop(operation);
        let pending_id = store.state.pending.as_ref().unwrap().worker.id;
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(store.volume(volume.id)?.cursor, Some(previous));
        let blocked = metadata_turn(&mut store, &volume, &cfg, &mut progress)
            .await
            .unwrap_err();
        assert!(format!("{blocked:#}").contains("pending work must finish"));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        drop(progress);
        drop(store);
        let mut store = StateStore::open(&cfg)?;
        let pending = store
            .state
            .pending
            .clone()
            .context("cancelled MFT intent lost")?;
        assert_eq!(pending.worker.id, pending_id);
        assert_eq!(store.volume(volume.id)?.cursor, Some(previous));
        assert!(store.volume(volume.id)?.needs_scan);
        worker_commit(&cfg, &pending, None)?;
        finish(&mut store, &cfg, &pending)?;
        assert!(store.state.pending.is_none());
        assert!(store.volume(volume.id)?.needs_scan);
        assert_eq!(store.volume(volume.id)?.cursor, Some(previous));
        assert!(read_visibility().volumes.contains(&volume.id));
        // The volatile reader was lost at restart. A new attempt must first
        // reset the partial index, then retain its own pre-enumeration head.
        let restarted = JournalCursor {
            last_usn: 300,
            ..previous
        };
        let mut fresh = MftProgress::new(
            restarted,
            vec![Ok(vec![meta(
                DocKey::from_parts(volume.id, 11),
                "restartedbaseline.txt",
                20,
            )])]
            .into_iter(),
        );
        assert!(!metadata_turn(&mut store, &volume, &cfg, &mut fresh).await?);
        let index = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
        assert_eq!(meta_index::open_reader(&index)?.searcher().num_docs(), 0);
        assert_eq!(store.volume(volume.id)?.cursor, Some(previous));
        assert!(!metadata_turn(&mut store, &volume, &cfg, &mut fresh).await?);
        assert!(metadata_turn(&mut store, &volume, &cfg, &mut fresh).await?);
        assert_eq!(store.volume(volume.id)?.cursor, Some(restarted));
        assert!(!store.volume(volume.id)?.needs_scan);
        assert!(store.volume(volume.id)?.catching_up);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "discarded reader cannot resume after restart"
        );
        show_volume(volume.id);
        Ok(())
    }

    #[tokio::test]
    async fn bounded_baseline_turns_allow_other_volume_changes_and_track_new_backlog() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let (mut cfg, mut store, baseline, previous) = baseline_store(root.path(), 60_020)?;
        cfg.volumes.push("Y:\\".into());
        let mut live = VolumeInfo {
            id: 0,
            guid_path: "live-during-another-baseline".into(),
            drive_letters: vec!['Y'],
        };
        store.bind_volume(&mut live)?;
        let live_start = JournalCursor {
            journal_id: 27,
            last_usn: 700,
        };
        let live_state = store.volume_mut(live.id)?;
        live_state.cursor = Some(live_start);
        live_state.needs_scan = false;
        live_state.catching_up = false;
        store.save()?;
        show_volume(live.id);

        let first = meta(
            DocKey::from_parts(baseline.id, 10),
            "firstfairbaseline.txt",
            10,
        );
        let last = meta(
            DocKey::from_parts(baseline.id, 20),
            "lastfairbaseline.txt",
            20,
        );
        let mut pages = vec![Ok(vec![first.clone()]), Ok(Vec::new()), Ok(vec![last])].into_iter();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = reads.clone();
        let mut scan = MftProgress::new(
            JournalCursor {
                last_usn: 200,
                ..previous
            },
            std::iter::from_fn(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                pages.next()
            }),
        );
        let live_key = DocKey::from_parts(live.id, 30);
        let old_live = meta(live_key, "oldfairlive.txt", 30);
        let seed = pending_for_volume(
            &live,
            vec![MetadataChange::Upsert(old_live)],
            None,
            false,
            &cfg,
        );
        store.begin(seed)?;
        commit_metadata_batch(&mut store, &cfg).await?;
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        assert_eq!(
            search(&handler, "oldfairlive", SearchMode::NameOnly).total,
            1
        );

        assert!(!metadata_turn(&mut store, &baseline, &cfg, &mut scan).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(!metadata_turn(&mut store, &baseline, &cfg, &mut scan).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(store.volume(baseline.id)?.cursor, Some(previous));
        assert_eq!(
            search(&handler, "firstfairbaseline", SearchMode::NameOnly).total,
            0
        );

        // A healthy volume can receive more than one bounded journal turn.
        // Persist its backlog marker before worker admission, even while the
        // other volume still has an incomplete, hidden baseline.
        let mut saw_backlog_before_commit = false;
        let renamed = meta(live_key, "newfairlive.txt", 40);
        let progress = apply_journal_batch(
            &mut store,
            &live,
            &cfg,
            ntfs_watcher::JournalBatch {
                events: vec![FileEvent::Renamed {
                    from: live_key,
                    to: renamed,
                }],
                cursor: JournalCursor {
                    last_usn: 780,
                    ..live_start
                },
                caught_up: false,
            },
            async |store: &mut StateStore, cfg: &AppConfig| {
                let disk: serde_json::Value = serde_json::from_slice(&std::fs::read(
                    Path::new(&cfg.paths.state_dir).join("ingestion-v2.json"),
                )?)?;
                saw_backlog_before_commit =
                    disk["volumes"].as_array().unwrap().iter().any(|volume| {
                        volume["id"].as_u64() == Some(u64::from(live.id))
                            && volume["catching_up"] == true
                    });
                commit_metadata_batch(store, cfg).await
            },
        )
        .await?;
        assert!(saw_backlog_before_commit);
        assert!(progress.journal_read && progress.work_remaining);
        assert!(store.volume(live.id)?.catching_up);
        assert_eq!(store.volume(live.id)?.cursor.unwrap().last_usn, 780);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(
            search(&handler, "oldfairlive", SearchMode::NameOnly).total,
            0
        );
        let result = search(&handler, "newfairlive", SearchMode::NameOnly);
        assert_eq!(result.total, 1);
        assert_eq!(result.hits[0].key, live_key);

        // Empty filtered progress must yield rather than pulling the next page.
        assert!(!metadata_turn(&mut store, &baseline, &cfg, &mut scan).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert!(store.volume(baseline.id)?.needs_scan);
        let progress = apply_journal_batch(
            &mut store,
            &live,
            &cfg,
            ntfs_watcher::JournalBatch {
                events: vec![FileEvent::Deleted(live_key)],
                cursor: JournalCursor {
                    last_usn: 860,
                    ..live_start
                },
                caught_up: true,
            },
            commit_metadata_batch,
        )
        .await?;
        assert!(progress.journal_read && !progress.work_remaining);
        assert!(!store.volume(live.id)?.catching_up);
        assert_eq!(
            search(&handler, "newfairlive", SearchMode::NameOnly).total,
            0
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2);

        assert!(!metadata_turn(&mut store, &baseline, &cfg, &mut scan).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 3);
        assert_eq!(store.volume(baseline.id)?.cursor, Some(previous));
        assert!(metadata_turn(&mut store, &baseline, &cfg, &mut scan).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 4);
        assert_eq!(store.volume(baseline.id)?.cursor, Some(scan.start));
        assert!(read_visibility().volumes.contains(&baseline.id));

        // Replay a change made after the captured baseline head. The snapshot
        // prefix cannot become visible before this journal replacement commits.
        let current = meta(first.key, "currentfairbaseline.txt", 50);
        apply_journal_batch(
            &mut store,
            &baseline,
            &cfg,
            ntfs_watcher::JournalBatch {
                events: vec![FileEvent::Modified(current)],
                cursor: JournalCursor {
                    last_usn: 300,
                    ..previous
                },
                caught_up: true,
            },
            commit_metadata_batch,
        )
        .await?;
        assert!(!read_visibility().volumes.contains(&baseline.id));
        assert_eq!(
            search(&handler, "firstfairbaseline", SearchMode::NameOnly).total,
            0
        );
        for term in ["currentfairbaseline", "lastfairbaseline"] {
            assert_eq!(search(&handler, term, SearchMode::NameOnly).total, 1);
        }
        drop(store);
        let restored = StateStore::open(&cfg)?;
        assert_eq!(restored.volume(baseline.id)?.cursor.unwrap().last_usn, 300);
        assert_eq!(restored.volume(live.id)?.cursor.unwrap().last_usn, 860);
        assert!(!restored.volume(baseline.id)?.needs_scan);
        assert!(!restored.volume(live.id)?.catching_up);
        Ok(())
    }

    #[tokio::test]
    async fn rejected_mft_page_stays_bounded_until_cleanup_frees_retry_capacity() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (mut cfg, mut store, volume, previous) = baseline_store(root.path(), 60_022)?;
        cfg.content_index_volumes = vec!["X:\\".into()];
        let mut other = VolumeInfo {
            id: 0,
            guid_path: "other-volume-reserved-capacity".into(),
            drive_letters: vec!['Y'],
        };
        store.bind_volume(&mut other)?;
        // These already-tombstoned obligations belong to another volume, so
        // this baseline's reset cannot release their reserved retry capacity.
        store.state.deferred = (1..=MAX_DEFERRED_FILES)
            .map(|file| DeferredFile {
                meta: meta(
                    DocKey::from_parts(other.id, file as u64),
                    "deferredcapacity.txt",
                    1,
                ),
                attempts: 1,
                retry_at: i64::MAX,
            })
            .collect();
        store.save()?;
        let file = meta(DocKey::from_parts(volume.id, 10), "retainedpage.txt", 10);
        let mut pages = vec![Ok(vec![file.clone()])].into_iter();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = reads.clone();
        let mut scan = MftProgress::new(
            JournalCursor {
                last_usn: 200,
                ..previous
            },
            std::iter::from_fn(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                pages.next()
            }),
        );
        assert!(!metadata_turn(&mut store, &volume, &cfg, &mut scan).await?);
        for _ in 0..2 {
            let error = metadata_turn(&mut store, &volume, &cfg, &mut scan)
                .await
                .unwrap_err();
            assert!(error.is::<state::DeferredCapacity>());
            assert_eq!(reads.load(Ordering::SeqCst), 1);
            assert_eq!(scan.page.as_deref(), Some(std::slice::from_ref(&file)));
            assert!(store.state.pending.is_none());
            assert_eq!(store.volume(volume.id)?.cursor, Some(previous));
        }

        // An actual durable deletion, rather than a manual ledger edit, frees
        // one slot while the original scan keeps its single unadmitted page.
        let released_key = store.state.deferred[0].meta.key;
        store.begin(pending_for_volume(
            &other,
            vec![MetadataChange::Delete(released_key)],
            None,
            false,
            &cfg,
        ))?;
        commit_metadata_batch(&mut store, &cfg).await?;
        assert_eq!(store.state.deferred.len(), MAX_DEFERRED_FILES - 1);
        assert!(
            !apply_mft_scan(
                &mut store,
                &volume,
                &cfg,
                &mut scan,
                async |store: &mut StateStore, cfg: &AppConfig| {
                    let pending = store
                        .state
                        .pending
                        .clone()
                        .context("retained page was not admitted")?;
                    hide_pending(&pending);
                    worker_commit(cfg, &pending, Some((&file, "retainedpagetoken")))?;
                    finish(store, cfg, &pending)
                },
            )
            .await?
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "admission retry cannot skip to the next page"
        );
        assert!(scan.page.is_none());
        assert!(metadata_turn(&mut store, &volume, &cfg, &mut scan).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(store.volume(volume.id)?.cursor, Some(scan.start));
        show_volume(volume.id);
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        assert_eq!(
            search(&handler, "retainedpage", SearchMode::NameOnly).total,
            1
        );
        assert_eq!(
            search(&handler, "retainedpagetoken", SearchMode::Content).total,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_mft_page_commit_replays_before_the_same_reader_continues() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (cfg, mut store, volume, previous) = baseline_store(root.path(), 60_024)?;
        let first = meta(
            DocKey::from_parts(volume.id, 10),
            "replayedfirstpage.txt",
            10,
        );
        let last = meta(
            DocKey::from_parts(volume.id, 20),
            "continuedlastpage.txt",
            20,
        );
        let mut pages = vec![Ok(vec![first]), Ok(vec![last])].into_iter();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = reads.clone();
        let mut scan = MftProgress::new(
            JournalCursor {
                last_usn: 200,
                ..previous
            },
            std::iter::from_fn(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                pages.next()
            }),
        );
        assert!(!metadata_turn(&mut store, &volume, &cfg, &mut scan).await?);
        let error = apply_mft_scan(
            &mut store,
            &volume,
            &cfg,
            &mut scan,
            async |store: &mut StateStore, cfg: &AppConfig| {
                let pending = store
                    .state
                    .pending
                    .clone()
                    .context("missing failed-page intent")?;
                hide_pending(&pending);
                worker_commit(cfg, &pending, None)?;
                anyhow::bail!("injected failure before metadata commit")
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("before metadata commit"));
        let pending_id = store
            .state
            .pending
            .as_ref()
            .context("failed page lost its intent")?
            .worker
            .id;
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert!(
            scan.page.is_none(),
            "durable intent now owns the consumed page"
        );
        assert!(
            metadata_turn(&mut store, &volume, &cfg, &mut scan)
                .await
                .is_err()
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        replay_pending(
            &mut store,
            &cfg,
            &BTreeSet::from([volume.id]),
            commit_metadata_batch,
        )
        .await?;
        assert_eq!(store.state.completed_batch, Some(pending_id));
        assert!(store.state.pending.is_none());
        assert_eq!(store.volume(volume.id)?.cursor, Some(previous));
        assert!(!metadata_turn(&mut store, &volume, &cfg, &mut scan).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert!(metadata_turn(&mut store, &volume, &cfg, &mut scan).await?);
        assert_eq!(reads.load(Ordering::SeqCst), 3);
        assert_eq!(store.volume(volume.id)?.cursor, Some(scan.start));
        show_volume(volume.id);
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        for term in ["replayedfirstpage", "continuedlastpage"] {
            assert_eq!(search(&handler, term, SearchMode::NameOnly).total, 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_pending_volume_defers_without_blocking_other_volumes_and_recovers()
    -> Result<()> {
        for no_volumes_mounted in [false, true] {
            let root = tempfile::tempdir()?;
            let (mut cfg, mut store, source, start) = baseline_store(root.path(), 60_025)?;
            cfg.volumes.push("Y:\\".into());
            cfg.content_index_volumes = vec!["X:\\".into(), "Y:\\".into()];
            let mut available = VolumeInfo {
                id: 0,
                guid_path: "available-after-source-volume-loss".into(),
                drive_letters: vec!['Y'],
            };
            store.bind_volume(&mut available)?;
            let available_start = JournalCursor {
                journal_id: 71,
                last_usn: 700,
            };
            for (id, cursor) in [(source.id, start), (available.id, available_start)] {
                let checkpoint = store.volume_mut(id)?;
                checkpoint.cursor = Some(cursor);
                checkpoint.needs_scan = false;
                checkpoint.catching_up = false;
                show_volume(id);
            }
            store.save()?;
            let key = DocKey::from_parts(source.id, 10);
            let original = meta(key, "offlinequeued.txt", 10);
            let seed = pending_for_volume(
                &source,
                vec![MetadataChange::Upsert(original.clone())],
                None,
                false,
                &cfg,
            );
            store.begin(seed.clone())?;
            worker_commit(&cfg, &seed, Some((&original, "beforeofflinetoken")))?;
            finish(&mut store, &cfg, &seed)?;
            let handler = UnifiedSearchHandler::try_new(
                Path::new(&cfg.paths.meta_index),
                Path::new(&cfg.paths.content_index),
            )?;
            assert_eq!(
                search(&handler, "beforeofflinetoken", SearchMode::Content).total,
                1
            );

            let queued = meta(key, "offlinequeued.txt", 30);
            let pending = pending_for_volume(
                &source,
                vec![MetadataChange::Upsert(queued.clone())],
                Some(JournalCursor {
                    last_usn: 300,
                    ..start
                }),
                false,
                &cfg,
            );
            store.begin(pending.clone())?;
            drop(store);
            let mut store = StateStore::open(&cfg)?;
            assert_eq!(
                store.state.pending.as_ref().unwrap().worker.id,
                pending.worker.id
            );
            // The source is now deselected or absent. Persisted admission
            // retains its original extraction policy and exact batch identity.
            cfg.volumes = vec!["Y:\\".into()];
            cfg.content_index_volumes = vec!["Y:\\".into()];
            let selected = if no_volumes_mounted {
                BTreeSet::new()
            } else {
                BTreeSet::from([available.id])
            };
            let mut committed_unavailable = false;
            replay_pending(
                &mut store,
                &cfg,
                &selected,
                async |store: &mut StateStore, cfg: &AppConfig| {
                    assert!(read_visibility().volumes.contains(&source.id));
                    let batch = store
                        .state
                        .pending
                        .clone()
                        .context("unavailable intent vanished")?;
                    assert_eq!(batch.worker.id, pending.worker.id);
                    assert_eq!(batch.worker.jobs[0].operation, JobOperation::Reconcile);
                    // Model the real worker's unavailable-GUID outcome with
                    // actual durable content tombstones and commit receipts.
                    worker_outcome(cfg, &batch, &[], &[key])?;
                    finish(store, cfg, &batch)?;
                    committed_unavailable = true;
                    Ok(())
                },
            )
            .await?;
            assert!(committed_unavailable);
            assert!(store.state.pending.is_none());
            assert_eq!(store.volume(source.id)?.cursor.unwrap().last_usn, 300);
            assert_eq!(store.state.deferred.len(), 1);
            assert_eq!(store.state.deferred[0].meta, queued);
            assert!(read_visibility().volumes.contains(&source.id));
            for mode in [
                SearchMode::NameOnly,
                SearchMode::Content,
                SearchMode::Hybrid,
            ] {
                assert_eq!(search(&handler, "offlinequeued", mode).total, 0);
                assert_eq!(search(&handler, "beforeofflinetoken", mode).total, 0);
            }

            let other = meta(
                DocKey::from_parts(available.id, 20),
                "availablevolume.txt",
                40,
            );
            apply_journal_batch(
                &mut store,
                &available,
                &cfg,
                ntfs_watcher::JournalBatch {
                    events: vec![FileEvent::Created(other.clone())],
                    cursor: JournalCursor {
                        last_usn: 800,
                        ..available_start
                    },
                    caught_up: true,
                },
                async |store: &mut StateStore, cfg: &AppConfig| {
                    let batch = store
                        .state
                        .pending
                        .clone()
                        .context("available volume was blocked")?;
                    worker_commit(cfg, &batch, Some((&other, "availablevolumetoken")))?;
                    finish(store, cfg, &batch)
                },
            )
            .await?;
            for mode in [
                SearchMode::NameOnly,
                SearchMode::Content,
                SearchMode::Hybrid,
            ] {
                assert_eq!(search(&handler, "availablevolume", mode).total, 1);
            }
            let due_at = store.state.deferred[0].retry_at;
            assert!(
                store
                    .due_retry(std::slice::from_ref(&available), &cfg, due_at, 128)
                    .is_none()
            );
            drop(store);
            let mut store = StateStore::open(&cfg)?;
            assert_eq!(store.state.deferred[0].meta, queued);
            assert!(store.state.retired_indices.is_empty());

            cfg.volumes.push("X:\\".into());
            cfg.content_index_volumes.push("X:\\".into());
            let retry = store
                .due_retry(std::slice::from_ref(&source), &cfg, due_at, 128)
                .context("remounted source did not recover its retry")?;
            assert!(retry.retry && retry.next_cursor.is_none());
            store.begin(retry)?;
            let recovered = meta(key, "offlinequeued.txt", 90);
            replay_pending(
                &mut store,
                &cfg,
                &BTreeSet::from([source.id, available.id]),
                async |store: &mut StateStore, cfg: &AppConfig| {
                    let batch = store
                        .state
                        .pending
                        .clone()
                        .context("retry intent missing")?;
                    worker_commit(cfg, &batch, Some((&recovered, "recoveredofflinetoken")))?;
                    finish(store, cfg, &batch)
                },
            )
            .await?;
            assert!(store.state.deferred.is_empty());
            assert_eq!(store.volume(source.id)?.cursor.unwrap().last_usn, 300);
            apply_journal_batch(
                &mut store,
                &source,
                &cfg,
                ntfs_watcher::JournalBatch {
                    events: Vec::new(),
                    cursor: JournalCursor {
                        last_usn: 300,
                        ..start
                    },
                    caught_up: true,
                },
                commit_metadata_batch,
            )
            .await?;
            for mode in [
                SearchMode::NameOnly,
                SearchMode::Content,
                SearchMode::Hybrid,
            ] {
                let result = search(&handler, "offlinequeued", mode);
                assert_eq!(result.total, 1);
                assert_eq!(result.hits[0].key, key);
                assert_eq!(result.hits[0].size, Some(90));
                assert_eq!(search(&handler, "beforeofflinetoken", mode).total, 0);
            }
            assert_eq!(
                search(&handler, "recoveredofflinetoken", SearchMode::Content).total,
                1
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn saturated_unavailable_retries_release_other_volumes_and_require_full_return_scan()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let (mut cfg, mut store, source, source_start) = baseline_store(root.path(), 60_028)?;
        cfg.volumes.push("Y:\\".into());
        cfg.content_index_volumes = vec!["X:\\".into(), "Y:\\".into()];
        let mut active = VolumeInfo {
            id: 0,
            guid_path: "active-with-saturated-offline-retries".into(),
            drive_letters: vec!['Y'],
        };
        store.bind_volume(&mut active)?;
        let active_start = JournalCursor {
            journal_id: 71,
            last_usn: 700,
        };
        for (id, cursor) in [(source.id, source_start), (active.id, active_start)] {
            let checkpoint = store.volume_mut(id)?;
            checkpoint.cursor = Some(cursor);
            checkpoint.needs_scan = false;
            checkpoint.catching_up = false;
            show_volume(id);
        }
        store.save()?;
        let old_source = meta(DocKey::from_parts(source.id, 2000), "offlinestale.txt", 10);
        let removed_source = meta(DocKey::from_parts(source.id, 2001), "offlinegone.txt", 20);
        let original = meta(DocKey::from_parts(active.id, 10), "activeretained.txt", 30);
        let deleted = meta(DocKey::from_parts(active.id, 20), "activedeleted.txt", 40);
        for (volume, snapshots) in [
            (
                &source,
                vec![
                    (&old_source, "offlinestaletoken"),
                    (&removed_source, "offlinegonetoken"),
                ],
            ),
            (
                &active,
                vec![
                    (&original, "activeoldtoken"),
                    (&deleted, "activedeletedtoken"),
                ],
            ),
        ] {
            let seed = pending_for_volume(
                volume,
                snapshots
                    .iter()
                    .map(|(meta, _)| MetadataChange::Upsert((*meta).clone()))
                    .collect(),
                None,
                false,
                &cfg,
            );
            store.begin(seed.clone())?;
            worker_outcome(&cfg, &seed, &snapshots, &[])?;
            finish(&mut store, &cfg, &seed)?;
        }
        // Model a valid, already-tombstoned retry ledger left by earlier
        // bounded worker batches. Every slot belongs to the absent volume.
        store.state.deferred = (1..=MAX_DEFERRED_FILES as u64)
            .map(|id| DeferredFile {
                meta: meta(DocKey::from_parts(source.id, id), "offlineheld.txt", 10),
                attempts: 3,
                retry_at: i64::MAX,
            })
            .collect();
        store.save()?;
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        assert_eq!(
            search(&handler, "offlinestaletoken", SearchMode::Content).total,
            1
        );
        let modified = meta(original.key, "activeretained.txt", 90);
        let changes = ntfs_watcher::JournalBatch {
            events: vec![
                FileEvent::Modified(modified.clone()),
                FileEvent::Deleted(deleted.key),
            ],
            cursor: JournalCursor {
                last_usn: 800,
                ..active_start
            },
            caught_up: true,
        };
        let error = apply_journal_batch(
            &mut store,
            &active,
            &cfg,
            changes.clone(),
            async |_: &mut StateStore, _: &AppConfig| {
                anyhow::bail!("full retry capacity unexpectedly admitted new work")
            },
        )
        .await
        .unwrap_err();
        assert!(error.is::<state::DeferredCapacity>());
        assert!(store.state.pending.is_none());
        assert_eq!(store.volume(active.id)?.cursor, Some(active_start));

        // watch_changes masks unselected volumes, replays any global intent,
        // then durably transfers their retries before admitting active work.
        hide_volume(source.id);
        let selected = BTreeSet::from([active.id]);
        assert_eq!(
            store.reclaim_unavailable_retries(&selected)?,
            MAX_DEFERRED_FILES
        );
        assert!(store.state.deferred.is_empty());
        drop(store);
        let mut store = StateStore::open(&cfg)?;
        assert!(store.state.retired_indices.is_empty());
        assert!(store.volume(source.id)?.needs_scan);
        assert!(store.volume(source.id)?.catching_up);
        assert_eq!(store.volume(source.id)?.cursor, Some(source_start));
        assert!(store.state.deferred.is_empty());
        apply_journal_batch(
            &mut store,
            &active,
            &cfg,
            changes,
            async |store: &mut StateStore, cfg: &AppConfig| {
                let batch = store
                    .state
                    .pending
                    .clone()
                    .context("active intent missing")?;
                hide_pending(&batch);
                worker_commit(cfg, &batch, Some((&modified, "activeupdatedtoken")))?;
                finish(store, cfg, &batch)
            },
        )
        .await?;
        assert_eq!(store.volume(active.id)?.cursor.unwrap().last_usn, 800);
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Hybrid,
        ] {
            let result = search(&handler, "activeretained", mode);
            assert_eq!(result.total, 1);
            assert_eq!(result.hits[0].key, original.key);
            assert_eq!(result.hits[0].size, Some(90));
            assert_eq!(search(&handler, "activedeleted", mode).total, 0);
            assert_eq!(search(&handler, "offlinestale", mode).total, 0);
        }
        assert_eq!(
            search(&handler, "activeoldtoken", SearchMode::Content).total,
            0
        );
        assert_eq!(
            search(&handler, "activeupdatedtoken", SearchMode::Content).total,
            1
        );

        // Returning to selection cannot reveal the historical source cursor.
        // Even an empty journal read is refused until the full scan completes.
        assert_eq!(
            store.reclaim_unavailable_retries(&BTreeSet::from([source.id, active.id]))?,
            0
        );
        assert!(
            apply_journal_batch(
                &mut store,
                &source,
                &cfg,
                ntfs_watcher::JournalBatch {
                    events: Vec::new(),
                    cursor: source_start,
                    caught_up: true,
                },
                commit_metadata_batch,
            )
            .await
            .is_err()
        );
        assert!(read_visibility().volumes.contains(&source.id));
        let renamed = meta(old_source.key, "offlinerenamed.txt", 50);
        let recovered = meta(DocKey::from_parts(source.id, 1), "offlinerecovered.txt", 60);
        let mut scan = MftProgress::new(
            JournalCursor {
                last_usn: 500,
                ..source_start
            },
            vec![Ok(vec![renamed.clone(), recovered.clone()])].into_iter(),
        );
        assert!(!metadata_turn(&mut store, &source, &cfg, &mut scan).await?);
        assert!(read_visibility().volumes.contains(&source.id));
        assert!(
            !apply_mft_scan(
                &mut store,
                &source,
                &cfg,
                &mut scan,
                async |store: &mut StateStore, cfg: &AppConfig| {
                    let batch = store.state.pending.clone().context("source page missing")?;
                    hide_pending(&batch);
                    worker_outcome(
                        cfg,
                        &batch,
                        &[
                            (&renamed, "offlinerenamedtoken"),
                            (&recovered, "offlinebaselinetoken"),
                        ],
                        &[],
                    )?;
                    finish(store, cfg, &batch)
                },
            )
            .await?
        );
        assert!(metadata_turn(&mut store, &source, &cfg, &mut scan).await?);
        assert!(!store.volume(source.id)?.needs_scan);
        assert!(store.volume(source.id)?.catching_up);
        assert_eq!(store.volume(source.id)?.cursor, Some(scan.start));
        assert_eq!(
            search(&handler, "offlinerenamed", SearchMode::NameOnly).total,
            0
        );
        assert_eq!(
            search(&handler, "offlinebaselinetoken", SearchMode::Content).total,
            0
        );
        let current = meta(recovered.key, "offlinerecovered.txt", 80);
        apply_journal_batch(
            &mut store,
            &source,
            &cfg,
            ntfs_watcher::JournalBatch {
                events: vec![FileEvent::Modified(current.clone())],
                cursor: JournalCursor {
                    last_usn: 600,
                    ..source_start
                },
                caught_up: true,
            },
            async |store: &mut StateStore, cfg: &AppConfig| {
                let batch = store
                    .state
                    .pending
                    .clone()
                    .context("source catch-up missing")?;
                hide_pending(&batch);
                worker_commit(cfg, &batch, Some((&current, "offlinecurrenttoken")))?;
                finish(store, cfg, &batch)
            },
        )
        .await?;
        assert!(store.volume(source.id)?.catching_up);
        assert!(read_visibility().volumes.contains(&source.id));
        let verification_cursor = store.volume(source.id)?.cursor.unwrap();
        apply_journal_batch(
            &mut store,
            &source,
            &cfg,
            JournalBatch {
                events: Vec::new(),
                cursor: verification_cursor,
                caught_up: true,
            },
            commit_metadata_batch,
        )
        .await?;
        assert!(!read_visibility().volumes.contains(&source.id));
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Hybrid,
        ] {
            let result = search(&handler, "offlinerecovered", mode);
            assert_eq!(result.total, 1);
            assert_eq!(result.hits[0].key, recovered.key);
            assert_eq!(result.hits[0].size, Some(80));
            assert_eq!(search(&handler, "offlinerenamed", mode).total, 1);
            assert_eq!(search(&handler, "offlinestale", mode).total, 0);
            assert_eq!(search(&handler, "offlinegone", mode).total, 0);
            assert_eq!(search(&handler, "activeretained", mode).total, 1);
        }
        for token in [
            "offlinestaletoken",
            "offlinegonetoken",
            "offlinebaselinetoken",
        ] {
            assert_eq!(search(&handler, token, SearchMode::Content).total, 0);
        }
        assert_eq!(
            search(&handler, "offlinecurrenttoken", SearchMode::Content).total,
            1
        );
        drop(store);
        let store = StateStore::open(&cfg)?;
        assert!(store.state.deferred.is_empty());
        assert!(store.state.retired_indices.is_empty());
        assert!(!store.volume(source.id)?.needs_scan);
        assert!(!store.volume(source.id)?.catching_up);
        assert_eq!(store.volume(source.id)?.cursor.unwrap().last_usn, 600);
        assert_eq!(store.volume(active.id)?.cursor.unwrap().last_usn, 800);
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_metadata_task_keeps_ingestion_lock_until_its_write_finishes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (cfg, mut store, volume, previous) = baseline_store(root.path(), 60_004)?;
        let file = meta(DocKey::from_parts(volume.id, 10), "leasedmetadata.txt", 10);
        let pending = pending_for_volume(
            &volume,
            vec![MetadataChange::Upsert(file)],
            Some(JournalCursor {
                last_usn: 200,
                ..previous
            }),
            false,
            &cfg,
        );
        store.begin(pending.clone())?;
        hide_pending(&pending);
        worker_commit(&cfg, &pending, None)?;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let apply_cfg = cfg.clone();
        let apply_batch = pending.clone();
        let mut operation = Box::pin(run_index_mutation(store.mutation_lease(), move || {
            let _ = started_tx.send(());
            release_rx
                .recv()
                .context("metadata release channel closed")?;
            apply_metadata(&apply_cfg, &apply_batch)
        }));
        tokio::select! {
            result = &mut operation => panic!("metadata mutation did not pause: {result:?}"),
            result = started_rx => result?,
        }
        drop(operation);
        drop(store);
        match StateStore::open(&cfg) {
            Ok(_) => anyhow::bail!("a new session overtook a detached metadata mutation"),
            Err(error) => assert!(format!("{error:#}").contains("another service owns")),
        }

        release_tx.send(())?;
        // Cancellation discards the task's result, not its ownership. Wait for
        // the real metadata commit and the final lock owner to leave the task.
        let mut recovered = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match StateStore::open(&cfg) {
                    Ok(store) => break Ok(store),
                    Err(error) if format!("{error:#}").contains("another service owns") => {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    Err(error) => break Err(error),
                }
            }
        })
        .await??;
        assert_eq!(recovered.volume(volume.id)?.cursor, Some(previous));
        assert_eq!(
            recovered.state.pending.as_ref().unwrap().worker.id,
            pending.worker.id
        );
        recovered.finish()?;
        assert_eq!(recovered.volume(volume.id)?.cursor.unwrap().last_usn, 200);
        show_pending(&pending);
        show_volume(volume.id);
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        assert_eq!(
            search(&handler, "leasedmetadata", SearchMode::NameOnly).total,
            1
        );
        Ok(())
    }

    #[test]
    fn volume_content_policy_reconciles_changes_and_replays_recorded_operations() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut cfg = config(root.path());
        cfg.volumes = vec!["X:\\".into()];
        let mut store = StateStore::open(&cfg)?;
        let mut volume = VolumeInfo {
            id: 0,
            guid_path: "content-policy-volume".into(),
            drive_letters: vec!['X'],
        };
        store.bind_volume(&mut volume)?;
        // Keep this fixture's global visibility distinct from other test indices.
        store.volume_mut(volume.id)?.id = 61_000;
        volume.id = 61_000;
        store.save()?;
        let key = DocKey::from_parts(volume.id, 0xabcd_0000_0000_0042);
        let file = meta(key, "policyreport.txt", 10);
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        let start = JournalCursor {
            last_usn: 100,
            journal_id: 7,
        };

        assert!(refresh_content_policy(&mut store, &volume, &cfg)?);
        assert!(!refresh_content_policy(&mut store, &volume, &cfg)?);
        {
            let state = store.volume_mut(volume.id)?;
            assert!(!state.content_policy.as_ref().unwrap().enabled);
            state.cursor = Some(start);
            state.needs_scan = false;
            state.catching_up = false;
        }
        store.save()?;
        show_volume(volume.id);

        let disabled = pending_for_volume(
            &volume,
            vec![MetadataChange::Upsert(file.clone())],
            Some(JournalCursor {
                last_usn: 200,
                ..start
            }),
            false,
            &cfg,
        );
        assert_eq!(disabled.worker.jobs[0].operation, JobOperation::Delete);
        assert!(disabled.worker.jobs[0].path.as_os_str().is_empty());
        store.begin(disabled.clone())?;
        drop(store);

        // Enable content while a metadata-only intent remains on disk. Reopening
        // must schedule a new scan, without reinterpreting the old Delete job.
        cfg.content_index_volumes = vec!["x:\\".into()];
        let mut store = StateStore::open(&cfg)?;
        assert!(refresh_content_policy(&mut store, &volume, &cfg)?);
        let replay = store.state.pending.clone().context("pending policy lost")?;
        assert_eq!(replay.worker.id, disabled.worker.id);
        assert_eq!(replay.worker.jobs[0].operation, JobOperation::Delete);
        assert_eq!(store.volume(volume.id)?.cursor, Some(start));
        assert!(store.volume(volume.id)?.needs_scan);
        assert!(store.volume(volume.id)?.catching_up);
        worker_commit(&cfg, &replay, None)?;
        finish(&mut store, &cfg, &replay)?;
        assert!(store.volume(volume.id)?.needs_scan);
        assert_eq!(
            search(&handler, "policyreport", SearchMode::NameOnly).total,
            0
        );

        // Apply the same reset/metadata batches used by full reconciliation.
        let reset = pending_for_volume(&volume, Vec::new(), None, true, &cfg);
        store.begin(reset.clone())?;
        worker_commit(&cfg, &reset, None)?;
        finish(&mut store, &cfg, &reset)?;
        let enabled = pending_for_volume(
            &volume,
            vec![MetadataChange::Upsert(file.clone())],
            None,
            false,
            &cfg,
        );
        assert_eq!(enabled.worker.jobs[0].operation, JobOperation::Reconcile);
        assert_eq!(
            enabled.worker.jobs[0].max_chars,
            Some(usize::try_from(cfg.extract.max_chars_per_file)?)
        );
        store.begin(enabled.clone())?;
        worker_commit(&cfg, &enabled, Some((&file, "policycontenttoken")))?;
        finish(&mut store, &cfg, &enabled)?;
        {
            let state = store.volume_mut(volume.id)?;
            state.cursor = Some(JournalCursor {
                last_usn: 300,
                ..start
            });
            state.needs_scan = false;
            state.catching_up = false;
        }
        store.save()?;
        show_volume(volume.id);
        assert_eq!(
            search(&handler, "policyreport", SearchMode::NameOnly).hits[0].key,
            key
        );
        assert_eq!(
            search(&handler, "policycontenttoken", SearchMode::Content).hits[0].key,
            key
        );

        // Disabling content hides the old result immediately and forces a reset;
        // afterwards the filename survives and the previously indexed text does not.
        cfg.content_index_volumes.clear();
        assert!(refresh_content_policy(&mut store, &volume, &cfg)?);
        assert_eq!(
            search(&handler, "policycontenttoken", SearchMode::Content).total,
            0
        );
        let reset = pending_for_volume(&volume, Vec::new(), None, true, &cfg);
        store.begin(reset.clone())?;
        worker_commit(&cfg, &reset, None)?;
        finish(&mut store, &cfg, &reset)?;
        let disabled = pending_for_volume(
            &volume,
            vec![MetadataChange::Upsert(file)],
            None,
            false,
            &cfg,
        );
        assert_eq!(disabled.worker.jobs[0].operation, JobOperation::Delete);
        store.begin(disabled.clone())?;
        worker_commit(&cfg, &disabled, None)?;
        finish(&mut store, &cfg, &disabled)?;
        {
            let state = store.volume_mut(volume.id)?;
            state.needs_scan = false;
            state.catching_up = false;
        }
        store.save()?;
        show_volume(volume.id);
        assert_eq!(
            search(&handler, "policyreport", SearchMode::NameOnly).total,
            1
        );
        assert_eq!(
            search(&handler, "policycontenttoken", SearchMode::Content).total,
            0
        );

        cfg.extract.max_chars_per_file += 1;
        assert!(refresh_content_policy(&mut store, &volume, &cfg)?);
        drop(store);
        let mut store = StateStore::open(&cfg)?;
        assert!(!refresh_content_policy(&mut store, &volume, &cfg)?);
        assert!(store.volume(volume.id)?.needs_scan);
        assert_eq!(
            store
                .volume(volume.id)?
                .content_policy
                .as_ref()
                .unwrap()
                .max_chars_per_file,
            cfg.extract.max_chars_per_file,
        );
        show_volume(volume.id);
        Ok(())
    }

    #[test]
    fn mutations_replay_without_stale_results_across_partial_commit_and_restart() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = config(root.path());
        let mut store = StateStore::open(&cfg)?;
        let mut volume = VolumeInfo {
            id: 0,
            guid_path: "stable-test-volume".into(),
            drive_letters: vec!['X'],
        };
        store.bind_volume(&mut volume)?;
        let key = DocKey::from_parts(volume.id, 0xabcd_0000_0000_0042);
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        let start = JournalCursor {
            last_usn: 100,
            journal_id: 7,
        };
        store.volume_mut(volume.id)?.cursor = Some(start);
        store.volume_mut(volume.id)?.needs_scan = false;

        let original = meta(key, "firstreport.txt", 10);
        let first = make_pending(
            volume.id,
            event_changes(&[FileEvent::Created(original.clone())])?,
            Some(JournalCursor {
                last_usn: 200,
                ..start
            }),
            false,
            &cfg,
        );
        store.begin(first.clone())?;
        hide_pending(&first);
        worker_commit(&cfg, &first, Some((&original, "olduniquetoken")))?;
        finish(&mut store, &cfg, &first)?;
        assert_eq!(
            search(&handler, "olduniquetoken", SearchMode::Content).total,
            1
        );
        assert_eq!(
            search(&handler, "firstreport", SearchMode::NameOnly).hits[0].key,
            key
        );

        // Source metadata at extraction differs from its journal observation.
        let observed = meta(key, "firstreport.txt", 20);
        let actual = meta(key, "firstreport.txt", 80);
        let changed = make_pending(
            volume.id,
            event_changes(&[FileEvent::Modified(observed)])?,
            Some(JournalCursor {
                last_usn: 300,
                ..start
            }),
            false,
            &cfg,
        );
        store.begin(changed.clone())?;
        hide_pending(&changed);
        worker_commit(&cfg, &changed, Some((&actual, "newuniquetoken")))?;
        assert_eq!(store.volume(volume.id)?.cursor.unwrap().last_usn, 200);
        for (term, mode) in [
            ("olduniquetoken", SearchMode::Content),
            ("newuniquetoken", SearchMode::Content),
            ("firstreport", SearchMode::NameOnly),
            ("firstreport", SearchMode::Hybrid),
        ] {
            assert_eq!(
                search(&handler, term, mode).total,
                0,
                "partial replacement must remain hidden"
            );
        }
        drop(store);
        let mut store = StateStore::open(&cfg)?;
        let replay = store
            .state
            .pending
            .clone()
            .context("pending intent lost on restart")?;
        assert_eq!(replay.worker.id, changed.worker.id);
        assert_eq!(store.volume(volume.id)?.cursor.unwrap().last_usn, 200);
        worker_commit(&cfg, &replay, Some((&actual, "newuniquetoken")))?;
        finish(&mut store, &cfg, &replay)?;
        assert_eq!(
            search(&handler, "olduniquetoken", SearchMode::Content).total,
            0
        );
        let content = search(&handler, "newuniquetoken", SearchMode::Content);
        let names = search(&handler, "firstreport", SearchMode::NameOnly);
        assert_eq!((content.total, names.total), (1, 1));
        assert_eq!((content.hits[0].key, names.hits[0].key), (key, key));
        assert_eq!(
            (content.hits[0].size, names.hits[0].size),
            (Some(80), Some(80))
        );

        // Replay a fully committed operation too: exactly one result survives.
        store.begin(replay.clone())?;
        hide_pending(&replay);
        worker_commit(&cfg, &replay, Some((&actual, "newuniquetoken")))?;
        finish(&mut store, &cfg, &replay)?;
        assert_eq!(
            search(&handler, "newuniquetoken", SearchMode::Hybrid).total,
            1
        );

        let renamed = meta(key, "renamedreport.txt", 80);
        let rename = make_pending(
            volume.id,
            event_changes(&[FileEvent::Renamed {
                from: key,
                to: renamed.clone(),
            }])?,
            Some(JournalCursor {
                last_usn: 400,
                ..start
            }),
            false,
            &cfg,
        );
        store.begin(rename.clone())?;
        hide_pending(&rename);
        worker_commit(&cfg, &rename, Some((&renamed, "newuniquetoken")))?;
        finish(&mut store, &cfg, &rename)?;
        assert_eq!(
            search(&handler, "firstreport", SearchMode::NameOnly).total,
            0
        );
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Hybrid,
        ] {
            let response = search(&handler, "renamedreport", mode);
            assert_eq!(response.total, 1);
            assert_eq!(response.hits[0].key, key);
            assert_eq!(response.hits[0].path, renamed.path);
        }

        let mut attributes = renamed.clone();
        attributes.flags = FileFlags::OFFLINE;
        let attr = make_pending(
            volume.id,
            event_changes(&[FileEvent::AttributesChanged(attributes)])?,
            None,
            false,
            &cfg,
        );
        assert_eq!(attr.worker.jobs[0].operation, JobOperation::Delete);
        store.begin(attr.clone())?;
        hide_pending(&attr);
        worker_commit(&cfg, &attr, None)?;
        finish(&mut store, &cfg, &attr)?;
        assert_eq!(
            search(&handler, "renamedreport", SearchMode::NameOnly).total,
            1
        );
        assert_eq!(
            search(&handler, "newuniquetoken", SearchMode::Content).total,
            0
        );

        let deletion = make_pending(
            volume.id,
            resolve_events(&[FileEvent::Deleted(key)], &cfg, &[])?.unwrap(),
            Some(JournalCursor {
                last_usn: 500,
                ..start
            }),
            false,
            &cfg,
        );
        store.begin(deletion.clone())?;
        hide_pending(&deletion);
        worker_commit(&cfg, &deletion, None)?;
        finish(&mut store, &cfg, &deletion)?;
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Hybrid,
        ] {
            assert_eq!(search(&handler, "renamedreport", mode).total, 0);
        }
        assert_eq!(store.volume(volume.id)?.cursor.unwrap().last_usn, 500);
        assert!(
            resolve_events(&[FileEvent::Deleted(key)], &cfg, &[])?
                .unwrap()
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn replay_uses_persisted_content_policy_and_exclusions_remove_existing_entries() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut cfg = config(root.path());
        let _store = StateStore::open(&cfg)?;
        let key = DocKey::from_parts(17, 91);
        let file = meta(key, "oversize.txt", 100);
        cfg.extract.max_bytes_per_file = 50;
        let pending = make_pending(
            17,
            vec![MetadataChange::Upsert(file.clone())],
            None,
            false,
            &cfg,
        );
        assert_eq!(pending.worker.jobs[0].operation, JobOperation::Delete);
        cfg.extract.max_bytes_per_file = 200;
        worker_commit(&cfg, &pending, None)?;
        apply_metadata(&cfg, &pending)?;
        // A configuration change must not reinterpret a successful metadata-only
        // job as a missing content snapshot and discard its filename result.
        let handler = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        assert_eq!(search(&handler, "oversize", SearchMode::NameOnly).total, 1);
        let excluded = [FileEvent::Excluded {
            doc: key,
            is_dir: false,
        }];
        assert_eq!(
            resolve_events(&excluded, &cfg, &[])?.unwrap(),
            vec![MetadataChange::Delete(key)]
        );
        assert!(
            resolve_events(
                &[FileEvent::Excluded {
                    doc: DocKey::from_parts(17, 92),
                    is_dir: false
                }],
                &cfg,
                &[]
            )?
            .unwrap()
            .is_empty()
        );
        assert!(
            resolve_events(
                &[FileEvent::Excluded {
                    doc: key,
                    is_dir: true
                }],
                &cfg,
                &[]
            )?
            .is_none()
        );
        Ok(())
    }
}
