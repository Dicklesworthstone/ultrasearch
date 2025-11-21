use scheduler::{
    SchedulerConfig, idle::IdleTracker, metrics::SystemLoadSampler, should_spawn_content_worker,
};
use std::time::Instant;

use crate::status_provider::{
    update_status_metrics, update_status_queue_state, update_status_scheduler_state,
};

/// Lightweight runtime wrapper that samples idle/load, surfaces queue state, and
/// decides when to spawn content workers. The actual worker execution is owned
/// by higher layers; this struct focuses on bookkeeping + status updates.
pub struct SchedulerRuntime {
    config: SchedulerConfig,
    idle: IdleTracker,
    load: SystemLoadSampler,
    last_content_spawn: Option<Instant>,
    active_workers: u32,
    queue_critical: usize,
    queue_metadata: usize,
    queue_content: usize,
}

impl SchedulerRuntime {
    pub fn new(config: SchedulerConfig) -> Self {
        let idle = IdleTracker::new(config.warm_idle, config.deep_idle);
        let load = SystemLoadSampler::new(config.disk_busy_threshold_bps);
        Self {
            config,
            idle,
            load,
            last_content_spawn: None,
            active_workers: 0,
            queue_critical: 0,
            queue_metadata: 0,
            queue_content: 0,
        }
    }

    /// Update queue sizes from the scheduler/worker system.
    pub fn set_queue_counts(&mut self, critical: usize, metadata: usize, content: usize) {
        self.queue_critical = critical;
        self.queue_metadata = metadata;
        self.queue_content = content;
    }

    pub fn set_active_workers(&mut self, active: u32) {
        self.active_workers = active;
    }

    /// Run a single tick: sample idle/load, update status/metrics, and decide
    /// whether a content worker should be spawned (returns a suggested batch
    /// size if so).
    pub fn tick(&mut self) -> Option<usize> {
        let idle_sample = self.idle.sample();
        let load = self.load.sample();
        let depth = (self.queue_critical + self.queue_metadata + self.queue_content) as u64;

        update_status_scheduler_state(format!(
            "idle={:?} cpu={:.1}% mem={:.1}% queues(c/m/t)={}/{}/{}",
            idle_sample.state,
            load.cpu_percent,
            load.mem_used_percent,
            self.queue_critical,
            self.queue_metadata,
            self.queue_content
        ));
        update_status_queue_state(Some(depth), Some(self.active_workers));
        update_status_metrics(None);

        let spawn = should_spawn_content_worker(
            self.queue_content,
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
