use std::{
    ops::ControlFlow,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use core_types::config::AppConfig;
use ipc::VolumeStatus;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};

#[derive(Debug, Clone, Default)]
pub struct BootstrapOptions {
    /// If provided, seed the meta index with these file entries instead of discovering NTFS volumes.
    pub initial_metas: Option<Vec<core_types::FileMeta>>,
    /// Skip initial ingest entirely (used for tests that want a blank service).
    pub skip_initial_ingest: bool,
    /// Override IPC pipe name (default is \\\\.\\pipe\\ultrasearch).
    pub pipe_name: Option<String>,
    /// Force scheduler to run content jobs even if idle/load gates are active (tests).
    pub force_content_jobs: bool,
}

/// Completed initialization steps, not journal coverage or ingestion health.
/// The host may report Running only after EndpointReady. MFT reconciliation
/// and live journal catch-up remain background work with their own status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupStage {
    RuntimeReady,
    ConfigurationBound,
    IndexesReady,
    EndpointReady,
}

use crate::{
    init_tracing_with_config,
    meta_ingest::ingest_with_paths,
    metrics::{init_metrics_from_config, set_global_metrics},
    priority::apply_service_priorities,
    scanner::{initialize_indexes, watch_changes},
    scheduler_runtime::SchedulerRuntime,
    search_handler::set_search_handler,
    status_provider::{
        init_basic_status_provider, update_status_ingestion_state, update_status_last_commit,
        update_status_volumes,
    },
};

pub fn run_app(cfg: &AppConfig, shutdown_rx: mpsc::Receiver<()>) -> Result<()> {
    run_app_with_options(cfg, shutdown_rx, BootstrapOptions::default())
}

pub fn run_app_with_options(
    cfg: &AppConfig,
    shutdown_rx: mpsc::Receiver<()>,
    opts: BootstrapOptions,
) -> Result<()> {
    run_app_with_startup(cfg, shutdown_rx, opts, |_| Ok(ControlFlow::Continue(())))
}

/// Report actual initialization progress and allow the host to stop between
/// steps. Break means an explicit host shutdown, not successful readiness.
/// Reporting errors abort startup and retain the existing ownership cleanup.
pub fn run_app_with_startup(
    cfg: &AppConfig,
    mut shutdown_rx: mpsc::Receiver<()>,
    opts: BootstrapOptions,
    mut notify: impl FnMut(StartupStage) -> Result<ControlFlow<()>>,
) -> Result<()> {
    cfg.validate().context("validate service configuration")?;
    let _guard = init_tracing_with_config(&cfg.logging)
        .with_context(|| format!("initialize service log {}", cfg.logging.file))?;
    // Opt-in crash reports for the service host. The hook is process-wide and
    // idempotent, so a same-process restart does not stack hooks.
    core_types::install_opt_in_crash_hook("service");

    // The resident service answers interactive queries and advances journals.
    // Only extraction workers enter idle/background scheduling. Apply before
    // spawning runtime threads, and report OS failures instead of hiding them.
    apply_service_priorities().context("initialize service process priorities")?;

    // Initialize Tokio runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create service runtime")?;
    let _rt_guard = rt.enter();
    if notify(StartupStage::RuntimeReady)?.is_break() {
        return Ok(());
    }

    // Install status provider so IPC/status can respond.
    init_basic_status_provider();

    if cfg.metrics.enabled {
        let metrics = Arc::new(init_metrics_from_config(&cfg.metrics)?);
        set_global_metrics(metrics.clone());
        if let Err(e) = crate::metrics::start_metrics_http_server(&cfg.metrics.bind, metrics) {
            tracing::warn!(
                bind = %cfg.metrics.bind,
                "failed to start Prometheus /metrics HTTP listener: {e}"
            );
        }
    }

    let mut pending_jobs = Vec::new();

    // Empty scopes mean automatic discovery on every watcher turn, not a
    // one-time snapshot of mounted drive letters. Startup must not freeze that
    // policy or serialize this expanded (possibly custom-source) configuration
    // over the unrelated default configuration file.
    let cfg_owned = cfg.clone();
    ensure_data_paths_exist(&cfg_owned)?;
    core_types::config::set_current_config(cfg_owned.clone())
        .context("publish active service configuration")?;
    if notify(StartupStage::ConfigurationBound)?.is_break() {
        return Ok(());
    }
    let ingestion = initialize_indexes(&cfg_owned).with_context(|| {
        format!(
            "initialize indexes (metadata={}, content={}, state={})",
            cfg_owned.paths.meta_index, cfg_owned.paths.content_index, cfg_owned.paths.state_dir,
        )
    })?;
    if notify(StartupStage::IndexesReady)?.is_break() {
        return Ok(());
    }

    let use_journal = opts.initial_metas.is_none() && !opts.skip_initial_ingest;
    match opts.initial_metas {
        Some(metas) => ingest_seed_metadata(&cfg_owned, metas, &mut pending_jobs)?,
        None if opts.skip_initial_ingest => {
            tracing::info!("skip_initial_ingest=true; leaving indices empty");
        }
        None => tracing::info!(
            "MFT reconciliation and journal replay will run on the durable ingestion lane"
        ),
    }

    // Start scheduler loop
    // We need to clone cfg for the scheduler (or pass reference if new() takes ref).
    // SchedulerRuntime::new takes &AppConfig.
    let mut scheduler = SchedulerRuntime::new(&cfg_owned);
    if opts.force_content_jobs {
        scheduler.force_allow_content();
    }
    if !pending_jobs.is_empty() {
        tracing::info!(
            "Seeding {} content jobs into scheduler queue",
            pending_jobs.len()
        );
        scheduler
            .submit_content_jobs(pending_jobs)
            .map_err(|rejected| {
                anyhow::anyhow!(
                    "seed content queue limit exceeded; {} jobs were not admitted",
                    rejected.len()
                )
            })?;
    }
    // Try to install unified search handler.
    // We pass both meta and content index paths.
    let meta_path = Path::new(&cfg_owned.paths.meta_index);
    let content_path = Path::new(&cfg_owned.paths.content_index);

    // Schema migration was completed before starting writers. Do not rename an
    // index out from under an active worker based on an error-string heuristic.
    let handler = crate::search_handler::UnifiedSearchHandler::try_new(meta_path, content_path)
        .with_context(|| {
            format!(
                "open search readers (metadata={}, content={})",
                meta_path.display(),
                content_path.display(),
            )
        })?;
    set_search_handler(Box::new(handler));

    let mut tasks = ServiceTasks::default();
    #[cfg(target_os = "windows")]
    {
        tasks.listener =
            Some(rt.block_on(crate::ipc::start_pipe_server(opts.pipe_name.as_deref()))?);
    }

    // The endpoint is owned and index readers are installed. Signal this
    // before launching writers so a stop latched during initialization cannot
    // start a new ingestion lane. ServiceTasks owns the listener on error/break.
    if notify(StartupStage::EndpointReady)?.is_break() {
        return Ok(());
    }

    // Readers and the endpoint are ready before writers can run. A failed pipe
    // bind returns an error, drops this ingestion session, and never leaves a
    // second invisible scanner/worker running behind another service's pipe.
    tasks.scheduler = Some(rt.spawn(scheduler.run_loop()));
    // Test seeds explicitly opt out of native discovery; production never
    // substitutes a successful no-op watcher.
    if use_journal {
        let cfg_clone = cfg_owned.clone();
        tasks.watcher = Some(rt.spawn(watch_changes(cfg_clone, ingestion)));
    } else {
        update_status_ingestion_state("native ingestion disabled by test bootstrap options");
    }

    tracing::info!("UltraSearch service started; supervising scheduler, journal and IPC tasks");
    // A returned error reaches the console caller or the SCM host's existing
    // nonzero exit reporting. Never keep a query endpoint alive indefinitely
    // after its ingestion/scheduling lane has vanished.
    rt.block_on(tasks.supervise(&mut shutdown_rx, update_status_ingestion_state))
}

/// Retain ownership of every essential loop. Dropping a bare JoinHandle would
/// detach it, including on a bootstrap error or supervisor cancellation.
#[derive(Default)]
struct ServiceTasks {
    scheduler: Option<JoinHandle<()>>,
    watcher: Option<JoinHandle<Result<()>>>,
    listener: Option<JoinHandle<()>>,
}

impl ServiceTasks {
    fn abort_all(&self) {
        if let Some(task) = &self.scheduler {
            task.abort();
        }
        if let Some(task) = &self.watcher {
            task.abort();
        }
        if let Some(task) = &self.listener {
            task.abort();
        }
    }

    async fn supervise(
        mut self,
        shutdown: &mut mpsc::Receiver<()>,
        mut report: impl FnMut(String),
    ) -> Result<()> {
        // Optional loops stay pending rather than producing a false exit in
        // non-Windows/test bootstrap. The scheduler is never optional.
        anyhow::ensure!(
            self.scheduler.is_some(),
            "service scheduler was not started"
        );
        let mut outcome = tokio::select! {
            // A known failure must not be hidden by a simultaneously queued
            // stop signal. Unexpected success is also a failure for these loops.
            biased;
            result = wait_task(&mut self.scheduler) => {
                let _ = self.scheduler.take();
                Err(unexpected_exit("scheduler", result.map(Ok)))
            }
            result = wait_task(&mut self.watcher) => {
                let _ = self.watcher.take();
                Err(unexpected_exit("journal watcher", result))
            }
            result = wait_task(&mut self.listener) => {
                let _ = self.listener.take();
                Err(unexpected_exit("IPC listener", result.map(Ok)))
            }
            signal = shutdown.recv() => {
                tracing::info!(explicit = signal.is_some(), "service shutdown requested");
                Ok(())
            }
        };
        if let Err(error) = &outcome {
            report(format!("stopped: {error:#}"));
            tracing::error!(%error, "essential service task stopped; stopping sibling tasks");
        } else {
            report("stopping: shutdown requested".into());
        }

        // Stop every sibling before joining any one of them. Joining observes
        // destruction of the watcher/session (hiding its volumes) and scheduler
        // queues (closing unacknowledged submissions), not a fabricated commit.
        self.abort_all();
        let stopped = [
            reap_task("scheduler", &mut self.scheduler, Ok).await,
            reap_task("journal watcher", &mut self.watcher, std::convert::identity).await,
            reap_task("IPC listener", &mut self.listener, Ok).await,
        ];
        for result in stopped {
            if let Err(error) = result {
                tracing::error!(%error, "essential task also failed during shutdown");
                if outcome.is_ok() {
                    report(format!("stopped: {error:#}"));
                    outcome = Err(error);
                }
            }
        }
        if outcome.is_ok() {
            report("stopped: shutdown requested".into());
        }
        // Abort cannot preempt synchronous native calls or spawn_blocking.
        // Existing blocking writers retain their own worker permit and file
        // lease, and runtime destruction still waits for them. Do not detach
        // that ownership or weaken the SCM's existing stop-grace policy here.
        outcome
    }
}

impl Drop for ServiceTasks {
    fn drop(&mut self) {
        self.abort_all();
    }
}

async fn wait_task<T>(task: &mut Option<JoinHandle<T>>) -> std::result::Result<T, JoinError> {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

fn unexpected_exit(
    name: &str,
    outcome: std::result::Result<Result<()>, JoinError>,
) -> anyhow::Error {
    match outcome {
        Ok(Ok(())) => anyhow::anyhow!("essential {name} task returned unexpectedly"),
        Ok(Err(error)) => error.context(format!("essential {name} task failed")),
        Err(error) => {
            anyhow::Error::new(error).context(format!("essential {name} task terminated"))
        }
    }
}

async fn reap_task<T>(
    name: &str,
    task: &mut Option<JoinHandle<T>>,
    result: impl FnOnce(T) -> Result<()>,
) -> Result<()> {
    let Some(handle) = task.as_mut() else {
        return Ok(());
    };
    let outcome = handle.await;
    // Keep the handle in its abort-on-drop owner until the await completes,
    // but never poll an already-completed handle twice.
    let _ = task.take();
    match outcome {
        Err(error) if error.is_cancelled() => Ok(()),
        outcome => Err(unexpected_exit(name, outcome.map(result))),
    }
}

/// Make sure all configured data paths exist so worker processes don’t fail with ENOENT.
fn ensure_data_paths_exist(cfg: &AppConfig) -> Result<()> {
    use std::fs;
    let paths = [
        &cfg.paths.meta_index,
        &cfg.paths.content_index,
        &cfg.paths.state_dir,
        &cfg.paths.jobs_dir,
    ];

    for p in paths {
        let path = std::path::Path::new(p);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create service data parent {}", parent.display()))?;
        }
        // If path itself is meant to be a directory (indexes/dirs), create it too.
        fs::create_dir_all(path)
            .with_context(|| format!("create service data directory {}", path.display()))?;
    }
    Ok(())
}

fn ingest_seed_metadata(
    cfg: &AppConfig,
    metas: Vec<core_types::FileMeta>,
    pending_jobs: &mut Vec<crate::dispatcher::job_dispatch::JobSpec>,
) -> Result<()> {
    if metas.is_empty() {
        tracing::info!("seed metadata list empty; skipping ingest");
        return Ok(());
    }

    ingest_with_paths(&cfg.paths, metas.clone(), None)?;

    let mut by_vol: std::collections::HashMap<core_types::VolumeId, (u64, u64)> =
        std::collections::HashMap::new();
    for meta in &metas {
        let entry = by_vol.entry(meta.volume).or_insert((0, 0));
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(meta.size);
    }

    let mut status = Vec::with_capacity(by_vol.len());
    for (vol, (count, bytes)) in by_vol {
        status.push(VolumeStatus {
            volume: vol,
            indexed_files: count,
            indexed_bytes: bytes,
            pending_files: 0,
            pending_bytes: 0,
            last_usn: None,
            journal_id: None,
        });
    }

    // Seed content jobs for any provided seed files (if they have paths).
    for meta in metas {
        if let Some(job) = crate::scheduler_runtime::content_job_from_meta(&meta, &cfg.extract) {
            pending_jobs.push(job);
        }
    }

    update_status_last_commit(Some(unix_timestamp_secs()));
    update_status_volumes(status);
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
    use super::*;
    use anyhow::{Context, ensure};
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    struct TaskDrop(Arc<AtomicBool>);

    impl Drop for TaskDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn guarded_task<T: Send + 'static>(
        finish: bool,
        outcome: T,
    ) -> (JoinHandle<T>, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        // Capture the guard before spawn: cancellation before the first poll
        // must also destroy the owned state rather than detach it.
        let guard = TaskDrop(Arc::clone(&dropped));
        let task = tokio::spawn(async move {
            let _guard = guard;
            if !finish {
                std::future::pending::<()>().await;
            }
            outcome
        });
        (task, dropped)
    }

    #[tokio::test]
    async fn each_unexpected_success_stops_and_joins_the_other_essential_loops() {
        for name in ["scheduler", "journal watcher", "IPC listener"] {
            let (scheduler, scheduler_dropped) = guarded_task(name == "scheduler", ());
            let (watcher, watcher_dropped) = guarded_task(name == "journal watcher", Ok(()));
            let (listener, listener_dropped) = guarded_task(name == "IPC listener", ());
            let tasks = ServiceTasks {
                scheduler: Some(scheduler),
                watcher: Some(watcher),
                listener: Some(listener),
            };
            let (_shutdown, mut receiver) = mpsc::channel(1);
            let mut states = Vec::new();
            let error = tokio::time::timeout(
                Duration::from_secs(5),
                tasks.supervise(&mut receiver, |state| states.push(state)),
            )
            .await
            .expect("a terminated essential task must not wait for a stop command")
            .expect_err("an infinite service loop cannot successfully finish on its own");
            assert!(error.to_string().contains(name));
            assert!(error.to_string().contains("returned unexpectedly"));
            assert_eq!(states.len(), 1);
            assert!(states[0].starts_with("stopped:"));
            for dropped in [scheduler_dropped, watcher_dropped, listener_dropped] {
                assert!(
                    dropped.load(Ordering::SeqCst),
                    "sibling state was not destroyed"
                );
            }
        }
    }

    #[tokio::test]
    async fn watcher_error_preserves_its_cause_and_cancels_scheduler_and_listener() {
        let (scheduler, scheduler_dropped) = guarded_task(false, ());
        let (listener, listener_dropped) = guarded_task(false, ());
        let tasks = ServiceTasks {
            scheduler: Some(scheduler),
            watcher: Some(tokio::spawn(async {
                Err(anyhow::anyhow!("fixture disk full").context("save journal checkpoint"))
            })),
            listener: Some(listener),
        };
        let (_shutdown, mut receiver) = mpsc::channel(1);
        let mut states = Vec::new();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            tasks.supervise(&mut receiver, |state| states.push(state)),
        )
        .await
        .unwrap()
        .unwrap_err();
        let message = format!("{error:#}");
        for expected in [
            "journal watcher",
            "save journal checkpoint",
            "fixture disk full",
        ] {
            assert!(message.contains(expected), "{message}");
            assert!(states[0].contains(expected));
        }
        assert!(scheduler_dropped.load(Ordering::SeqCst));
        assert!(listener_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unexpected_panic_and_external_cancellation_are_service_errors() {
        for cancel in [false, true] {
            let (watcher, watcher_dropped) = guarded_task(false, Ok(()));
            let scheduler = if cancel {
                let (task, _) = guarded_task(false, ());
                task.abort();
                task
            } else {
                tokio::spawn(async { panic!("fixture scheduler panic") })
            };
            let tasks = ServiceTasks {
                scheduler: Some(scheduler),
                watcher: Some(watcher),
                listener: None,
            };
            let (_shutdown, mut receiver) = mpsc::channel(1);
            let error = tokio::time::timeout(
                Duration::from_secs(5),
                tasks.supervise(&mut receiver, |_| {}),
            )
            .await
            .unwrap()
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("scheduler"));
            assert!(message.contains(if cancel {
                "cancel"
            } else {
                "fixture scheduler panic"
            }));
            assert!(watcher_dropped.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn requested_shutdown_and_sender_loss_reap_tasks_with_optional_loops_absent() {
        for explicit in [false, true] {
            for native_loops in [false, true] {
                let (scheduler, scheduler_dropped) = guarded_task(false, ());
                let (watcher, watcher_dropped) = if native_loops {
                    let (task, dropped) = guarded_task(false, Ok(()));
                    (Some(task), Some(dropped))
                } else {
                    (None, None)
                };
                let (listener, listener_dropped) = if native_loops {
                    let (task, dropped) = guarded_task(false, ());
                    (Some(task), Some(dropped))
                } else {
                    (None, None)
                };
                let tasks = ServiceTasks {
                    scheduler: Some(scheduler),
                    watcher,
                    listener,
                };
                let (shutdown, mut receiver) = mpsc::channel(1);
                if explicit {
                    shutdown.try_send(()).unwrap();
                }
                drop(shutdown);
                let mut states = Vec::new();
                tokio::time::timeout(
                    Duration::from_secs(5),
                    tasks.supervise(&mut receiver, |state| states.push(state)),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(
                    states,
                    [
                        "stopping: shutdown requested",
                        "stopped: shutdown requested"
                    ]
                );
                assert!(scheduler_dropped.load(Ordering::SeqCst));
                for dropped in [watcher_dropped, listener_dropped].into_iter().flatten() {
                    assert!(dropped.load(Ordering::SeqCst));
                }
            }
        }
    }

    #[tokio::test]
    async fn ready_failure_is_not_overwritten_by_simultaneous_shutdown() {
        let (scheduler, scheduler_dropped) = guarded_task(false, ());
        let watcher = tokio::spawn(async { anyhow::bail!("fixture fatal journal error") });
        // Arrange readiness without consuming the JoinHandle result. The stop
        // signal and the fatal result must both be ready in the production select.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !watcher.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let tasks = ServiceTasks {
            scheduler: Some(scheduler),
            watcher: Some(watcher),
            listener: None,
        };
        let (shutdown, mut receiver) = mpsc::channel(1);
        shutdown.try_send(()).unwrap();
        let mut states = Vec::new();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            tasks.supervise(&mut receiver, |state| states.push(state)),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(format!("{error:#}").contains("fixture fatal journal error"));
        assert_eq!(states.len(), 1);
        assert!(states[0].contains("journal watcher"));
        assert!(scheduler_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelled_supervisor_aborts_owned_tasks_without_acknowledging_work() -> Result<()> {
        use std::future::Future;
        use std::task::{Context as TaskContext, Waker};
        use tokio::sync::oneshot;

        let root = tempfile::tempdir()?;
        let path = root.path().join("supervisor.lock");
        let lease = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        lease.try_lock()?;
        let contender = fs::OpenOptions::new().read(true).write(true).open(&path)?;
        let (acknowledgement, mut response) = oneshot::channel::<()>();
        let scheduler = tokio::spawn(async move {
            std::future::pending::<()>().await;
            let _ = acknowledgement.send(());
        });
        let watcher = tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(lease);
            Ok(())
        });
        let tasks = ServiceTasks {
            scheduler: Some(scheduler),
            watcher: Some(watcher),
            listener: None,
        };
        let (_shutdown, mut receiver) = mpsc::channel(1);
        let mut supervisor = Box::pin(tasks.supervise(&mut receiver, |_| {}));
        let mut context = TaskContext::from_waker(Waker::noop());
        assert!(supervisor.as_mut().poll(&mut context).is_pending());
        assert!(contender.try_lock().is_err());
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        drop(supervisor);
        // Dropping the supervisor requests cancellation even without a normal
        // shutdown path. Await actual destruction, not a timer-based guess.
        assert!(
            tokio::time::timeout(Duration::from_secs(5), &mut response)
                .await?
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if contender.try_lock().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn failed_service_keeps_detached_blocking_writer_lease_until_writer_finishes()
    -> Result<()> {
        use tokio::sync::oneshot;

        let root = tempfile::tempdir()?;
        let path = root.path().join("blocking-writer.lock");
        let lease = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        lease
            .try_lock()
            .context("acquire blocking writer fixture lease")?;
        let contender = fs::OpenOptions::new().read(true).write(true).open(&path)?;
        let (release, released) = std::sync::mpsc::channel();
        let (started, running) = oneshot::channel();
        let (finished, completion) = oneshot::channel();
        let writer = tokio::task::spawn_blocking(move || {
            let owned_lease = lease;
            let _ = started.send(());
            // A failed assertion drops release, so the reader cannot strand a
            // blocking thread. This is a real lock owner, not a worker ACK mock.
            let result = released.recv_timeout(Duration::from_secs(5));
            drop(owned_lease);
            let _ = finished.send(result.is_ok());
        });
        tokio::time::timeout(Duration::from_secs(5), running).await??;
        let (acknowledgement, response) = oneshot::channel();
        let scheduler = tokio::spawn(async move {
            let _ = writer.await;
            let _ = acknowledgement.send(());
        });
        let tasks = ServiceTasks {
            scheduler: Some(scheduler),
            watcher: Some(tokio::spawn(async {
                anyhow::bail!("fixture watcher stopped")
            })),
            listener: None,
        };
        let (_shutdown, mut receiver) = mpsc::channel(1);
        let failure = tokio::time::timeout(
            Duration::from_secs(5),
            tasks.supervise(&mut receiver, |_| {}),
        )
        .await?;
        let still_locked = contender.try_lock().is_err();
        let unacknowledged = tokio::time::timeout(Duration::from_secs(5), response)
            .await?
            .is_err();
        release.send(())?;
        let writer_released = tokio::time::timeout(Duration::from_secs(5), completion).await??;
        assert!(format!("{:#}", failure.unwrap_err()).contains("fixture watcher stopped"));
        assert!(
            still_locked,
            "aborting the async owner released a still-running writer's lease"
        );
        assert!(
            unacknowledged,
            "scheduler cancellation manufactured a commit ACK"
        );
        assert!(
            writer_released,
            "fixture writer timed out instead of receiving its release"
        );
        // Parallel subprocess tests can briefly inherit this descriptor between
        // fork and exec. Completion proves the writer dropped its lease; still
        // require actual OS lock ownership within a bounded deadline.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if contender.try_lock().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("reacquire fixture lease after the blocking writer dropped it")?;
        Ok(())
    }

    struct ChildGuard(Child);

    #[test]
    fn startup_readiness_requires_real_readers_and_endpoint_and_allows_cancellation() -> Result<()>
    {
        const ROOT: &str = "ULTRASEARCH_STARTUP_READY_TEST_ROOT";
        const MODE: &str = "ULTRASEARCH_STARTUP_READY_TEST_MODE";
        const PROOF: &[u8] = b"startup-readiness-assertions-executed";
        if let Some(root) = std::env::var_os(ROOT) {
            let root = PathBuf::from(root);
            let mode = std::env::var(MODE)?;
            let cfg = core_types::config::load_or_create_config(Some(&root.join("config.toml")))?;
            let pipe_name = format!(r"\\.\pipe\ultrasearch-ready-{}", uuid::Uuid::new_v4());
            let seeded = mode == "seeded-search";
            let initial_metas = if seeded {
                let path = root.join("startupreadiness.txt");
                fs::write(&path, b"readiness search fixture")?;
                Some(vec![core_types::FileMeta::new(
                    core_types::DocKey::from_parts(1, 42),
                    1,
                    None,
                    "startupreadiness.txt".into(),
                    Some(path.to_str().context("non-UTF-8 fixture path")?.into()),
                    fs::metadata(&path)?.len(),
                    0,
                    0,
                    core_types::FileFlags::empty(),
                )])
            } else {
                None
            };
            let (shutdown, receiver) = mpsc::channel(1);
            shutdown.try_send(())?;
            let mut stages = Vec::new();
            let result = run_app_with_startup(
                &cfg,
                receiver,
                BootstrapOptions {
                    initial_metas,
                    // Cancel production ingestion at the endpoint before any
                    // native volume enumeration or writer can start.
                    skip_initial_ingest: mode != "before-ingest",
                    pipe_name: Some(pipe_name.clone()),
                    ..BootstrapOptions::default()
                },
                |stage| {
                    stages.push(stage);
                    if stage == StartupStage::RuntimeReady {
                        if mode == "cancel" {
                            return Ok(ControlFlow::Break(()));
                        }
                        if mode == "report-error" {
                            anyhow::bail!("fixture startup reporter failed");
                        }
                    }
                    if stage == StartupStage::EndpointReady {
                        // Check actual committed resources, not just the order
                        // of callbacks. A unique native pipe cannot be an older
                        // service instance answering on the default endpoint.
                        tantivy::Index::open_in_dir(&cfg.paths.meta_index)?;
                        tantivy::Index::open_in_dir(&cfg.paths.content_index)?;
                        if mode == "before-ingest" {
                            let state: serde_json::Value = serde_json::from_slice(&fs::read(
                                Path::new(&cfg.paths.state_dir).join("ingestion-v2.json"),
                            )?)?;
                            ensure!(state["volumes"].as_array().is_some_and(Vec::is_empty));
                            ensure!(state["pending"].is_null());
                            ensure!(crate::status_snapshot().last_index_commit_ts.is_none());
                        }
                        let request = ipc::SearchRequest {
                            id: uuid::Uuid::new_v4(),
                            query: ipc::QueryExpr::Term(ipc::TermExpr {
                                field: Some(ipc::FieldKind::Name),
                                value: "startupreadiness".into(),
                                modifier: ipc::TermModifier::Term,
                            }),
                            limit: 10,
                            mode: ipc::SearchMode::NameOnly,
                            timeout: Some(Duration::from_secs(2)),
                            offset: 0,
                        };
                        let response = crate::search(request.clone());
                        response.validate_for(request.id)?;
                        ensure!(response.total == u64::from(seeded));
                        ensure!(response.hits.len() == usize::from(seeded));
                        #[cfg(windows)]
                        {
                            let pipe_name = pipe_name.clone();
                            std::thread::spawn(move || -> Result<()> {
                                tokio::runtime::Builder::new_current_thread()
                                    .enable_all()
                                    .build()?
                                    .block_on(async {
                                        let client = ipc::client::PipeClient::new(pipe_name)
                                            .with_request_timeout(Duration::from_secs(5));
                                        let status = client
                                            .status(ipc::StatusRequest {
                                                id: uuid::Uuid::new_v4(),
                                            })
                                            .await?;
                                        ensure!(status.last_index_commit_ts.is_some() == seeded);
                                        let response = client.search(request).await?;
                                        ensure!(response.total == u64::from(seeded));
                                        ensure!(response.hits.len() == usize::from(seeded));
                                        Ok(())
                                    })
                            })
                            .join()
                            .map_err(|_| anyhow::anyhow!("readiness client thread panicked"))??;
                        }
                        if mode == "endpoint-error" {
                            anyhow::bail!("fixture endpoint acknowledgement failed");
                        }
                        if mode == "before-ingest" || seeded {
                            return Ok(ControlFlow::Break(()));
                        }
                    }
                    Ok(ControlFlow::Continue(()))
                },
            );
            match mode.as_str() {
                "success" | "endpoint-error" | "before-ingest" | "seeded-search" => {
                    ensure!(
                        stages
                            == [
                                StartupStage::RuntimeReady,
                                StartupStage::ConfigurationBound,
                                StartupStage::IndexesReady,
                                StartupStage::EndpointReady,
                            ]
                    );
                    if mode != "endpoint-error" {
                        result?;
                    } else {
                        ensure!(
                            format!("{:#}", result.unwrap_err())
                                .contains("endpoint acknowledgement failed")
                        );
                    }
                }
                "cancel" | "report-error" | "path-error" => {
                    ensure!(stages == [StartupStage::RuntimeReady]);
                    if mode == "cancel" {
                        result?;
                    } else {
                        let error = result.expect_err("startup failure must not report success");
                        if mode == "report-error" {
                            ensure!(format!("{error:#}").contains("startup reporter failed"));
                        } else if mode == "path-error" {
                            let diagnostic = format!("{error:#}");
                            ensure!(diagnostic.contains("create service data directory"));
                            ensure!(diagnostic.contains(&cfg.paths.meta_index));
                        }
                    }
                    if mode != "path-error" {
                        ensure!(!Path::new(&cfg.paths.meta_index).exists());
                    }
                }
                _ => anyhow::bail!("unknown readiness fixture mode"),
            }
            ensure!(!crate::scheduler_runtime::content_admission_available());
            fs::write(root.join("proof"), PROOF)?;
            return Ok(());
        }

        // Each child owns its tracing/configuration/runtime/native-priority
        // state. A proof file prevents an empty --exact selection from passing.
        for mode in [
            "success",
            "path-error",
            "cancel",
            "report-error",
            "endpoint-error",
            "before-ingest",
            "seeded-search",
        ] {
            let root = tempfile::tempdir()?.keep();
            let mut cfg = AppConfig::default();
            cfg.app.data_dir = root
                .join("data")
                .to_str()
                .context("non-UTF-8 fixture")?
                .into();
            if mode == "path-error" {
                let path = root.join("not-an-index-directory");
                fs::write(&path, b"retain this failing startup fixture")?;
                cfg.paths.meta_index = path.to_str().context("non-UTF-8 fixture")?.into();
            }
            fs::write(root.join("config.toml"), toml::to_string_pretty(&cfg)?)?;
            let name = concat!(
                module_path!(),
                "::startup_readiness_requires_real_readers_and_endpoint_and_allows_cancellation"
            );
            let name = name
                .split_once("::")
                .context("test crate prefix missing")?
                .1;
            let mut child = ChildGuard(
                Command::new(std::env::current_exe()?)
                    .args(["--exact", name, "--nocapture", "--test-threads=1"])
                    .env(ROOT, &root)
                    .env(MODE, mode)
                    .spawn()?,
            );
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if let Some(status) = child.0.try_wait()? {
                    ensure!(status.success(), "readiness case {mode} failed: {status}");
                    ensure!(
                        fs::read(root.join("proof"))? == PROOF,
                        "readiness assertions did not execute"
                    );
                    break;
                }
                ensure!(Instant::now() < deadline, "readiness case {mode} timed out");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    #[test]
    fn startup_preserves_automatic_scope_and_the_custom_configuration_source() -> Result<()> {
        const CHILD_ROOT: &str = "ULTRASEARCH_BOOTSTRAP_CONFIG_TEST_ROOT";
        const PROOF: &[u8] = b"automatic-scope-and-custom-source-preserved";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let source = root.join("custom.toml");
            let default_path = core_types::config::default_config_path();
            let source_before = fs::read(&source)?;
            let default_before = fs::read(&default_path)?;
            let cfg = core_types::config::load_or_create_config(Some(&source))?;
            ensure!(cfg.volumes.is_empty() && cfg.content_index_volumes.is_empty());
            let (shutdown, receiver) = mpsc::channel(1);
            shutdown.try_send(())?;
            // Exercise real bootstrap, readers, state and (on Windows) the
            // named pipe without mutating or scanning the host's filesystems.
            // Native discovery belongs exclusively to watch_changes, not this
            // explicit disabled-ingestion bootstrap.
            run_app_with_options(
                &cfg,
                receiver,
                BootstrapOptions {
                    skip_initial_ingest: true,
                    pipe_name: Some(format!(
                        r"\\.\pipe\ultrasearch-bootstrap-{}",
                        uuid::Uuid::new_v4()
                    )),
                    ..BootstrapOptions::default()
                },
            )?;
            let active = core_types::config::get_current_config();
            ensure!(
                active.volumes.is_empty(),
                "startup froze automatic metadata scope"
            );
            ensure!(
                active.content_index_volumes.is_empty(),
                "startup froze automatic content scope"
            );
            ensure!(
                fs::read(&source)? == source_before,
                "startup rewrote its custom source"
            );
            ensure!(
                fs::read(&default_path)? == default_before,
                "startup overwrote the default configuration"
            );
            // The default file is deliberately invalid. Reload(None) must use
            // the custom source retained by the production configuration API.
            let mut next = cfg.clone();
            next.extract.max_chars_per_file += 1;
            fs::write(&source, toml::to_string_pretty(&next)?)?;
            let reloaded = core_types::config::reload_config(None)?;
            ensure!(reloaded.extract.max_chars_per_file == next.extract.max_chars_per_file);
            ensure!(reloaded.volumes.is_empty() && reloaded.content_index_volumes.is_empty());
            fs::write(root.join("proof.txt"), PROOF)?;
            return Ok(());
        }

        // Isolate tracing, process priorities, global readers/configuration and
        // PROGRAMDATA from the parent running the rest of the test suite.
        let root = tempfile::tempdir()?.keep();
        let program_data = root.join("system-data");
        let default_path = program_data.join("UltraSearch/config/config.toml");
        fs::create_dir_all(
            default_path
                .parent()
                .context("default config parent missing")?,
        )?;
        fs::write(
            &default_path,
            b"unrelated configuration: preserve these exact bytes",
        )?;
        let mut cfg = AppConfig::default();
        cfg.app.data_dir = root
            .join("test-data")
            .to_str()
            .context("fixture path is not UTF-8")?
            .into();
        fs::write(root.join("custom.toml"), toml::to_string_pretty(&cfg)?)?;
        let name = concat!(
            module_path!(),
            "::startup_preserves_automatic_scope_and_the_custom_configuration_source"
        );
        let name = name
            .split_once("::")
            .context("test crate prefix missing")?
            .1;
        let mut child = ChildGuard(
            Command::new(std::env::current_exe()?)
                .args(["--exact", name, "--nocapture", "--test-threads=1"])
                .env(CHILD_ROOT, &root)
                .env("PROGRAMDATA", &program_data)
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.0.try_wait()? {
                ensure!(
                    status.success(),
                    "bootstrap regression child failed: {status}"
                );
                ensure!(
                    fs::read(root.join("proof.txt"))? == PROOF,
                    "bootstrap assertions did not execute"
                );
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "bootstrap regression child timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
