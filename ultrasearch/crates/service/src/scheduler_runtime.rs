use crate::dispatcher::job_dispatch::{IndexBatch, JobDispatcher, JobOperation, JobSpec};
use crate::scanner;
use crate::status_provider::{
    increment_content_plan, update_content_remaining, update_status_metrics,
    update_status_queue_state, update_status_scheduler_state,
};
use core_types::config::{AppConfig, ExtractSection};
use core_types::{FileFlags, FileMeta};
use parking_lot::Mutex;
use scheduler::{
    SchedulerConfig, allow_content_jobs,
    idle::{IdleSample, IdleTracker},
    metrics::{SystemLoad, SystemLoadSampler},
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Default)]
struct SchedulerLiveState {
    critical: AtomicUsize,
    metadata: AtomicUsize,
    content: AtomicUsize,
    active_workers: AtomicU32,
    dropped_content: AtomicUsize,
    enqueued_content: AtomicUsize,
}

static LIVE_STATE: OnceLock<SchedulerLiveState> = OnceLock::new();
static JOB_SENDER: OnceLock<Mutex<Option<ProducerChannel>>> = OnceLock::new();

const MAX_CONTENT_QUEUE: usize = 100_000;
const MAX_PENDING_SUBMISSIONS: usize = 256;
const MAX_INDEX_BATCH_JOBS: usize = 4096;

struct ProducerChannel {
    runtime_id: uuid::Uuid,
    sender: mpsc::Sender<Submission>,
    allow_content: Arc<AtomicBool>,
}

enum Submission {
    Content(JobSpec),
    IndexBatch(PendingIndexBatch),
}

struct PendingIndexBatch {
    batch: IndexBatch,
    mutation_lease: Option<Arc<std::fs::File>>,
    acknowledgement: oneshot::Sender<Result<(), String>>,
}

fn submission_sender() -> Option<mpsc::Sender<Submission>> {
    JOB_SENDER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .as_ref()
        .map(|channel| channel.sender.clone())
}

/// Opportunistic retries must not be placed ahead of fresh journal work while
/// extraction is paused. This is an admission hint, never an acknowledgement;
/// the runtime still applies its current policy before launching a worker.
pub(crate) fn content_admission_available() -> bool {
    JOB_SENDER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .as_ref()
        .is_some_and(|channel| {
            !channel.sender.is_closed() && channel.allow_content.load(Ordering::Relaxed)
        })
}

fn requires_content(batch: &IndexBatch) -> bool {
    batch
        .jobs
        .iter()
        .any(|job| job.operation != JobOperation::Delete)
}

/// Runtime wrapper that drives a simple scheduling loop and dispatches content batches.
pub struct SchedulerRuntime {
    config: SchedulerConfig,
    idle: IdleTracker,
    load: SystemLoadSampler,
    content_jobs: VecDeque<JobSpec>,
    job_rx: mpsc::Receiver<Submission>,
    pending_index: Option<PendingIndexBatch>,
    legacy_retry: Option<IndexBatch>,
    runtime_id: uuid::Uuid,
    dispatcher: JobDispatcher,
    live: &'static SchedulerLiveState,
    current_volumes: Vec<String>,
    force_allow_content: bool,
    allow_content: Arc<AtomicBool>,
}

impl SchedulerRuntime {
    pub fn new(app_cfg: &AppConfig) -> Self {
        let config = SchedulerConfig {
            warm_idle: Duration::from_secs(app_cfg.scheduler.idle_warm_seconds),
            deep_idle: Duration::from_secs(app_cfg.scheduler.idle_deep_seconds),
            cpu_metadata_max: app_cfg.scheduler.cpu_soft_limit_pct as f32,
            cpu_content_max: app_cfg.scheduler.cpu_hard_limit_pct as f32,
            disk_busy_threshold_bps: app_cfg.scheduler.disk_busy_bytes_per_s,
            content_batch_size: app_cfg.scheduler.content_batch_size as usize,
            power_save_mode: app_cfg.scheduler.power_save_mode,
            ..SchedulerConfig::default()
        };

        let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);
        let (tx, rx) = mpsc::channel(MAX_PENDING_SUBMISSIONS);
        let runtime_id = uuid::Uuid::new_v4();
        let allow_content = Arc::new(AtomicBool::new(false));
        // Replace a closed runtime's sender on restart. A OnceLock containing
        // the sender itself would permanently retain the first closed channel.
        *JOB_SENDER.get_or_init(|| Mutex::new(None)).lock() = Some(ProducerChannel {
            runtime_id,
            sender: tx,
            allow_content: Arc::clone(&allow_content),
        });

        Self {
            idle: IdleTracker::new(config.warm_idle, config.deep_idle),
            load: SystemLoadSampler::new(config.disk_busy_threshold_bps),
            content_jobs: VecDeque::new(),
            job_rx: rx,
            pending_index: None,
            legacy_retry: None,
            runtime_id,
            dispatcher: JobDispatcher::new(app_cfg),
            config,
            live,
            current_volumes: app_cfg.volumes.clone(),
            force_allow_content: false,
            allow_content,
        }
    }

    fn update_config(&mut self, app_cfg: &AppConfig) {
        // Check for volume changes
        if self.current_volumes != app_cfg.volumes {
            tracing::info!("Volume configuration changed, triggering rescan...");
            self.current_volumes = app_cfg.volumes.clone();
            scanner::request_rescan();
        }

        self.config.warm_idle = Duration::from_secs(app_cfg.scheduler.idle_warm_seconds);
        self.config.deep_idle = Duration::from_secs(app_cfg.scheduler.idle_deep_seconds);
        self.config.cpu_metadata_max = app_cfg.scheduler.cpu_soft_limit_pct as f32;
        self.config.cpu_content_max = app_cfg.scheduler.cpu_hard_limit_pct as f32;
        self.config.disk_busy_threshold_bps = app_cfg.scheduler.disk_busy_bytes_per_s;
        self.config.content_batch_size = app_cfg.scheduler.content_batch_size as usize;
        self.config.power_save_mode = app_cfg.scheduler.power_save_mode;
    }

    /// Submit a content indexing job (path + doc ids).
    pub fn submit_content_job(&mut self, job: JobSpec) -> Result<(), JobSpec> {
        self.push_job(job)
    }

    /// Submit a batch of content indexing jobs.
    pub fn submit_content_jobs<I>(&mut self, jobs: I) -> Result<(), Vec<JobSpec>>
    where
        I: IntoIterator<Item = JobSpec>,
    {
        let mut rejected = Vec::new();
        for job in jobs {
            if let Err(job) = self.submit_content_job(job) {
                rejected.push(job);
            }
        }
        if rejected.is_empty() {
            Ok(())
        } else {
            Err(rejected)
        }
    }

    /// Force content jobs to run regardless of idle/load (useful for tests).
    pub fn force_allow_content(&mut self) {
        self.force_allow_content = true;
        self.allow_content.store(true, Ordering::Relaxed);
    }

    fn update_live_counts(&self) {
        self.live
            .content
            .store(self.pending_jobs(), Ordering::Relaxed);
        // Metadata/critical queues not implemented yet; keep zero.
        self.live.critical.store(0, Ordering::Relaxed);
        self.live.metadata.store(0, Ordering::Relaxed);
    }

    fn publish_active_workers(&self) {
        update_status_queue_state(
            None,
            Some(self.live.active_workers.load(Ordering::Relaxed)),
            None,
            None,
        );
    }

    fn pending_jobs(&self) -> usize {
        self.content_jobs.len()
            + self
                .legacy_retry
                .as_ref()
                .map_or(0, |batch| batch.jobs.len())
            + self
                .pending_index
                .as_ref()
                .map_or(0, |pending| pending.batch.jobs.len())
    }

    fn receive_submissions(&mut self) {
        // A durable batch is an ordering barrier. Jobs received after it must
        // never overwrite its newer update or resurrect a deleted document.
        if self.pending_index.is_some() {
            return;
        }
        while self.content_jobs.len() < MAX_CONTENT_QUEUE {
            match self.job_rx.try_recv() {
                Ok(Submission::Content(job)) => {
                    // Capacity was checked before receiving, so an accepted
                    // channel submission cannot disappear at a second queue.
                    let size_hint = job.file_size;
                    self.content_jobs.push_back(job);
                    self.live.enqueued_content.fetch_add(1, Ordering::Relaxed);
                    increment_content_plan(1, size_hint);
                }
                Ok(Submission::IndexBatch(pending)) => {
                    self.pending_index = Some(pending);
                    break;
                }
                Err(_) => break,
            }
        }
    }

    pub async fn run_loop(mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            self.tick().await;
        }
    }

    pub async fn tick(&mut self) {
        let idle_sample = self.idle.sample();
        let load = self.load.sample();
        self.tick_with_observation(idle_sample, load).await;
    }

    // Keep OS observation separate from admission and actual dispatch so policy
    // regressions can use deterministic inputs without synthesizing worker ACKs.
    async fn tick_with_observation(&mut self, idle_sample: IdleSample, load: SystemLoad) {
        // Reload config dynamically (from memory cache updated by IPC)
        let app_cfg = core_types::config::get_current_config();
        self.update_config(&app_cfg);

        self.receive_submissions();
        self.update_live_counts();

        // Update status snapshot counts + active workers.
        let ct = self.pending_jobs();
        let workers = self.live.active_workers.load(Ordering::Relaxed);
        let dropped = self.live.dropped_content.load(Ordering::Relaxed);
        let enqueued = self.live.enqueued_content.load(Ordering::Relaxed);
        update_status_scheduler_state(format!(
            "idle={:?} cpu={:.1}% mem={:.1}% queue(content)={} dropped={} enqueued={}",
            idle_sample.state, load.cpu_percent, load.mem_used_percent, ct, dropped, enqueued
        ));
        update_status_queue_state(
            Some(ct as u64),
            Some(workers),
            Some(self.live.enqueued_content.load(Ordering::Relaxed) as u64),
            Some(self.live.dropped_content.load(Ordering::Relaxed) as u64),
        );
        update_content_remaining(ct as u64, workers);
        update_status_metrics(None);

        // Extraction follows idle/load policy. Bounded deletion, reset, and
        // metadata-only batches do not extract content and must remain usable
        // while the user is active. Preserve older legacy work as a FIFO barrier.
        let allow_content =
            self.force_allow_content || allow_content_jobs(idle_sample.state, load, &self.config);
        self.allow_content.store(allow_content, Ordering::Relaxed);
        if !allow_content
            && (self.legacy_retry.is_some()
                || !self.content_jobs.is_empty()
                || self
                    .pending_index
                    .as_ref()
                    .is_some_and(|pending| requires_content(&pending.batch)))
        {
            return;
        }

        if self.legacy_retry.is_none() && !self.content_jobs.is_empty() {
            let batch_size = self
                .config
                .content_batch_size
                .min(self.content_jobs.len())
                .max(1);

            let mut batch = Vec::with_capacity(batch_size);
            for _ in 0..batch_size {
                if let Some(job) = self.content_jobs.pop_front() {
                    batch.push(job);
                }
            }

            self.legacy_retry = Some(IndexBatch {
                id: uuid::Uuid::new_v4(),
                jobs: batch,
                reset_volumes: Vec::new(),
            });
        }

        if let Some(batch) = self.legacy_retry.as_ref() {
            self.live.active_workers.fetch_add(1, Ordering::Relaxed);
            self.publish_active_workers();
            let result = self.dispatcher.spawn_index_batch(batch, None).await;
            self.live.active_workers.fetch_sub(1, Ordering::Relaxed);
            self.publish_active_workers();
            match result {
                Ok(()) => self.legacy_retry = None,
                Err(error) => {
                    tracing::error!(%error, batch_id = %batch.id, "content batch failed; retaining for retry");
                }
            }
            self.update_live_counts();
            return;
        }

        if let Some(pending) = self.pending_index.take() {
            self.live.active_workers.fetch_add(1, Ordering::Relaxed);
            self.publish_active_workers();
            let result = self
                .dispatcher
                .spawn_index_batch(&pending.batch, pending.mutation_lease)
                .await;
            self.live.active_workers.fetch_sub(1, Ordering::Relaxed);
            self.publish_active_workers();
            if let Err(error) = &result {
                tracing::error!(%error, batch_id = %pending.batch.id, "journal batch failed; checkpoint remains pending");
            }
            // The caller persists journal progress only after this successful
            // worker exit, which occurs after the worker's index commit.
            let _ = pending
                .acknowledgement
                .send(result.map_err(|error| error.to_string()));
            self.update_live_counts();
        }
    }

    fn push_job(&mut self, job: JobSpec) -> Result<(), JobSpec> {
        if self.content_jobs.len() >= MAX_CONTENT_QUEUE || self.pending_index.is_some() {
            self.live.dropped_content.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                queue_len = self.content_jobs.len(),
                max = MAX_CONTENT_QUEUE,
                "content queue unavailable; returning job for {:?}",
                job.path
            );
            return Err(job);
        }
        let size_hint = job.file_size;
        self.content_jobs.push_back(job);
        self.live.enqueued_content.fetch_add(1, Ordering::Relaxed);
        increment_content_plan(1, size_hint);
        self.update_live_counts();
        Ok(())
    }
}

/// Enqueue a content indexing job for the scheduler loop.
/// Returns `false` without accepting the job when unavailable or under backpressure.
pub fn enqueue_content_job(job: JobSpec) -> bool {
    if let Some(sender) = submission_sender()
        && sender.try_send(Submission::Content(job)).is_ok()
    {
        return true;
    }
    tracing::warn!("scheduler unavailable or full; content job was not accepted");
    LIVE_STATE
        .get_or_init(SchedulerLiveState::default)
        .dropped_content
        .fetch_add(1, Ordering::Relaxed);
    false
}

/// Wait for bounded admission and the worker's durable commit acknowledgement.
/// The producer must retain its batch and cursor until this returns success.
/// The lease keeps an ingestion session alive across producer cancellation,
/// queueing, and detached blocking worker ownership. Legacy callers use None.
pub async fn submit_index_batch(
    batch: IndexBatch,
    mutation_lease: Option<Arc<std::fs::File>>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        batch.jobs.len() <= MAX_INDEX_BATCH_JOBS,
        "index batch exceeds the {MAX_INDEX_BATCH_JOBS}-job bound"
    );
    let sender = submission_sender()
        .ok_or_else(|| anyhow::anyhow!("scheduler is not running; index batch remains pending"))?;
    let (acknowledgement, response) = oneshot::channel();
    sender
        .send(Submission::IndexBatch(PendingIndexBatch {
            batch,
            mutation_lease,
            acknowledgement,
        }))
        .await
        .map_err(|_| anyhow::anyhow!("scheduler stopped before admitting the index batch"))?;
    response
        .await
        .map_err(|_| anyhow::anyhow!("scheduler stopped before acknowledging the index commit"))?
        .map_err(anyhow::Error::msg)
}

/// Utility to let other components set active worker count directly (e.g., worker manager updates).
pub fn set_live_active_workers(active: u32) {
    let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);
    live.active_workers.store(active, Ordering::Relaxed);
}

/// Utility to set live queue counts directly (for external schedulers/testing).
pub fn set_live_queue_counts(critical: usize, metadata: usize, content: usize) {
    let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);
    live.critical.store(critical, Ordering::Relaxed);
    live.metadata.store(metadata, Ordering::Relaxed);
    live.content.store(content, Ordering::Relaxed);
}

impl Drop for SchedulerRuntime {
    fn drop(&mut self) {
        let mut channel = JOB_SENDER.get_or_init(|| Mutex::new(None)).lock();
        if channel
            .as_ref()
            .is_some_and(|channel| channel.runtime_id == self.runtime_id)
        {
            *channel = None;
        }
    }
}

/// Convert a `FileMeta` into a `JobSpec` if it looks indexable.
pub fn content_job_from_meta(meta: &FileMeta, extract: &ExtractSection) -> Option<JobSpec> {
    if meta
        .flags
        .intersects(FileFlags::IS_DIR | FileFlags::REPARSE | FileFlags::OFFLINE | FileFlags::SYSTEM)
        || meta.size > extract.max_bytes_per_file
    {
        return None;
    }
    let path_str = meta.path.as_ref()?;
    let path = PathBuf::from(path_str);
    let file_id = meta.key.file_id();

    let to_usize = |v: u64| -> usize {
        if v > usize::MAX as u64 {
            usize::MAX
        } else {
            v as usize
        }
    };

    Some(JobSpec {
        operation: JobOperation::Upsert,
        volume_id: meta.volume,
        file_id,
        path,
        max_bytes: Some(to_usize(extract.max_bytes_per_file)),
        max_chars: Some(to_usize(extract.max_chars_per_file)),
        file_size: meta.size,
    })
}

#[cfg(test)]
pub fn live_counters() -> (usize, usize) {
    let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);
    (
        live.enqueued_content.load(Ordering::Relaxed),
        live.dropped_content.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_provider::init_basic_status_provider;
    use std::future::Future;
    use std::task::{Context, Waker};

    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn dummy_job() -> JobSpec {
        JobSpec {
            operation: JobOperation::Upsert,
            volume_id: 1,
            file_id: 1,
            path: PathBuf::from("C:\\dummy"),
            max_bytes: None,
            max_chars: None,
            file_size: 0,
        }
    }

    struct RestoreConfig(AppConfig);

    impl RestoreConfig {
        fn install(cfg: &AppConfig) -> anyhow::Result<Self> {
            let previous = core_types::config::get_current_config();
            core_types::config::set_current_config(cfg.clone())?;
            Ok(Self(previous))
        }
    }

    impl Drop for RestoreConfig {
        fn drop(&mut self) {
            core_types::config::set_current_config(self.0.clone())
                .expect("restore previously validated test configuration");
        }
    }

    fn blocked_dispatch_config() -> anyhow::Result<(AppConfig, String)> {
        // A genuine filesystem failure proves dispatch was attempted, while
        // preventing any process launch regardless of the worker environment.
        // Retain this fixture as failure evidence; never fabricate a success ACK.
        let root = tempfile::tempdir()?.keep();
        let jobs_path = root.join("jobs-is-a-regular-file");
        std::fs::write(&jobs_path, b"scheduler dispatch prerequisite failure\n")?;
        let expected_error = std::fs::create_dir_all(&jobs_path)
            .expect_err("regular file must not become a jobs directory")
            .to_string();
        let mut cfg = AppConfig::default();
        cfg.paths.jobs_dir = jobs_path.to_string_lossy().into_owned();
        cfg.paths.content_index = root.join("content").to_string_lossy().into_owned();
        cfg.scheduler.power_save_mode = true;
        Ok((cfg, expected_error))
    }

    async fn power_tick(runtime: &mut SchedulerRuntime, on_battery: bool) {
        runtime
            .tick_with_observation(
                IdleSample {
                    state: scheduler::idle::IdleState::DeepIdle,
                    idle_for: Duration::from_secs(3600),
                    since_state_change: Duration::from_secs(3600),
                },
                SystemLoad {
                    cpu_percent: 0.0,
                    mem_used_percent: 10.0,
                    disk_bytes_per_sec: 0,
                    disk_busy: false,
                    sample_duration: Duration::from_secs(1),
                    on_battery,
                    game_mode: false,
                },
            )
            .await;
    }

    #[test]
    fn content_jobs_preserve_full_identity_and_skip_reparse_or_offline_files() {
        let key = core_types::DocKey::from_parts(12, 0xfedc_1234_5678_9abc);
        let mut meta = FileMeta::new(
            key,
            key.volume(),
            None,
            "document.txt".into(),
            Some("C:\\document.txt".into()),
            42,
            1,
            2,
            FileFlags::empty(),
        );
        let mut cfg = AppConfig::default();
        cfg.extract.max_bytes_per_file = 64;
        let job = content_job_from_meta(&meta, &cfg.extract).expect("regular file is eligible");
        assert_eq!(job.file_id, key.file_id());
        assert_eq!(job.volume_id, key.volume());
        for flag in [
            FileFlags::IS_DIR,
            FileFlags::REPARSE,
            FileFlags::OFFLINE,
            FileFlags::SYSTEM,
        ] {
            meta.flags = flag;
            assert!(content_job_from_meta(&meta, &cfg.extract).is_none());
        }
        meta.flags = FileFlags::empty();
        meta.size = 64;
        assert!(content_job_from_meta(&meta, &cfg.extract).is_some());
        meta.size = 65;
        assert!(content_job_from_meta(&meta, &cfg.extract).is_none());
        meta.size = 42;
        meta.path = None;
        assert!(content_job_from_meta(&meta, &cfg.extract).is_none());
    }

    #[tokio::test]
    async fn enqueue_without_runtime_increments_dropped() {
        let _guard = TEST_LOCK.lock().await;
        // Ensure we start from a clean slate in case another test initialized the runtime.
        *JOB_SENDER.get_or_init(|| Mutex::new(None)).lock() = None;
        let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);
        live.dropped_content.store(0, Ordering::Relaxed);

        let before = live_counters().1;
        let ok = enqueue_content_job(dummy_job());
        assert!(!ok);
        let after = live_counters().1;
        assert!(after > before, "dropped counter should increase");
    }

    #[tokio::test]
    async fn submit_content_job_increments_enqueued_counter() {
        let _guard = TEST_LOCK.lock().await;
        // Initialize status provider once for metric updates (harmless if already set).
        let _ = init_basic_status_provider();
        let cfg = AppConfig::default();
        let mut rt = SchedulerRuntime::new(&cfg);

        let before = live_counters().0;
        rt.submit_content_job(dummy_job()).expect("queue available");
        rt.update_live_counts();
        let after = live_counters().0;
        assert_eq!(after, before + 1, "enqueued counter should increase");
    }

    #[tokio::test]
    async fn durable_batch_preserves_order_between_legacy_jobs() {
        let _guard = TEST_LOCK.lock().await;
        let mut runtime = SchedulerRuntime::new(&AppConfig::default());
        let sender = submission_sender().expect("runtime publishes sender");
        let mut before = dummy_job();
        before.file_id = 10;
        let mut after = dummy_job();
        after.file_id = 30;
        let (acknowledgement, mut response) = oneshot::channel();
        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: vec![JobSpec {
                operation: JobOperation::Delete,
                file_id: 20,
                ..dummy_job()
            }],
            reset_volumes: Vec::new(),
        };
        sender.try_send(Submission::Content(before)).ok().unwrap();
        sender
            .try_send(Submission::IndexBatch(PendingIndexBatch {
                batch: batch.clone(),
                mutation_lease: None,
                acknowledgement,
            }))
            .ok()
            .unwrap();
        sender.try_send(Submission::Content(after)).ok().unwrap();

        runtime.receive_submissions();
        assert_eq!(runtime.content_jobs.len(), 1);
        assert_eq!(runtime.content_jobs[0].file_id, 10);
        assert_eq!(runtime.pending_index.as_ref().unwrap().batch.id, batch.id);
        assert_eq!(runtime.job_rx.len(), 1);
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        // Draining another tick cannot move the newer job before the delete.
        runtime.receive_submissions();
        assert_eq!(runtime.content_jobs.len(), 1);
        assert_eq!(runtime.job_rx.len(), 1);
        drop(runtime);
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn bounded_admission_waits_and_shutdown_never_acknowledges_commit() {
        let _guard = TEST_LOCK.lock().await;
        let mut runtime = SchedulerRuntime::new(&AppConfig::default());
        for _ in 0..MAX_PENDING_SUBMISSIONS {
            assert!(enqueue_content_job(dummy_job()));
        }
        assert!(!enqueue_content_job(dummy_job()));

        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: vec![dummy_job()],
            reset_volumes: Vec::new(),
        };
        let mut submission = Box::pin(submit_index_batch(batch, None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(submission.as_mut().poll(&mut context).is_pending());
        assert_eq!(runtime.job_rx.len(), MAX_PENDING_SUBMISSIONS);

        runtime.receive_submissions();
        assert_eq!(runtime.content_jobs.len(), MAX_PENDING_SUBMISSIONS);
        assert!(submission.as_mut().poll(&mut context).is_pending());
        runtime.receive_submissions();
        assert!(runtime.pending_index.is_some());
        assert!(submission.as_mut().poll(&mut context).is_pending());
        drop(runtime);
        assert!(submission.await.is_err());
    }

    #[tokio::test]
    async fn runtime_restart_replaces_sender_and_old_drop_preserves_new_runtime() {
        let _guard = TEST_LOCK.lock().await;
        let first = SchedulerRuntime::new(&AppConfig::default());
        let mut second = SchedulerRuntime::new(&AppConfig::default());
        drop(first);
        assert!(enqueue_content_job(dummy_job()));
        second.receive_submissions();
        assert_eq!(second.content_jobs.len(), 1);
        drop(second);
        assert!(!enqueue_content_job(dummy_job()));
    }

    #[tokio::test]
    async fn power_save_dispatches_delete_reset_and_empty_batches_without_false_acknowledgement()
    -> anyhow::Result<()> {
        let _guard = TEST_LOCK.lock().await;
        let (cfg, expected_error) = blocked_dispatch_config()?;
        let _restore = RestoreConfig::install(&cfg)?;
        let delete = JobSpec {
            operation: JobOperation::Delete,
            path: PathBuf::new(),
            ..dummy_job()
        };
        for (jobs, reset_volumes) in [
            (vec![delete], Vec::new()),
            (Vec::new(), vec![1]),
            (Vec::new(), Vec::new()),
        ] {
            let mut runtime = SchedulerRuntime::new(&cfg);
            let batch = IndexBatch {
                id: uuid::Uuid::new_v4(),
                jobs,
                reset_volumes,
            };
            let mut submission = Box::pin(submit_index_batch(batch, None));
            let mut context = Context::from_waker(Waker::noop());
            assert!(submission.as_mut().poll(&mut context).is_pending());

            power_tick(&mut runtime, true).await;
            assert!(
                !content_admission_available(),
                "extraction must remain paused"
            );
            assert!(
                runtime.pending_index.is_none(),
                "cleanup was not dispatched"
            );
            assert!(runtime.job_rx.is_empty());
            let std::task::Poll::Ready(result) = submission.as_mut().poll(&mut context) else {
                panic!("completed dispatch did not return its acknowledgement");
            };
            let error = result.expect_err("failed dispatch must never acknowledge a commit");
            assert_eq!(error.to_string(), expected_error);
            assert!(!PathBuf::from(&cfg.paths.content_index).exists());
            assert!(PathBuf::from(&cfg.paths.jobs_dir).is_file());
        }
        Ok(())
    }

    #[tokio::test]
    async fn power_save_keeps_reconciliation_pending_until_extraction_is_allowed()
    -> anyhow::Result<()> {
        let _guard = TEST_LOCK.lock().await;
        let (mut cfg, expected_error) = blocked_dispatch_config()?;
        let _restore = RestoreConfig::install(&cfg)?;
        let reconcile = JobSpec {
            operation: JobOperation::Reconcile,
            ..dummy_job()
        };
        let delete = JobSpec {
            operation: JobOperation::Delete,
            file_id: 2,
            path: PathBuf::new(),
            ..dummy_job()
        };
        for (jobs, reset_volumes) in [
            (vec![reconcile.clone()], Vec::new()),
            (vec![delete, reconcile], vec![1]),
        ] {
            cfg.scheduler.power_save_mode = true;
            core_types::config::set_current_config(cfg.clone())?;
            let mut runtime = SchedulerRuntime::new(&cfg);
            let batch = IndexBatch {
                id: uuid::Uuid::new_v4(),
                jobs,
                reset_volumes,
            };
            let mut submission = Box::pin(submit_index_batch(batch.clone(), None));
            let mut context = Context::from_waker(Waker::noop());
            assert!(submission.as_mut().poll(&mut context).is_pending());
            for _ in 0..2 {
                power_tick(&mut runtime, true).await;
                assert!(!content_admission_available());
                assert_eq!(runtime.pending_index.as_ref().unwrap().batch.id, batch.id);
                assert!(submission.as_mut().poll(&mut context).is_pending());
            }

            // Changing the real cached setting must affect the very next tick;
            // the same on-battery observation then permits actual extraction.
            cfg.scheduler.power_save_mode = false;
            core_types::config::set_current_config(cfg.clone())?;
            power_tick(&mut runtime, true).await;
            assert!(content_admission_available());
            assert!(runtime.pending_index.is_none());
            let std::task::Poll::Ready(result) = submission.as_mut().poll(&mut context) else {
                panic!("resumed dispatch did not return its acknowledgement");
            };
            let error = result.expect_err("real dispatch prerequisite fails");
            assert_eq!(error.to_string(), expected_error);
            assert!(!PathBuf::from(&cfg.paths.content_index).exists());
        }
        Ok(())
    }

    #[tokio::test]
    async fn power_save_never_lets_cleanup_overtake_older_legacy_extraction() -> anyhow::Result<()>
    {
        let _guard = TEST_LOCK.lock().await;
        let (mut cfg, _) = blocked_dispatch_config()?;
        let _restore = RestoreConfig::install(&cfg)?;
        let mut runtime = SchedulerRuntime::new(&cfg);
        let before = JobSpec {
            file_id: 10,
            ..dummy_job()
        };
        let after = JobSpec {
            file_id: 30,
            ..dummy_job()
        };
        assert!(enqueue_content_job(before));
        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: vec![JobSpec {
                operation: JobOperation::Delete,
                file_id: 20,
                path: PathBuf::new(),
                ..dummy_job()
            }],
            reset_volumes: Vec::new(),
        };
        let mut submission = Box::pin(submit_index_batch(batch.clone(), None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(submission.as_mut().poll(&mut context).is_pending());
        assert!(enqueue_content_job(after));

        power_tick(&mut runtime, true).await;
        assert_eq!(runtime.content_jobs.front().unwrap().file_id, 10);
        assert!(runtime.legacy_retry.is_none());
        assert_eq!(runtime.pending_index.as_ref().unwrap().batch.id, batch.id);
        assert_eq!(runtime.job_rx.len(), 1);
        assert!(submission.as_mut().poll(&mut context).is_pending());

        // The older extraction reaches the real dispatcher first and fails.
        // Its retained retry must remain a barrier when power saving resumes.
        cfg.scheduler.power_save_mode = false;
        core_types::config::set_current_config(cfg.clone())?;
        power_tick(&mut runtime, true).await;
        let retry = runtime
            .legacy_retry
            .as_ref()
            .expect("failed legacy extraction retained");
        let retry_id = retry.id;
        assert_eq!(retry.jobs.len(), 1);
        assert_eq!(retry.jobs[0].file_id, 10);
        assert!(runtime.content_jobs.is_empty());
        assert_eq!(runtime.pending_index.as_ref().unwrap().batch.id, batch.id);
        assert!(submission.as_mut().poll(&mut context).is_pending());

        cfg.scheduler.power_save_mode = true;
        core_types::config::set_current_config(cfg.clone())?;
        for _ in 0..2 {
            power_tick(&mut runtime, true).await;
            assert!(!content_admission_available());
            assert_eq!(runtime.legacy_retry.as_ref().unwrap().id, retry_id);
            assert_eq!(runtime.pending_index.as_ref().unwrap().batch.id, batch.id);
            assert_eq!(
                runtime.job_rx.len(),
                1,
                "newer legacy job crossed the durable barrier"
            );
            assert!(submission.as_mut().poll(&mut context).is_pending());
        }
        drop(runtime);
        let error = submission
            .await
            .expect_err("shutdown must not forge a cleanup commit");
        assert!(error.to_string().contains("stopped before acknowledging"));
        Ok(())
    }

    #[tokio::test]
    async fn content_admission_hint_belongs_only_to_the_current_runtime() -> anyhow::Result<()> {
        let _guard = TEST_LOCK.lock().await;
        let mut cfg = AppConfig::default();
        cfg.scheduler.power_save_mode = true;
        let _restore = RestoreConfig::install(&cfg)?;
        let mut first = SchedulerRuntime::new(&cfg);
        assert!(!content_admission_available());
        power_tick(&mut first, false).await;
        assert!(content_admission_available());

        let mut replacement = SchedulerRuntime::new(&cfg);
        assert!(
            !content_admission_available(),
            "new runtime inherited an old admission hint"
        );
        power_tick(&mut first, false).await;
        assert!(
            !content_admission_available(),
            "old runtime changed its replacement's hint"
        );
        power_tick(&mut replacement, false).await;
        assert!(content_admission_available());
        power_tick(&mut first, true).await;
        assert!(
            content_admission_available(),
            "old runtime paused its replacement"
        );
        drop(first);
        assert!(
            content_admission_available(),
            "old shutdown cleared the live replacement"
        );
        power_tick(&mut replacement, true).await;
        assert!(!content_admission_available());
        drop(replacement);
        assert!(
            !content_admission_available(),
            "closed runtime still admits content"
        );
        Ok(())
    }

    #[tokio::test]
    async fn full_internal_queue_retains_already_accepted_channel_job() {
        let _guard = TEST_LOCK.lock().await;
        let mut runtime = SchedulerRuntime::new(&AppConfig::default());
        runtime.content_jobs.resize(MAX_CONTENT_QUEUE, dummy_job());
        let mut accepted = dummy_job();
        accepted.file_id = 99;
        assert!(enqueue_content_job(accepted));
        runtime.receive_submissions();
        assert_eq!(runtime.job_rx.len(), 1);
        assert_eq!(runtime.content_jobs.len(), MAX_CONTENT_QUEUE);
        runtime.content_jobs.pop_front();
        runtime.receive_submissions();
        assert!(runtime.job_rx.is_empty());
        assert_eq!(runtime.content_jobs.len(), MAX_CONTENT_QUEUE);
        assert_eq!(runtime.content_jobs.back().unwrap().file_id, 99);
    }

    #[tokio::test]
    async fn oversized_durable_batch_is_rejected_before_admission() {
        let _guard = TEST_LOCK.lock().await;
        let runtime = SchedulerRuntime::new(&AppConfig::default());
        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: vec![dummy_job(); MAX_INDEX_BATCH_JOBS + 1],
            reset_volumes: Vec::new(),
        };
        let error = submit_index_batch(batch, None).await.unwrap_err();
        assert!(error.to_string().contains("4096-job bound"));
        assert!(runtime.job_rx.is_empty());
    }

    #[tokio::test]
    async fn admitted_batch_keeps_session_locked_after_producer_cancellation() -> anyhow::Result<()>
    {
        let _guard = TEST_LOCK.lock().await;
        let mut runtime = SchedulerRuntime::new(&AppConfig::default());
        let root = tempfile::tempdir()?;
        let lock_path = root.path().join("ingestion.lock");
        let lease = Arc::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&lock_path)?,
        );
        lease.try_lock()?;
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)?;
        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: vec![dummy_job()],
            reset_volumes: Vec::new(),
        };
        let mut submission = Box::pin(submit_index_batch(batch, Some(lease)));
        let mut context = Context::from_waker(Waker::noop());
        assert!(submission.as_mut().poll(&mut context).is_pending());
        runtime.receive_submissions();
        assert!(runtime.pending_index.is_some());
        drop(submission);
        assert!(
            contender.try_lock().is_err(),
            "admitted work still owns the ingestion session"
        );
        drop(runtime);
        contender.try_lock()?;
        Ok(())
    }
}
