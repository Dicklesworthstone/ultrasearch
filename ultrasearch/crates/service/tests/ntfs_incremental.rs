//! Native acceptance test for the complete MFT/USN -> durable batch -> real
//! worker -> metadata/content search pipeline. No events or index rows are seeded.
//!
//! Run elevated on Windows with an active journal on an isolated NTFS volume:
//! `ULTRASEARCH_NTFS_TEST_ROOT=V:\` and `ULTRASEARCH_WORKER_PATH=<absolute worker.exe>`.
//! Then run `cargo test -p service --features e2e-windows --test ntfs_incremental
//! -- --ignored --nocapture --test-threads=1`.
//!
//! The test indexes the configured volume. It creates a unique fixture directory
//! and retains its indices, journal checkpoints, and success evidence for inspection.

#![cfg(all(windows, feature = "e2e-windows"))]

use anyhow::{Context, Result, bail, ensure};
use core_types::{DocKey, VolumeId, config::AppConfig};
use ipc::{
    QueryExpr, SearchHit, SearchMode, SearchRequest, SearchResponse, TermExpr, TermModifier,
};
use ntfs_watcher::{canonical_path, discover_volumes, query_journal};
use serde_json::Value;
use service::{SchedulerRuntime, SearchHandler, UnifiedSearchHandler, scanner, status_snapshot};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep, timeout};
use uuid::Uuid;

const DEADLINE: Duration = Duration::from_secs(180);
const MODES: [SearchMode; 3] = [
    SearchMode::NameOnly,
    SearchMode::Content,
    SearchMode::Hybrid,
];

struct WorkerLoop {
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl WorkerLoop {
    fn start(cfg: &AppConfig) -> Self {
        let mut scheduler = SchedulerRuntime::new(cfg);
        scheduler.force_allow_content();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut ticker = interval(Duration::from_millis(200));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    _ = ticker.tick() => {}
                }
                // Do not interrupt a real worker or its commit during shutdown.
                scheduler.tick().await;
            }
        });
        Self {
            stop: Some(stop),
            task: Some(task),
        }
    }

    async fn stop(mut self) -> Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let mut task = self.task.take().context("worker loop already stopped")?;
        match timeout(DEADLINE, &mut task).await {
            Ok(result) => result.context("worker loop failed during shutdown"),
            Err(error) => {
                task.abort();
                let _ = task.await;
                Err(error).context("real worker did not finish before shutdown deadline")
            }
        }
    }
}

impl Drop for WorkerLoop {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

struct Pipeline {
    cfg: AppConfig,
    search: UnifiedSearchHandler,
    watcher: Option<JoinHandle<Result<()>>>,
    workers: Option<WorkerLoop>,
}

impl Pipeline {
    fn start(cfg: &AppConfig) -> Result<Self> {
        core_types::config::set_current_config(cfg.clone())?;
        let session = scanner::initialize_indexes(cfg)?;
        let search = UnifiedSearchHandler::try_new(
            Path::new(&cfg.paths.meta_index),
            Path::new(&cfg.paths.content_index),
        )?;
        let workers = WorkerLoop::start(cfg);
        let watcher = tokio::spawn(scanner::watch_changes(cfg.clone(), session));
        Ok(Self {
            cfg: cfg.clone(),
            search,
            watcher: Some(watcher),
            workers: Some(workers),
        })
    }

    fn ensure_running(&self) -> Result<()> {
        ensure!(
            self.watcher
                .as_ref()
                .is_some_and(|task| !task.is_finished()),
            "journal task exited; status: {:?}",
            status_snapshot()
        );
        if let Some(workers) = &self.workers {
            ensure!(
                workers
                    .task
                    .as_ref()
                    .is_some_and(|task| !task.is_finished()),
                "scheduler task exited"
            );
        }
        Ok(())
    }

    fn ensure_ready(&self) -> Result<()> {
        self.ensure_running()?;
        let status = status_snapshot();
        ensure!(
            status
                .scheduler_state
                .contains("ingestion=watching; last bounded journal read succeeded"),
            "ingestion is not healthy: {}",
            status.scheduler_state
        );
        ensure!(
            status.volumes.len() == 1
                && status.volumes[0].last_usn.is_some()
                && status.volumes[0].journal_id.is_some()
                && status.volumes[0].pending_files == 0,
            "volume checkpoint is not ready: {:?}",
            status.volumes
        );
        ensure!(
            status.metrics.as_ref().is_some_and(|metrics| {
                metrics.queue_depth == Some(0) && metrics.active_workers == Some(0)
            }),
            "worker has not drained: {:?}",
            status.metrics
        );
        let state = read_state(&self.cfg)?;
        ensure!(
            state["pending"].is_null(),
            "durable intent is still pending"
        );
        let volumes = state["volumes"]
            .as_array()
            .context("volume state missing")?;
        ensure!(
            volumes.len() == 1
                && volumes[0]["needs_scan"] == false
                && volumes[0]["catching_up"] == false,
            "persisted reconciliation is incomplete: {volumes:?}"
        );
        Ok(())
    }

    async fn pause_workers(&mut self) -> Result<()> {
        self.workers
            .take()
            .context("worker loop is already paused")?
            .stop()
            .await
    }

    fn resume_workers(&mut self) -> Result<()> {
        ensure!(self.workers.is_none(), "worker loop is already running");
        self.workers = Some(WorkerLoop::start(&self.cfg));
        Ok(())
    }

    async fn stop(mut self) -> Result<()> {
        let watcher = self
            .watcher
            .take()
            .context("journal task already stopped")?;
        watcher.abort();
        let stopped = watcher.await;
        if self.workers.is_some() {
            self.pause_workers().await?;
        }
        match stopped {
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(error).context("journal task panicked"),
            Ok(result) => {
                result?;
                bail!("journal task exited unexpectedly before shutdown")
            }
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
    }
}

async fn eventually<T>(label: &str, mut check: impl FnMut() -> Result<T>) -> Result<T> {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match check() {
            Ok(value) => return Ok(value),
            Err(error) if Instant::now() >= deadline => {
                bail!(
                    "{label} timed out: {error:#}; status: {:?}",
                    status_snapshot()
                );
            }
            Err(_) => sleep(Duration::from_millis(200)).await,
        }
    }
}

fn search(handler: &UnifiedSearchHandler, term: &str, mode: SearchMode) -> Result<SearchResponse> {
    let response = handler.search(SearchRequest {
        id: Uuid::new_v4(),
        query: QueryExpr::Term(TermExpr {
            field: None,
            value: term.to_owned(),
            modifier: TermModifier::Term,
        }),
        limit: 20,
        mode,
        timeout: Some(Duration::from_secs(5)),
        offset: 0,
    });
    ensure!(
        !matches!(
            response.served_by.as_deref(),
            Some("service-unavailable" | "service-stub")
        ),
        "search unavailable for {term:?} in {mode:?}"
    );
    // Every term is unique to one fixture. Fail immediately on duplicates even
    // during replay; retrying until they disappear would conceal the regression.
    assert!(
        response.hits.len() <= 1 && response.total <= 1,
        "duplicate/stale results for {term:?} in {mode:?}: {response:?}"
    );
    Ok(response)
}

fn one_hit(handler: &UnifiedSearchHandler, term: &str, mode: SearchMode) -> Result<SearchHit> {
    let mut response = search(handler, term, mode)?;
    ensure!(
        response.hits.len() == 1 && response.total == 1,
        "expected one {mode:?} result for {term:?}, got {response:?}"
    );
    Ok(response.hits.remove(0))
}

fn absent(handler: &UnifiedSearchHandler, term: &str, modes: &[SearchMode]) -> Result<()> {
    for mode in modes {
        let response = search(handler, term, *mode)?;
        ensure!(
            response.hits.is_empty() && response.total == 0,
            "stale {mode:?} result for {term:?}: {response:?}"
        );
    }
    Ok(())
}

fn document(
    handler: &UnifiedSearchHandler,
    name_term: &str,
    content_term: &str,
    path: &Path,
    expected_key: Option<DocKey>,
) -> Result<DocKey> {
    let expected_path = canonical_path(path)?;
    let expected_name = path
        .file_name()
        .context("fixture has no name")?
        .to_string_lossy();
    let file_metadata = fs::metadata(path)?;
    let expected_size = file_metadata.len();
    let expected_modified = i64::try_from(
        file_metadata
            .modified()?
            .duration_since(UNIX_EPOCH)?
            .as_secs(),
    )?;
    let key = one_hit(handler, name_term, SearchMode::NameOnly)?.key;
    if let Some(expected_key) = expected_key {
        ensure!(
            key == expected_key,
            "document identity changed: {expected_key} -> {key}"
        );
    }
    for (term, mode) in MODES
        .into_iter()
        .map(|mode| (name_term, mode))
        .chain([SearchMode::Content, SearchMode::Hybrid].map(|mode| (content_term, mode)))
    {
        let hit = one_hit(handler, term, mode)?;
        ensure!(
            hit.key == key,
            "metadata/content identity disagrees: {hit:?}"
        );
        ensure!(
            hit.name.as_deref() == Some(expected_name.as_ref()),
            "stale name: {hit:?}"
        );
        ensure!(
            hit.path
                .as_ref()
                .is_some_and(|actual| actual.eq_ignore_ascii_case(&expected_path)),
            "stale or non-GUID path: expected {expected_path:?}, got {hit:?}"
        );
        ensure!(
            hit.size == Some(expected_size),
            "stale size: {hit:?}, expected {expected_size}"
        );
        ensure!(
            hit.modified == Some(expected_modified),
            "stale modified timestamp: {hit:?}, expected {expected_modified}"
        );
    }
    // Content-only tokens must come from actual extraction, not file names.
    absent(handler, content_term, &[SearchMode::NameOnly])?;
    Ok(key)
}

fn replace_contents(path: &Path, contents: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn read_state(cfg: &AppConfig) -> Result<Value> {
    let path = Path::new(&cfg.paths.state_dir).join("ingestion-v2.json");
    serde_json::from_slice(&fs::read(&path)?).with_context(|| format!("read {}", path.display()))
}

fn volume_state(state: &Value, volume: VolumeId) -> Result<&Value> {
    state["volumes"]
        .as_array()
        .context("volume state missing")?
        .iter()
        .find(|entry| entry["id"].as_u64() == Some(u64::from(volume)))
        .context("persisted volume identity missing")
}

fn isolated_config(root: &Path, mount: &str) -> Result<AppConfig> {
    let data = root.join("data");
    let mut cfg = AppConfig::default();
    cfg.app.data_dir = data.to_string_lossy().into_owned();
    cfg.paths.meta_index = data.join("meta").to_string_lossy().into_owned();
    cfg.paths.content_index = data.join("content").to_string_lossy().into_owned();
    cfg.paths.state_dir = data.join("state").to_string_lossy().into_owned();
    cfg.paths.jobs_dir = data.join("jobs").to_string_lossy().into_owned();
    cfg.logging.file = data.join("log/service.log").to_string_lossy().into_owned();
    cfg.semantic.index_dir = data.join("semantic").to_string_lossy().into_owned();
    cfg.semantic.enabled = false;
    cfg.metrics.enabled = false;
    cfg.volumes = vec![mount.to_owned()];
    cfg.content_index_volumes = cfg.volumes.clone();
    for directory in [
        &cfg.paths.meta_index,
        &cfg.paths.content_index,
        &cfg.paths.state_dir,
        &cfg.paths.jobs_dir,
        &cfg.semantic.index_dir,
    ] {
        fs::create_dir_all(directory)?;
    }
    fs::create_dir_all(data.join("log"))?;
    Ok(cfg)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires elevated native Windows, an isolated journal-enabled NTFS volume, and a real built worker.exe"]
async fn production_ntfs_incremental_lifecycle_and_restart() -> Result<()> {
    dotenvy::dotenv().ok();
    let mount = std::env::var("ULTRASEARCH_NTFS_TEST_ROOT")
        .context("set ULTRASEARCH_NTFS_TEST_ROOT to an isolated NTFS volume root, e.g. V:\\")?;
    ensure!(
        mount.len() == 3 && mount.as_bytes()[0].is_ascii_alphabetic() && &mount[1..] == ":\\",
        "ULTRASEARCH_NTFS_TEST_ROOT must be an isolated drive root such as V:\\"
    );
    let worker =
        PathBuf::from(std::env::var("ULTRASEARCH_WORKER_PATH").context(
            "set ULTRASEARCH_WORKER_PATH to the absolute path of a real built worker.exe",
        )?);
    ensure!(
        worker.is_absolute() && worker.is_file(),
        "real worker executable missing: {}",
        worker.display()
    );
    let letter = mount
        .chars()
        .next()
        .context("drive letter missing")?
        .to_ascii_uppercase();
    let volume = discover_volumes()?
        .into_iter()
        .find(|volume| volume.drive_letters.contains(&letter))
        .context("configured test volume is not mounted NTFS")?;
    let journal_before = query_journal(&volume)
        .context("native journal unavailable; run elevated with an active NTFS journal")?;

    let nonce = Uuid::new_v4().simple().to_string();
    let root = PathBuf::from(&mount).join(format!("ultrasearch-ingestion-{nonce}"));
    let documents = root.join("documents");
    fs::create_dir_all(&documents)?;
    let cfg = isolated_config(&root, &mount)?;
    eprintln!(
        "Native NTFS service test artifacts retained at {}",
        root.display()
    );
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let _status = service::init_basic_status_provider();
    let mut pipeline = Pipeline::start(&cfg)?;
    eventually(
        "initial MFT reconciliation and live journal catch-up",
        || pipeline.ensure_ready(),
    )
    .await?;

    // Create only after baseline completion: passing this phase requires live USN
    // ingestion and a real content worker, not a successful initial MFT scan.
    let old_name = format!("old{nonce}");
    let new_name = format!("new{nonce}");
    let keep_name = format!("keep{nonce}");
    let first = format!("first{nonce}");
    let second = format!("second{nonce}");
    let third = format!("third{nonce}");
    let guard = format!("guard{nonce}");
    let original = documents.join(format!("{old_name}.txt"));
    let renamed = documents.join(format!("{new_name}.txt"));
    let sentinel = documents.join(format!("{keep_name}.txt"));
    replace_contents(&original, &format!("{first}\n"))?;
    replace_contents(&sentinel, &format!("{guard}\n"))?;
    let (key, sentinel_key) = eventually("journal create", || {
        pipeline.ensure_ready()?;
        Ok((
            document(&pipeline.search, &old_name, &first, &original, None)?,
            document(&pipeline.search, &keep_name, &guard, &sentinel, None)?,
        ))
    })
    .await?;
    ensure!(
        key.file_id() != 0 && key != sentinel_key,
        "invalid native file identities"
    );
    let initial_state = read_state(&cfg)?;
    let initial_volume = volume_state(&initial_state, key.volume())?.clone();

    replace_contents(
        &original,
        &format!("{second} the contents have changed and grown\n"),
    )?;
    eventually("journal content replacement", || {
        pipeline.ensure_ready()?;
        document(&pipeline.search, &old_name, &second, &original, Some(key))?;
        absent(&pipeline.search, &first, &MODES)
    })
    .await?;

    fs::rename(&original, &renamed)?;
    eventually("journal rename", || {
        pipeline.ensure_ready()?;
        document(&pipeline.search, &new_name, &second, &renamed, Some(key))?;
        absent(&pipeline.search, &old_name, &MODES)
    })
    .await?;

    // Suspend admission to prove that a forced rescan persists its intent and
    // retains its checkpoint while no worker can acknowledge the reset.
    pipeline.pause_workers().await?;
    scanner::request_rescan();
    let blocked = eventually("durable rescan under backpressure", || {
        pipeline.ensure_running()?;
        let state = read_state(&cfg)?;
        ensure!(
            state["pending"]["worker"]["reset_volumes"]
                .as_array()
                .is_some_and(|volumes| volumes
                    .iter()
                    .any(|id| id.as_u64() == Some(u64::from(key.volume())))),
            "rescan has not durably admitted its reset intent"
        );
        ensure!(
            !status_snapshot()
                .scheduler_state
                .contains("ingestion=watching"),
            "blocked ingestion incorrectly reports healthy idle"
        );
        for mode in MODES {
            ensure!(
                search(&pipeline.search, &new_name, mode)?.hits.is_empty(),
                "rescan serves stale {mode:?} hits"
            );
        }
        Ok(state)
    })
    .await?;
    sleep(Duration::from_millis(2200)).await;
    let retried = read_state(&cfg)?;
    ensure!(
        blocked["pending"] == retried["pending"],
        "retry changed or discarded the durable intent"
    );
    ensure!(
        volume_state(&blocked, key.volume())?["cursor"]
            == volume_state(&retried, key.volume())?["cursor"],
        "checkpoint advanced without worker acknowledgement"
    );
    pipeline.resume_workers()?;
    eventually("rescan replay without duplicates", || {
        pipeline.ensure_ready()?;
        document(&pipeline.search, &new_name, &second, &renamed, Some(key))?;
        absent(&pipeline.search, &old_name, &MODES)?;
        absent(&pipeline.search, &first, &MODES)
    })
    .await?;

    // Reopen the same indices and state with new scanner/scheduler instances.
    // An edit while stopped must be replayed from the persisted USN checkpoint.
    pipeline.stop().await?;
    replace_contents(
        &renamed,
        &format!("{third} changed while the service was stopped with additional bytes\n"),
    )?;
    pipeline = Pipeline::start(&cfg)?;
    eventually("persisted journal restart and offline edit", || {
        pipeline.ensure_ready()?;
        document(&pipeline.search, &new_name, &third, &renamed, Some(key))?;
        document(
            &pipeline.search,
            &keep_name,
            &guard,
            &sentinel,
            Some(sentinel_key),
        )?;
        absent(&pipeline.search, &old_name, &MODES)?;
        absent(&pipeline.search, &first, &MODES)?;
        absent(&pipeline.search, &second, &MODES)
    })
    .await?;
    let resumed_state = read_state(&cfg)?;
    ensure!(
        resumed_state["generation"] == initial_state["generation"],
        "restart rebuilt compatible indices"
    );
    let resumed_volume = volume_state(&resumed_state, key.volume())?;
    ensure!(
        resumed_volume["guid"] == initial_volume["guid"],
        "restart rebound the stable volume identity"
    );
    ensure!(
        resumed_volume["cursor"]["journal_id"] == initial_volume["cursor"]["journal_id"],
        "journal identity unexpectedly changed on the isolated volume"
    );

    fs::remove_file(&renamed)?;
    eventually("journal delete from metadata and content", || {
        pipeline.ensure_ready()?;
        // A live sentinel prevents a hidden/unavailable volume from satisfying
        // every deletion assertion by returning empty results.
        document(
            &pipeline.search,
            &keep_name,
            &guard,
            &sentinel,
            Some(sentinel_key),
        )?;
        for term in [&old_name, &new_name, &first, &second, &third] {
            absent(&pipeline.search, term, &MODES)?;
        }
        Ok(())
    })
    .await?;
    let final_state = read_state(&cfg)?;
    let final_volume = volume_state(&final_state, key.volume())?;
    ensure!(
        final_volume["cursor"]["last_usn"].as_u64() > initial_volume["cursor"]["last_usn"].as_u64(),
        "durable journal checkpoint never advanced"
    );
    pipeline.stop().await?;
    let journal_after = query_journal(&volume)?;
    ensure!(
        journal_after.journal_id == journal_before.journal_id
            && journal_after.last_usn > journal_before.last_usn,
        "native journal did not record the lifecycle"
    );
    let evidence = root.join("data/log/native-ingestion-evidence.json");
    fs::write(
        &evidence,
        serde_json::to_vec_pretty(&serde_json::json!({
            "platform": "native Windows NTFS",
            "worker": worker,
            "document_key": key,
            "sentinel_key": sentinel_key,
            "journal_before": journal_before,
            "journal_after": journal_after,
            "initial_checkpoint": initial_volume,
            "final_checkpoint": final_volume,
            "verified": ["live create", "content replacement", "rename", "durable backpressure", "rescan replay", "restart with offline edit", "delete", "all search modes", "stable identity", "no duplicates"]
        }))?,
    )?;
    eprintln!(
        "Native NTFS service lifecycle passed; evidence: {}",
        evidence.display()
    );
    Ok(())
}
