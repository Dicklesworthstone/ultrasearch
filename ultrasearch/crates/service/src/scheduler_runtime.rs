use crate::dispatcher::job_dispatch::{IndexBatch, JobDispatcher, JobOperation, JobSpec};
use crate::scanner;
use crate::status_provider::{
    increment_content_plan, update_content_remaining, update_status_metrics,
    update_status_queue_state, update_status_scheduler_state,
};
use core_types::config::{AppConfig, ExtractSection};
use core_types::{DocKey, FileFlags, FileMeta};
use parking_lot::Mutex;
use scheduler::{
    AdaptivePolicy, SchedulerConfig, allow_content_jobs,
    idle::{IdleSample, IdleTracker},
    metrics::{SystemLoad, SystemLoadSampler},
};
use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
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
const OBSERVATION_INTERVAL: Duration = Duration::from_secs(1);

fn adaptive_feature_enabled(app_cfg: &AppConfig) -> bool {
    app_cfg.features.adaptive_scheduler || app_cfg.features.auto_tuning
}

fn scheduler_config(app_cfg: &AppConfig) -> SchedulerConfig {
    let warm_idle = Duration::from_secs(app_cfg.scheduler.idle_warm_seconds);
    // Keep deep idle ordered even if a caller bypasses config validation.
    // `IdleTracker::set_thresholds` rejects an unordered pair outright.
    let deep_idle = Duration::from_secs(app_cfg.scheduler.idle_deep_seconds).max(warm_idle);
    SchedulerConfig {
        warm_idle,
        deep_idle,
        cpu_metadata_max: app_cfg.scheduler.cpu_soft_limit_pct as f32,
        cpu_content_max: app_cfg.scheduler.cpu_hard_limit_pct as f32,
        disk_busy_threshold_bps: app_cfg.scheduler.disk_busy_bytes_per_s,
        // Bound in u64 before converting: a large setting must not truncate on
        // 32-bit hosts or create a legacy retry too large for bounded dispatch.
        content_batch_size: app_cfg
            .scheduler
            .content_batch_size
            .clamp(1, MAX_INDEX_BATCH_JOBS as u64) as usize,
        power_save_mode: app_cfg.scheduler.power_save_mode,
        ..SchedulerConfig::default()
    }
}

fn adaptive_config_at(
    adaptive: &mut Option<AdaptivePolicy>,
    enabled: bool,
    base: &SchedulerConfig,
    load: &SystemLoad,
    now: Instant,
) -> SchedulerConfig {
    if !enabled {
        // Turning the feature off restores the operator policy immediately;
        // a later enable starts a new feedback window, not stale learned load.
        *adaptive = None;
        return base.clone();
    }
    let policy = adaptive.get_or_insert_with(|| AdaptivePolicy::new(base.clone()));
    policy.reconfigure(base.clone());
    policy.update_at(load, now);
    policy.config().clone()
}

fn content_allowed(
    idle: scheduler::IdleState,
    load: SystemLoad,
    effective: &SchedulerConfig,
    adaptive: Option<&AdaptivePolicy>,
) -> bool {
    adaptive.is_none_or(AdaptivePolicy::can_admit_content)
        && allow_content_jobs(idle, load, effective)
}

fn content_admission_description(
    allowed: bool,
    idle: scheduler::IdleState,
    load: SystemLoad,
    effective: &SchedulerConfig,
) -> &'static str {
    if allowed {
        "ready"
    } else if effective.power_save_mode && load.on_battery {
        "waiting for AC power (power_save_mode=true)"
    } else if effective.power_save_mode && load.game_mode {
        "paused by Game Mode (power_save_mode=true)"
    } else if idle != scheduler::IdleState::DeepIdle {
        "waiting for deep idle (or idle observation unavailable)"
    } else if !load.cpu_percent.is_finite() {
        "waiting for a valid CPU load observation"
    } else if load.cpu_percent >= effective.cpu_content_max {
        "waiting for CPU load to fall below the configured limit"
    } else if load.disk_busy {
        "waiting for disk activity to subside"
    } else {
        "paused by adaptive resource policy"
    }
}

/// Poll one dispatch to completion without suspending observation for its whole
/// lifetime. Keep the same pinned future: a timer tick must never resubmit work
/// or release a mutation lease. Dropping this wrapper drops that future; it
/// does not detach another scheduler task or manufacture an acknowledgement.
async fn await_with_refresh<F: std::future::Future>(
    completion: F,
    period: Duration,
    mut refresh: impl FnMut(),
) -> F::Output {
    tokio::pin!(completion);
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            result = &mut completion => return result,
            _ = interval.tick() => refresh(),
        }
    }
}

async fn await_dispatch<F: std::future::Future>(
    completion: F,
    idle: &mut IdleTracker,
    load: &mut SystemLoadSampler,
    allow_content: &AtomicBool,
    force_allow_content: bool,
    sample: &mut impl FnMut(&mut IdleTracker, &mut SystemLoadSampler) -> (IdleSample, SystemLoad),
    adaptive: &mut Option<AdaptivePolicy>,
) -> F::Output {
    await_with_refresh(completion, OBSERVATION_INTERVAL, || {
        // Config publication is atomic. Use one snapshot for both sensors and
        // the admission hint, including while the current worker is running.
        // Do not replace readers: the native SessionClock needs observations
        // at intervals <= 5 s to retain earned, conservative idle time.
        let app_cfg = core_types::config::get_current_config();
        let cfg = scheduler_config(&app_cfg);
        idle.set_thresholds(cfg.warm_idle, cfg.deep_idle);
        load.set_disk_threshold(cfg.disk_busy_threshold_bps);
        // Close the opportunistic admission hint before potentially blocking
        // native calls. An unavailable idle observation stays Active. These
        // synchronous sensors are not forcibly interruptible by this timer.
        allow_content.store(false, Ordering::Relaxed);
        let (idle_sample, load_sample) = sample(idle, load);
        let effective = adaptive_config_at(
            adaptive,
            adaptive_feature_enabled(&app_cfg),
            &cfg,
            &load_sample,
            Instant::now(),
        );
        let allowed = force_allow_content
            || content_allowed(idle_sample.state, load_sample, &effective, adaptive.as_ref());
        allow_content.store(allowed, Ordering::Relaxed);
        let admission = content_admission_description(allowed, idle_sample.state, load_sample, &effective);
        update_status_scheduler_state(format!(
            "idle={:?} cpu={:.1}% mem={:.1}% dispatch=pending content_admission={admission} adaptive={} cpu_cap={:.1}% batch={}",
            idle_sample.state, load_sample.cpu_percent, load_sample.mem_used_percent,
            adaptive.is_some(), effective.cpu_content_max, effective.content_batch_size,
        ));
    })
    .await
}

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
    content_optional: bool,
    acknowledgement: oneshot::Sender<Result<DispatchOutcome, String>>,
}

#[derive(Debug)]
enum DispatchOutcome {
    Committed,
    ContentDeferred,
}

/// A scheduling decision, explicitly distinct from a physical commit ACK or
/// worker failure. The coordinator must retain/replace its durable intent.
#[derive(Debug)]
pub(crate) struct ContentAdmissionDeferred;

impl std::fmt::Display for ContentAdmissionDeferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("optional extraction was not dispatched; metadata-only commit required")
    }
}
impl std::error::Error for ContentAdmissionDeferred {}

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

fn check_index_batch_bounds(batch: &IndexBatch) -> anyhow::Result<()> {
    anyhow::ensure!(
        batch.jobs.len() <= MAX_INDEX_BATCH_JOBS,
        "index batch exceeds the {MAX_INDEX_BATCH_JOBS}-job bound"
    );
    let mut deferred = BTreeSet::new();
    for job in &batch.jobs {
        if job.operation == JobOperation::Reconcile {
            deferred.insert(DocKey::from_parts(job.volume_id, job.file_id));
            anyhow::ensure!(
                deferred.len() <= content_index::MAX_DEFERRED_KEYS,
                "index batch exceeds the {}-deferrable-identity bound",
                content_index::MAX_DEFERRED_KEYS,
            );
        }
    }
    Ok(())
}

fn take_legacy_batch(queue: &mut VecDeque<JobSpec>, limit: usize) -> Vec<JobSpec> {
    let limit = limit.clamp(1, MAX_INDEX_BATCH_JOBS).min(queue.len());
    let mut batch = Vec::with_capacity(limit);
    let mut deferred = BTreeSet::new();
    while batch.len() < limit {
        let Some(next) = queue.front() else {
            break;
        };
        if next.operation == JobOperation::Reconcile {
            let key = DocKey::from_parts(next.volume_id, next.file_id);
            if !deferred.contains(&key) && deferred.len() == content_index::MAX_DEFERRED_KEYS {
                break;
            }
            deferred.insert(key);
        }
        if let Some(job) = queue.pop_front() {
            batch.push(job);
        }
    }
    // Never skip a blocked head: even a deletion behind it must preserve FIFO.
    batch
}

/// Runtime wrapper that drives a simple scheduling loop and dispatches content batches.
pub struct SchedulerRuntime {
    config: SchedulerConfig,
    adaptive: Option<AdaptivePolicy>,
    idle: IdleTracker,
    last_idle_state: scheduler::idle::IdleState,
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
        let config = scheduler_config(app_cfg);

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
            last_idle_state: scheduler::idle::IdleState::Active,
            load: SystemLoadSampler::new(config.disk_busy_threshold_bps),
            content_jobs: VecDeque::new(),
            job_rx: rx,
            pending_index: None,
            legacy_retry: None,
            runtime_id,
            dispatcher: JobDispatcher::new(app_cfg),
            config,
            adaptive: None,
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

        self.config = scheduler_config(app_cfg);
        self.idle
            .set_thresholds(self.config.warm_idle, self.config.deep_idle);
        self.load
            .set_disk_threshold(self.config.disk_busy_threshold_bps);
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
        let mut interval = tokio::time::interval(OBSERVATION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            self.tick().await;
        }
    }

    pub async fn tick(&mut self) {
        self.tick_with_sampler(|idle, load| (idle.sample(), load.sample()))
            .await;
    }

    // Keep OS observation separate from admission and actual dispatch so policy
    // regressions can use deterministic inputs without synthesizing worker ACKs.
    #[cfg(test)]
    async fn tick_with_observation(&mut self, idle_sample: IdleSample, load: SystemLoad) {
        self.tick_with_sampler(|_, _| (idle_sample, load)).await;
    }

    async fn tick_with_sampler(
        &mut self,
        mut sample: impl FnMut(&mut IdleTracker, &mut SystemLoadSampler) -> (IdleSample, SystemLoad),
    ) {
        // Reload config dynamically (from memory cache updated by IPC)
        let app_cfg = core_types::config::get_current_config();
        self.update_config(&app_cfg);
        // Apply thresholds BEFORE sampling. Updating only SchedulerConfig, or
        // updating the sensors after sampling, admits work using stale policy.
        self.allow_content.store(false, Ordering::Relaxed);
        let (idle_sample, load) = sample(&mut self.idle, &mut self.load);
        let effective = adaptive_config_at(
            &mut self.adaptive,
            adaptive_feature_enabled(&app_cfg),
            &self.config,
            &load,
            Instant::now(),
        );

        if idle_sample.state == scheduler::idle::IdleState::DeepIdle
            && self.last_idle_state != scheduler::idle::IdleState::DeepIdle
        {
            crate::memory::trim_working_set();
        }
        self.last_idle_state = idle_sample.state;

        self.receive_submissions();
        self.update_live_counts();

        // Update status snapshot counts + active workers.
        let ct = self.pending_jobs();
        let workers = self.live.active_workers.load(Ordering::Relaxed);
        let dropped = self.live.dropped_content.load(Ordering::Relaxed);
        let enqueued = self.live.enqueued_content.load(Ordering::Relaxed);
        let allow_content = self.force_allow_content
            || content_allowed(idle_sample.state, load, &effective, self.adaptive.as_ref());
        self.allow_content.store(allow_content, Ordering::Relaxed);
        let admission =
            content_admission_description(allow_content, idle_sample.state, load, &effective);
        update_status_scheduler_state(format!(
            "idle={:?} cpu={:.1}% mem={:.1}% queue(content)={} dropped={} enqueued={} content_admission={admission} adaptive={} cpu_cap={:.1}% batch={}",
            idle_sample.state,
            load.cpu_percent,
            load.mem_used_percent,
            ct,
            dropped,
            enqueued,
            self.adaptive.is_some(),
            effective.cpu_content_max,
            effective.content_batch_size,
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
        if !allow_content
            && self
                .pending_index
                .as_ref()
                .is_some_and(|pending| pending.content_optional && requires_content(&pending.batch))
        {
            // Admission can close after the producer's hint, including after a
            // restart. No worker has started: return an explicit non-commit so
            // the coordinator can persist and run metadata-only cleanup.
            if let Some(pending) = self.pending_index.take() {
                let _ = pending
                    .acknowledgement
                    .send(Ok(DispatchOutcome::ContentDeferred));
            }
            self.update_live_counts();
        }
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
            let batch = take_legacy_batch(&mut self.content_jobs, effective.content_batch_size);

            self.legacy_retry = Some(IndexBatch {
                id: uuid::Uuid::new_v4(),
                jobs: batch,
                reset_volumes: Vec::new(),
            });
        }

        if let Some(batch) = self.legacy_retry.as_ref() {
            self.live.active_workers.fetch_add(1, Ordering::Relaxed);
            self.publish_active_workers();
            let result = await_dispatch(
                self.dispatcher.spawn_index_batch(batch, None),
                &mut self.idle,
                &mut self.load,
                &self.allow_content,
                self.force_allow_content,
                &mut sample,
                &mut self.adaptive,
            )
            .await;
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
            let result = await_dispatch(
                self.dispatcher
                    .spawn_index_batch(&pending.batch, pending.mutation_lease),
                &mut self.idle,
                &mut self.load,
                &self.allow_content,
                self.force_allow_content,
                &mut sample,
                &mut self.adaptive,
            )
            .await;
            self.live.active_workers.fetch_sub(1, Ordering::Relaxed);
            self.publish_active_workers();
            if let Err(error) = &result {
                tracing::error!(%error, batch_id = %pending.batch.id, "journal batch failed; checkpoint remains pending");
            }
            // The caller persists journal progress only after this successful
            // worker exit, which occurs after the worker's index commit.
            let _ = pending.acknowledgement.send(
                result
                    .map(|()| DispatchOutcome::Committed)
                    .map_err(|error| error.to_string()),
            );
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
    submit_batch(batch, mutation_lease, false).await
}

pub(crate) async fn submit_optional_index_batch(
    batch: IndexBatch,
    mutation_lease: Option<Arc<std::fs::File>>,
) -> anyhow::Result<()> {
    submit_batch(batch, mutation_lease, true).await
}

async fn submit_batch(
    batch: IndexBatch,
    mutation_lease: Option<Arc<std::fs::File>>,
    content_optional: bool,
) -> anyhow::Result<()> {
    // An admitted durable transaction is indivisible. Reject an envelope the
    // worker cannot receipt BEFORE queueing; never truncate it to fit a budget.
    check_index_batch_bounds(&batch)?;
    let sender = submission_sender()
        .ok_or_else(|| anyhow::anyhow!("scheduler is not running; index batch remains pending"))?;
    let (acknowledgement, response) = oneshot::channel();
    sender
        .send(Submission::IndexBatch(PendingIndexBatch {
            batch,
            mutation_lease,
            content_optional,
            acknowledgement,
        }))
        .await
        .map_err(|_| anyhow::anyhow!("scheduler stopped before admitting the index batch"))?;
    match response
        .await
        .map_err(|_| anyhow::anyhow!("scheduler stopped before acknowledging the index commit"))?
        .map_err(anyhow::Error::msg)?
    {
        DispatchOutcome::Committed => Ok(()),
        DispatchOutcome::ContentDeferred => Err(ContentAdmissionDeferred.into()),
    }
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

    #[tokio::test]
    async fn reloaded_thresholds_reach_sensors_before_admission() -> anyhow::Result<()> {
        let _guard = TEST_LOCK.lock().await;
        let (mut cfg, expected_error) = blocked_dispatch_config()?;
        cfg.scheduler.idle_warm_seconds = 15;
        cfg.scheduler.idle_deep_seconds = 120;
        cfg.scheduler.disk_busy_bytes_per_s = 1_000;
        let _restore = RestoreConfig::install(&cfg)?;
        let mut runtime = SchedulerRuntime::new(&cfg);
        runtime.idle = IdleTracker::with_reader(
            Duration::from_secs(15),
            Duration::from_secs(120),
            (|| Some(45_000)) as fn() -> Option<u64>,
        );
        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: vec![JobSpec {
                operation: JobOperation::Reconcile,
                ..dummy_job()
            }],
            reset_volumes: Vec::new(),
        };
        let mut submission = Box::pin(submit_index_batch(batch.clone(), None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(submission.as_mut().poll(&mut context).is_pending());

        for (deep_seconds, disk_threshold, expected_state) in [
            (180, 8_000, scheduler::IdleState::WarmIdle),
            (30, 2_000, scheduler::IdleState::DeepIdle),
        ] {
            cfg.scheduler.idle_deep_seconds = deep_seconds;
            cfg.scheduler.disk_busy_bytes_per_s = disk_threshold;
            core_types::config::set_current_config(cfg.clone())?;
            // Exercise the exact production ordering around observation. Load
            // values are deterministic; the real dispatcher must still fail
            // on its real filesystem prerequisite, never fabricate a commit.
            runtime
                .tick_with_sampler(|idle, load| {
                    let idle_sample = idle.sample();
                    assert_eq!(idle_sample.state, expected_state);
                    assert_eq!(load.disk_threshold(), disk_threshold);
                    (
                        idle_sample,
                        SystemLoad {
                            cpu_percent: 0.0,
                            mem_used_percent: 10.0,
                            disk_bytes_per_sec: 0,
                            disk_busy: false,
                            sample_duration: Duration::from_secs(1),
                            on_battery: false,
                            game_mode: false,
                        },
                    )
                })
                .await;
            if expected_state == scheduler::IdleState::WarmIdle {
                assert_eq!(runtime.pending_index.as_ref().unwrap().batch.id, batch.id);
                assert!(submission.as_mut().poll(&mut context).is_pending());
                assert!(!content_admission_available());
            }
        }
        let error = submission
            .await
            .expect_err("dispatch must not acknowledge its failed write");
        assert_eq!(error.to_string(), expected_error);
        assert!(runtime.pending_index.is_none());
        assert!(PathBuf::from(&cfg.paths.jobs_dir).is_file());
        assert!(!PathBuf::from(&cfg.paths.content_index).exists());
        Ok(())
    }

    fn observed_load(cpu: f32, memory: f32) -> SystemLoad {
        SystemLoad {
            cpu_percent: cpu,
            mem_used_percent: memory,
            disk_bytes_per_sec: 0,
            disk_busy: false,
            sample_duration: Duration::from_secs(1),
            on_battery: false,
            game_mode: false,
        }
    }

    #[test]
    fn adaptive_feedback_survives_ticks_and_toggle_restores_operator_policy() {
        let mut cfg = AppConfig::default();
        cfg.scheduler.cpu_hard_limit_pct = 40;
        cfg.scheduler.content_batch_size = 500;
        let base = scheduler_config(&cfg);
        let start = Instant::now();
        let mut adaptive = None;
        let mut effective = base.clone();
        for second in 0..=5 {
            effective = adaptive_config_at(
                &mut adaptive,
                true,
                &base,
                &observed_load(60.0, 50.0),
                start + Duration::from_secs(second),
            );
        }
        assert_eq!(effective.content_batch_size, 400);
        assert_eq!(effective.cpu_content_max, 35.0);
        let load = observed_load(37.0, 50.0);
        assert!(allow_content_jobs(
            scheduler::IdleState::DeepIdle,
            load,
            &base
        ));
        assert!(!content_allowed(
            scheduler::IdleState::DeepIdle,
            load,
            &effective,
            adaptive.as_ref(),
        ));
        effective = adaptive_config_at(
            &mut adaptive,
            false,
            &base,
            &load,
            start + Duration::from_secs(6),
        );
        assert!(adaptive.is_none());
        assert_eq!(effective.content_batch_size, 500);
        assert_eq!(effective.cpu_content_max, 40.0);
        assert!(content_allowed(
            scheduler::IdleState::DeepIdle,
            load,
            &effective,
            None,
        ));
        effective = adaptive_config_at(
            &mut adaptive,
            true,
            &base,
            &load,
            start + Duration::from_secs(7),
        );
        assert_eq!(effective.content_batch_size, 500);
        assert_eq!(effective.cpu_content_max, 40.0);
        assert_eq!(base.content_batch_size, 500);
        assert_eq!(cfg.scheduler.cpu_hard_limit_pct, 40);
    }

    #[test]
    fn legacy_batches_obey_deferral_and_count_bounds_without_skipping_a_head() {
        let mut queue = VecDeque::new();
        queue.push_back(JobSpec {
            operation: JobOperation::Delete,
            ..dummy_job()
        });
        for id in 1..=content_index::MAX_DEFERRED_KEYS + 1 {
            queue.push_back(JobSpec {
                operation: JobOperation::Reconcile,
                file_id: id as u64,
                ..dummy_job()
            });
        }
        queue.push_back(JobSpec {
            operation: JobOperation::Delete,
            ..dummy_job()
        });
        let original = serde_json::to_value(&queue).unwrap();
        let mut first = take_legacy_batch(&mut queue, usize::MAX);
        assert_eq!(first.len(), content_index::MAX_DEFERRED_KEYS + 1);
        assert_eq!(queue.front().unwrap().operation, JobOperation::Reconcile);
        assert_eq!(
            queue.len(),
            2,
            "later deletion must not overtake the blocked head"
        );
        first.extend(take_legacy_batch(&mut queue, usize::MAX));
        assert!(queue.is_empty());
        assert_eq!(serde_json::to_value(first).unwrap(), original);

        for setting in [0, 1, 1000, u64::MAX] {
            let mut cfg = AppConfig::default();
            cfg.scheduler.content_batch_size = setting;
            let limit = scheduler_config(&cfg).content_batch_size;
            assert_eq!(
                limit,
                setting.clamp(1, MAX_INDEX_BATCH_JOBS as u64) as usize
            );
            let mut queue = VecDeque::from(vec![dummy_job(); MAX_INDEX_BATCH_JOBS + 1]);
            let selected = take_legacy_batch(&mut queue, limit);
            assert_eq!(selected.len(), limit);
            assert_eq!(queue.len(), MAX_INDEX_BATCH_JOBS + 1 - limit);
        }
    }

    #[tokio::test]
    async fn deferral_capacity_is_checked_before_admission_using_full_identity() {
        let _guard = TEST_LOCK.lock().await;
        let runtime = SchedulerRuntime::new(&AppConfig::default());
        let mut batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: (1..=content_index::MAX_DEFERRED_KEYS)
                .map(|id| JobSpec {
                    operation: JobOperation::Reconcile,
                    file_id: id as u64,
                    ..dummy_job()
                })
                .collect(),
            reset_volumes: Vec::new(),
        };
        batch.jobs.push(batch.jobs[0].clone());
        check_index_batch_bounds(&batch).unwrap();
        for (volume_id, file_id) in [(2, 1), (1, (1u64 << 48) | 1)] {
            let last = batch.jobs.last_mut().unwrap();
            last.volume_id = volume_id;
            last.file_id = file_id;
            let error = submit_index_batch(batch.clone(), None).await.unwrap_err();
            assert!(error.to_string().contains("deferrable-identity bound"));
            assert!(runtime.job_rx.is_empty());
        }
        // Deletes and strict upserts do not consume a deferral slot.
        batch.jobs.last_mut().unwrap().operation = JobOperation::Delete;
        check_index_batch_bounds(&batch).unwrap();
        batch.jobs.last_mut().unwrap().operation = JobOperation::Upsert;
        check_index_batch_bounds(&batch).unwrap();
    }

    #[tokio::test]
    async fn adaptive_pause_preserves_durable_work_and_does_not_block_cleanup() -> anyhow::Result<()>
    {
        let _guard = TEST_LOCK.lock().await;
        let (mut cfg, expected_error) = blocked_dispatch_config()?;
        cfg.features.adaptive_scheduler = true;
        cfg.scheduler.content_batch_size = 1;
        let _restore = RestoreConfig::install(&cfg)?;
        let mut runtime = SchedulerRuntime::new(&cfg);
        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: (1..=4)
                .map(|id| JobSpec {
                    operation: JobOperation::Reconcile,
                    file_id: id,
                    ..dummy_job()
                })
                .collect(),
            reset_volumes: vec![1],
        };
        let mut submission = Box::pin(submit_index_batch(batch.clone(), None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(submission.as_mut().poll(&mut context).is_pending());
        let idle = IdleSample {
            state: scheduler::IdleState::DeepIdle,
            idle_for: Duration::from_secs(3600),
            since_state_change: Duration::from_secs(3600),
        };
        for _ in 0..2 {
            runtime
                .tick_with_observation(idle, observed_load(0.0, 95.0))
                .await;
            assert!(!content_admission_available());
            let pending = &runtime.pending_index.as_ref().unwrap().batch;
            assert_eq!(
                serde_json::to_value(pending)?,
                serde_json::to_value(&batch)?
            );
            assert!(submission.as_mut().poll(&mut context).is_pending());
        }
        // Disabling the optional policy restores baseline behavior on the
        // next tick, without splitting the already-admitted four-job batch.
        cfg.features.adaptive_scheduler = false;
        core_types::config::set_current_config(cfg.clone())?;
        runtime
            .tick_with_observation(idle, observed_load(0.0, 95.0))
            .await;
        assert!(runtime.adaptive.is_none());
        assert!(runtime.pending_index.is_none());
        assert_eq!(submission.await.unwrap_err().to_string(), expected_error);

        cfg.features.adaptive_scheduler = true;
        core_types::config::set_current_config(cfg)?;
        let cleanup = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: Vec::new(),
            reset_volumes: vec![1],
        };
        let mut submission = Box::pin(submit_index_batch(cleanup, None));
        assert!(submission.as_mut().poll(&mut context).is_pending());
        runtime
            .tick_with_observation(idle, observed_load(0.0, 95.0))
            .await;
        assert!(!content_admission_available());
        assert!(
            runtime.pending_index.is_none(),
            "cleanup must still reach dispatch"
        );
        assert_eq!(submission.await.unwrap_err().to_string(), expected_error);
        Ok(())
    }

    #[tokio::test]
    async fn pending_dispatch_observes_adaptive_enable_and_disable() -> anyhow::Result<()> {
        let _guard = TEST_LOCK.lock().await;
        let mut cfg = core_types::config::get_current_config();
        cfg.features.adaptive_scheduler = false;
        cfg.scheduler.idle_warm_seconds = 15;
        cfg.scheduler.idle_deep_seconds = 60;
        cfg.scheduler.cpu_hard_limit_pct = 40;
        let _restore = RestoreConfig::install(&cfg)?;
        let mut idle = IdleTracker::with_reader(
            Duration::from_secs(cfg.scheduler.idle_warm_seconds),
            Duration::from_secs(cfg.scheduler.idle_deep_seconds),
            (|| Some(3_600_000)) as fn() -> Option<u64>,
        );
        let mut load = SystemLoadSampler::new(cfg.scheduler.disk_busy_bytes_per_s);
        let allowed = AtomicBool::new(true);
        let mut adaptive = None;
        let (first_tx, first_rx) = oneshot::channel();
        let (second_tx, second_rx) = oneshot::channel();
        let mut releases = VecDeque::from([first_tx, second_tx]);
        let completion = async {
            cfg.features.adaptive_scheduler = true;
            core_types::config::set_current_config(cfg.clone())?;
            first_rx.await?;
            anyhow::ensure!(
                !allowed.load(Ordering::Relaxed),
                "adaptive pressure was ignored"
            );
            cfg.features.adaptive_scheduler = false;
            core_types::config::set_current_config(cfg)?;
            second_rx.await?;
            Ok::<(), anyhow::Error>(())
        };
        let mut sample = |tracker: &mut IdleTracker, _: &mut SystemLoadSampler| {
            releases.pop_front().unwrap().send(()).unwrap();
            (tracker.sample(), observed_load(0.0, 95.0))
        };
        tokio::time::timeout(
            Duration::from_secs(8),
            await_dispatch(
                completion,
                &mut idle,
                &mut load,
                &allowed,
                false,
                &mut sample,
                &mut adaptive,
            ),
        )
        .await
        .expect("both policy reloads must be observed while dispatch is pending")?;
        assert!(allowed.load(Ordering::Relaxed));
        assert!(adaptive.is_none());
        assert!(releases.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn pending_dispatch_is_not_restarted_while_observations_advance() {
        use std::cell::Cell;

        let starts = Cell::new(0);
        let refreshes = Cell::new(0);
        let (finish, finished) = oneshot::channel::<Result<uuid::Uuid, &'static str>>();
        let mut finish = Some(finish);
        let completion = async {
            starts.set(starts.get() + 1);
            finished
                .await
                .expect("refresh releases the single pending dispatch")
        };
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            await_with_refresh(completion, Duration::from_millis(1), || {
                refreshes.set(refreshes.get() + 1);
                if refreshes.get() == 3 {
                    finish
                        .take()
                        .unwrap()
                        .send(Err("worker commit failed"))
                        .unwrap();
                }
            }),
        )
        .await
        .expect("observations must not wait for dispatch completion");
        assert_eq!(result, Err("worker commit failed"));
        assert_eq!(starts.get(), 1);
        assert_eq!(refreshes.get(), 3);
    }

    #[tokio::test]
    async fn completed_dispatch_does_not_wait_for_or_invoke_a_refresh() {
        let receipt = uuid::Uuid::new_v4();
        assert_eq!(
            await_with_refresh(
                std::future::ready(receipt),
                Duration::from_secs(60),
                || panic!("a ready dispatch must not enter an unrelated native sensor"),
            )
            .await,
            receipt
        );
    }

    #[tokio::test]
    async fn running_dispatch_refresh_applies_reload_and_revokes_the_old_hint() -> anyhow::Result<()>
    {
        let _guard = TEST_LOCK.lock().await;
        let mut cfg = core_types::config::get_current_config();
        cfg.scheduler.idle_warm_seconds = 15;
        cfg.scheduler.idle_deep_seconds = 120;
        cfg.scheduler.disk_busy_bytes_per_s = 1_000;
        let _restore = RestoreConfig::install(&cfg)?;
        let mut idle = IdleTracker::with_reader(
            Duration::from_secs(15),
            Duration::from_secs(120),
            (|| Some(45_000)) as fn() -> Option<u64>,
        );
        let mut load = SystemLoadSampler::new(1_000);
        let mut adaptive = None;
        let allowed = AtomicBool::new(true);
        let (finish, finished) = oneshot::channel();
        let mut finish = Some(finish);
        let completion = async {
            // This runs after dispatch starts, before its first refresh.
            cfg.scheduler.idle_deep_seconds = 30;
            cfg.scheduler.disk_busy_bytes_per_s = 2_000;
            core_types::config::set_current_config(cfg)?;
            finished.await.map_err(anyhow::Error::from)
        };
        let mut sample = |tracker: &mut IdleTracker, sampler: &mut SystemLoadSampler| {
            assert!(!allowed.load(Ordering::Relaxed));
            let observed = tracker.sample();
            assert_eq!(observed.state, scheduler::IdleState::DeepIdle);
            assert_eq!(sampler.disk_threshold(), 2_000);
            finish.take().unwrap().send(()).unwrap();
            (
                observed,
                SystemLoad {
                    cpu_percent: 0.0,
                    mem_used_percent: 10.0,
                    disk_bytes_per_sec: 0,
                    disk_busy: false,
                    sample_duration: Duration::from_secs(1),
                    on_battery: false,
                    game_mode: false,
                },
            )
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            await_dispatch(
                completion,
                &mut idle,
                &mut load,
                &allowed,
                false,
                &mut sample,
                &mut adaptive,
            ),
        )
        .await
        .expect("dispatch must keep refreshing while completion is pending")?;
        assert!(allowed.load(Ordering::Relaxed));
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_observed_wait_neither_detaches_future_nor_acknowledges_commit()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let lock_path = root.path().join("observed-wait.lock");
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)?;
        lease.try_lock()?;
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)?;
        let (acknowledgement, mut response) = oneshot::channel::<()>();
        let completion = async move {
            std::future::pending::<()>().await;
            drop(lease);
            let _ = acknowledgement.send(());
        };
        let mut wait = Box::pin(await_with_refresh(
            completion,
            Duration::from_secs(60),
            || panic!("fixture cancellation occurs before the first refresh"),
        ));
        let mut context = Context::from_waker(Waker::noop());
        assert!(wait.as_mut().poll(&mut context).is_pending());
        assert!(contender.try_lock().is_err());
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        drop(wait);
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        contender.try_lock()?;
        Ok(())
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
                content_optional: false,
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
    async fn revoked_content_admission_returns_a_typed_non_commit_and_metadata_still_dispatches()
    -> anyhow::Result<()> {
        let _guard = TEST_LOCK.lock().await;
        init_basic_status_provider();
        let (cfg, expected_error) = blocked_dispatch_config()?;
        let _restore = RestoreConfig::install(&cfg)?;
        let mut runtime = SchedulerRuntime::new(&cfg);
        power_tick(&mut runtime, false).await;
        assert!(
            content_admission_available(),
            "fixture must first expose an open hint"
        );
        let mut batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: vec![JobSpec {
                operation: JobOperation::Reconcile,
                ..dummy_job()
            }],
            reset_volumes: Vec::new(),
        };
        let mut submitted = Box::pin(submit_optional_index_batch(batch.clone(), None));
        let mut context = Context::from_waker(Waker::noop());
        assert!(submitted.as_mut().poll(&mut context).is_pending());
        power_tick(&mut runtime, true).await;
        let error = submitted.await.unwrap_err();
        assert!(
            error.is::<ContentAdmissionDeferred>(),
            "admission must survive the ACK channel as a distinct non-commit"
        );
        assert!(runtime.pending_index.is_none());
        assert!(!PathBuf::from(&cfg.paths.content_index).exists());
        assert!(
            crate::status_snapshot()
                .scheduler_state
                .contains("waiting for AC power")
        );

        batch.jobs[0].operation = JobOperation::Delete;
        batch.jobs[0].path = PathBuf::new();
        let mut cleanup = Box::pin(submit_index_batch(batch, None));
        assert!(cleanup.as_mut().poll(&mut context).is_pending());
        power_tick(&mut runtime, true).await;
        // The real dispatcher now attempts its filesystem prerequisite. It
        // fails rather than acknowledging metadata that was never committed.
        assert_eq!(cleanup.await.unwrap_err().to_string(), expected_error);
        assert!(runtime.pending_index.is_none());
        assert!(!content_admission_available());
        Ok(())
    }

    #[test]
    fn admission_diagnostics_distinguish_power_idle_and_load_pauses() {
        let cfg = SchedulerConfig::default();
        let ready = observed_load(0.0, 10.0);
        let deep = scheduler::IdleState::DeepIdle;
        for (idle, load, expected) in [
            (
                deep,
                SystemLoad {
                    on_battery: true,
                    ..ready
                },
                "waiting for AC power (power_save_mode=true)",
            ),
            (
                deep,
                SystemLoad {
                    game_mode: true,
                    ..ready
                },
                "paused by Game Mode (power_save_mode=true)",
            ),
            (
                scheduler::IdleState::Active,
                ready,
                "waiting for deep idle (or idle observation unavailable)",
            ),
            (
                deep,
                SystemLoad {
                    cpu_percent: cfg.cpu_content_max,
                    ..ready
                },
                "waiting for CPU load to fall below the configured limit",
            ),
            (
                deep,
                SystemLoad {
                    disk_busy: true,
                    ..ready
                },
                "waiting for disk activity to subside",
            ),
            (deep, ready, "ready"),
        ] {
            let allowed = content_allowed(idle, load, &cfg, None);
            assert_eq!(allowed, expected == "ready");
            assert_eq!(
                content_admission_description(allowed, idle, load, &cfg),
                expected
            );
        }
    }

    #[tokio::test]
    async fn power_save_keeps_reconciliation_pending_until_extraction_is_allowed()
    -> anyhow::Result<()> {
        let _guard = TEST_LOCK.lock().await;
        init_basic_status_provider();
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
                assert!(
                    crate::status_snapshot()
                        .scheduler_state
                        .contains("waiting for AC power")
                );
            }

            // Changing the real cached setting must affect the very next tick;
            // the same on-battery observation then permits actual extraction.
            cfg.scheduler.power_save_mode = false;
            core_types::config::set_current_config(cfg.clone())?;
            power_tick(&mut runtime, true).await;
            assert!(content_admission_available());
            assert!(
                crate::status_snapshot()
                    .scheduler_state
                    .contains("content_admission=ready")
            );
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

    #[tokio::test]
    async fn hot_reload_updates_idle_tracker_disk_sampler_and_adaptive_policy_and_trims_on_deep_idle()
    -> anyhow::Result<()> {
        let _guard = TEST_LOCK.lock().await;
        let mut cfg = AppConfig::default();
        cfg.scheduler.idle_warm_seconds = 10;
        cfg.scheduler.idle_deep_seconds = 30;
        cfg.scheduler.disk_busy_bytes_per_s = 123_456;
        cfg.features.adaptive_scheduler = false;
        let _restore = RestoreConfig::install(&cfg)?;

        let mut runtime = SchedulerRuntime::new(&cfg);
        assert_eq!(
            runtime.idle.thresholds(),
            (Duration::from_secs(10), Duration::from_secs(30))
        );
        assert_eq!(runtime.load.disk_threshold(), 123_456);
        assert!(runtime.adaptive.is_none());

        // Hot-reload thresholds and enable adaptive_scheduler
        cfg.scheduler.idle_warm_seconds = 25;
        cfg.scheduler.idle_deep_seconds = 90;
        cfg.scheduler.disk_busy_bytes_per_s = 987_654;
        cfg.features.adaptive_scheduler = true;
        core_types::config::set_current_config(cfg.clone())?;

        let trims_before = crate::memory::working_set_trim_count();
        power_tick(&mut runtime, false).await;
        let trims_after_first_deep_idle = crate::memory::working_set_trim_count();
        assert_eq!(
            trims_after_first_deep_idle,
            trims_before + 1,
            "transitioning into DeepIdle must trigger working-set trim"
        );
        assert_eq!(
            runtime.idle.thresholds(),
            (Duration::from_secs(25), Duration::from_secs(90))
        );
        assert_eq!(runtime.load.disk_threshold(), 987_654);
        assert!(runtime.adaptive.is_some());

        // Staying in DeepIdle should not repeatedly trim on every tick
        power_tick(&mut runtime, false).await;
        assert_eq!(
            crate::memory::working_set_trim_count(),
            trims_after_first_deep_idle
        );
        Ok(())
    }
}
