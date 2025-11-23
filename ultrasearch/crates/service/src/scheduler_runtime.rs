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
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task;

#[derive(Debug, Default)]
struct SchedulerLiveState {
    critical: AtomicUsize,
    metadata: AtomicUsize,
    content: AtomicUsize,
    active_workers: AtomicU32,
}

static LIVE_STATE: OnceLock<SchedulerLiveState> = OnceLock::new();
static JOB_SENDER: OnceLock<mpsc::UnboundedSender<JobSpec>> = OnceLock::new();

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
        self.content_jobs.push_back(job);
        self.update_live_counts();
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
        update_status_scheduler_state(format!(
            "idle={:?} cpu={:.1}% mem={:.1}% queue(content)={}",
            idle_sample.state, load.cpu_percent, load.mem_used_percent, ct
        ));
        update_status_queue_state(Some(ct as u64), Some(workers));
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
}

/// Enqueue a content indexing job for the scheduler loop.
/// Returns `false` if the scheduler has not been initialized yet.
pub fn enqueue_content_job(job: JobSpec) -> bool {
    match JOB_SENDER.get() {
        Some(tx) => tx.send(job).is_ok(),
        None => {
            tracing::warn!("scheduler not initialized; dropping content job");
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
