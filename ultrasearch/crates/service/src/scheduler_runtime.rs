use crate::dispatcher::job_dispatch::{JobDispatcher, JobSpec};
use crate::scanner;
use crate::status_provider::{
    update_status_metrics, update_status_queue_state, update_status_scheduler_state,
};
use core_types::FileMeta;
use core_types::config::{AppConfig, ExtractSection};
use scheduler::{
    SchedulerConfig, allow_content_jobs, idle::IdleTracker, metrics::SystemLoadSampler,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task;

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
static JOB_SENDER: OnceLock<mpsc::UnboundedSender<JobSpec>> = OnceLock::new();
static RUNTIME_ACTIVE: AtomicBool = AtomicBool::new(false);

const MAX_CONTENT_QUEUE: usize = 10_000;

/// Runtime wrapper that drives a simple scheduling loop and dispatches content batches.
pub struct SchedulerRuntime {
    config: SchedulerConfig,
    idle: IdleTracker,
    load: SystemLoadSampler,
    content_jobs: VecDeque<JobSpec>,
    job_rx: mpsc::UnboundedReceiver<JobSpec>,
    dispatcher: JobDispatcher,
    live: &'static SchedulerLiveState,
    current_volumes: Vec<String>,
    force_allow_content: bool,
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
        let (tx, rx) = mpsc::unbounded_channel();
        // Keep a global sender so producers (scanner/USN) can enqueue work from anywhere.
        let _ = JOB_SENDER.get_or_init(|| tx.clone());

        RUNTIME_ACTIVE.store(true, Ordering::Relaxed);

        Self {
            idle: IdleTracker::new(config.warm_idle, config.deep_idle),
            load: SystemLoadSampler::new(config.disk_busy_threshold_bps),
            content_jobs: VecDeque::new(),
            job_rx: rx,
            dispatcher: JobDispatcher::new(app_cfg),
            config,
            live,
            current_volumes: app_cfg.volumes.clone(),
            force_allow_content: false,
        }
    }

    fn update_config(&mut self, app_cfg: &AppConfig) {
        // Check for volume changes
        if self.current_volumes != app_cfg.volumes {
            tracing::info!("Volume configuration changed, triggering rescan...");
            self.current_volumes = app_cfg.volumes.clone();
            let cfg_clone = app_cfg.clone();

            // Spawn blocking task to rescan
            task::spawn_blocking(move || match scanner::scan_volumes(&cfg_clone) {
                Ok(new_jobs) => {
                    for job in new_jobs {
                        enqueue_content_job(job);
                    }
                }
                Err(e) => tracing::error!("Failed to rescan volumes after config update: {}", e),
            });
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
    pub fn submit_content_job(&mut self, job: JobSpec) {
        self.push_job(job);
    }

    /// Submit a batch of content indexing jobs.
    pub fn submit_content_jobs<I>(&mut self, jobs: I)
    where
        I: IntoIterator<Item = JobSpec>,
    {
        for job in jobs {
            self.submit_content_job(job);
        }
    }

    /// Force content jobs to run regardless of idle/load (useful for tests).
    pub fn force_allow_content(&mut self) {
        self.force_allow_content = true;
    }

    fn update_live_counts(&self) {
        self.live
            .content
            .store(self.content_jobs.len(), Ordering::Relaxed);
        // Metadata/critical queues not implemented yet; keep zero.
        self.live.critical.store(0, Ordering::Relaxed);
        self.live.metadata.store(0, Ordering::Relaxed);
    }

    pub async fn run_loop(mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            self.tick().await;
        }
    }

    pub async fn tick(&mut self) {
        // Reload config dynamically (from memory cache updated by IPC)
        let app_cfg = core_types::config::get_current_config();
        self.update_config(&app_cfg);

        // Drain any newly submitted content jobs.
        while let Ok(job) = self.job_rx.try_recv() {
            self.content_jobs.push_back(job);
        }
        self.update_live_counts();

        let idle_sample = self.idle.sample();
        let load = self.load.sample();

        // Update status snapshot counts + active workers.
        let ct = self.content_jobs.len();
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
        update_status_metrics(None);

        // Gate metadata/content on policies; we only have content jobs for now.
        let allow_content =
            self.force_allow_content || allow_content_jobs(idle_sample.state, load, &self.config);

        if allow_content && !self.content_jobs.is_empty() {
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

            self.update_live_counts();
            self.live.active_workers.fetch_add(1, Ordering::Relaxed);

            if let Err(e) = self.dispatcher.spawn_batch(batch).await {
                tracing::error!("failed to dispatch batch: {e:?}");
            }

            self.live.active_workers.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn push_job(&mut self, job: JobSpec) {
        if self.content_jobs.len() >= MAX_CONTENT_QUEUE {
            self.live.dropped_content.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                queue_len = self.content_jobs.len(),
                max = MAX_CONTENT_QUEUE,
                "content queue full; dropping job for {:?}",
                job.path
            );
            return;
        }
        self.content_jobs.push_back(job);
        self.live.enqueued_content.fetch_add(1, Ordering::Relaxed);
        self.update_live_counts();
    }
}

/// Enqueue a content indexing job for the scheduler loop.
/// Returns `false` if the scheduler has not been initialized yet.
pub fn enqueue_content_job(job: JobSpec) -> bool {
    if !RUNTIME_ACTIVE.load(Ordering::Relaxed) {
        tracing::warn!("scheduler not initialized; dropping content job");
        let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);
        live.dropped_content.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    match JOB_SENDER.get() {
        Some(tx) => {
            if tx.send(job).is_ok() {
                true
            } else {
                let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);
                live.dropped_content.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
        None => {
            tracing::warn!("scheduler not initialized; dropping content job");
            let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);
            live.dropped_content.fetch_add(1, Ordering::Relaxed);
            false
        }
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
        RUNTIME_ACTIVE.store(false, Ordering::Relaxed);
    }
}

/// Convert a `FileMeta` into a `JobSpec` if it looks indexable.
pub fn content_job_from_meta(meta: &FileMeta, extract: &ExtractSection) -> Option<JobSpec> {
    if meta.flags.is_dir() {
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
        volume_id: meta.volume,
        file_id,
        path,
        max_bytes: Some(to_usize(extract.max_bytes_per_file)),
        max_chars: Some(to_usize(extract.max_chars_per_file)),
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

    fn dummy_job() -> JobSpec {
        JobSpec {
            volume_id: 1,
            file_id: 1,
            path: PathBuf::from("C:\\dummy"),
            max_bytes: None,
            max_chars: None,
        }
    }

    #[test]
    fn enqueue_without_runtime_increments_dropped() {
        let before = live_counters().1;
        let ok = enqueue_content_job(dummy_job());
        assert!(!ok);
        let after = live_counters().1;
        assert!(after > before, "dropped counter should increase");
    }

    #[test]
    fn submit_content_job_increments_enqueued_counter() {
        // Initialize status provider once for metric updates (harmless if already set).
        let _ = init_basic_status_provider();
        let cfg = AppConfig::default();
        let mut rt = SchedulerRuntime::new(&cfg);

        let before = live_counters().0;
        rt.submit_content_job(dummy_job());
        rt.update_live_counts();
        let after = live_counters().0;
        assert_eq!(after, before + 1, "enqueued counter should increase");
    }
}
