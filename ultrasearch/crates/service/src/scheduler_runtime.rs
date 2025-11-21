use crate::dispatcher::job_dispatch::{JobDispatcher, JobSpec};
use crate::status_provider::{
    update_status_metrics, update_status_queue_state, update_status_scheduler_state,
};
use core_types::config::AppConfig;
use scheduler::{
    SchedulerConfig, allow_content_jobs, idle::IdleTracker, metrics::SystemLoadSampler,
};
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Debug, Default)]
struct SchedulerLiveState {
    critical: AtomicUsize,
    metadata: AtomicUsize,
    content: AtomicUsize,
    active_workers: AtomicU32,
}

static LIVE_STATE: OnceLock<SchedulerLiveState> = OnceLock::new();

/// Runtime wrapper that drives a simple scheduling loop and dispatches content batches.
pub struct SchedulerRuntime {
    config: SchedulerConfig,
    idle: IdleTracker,
    load: SystemLoadSampler,
    content_jobs: VecDeque<JobSpec>,
    dispatcher: JobDispatcher,
    live: &'static SchedulerLiveState,
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
            ..SchedulerConfig::default()
        };

        let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);

        Self {
            idle: IdleTracker::new(config.warm_idle, config.deep_idle),
            load: SystemLoadSampler::new(config.disk_busy_threshold_bps),
            content_jobs: VecDeque::new(),
            dispatcher: JobDispatcher::new(app_cfg),
            config,
            live,
        }
    }

    /// Submit a content indexing job (path + doc ids).
    pub fn submit_content_job(&mut self, job: JobSpec) {
        self.content_jobs.push_back(job);
        self.update_live_counts();
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
        let idle_sample = self.idle.sample();
        let load = self.load.sample();

        // Update status snapshot counts + active workers.
        let ct = self.content_jobs.len();
        let workers = self.live.active_workers.load(Ordering::Relaxed);
        update_status_scheduler_state(format!(
            "idle={:?} cpu={:.1}% mem={:.1}% queues(c/m/t)={}/0/0",
            idle_sample.state, load.cpu_percent, load.mem_used_percent, ct
        ));
        update_status_queue_state(Some(ct as u64), Some(workers));
        update_status_metrics(None);

        // Gate metadata/content on policies; we only have content jobs for now.
        let allow_content = allow_content_jobs(
            idle_sample.state,
            scheduler::metrics::SystemLoad {
                cpu_percent: load.cpu_percent,
                mem_used_percent: load.mem_used_percent,
                disk_busy: load.disk_busy,
                disk_bytes_per_sec: load.disk_bytes_per_sec,
                sample_duration: load.sample_duration,
            },
        );

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
