//! Serialized, durable MFT reconciliation and USN ingestion.

mod state;
mod stats;

use crate::scheduler_runtime::submit_index_batch;
use crate::status_provider::{
    update_status_ingestion_state, update_status_last_commit, update_status_volumes,
};
use anyhow::{Context, Result, ensure};
use core_types::config::AppConfig;
use core_types::{DocKey, FileMeta, VolumeId};
use ipc::VolumeStatus;
use ntfs_watcher::{
    FileEvent, JournalCursor, NtfsError, ReaderConfig, VolumeInfo, begin_mft_scan, canonical_path,
    discover_volumes, tail_usn_batch_with_config,
};
use state::{
    BATCH_LIMIT, ContentPolicy, MetadataChange, PendingBatch, StateStore, event_changes,
    pending_for_volume,
};
use stats::IndexStatistics;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock, RwLockReadGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, MissedTickBehavior, interval};

static RESCAN_GENERATION: AtomicU64 = AtomicU64::new(0);
const MUTATION_BATCH_LIMIT: usize = 128;

/// Request reconciliation on the same serialized lane as journal changes.
pub fn request_rescan() {
    RESCAN_GENERATION.fetch_add(1, Ordering::Relaxed);
    update_status_ingestion_state("reconciliation requested");
}

/// Entries being replaced are hidden from both indices until their transaction
/// commits. Whole-volume reconciliation hides descendants of renamed directories.
#[derive(Default)]
pub(crate) struct Visibility {
    pub volumes: BTreeSet<VolumeId>,
    pub documents: BTreeSet<DocKey>,
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
    VISIBILITY
        .get_or_init(RwLock::default)
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .volumes
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
    let store = &mut session.store;
    let mut generation = RESCAN_GENERATION.load(Ordering::Relaxed);
    let mut last_idle_checkpoint = Instant::now();
    let mut statistics = IndexStatistics::new(Path::new(&cfg.paths.meta_index))?;
    let mut ticker = interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
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
            for volume in &mut store.state.volumes {
                volume.needs_scan = true;
                volume.catching_up = true;
                hide_volume(volume.id);
            }
            store.save()?;
            generation = requested;
        }

        let discovered = tokio::task::spawn_blocking(discover_volumes).await?;
        let volumes = match discovered {
            Ok(volumes) => filter_volumes(&cfg, volumes),
            Err(error) => {
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
            refresh_content_policy(store, volume, &cfg)?;
        }
        let selected: BTreeSet<_> = volumes.iter().map(|v| v.id).collect();
        for volume in &store.state.volumes {
            if !selected.contains(&volume.id) {
                hide_volume(volume.id);
            }
        }
        if volumes.is_empty() {
            update_status_ingestion_state("unavailable: no configured NTFS volume is mounted");
            publish_status(store, &mut statistics)?;
            continue;
        }

        // A failed batch remains durable and masks stale results. Replay it
        // before reading any later journal record, preserving worker ordering.
        if let Some(batch) = &store.state.pending {
            if !selected.contains(&batch.volume) {
                update_status_ingestion_state(
                    "blocked: pending volume is unavailable or deselected",
                );
                continue;
            }
            if let Err(error) = commit_pending(store, &cfg).await {
                update_status_ingestion_state(format!("retrying durable batch: {error:#}"));
                tracing::error!(%error, "pending batch failed; cursor retained");
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
        for volume in &volumes {
            if let Err(error) = process_volume(store, volume, &cfg).await {
                hide_volume(volume.id);
                failures.push(format!("volume {}: {error:#}", volume.id));
                tracing::error!(volume = volume.id, %error, "ingestion deferred; checkpoint retained");
                if store.state.pending.is_some() {
                    break;
                }
            }
        }
        if failures.is_empty() {
            let pending = store
                .state
                .volumes
                .iter()
                .any(|v| selected.contains(&v.id) && (v.needs_scan || v.catching_up));
            update_status_ingestion_state(if pending {
                "catching up with journal"
            } else {
                "watching; last bounded journal read succeeded"
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

async fn process_volume(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
) -> Result<()> {
    if store.volume(volume.id)?.needs_scan || store.volume(volume.id)?.cursor.is_none() {
        reconcile_volume(store, volume, cfg).await?;
    }
    let cursor = store
        .volume(volume.id)?
        .cursor
        .context("volume has no journal checkpoint")?;
    let read_volume = volume.clone();
    let reader_config = reader_config(cfg, &store.state.retired_indices)?;
    let result = tokio::task::spawn_blocking(move || {
        tail_usn_batch_with_config(&read_volume, cursor, &reader_config)
    })
    .await?;
    match result {
        Err(NtfsError::GapDetected) => {
            hide_volume(volume.id);
            let state = store.volume_mut(volume.id)?;
            state.needs_scan = true;
            state.catching_up = true;
            store.save()?;
            update_status_ingestion_state(format!("volume {} journal gap; reconciling", volume.id));
        }
        Err(error) => return Err(error.into()),
        Ok(batch) => {
            let events = batch.events;
            let next = batch.cursor;
            let Some(changes) = resolve_events(&events, cfg)? else {
                hide_volume(volume.id);
                let state = store.volume_mut(volume.id)?;
                state.needs_scan = true;
                state.catching_up = true;
                store.save()?;
                return Ok(());
            };
            if !changes.is_empty() {
                // Bound durable JSON even for long paths. The raw read's cursor
                // belongs only to its final mutation batch; earlier chunks can
                // safely be replayed if the service restarts between them.
                let count = changes.chunks(MUTATION_BATCH_LIMIT).len();
                for (number, chunk) in changes.chunks(MUTATION_BATCH_LIMIT).enumerate() {
                    let checkpoint = (number + 1 == count).then_some(next);
                    store.begin(pending_for_volume(
                        volume,
                        chunk.to_vec(),
                        checkpoint,
                        false,
                        cfg,
                    ))?;
                    commit_pending(store, cfg).await?;
                }
            } else {
                store.volume_mut(volume.id)?.cursor = Some(next);
            }
            // The reader compares against its observed pre-read journal head,
            // so our own checkpoint/log writes cannot prevent catch-up forever.
            if batch.caught_up {
                if store.volume(volume.id)?.catching_up {
                    store.volume_mut(volume.id)?.catching_up = false;
                    store.save()?;
                }
                show_volume(volume.id);
            }
        }
    }
    Ok(())
}

async fn reconcile_volume(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
) -> Result<()> {
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
        tokio::task::spawn_blocking(move || begin_mft_scan(&scan_volume, &reader_config)).await??;
    // Opening the scan captures the journal head before any MFT enumeration.
    let start = scan.journal_cursor();
    let batches = std::iter::from_fn(move || scan.next_batch().transpose());
    apply_mft_scan(store, volume, cfg, start, batches, commit_pending).await
}

/// Pull the next bounded page only after the previous page's durable work has
/// committed. There is no background producer that can accumulate the MFT while
/// worker admission is paused. Reader/worker failure or cancellation leaves the
/// persisted `needs_scan` marker and any pending intent intact.
async fn apply_mft_scan<I, C>(
    store: &mut StateStore,
    volume: &VolumeInfo,
    cfg: &AppConfig,
    start: JournalCursor,
    mut batches: I,
    mut commit: C,
) -> Result<()>
where
    I: Iterator<Item = std::result::Result<Vec<FileMeta>, NtfsError>> + Send + 'static,
    C: AsyncFnMut(&mut StateStore, &AppConfig) -> Result<()>,
{
    ensure!(
        store.volume(volume.id)?.needs_scan,
        "MFT reconciliation must be marked incomplete before it starts"
    );
    // Reset through the worker lane as well, so old extraction jobs cannot
    // resurrect files after deletion. A crash restarts this complete scan.
    store.begin(pending_for_volume(volume, Vec::new(), None, true, cfg))?;
    commit(store, cfg).await?;
    loop {
        let (returned, batch) = tokio::task::spawn_blocking(move || {
            let batch = batches.next();
            (batches, batch)
        })
        .await
        .context("MFT reader task failed")?;
        batches = returned;
        let Some(metas) = batch else {
            break;
        };
        let metas = metas?;
        ensure!(
            metas.len() <= MUTATION_BATCH_LIMIT,
            "MFT reader exceeded the mutation batch limit"
        );
        // An empty page is progress through excluded/missing records, not EOF.
        if metas.is_empty() {
            continue;
        }
        let changes = metas.into_iter().map(MetadataChange::Upsert).collect();
        store.begin(pending_for_volume(volume, changes, None, false, cfg))?;
        commit(store, cfg).await?;
    }
    // Only a successful, journal-validated native EOF completes the baseline.
    let state = store.volume_mut(volume.id)?;
    state.cursor = Some(start);
    state.needs_scan = false;
    state.catching_up = true;
    store.save()?;
    Ok(())
}

/// Unknown tombstones and our own output need no index commits. If an indexed
/// document moves into an excluded root, remove it from both search views.
fn resolve_events(events: &[FileEvent], cfg: &AppConfig) -> Result<Option<Vec<MetadataChange>>> {
    if events.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let meta = meta_index::open_or_create_index(Path::new(&cfg.paths.meta_index))?;
    let meta_reader = meta_index::open_reader(&meta)?;
    let content = content_index::open_or_create(Path::new(&cfg.paths.content_index))?;
    let content_reader = content_index::open_reader(&content)?;
    let mut known = std::collections::BTreeMap::new();
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
            FileEvent::RescanRequired { .. } => return Ok(None),
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
    ensure!(
        content_index::committed_batch(&content.index)? == Some(batch.worker.id),
        "worker did not commit the expected ingestion batch {}",
        batch.worker.id
    );
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
        volumes.push(VolumeStatus {
            volume: volume.id,
            indexed_files: count,
            indexed_bytes: bytes,
            pending_files: pending.map_or(0, |batch| batch.worker.jobs.len() as u64),
            pending_bytes: pending.map_or(0, |batch| {
                batch.worker.jobs.iter().map(|job| job.file_size).sum()
            }),
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
                && let Some((meta, text)) = snapshot.filter(|(meta, _)| meta.key == key)
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
        content_index::commit_batch(&mut writer, batch.worker.id)?;
        Ok(())
    }

    fn finish(store: &mut StateStore, cfg: &AppConfig, batch: &PendingBatch) -> Result<()> {
        apply_metadata(cfg, batch)?;
        store.finish()?;
        show_pending(batch);
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
            &mut store, &volume, &cfg, start, batches, commit,
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
        operation.await?;
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
        let result = apply_mft_scan(
            &mut store,
            &volume,
            &cfg,
            JournalCursor {
                last_usn: 200,
                ..previous
            },
            batches,
            async |store: &mut StateStore, cfg: &AppConfig| {
                let batch = store.state.pending.clone().context("missing MFT intent")?;
                worker_commit(cfg, &batch, None)?;
                finish(store, cfg, &batch)
            },
        )
        .await;
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
        let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
        let mut waiting_tx = Some(waiting_tx);
        let mut operation = Box::pin(apply_mft_scan(
            &mut store,
            &volume,
            &cfg,
            JournalCursor {
                last_usn: 200,
                ..previous
            },
            batches,
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
        show_volume(volume.id);
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
            resolve_events(&[FileEvent::Deleted(key)], &cfg)?.unwrap(),
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
            resolve_events(&[FileEvent::Deleted(key)], &cfg)?
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
            resolve_events(&excluded, &cfg)?.unwrap(),
            vec![MetadataChange::Delete(key)]
        );
        assert!(
            resolve_events(
                &[FileEvent::Excluded {
                    doc: DocKey::from_parts(17, 92),
                    is_dir: false
                }],
                &cfg
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
                &cfg
            )?
            .is_none()
        );
        Ok(())
    }
}
