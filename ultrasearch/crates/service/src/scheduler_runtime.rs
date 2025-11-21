use scheduler::{
    Job, JobCategory, JobQueues, SchedulerConfig, idle::IdleTracker, metrics::SystemLoadSampler,
    should_spawn_content_worker,
};
use std::sync::Arc;
use std::time::Instant;

use crate::status_provider::BasicStatusProvider;

/// Lightweight runtime wrapper that samples idle/load, surfaces queue state, and
/// decides when to spawn content workers. Worker orchestration lives elsewhere.
pub struct SchedulerRuntime {
    config: SchedulerConfig,
    queues: JobQueues,
    idle: IdleTracker,
    load: SystemLoadSampler,
    last_content_spawn: Option<Instant>,
    status: Arc<BasicStatusProvider>,
    active_workers: u32,
}

impl SchedulerRuntime {
    pub fn new(config: SchedulerConfig, status: Arc<BasicStatusProvider>) -> Self {
        let idle = IdleTracker::new(config.warm_idle, config.deep_idle);
        let load = SystemLoadSampler::new(config.disk_busy_threshold_bps);
        Self {
            config,
            queues: JobQueues::default(),
            idle,
            load,
            last_content_spawn: None,
            status,
            active_workers: 0,
        }
    }

    /// Inject a batch of jobs into the queues (counts only for now).
    pub fn enqueue(&mut self, category: JobCategory, job: Job, est_bytes: u64) {
        self.queues.push(category, job, est_bytes);
    }

    /// Update the number of active workers for status surfaces.
    pub fn set_active_workers(&mut self, count: u32) {
        self.active_workers = count;
    }

    /// Run a single sampling tick; returns a suggested content batch size if a worker should spawn.
    pub fn tick(&mut self) -> Option<usize> {
        let idle_sample = self.idle.sample();
        let load = self.load.sample();
        let (crit, meta, content) = self.queues.counts();
        let depth = (crit + meta + content) as u64;

        self.status.update_scheduler_state(format!(
            "idle={:?} cpu={:.1}% mem={:.1}% queues(c/m/t)={}/{}/{}",
            idle_sample.state, load.cpu_percent, load.mem_used_percent, crit, meta, content
        ));
        self.status
            .update_queue_state(Some(depth), Some(self.active_workers));

        let spawn = should_spawn_content_worker(
            content,
            idle_sample.state,
            load,
            &self.config,
            self.last_content_spawn,
        );
        if spawn {
            self.last_content_spawn = Some(Instant::now());
            return Some(self.config.content_batch_size);
        }

        None
    }
}
