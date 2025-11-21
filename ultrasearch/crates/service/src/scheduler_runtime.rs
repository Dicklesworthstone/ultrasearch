use crate::dispatcher::job_dispatch::{JobDispatcher, JobSpec};
use crate::status_provider::{
    update_status_metrics, update_status_queue_state, update_status_scheduler_state,
};
use core_types::config::AppConfig;
use scheduler::{
    Job, JobCategory, JobQueues, SchedulerConfig, idle::IdleTracker, metrics::SystemLoadSampler,
    select_jobs,
};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

/// Runtime wrapper that drives the scheduling loop.
pub struct SchedulerRuntime {
    config: SchedulerConfig,
    idle: IdleTracker,
    load: SystemLoadSampler,
    queues: JobQueues,
    dispatcher: JobDispatcher,
    live: &'static SchedulerLiveState,
}

#[derive(Debug, Default)]
struct SchedulerLiveState {
    critical: AtomicUsize,
    metadata: AtomicUsize,
    content: AtomicUsize,
    active_workers: AtomicU32,
}

static LIVE_STATE: OnceLock<SchedulerLiveState> = OnceLock::new();

impl SchedulerRuntime {
    pub fn new(app_config: &AppConfig) -> Self {
        let config = SchedulerConfig {
            warm_idle: Duration::from_secs(app_config.scheduler.idle_warm_seconds),
            deep_idle: Duration::from_secs(app_config.scheduler.idle_deep_seconds),
            cpu_metadata_max: app_config.scheduler.cpu_soft_limit_pct as f32,
            cpu_content_max: app_config.scheduler.cpu_hard_limit_pct as f32,
            disk_busy_threshold_bps: app_config.scheduler.disk_busy_bytes_per_s,
            content_batch_size: app_config.scheduler.content_batch_size as usize,
            ..SchedulerConfig::default()
        };

        let idle = IdleTracker::new(config.warm_idle, config.deep_idle);
        let load = SystemLoadSampler::new(config.disk_busy_threshold_bps);
        let dispatcher = JobDispatcher::new(app_config);

        Self {
            config,
            idle,
            load,
            queues: JobQueues::default(),
            dispatcher,
            live: LIVE_STATE.get_or_init(SchedulerLiveState::default),
        }
    }

    pub fn submit(&mut self, category: JobCategory, job: Job, est_bytes: u64) {
        self.queues.push(category, job, est_bytes);
        self.update_live_counts();
    }

    fn update_live_counts(&self) {
        let (c, m, t) = self.queues.counts();
        self.live.critical.store(c, Ordering::Relaxed);
        self.live.metadata.store(m, Ordering::Relaxed);
        self.live.content.store(t, Ordering::Relaxed);
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
        
        // Update status
        let (c, m, t) = self.queues.counts();
        update_status_scheduler_state(format!(
            "idle={:?} cpu={:.1}% mem={:.1}% queues={}/{}/{}",
            idle_sample.state, load.cpu_percent, load.mem_used_percent, c, m, t
        ));
        update_status_queue_state(Some((c + m + t) as u64), None);
        update_status_metrics(None);

        // Select jobs
        let selected = select_jobs(
            &mut self.queues,
            idle_sample.state,
            load,
            self.config.content_budget, // Use content budget for all for now?
        );

        if !selected.is_empty() {
            tracing::info!("Selected {} jobs for execution", selected.len());
            self.update_live_counts();
            
            // Convert Job to JobSpec and dispatch
            let mut specs: Vec<JobSpec> = Vec::new();
            for job in selected {
                if let Job::ContentIndex(_key) = job {
                    // TODO: Resolve path logic
                }
            }
            
            if !specs.is_empty() {
                if let Err(e) = self.dispatcher.spawn_batch(specs).await {
                    tracing::error!("Batch dispatch failed: {}", e);
                }
            }
        }
    }
}