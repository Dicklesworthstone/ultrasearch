use scheduler::{
    SchedulerConfig, idle::IdleTracker, metrics::SystemLoadSampler, should_spawn_content_worker,
};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
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
    pub fn new(config: SchedulerConfig) -> Self {
        let idle = IdleTracker::new(config.warm_idle, config.deep_idle);
        let load = SystemLoadSampler::new(config.disk_busy_threshold_bps);
        Self {
            config,
            idle,
            load,
            last_content_spawn: None,
            live: LIVE_STATE.get_or_init(SchedulerLiveState::default),
        }
    }

    /// Run a single tick: sample idle/load, update status/metrics, and decide
    /// whether a content worker should be spawned (returns a suggested batch
    /// size if so).
    pub fn tick(&mut self) -> Option<usize> {
        let idle_sample = self.idle.sample();
        let load = self.load.sample();
        let crit = self.live.critical.load(Ordering::Relaxed);
        let meta = self.live.metadata.load(Ordering::Relaxed);
        let content = self.live.content.load(Ordering::Relaxed);
        let workers = self.live.active_workers.load(Ordering::Relaxed);
        let depth = (crit + meta + content) as u64;

        update_status_scheduler_state(format!(
            "idle={:?} cpu={:.1}% mem={:.1}% queues(c/m/t)={}/{}/{}",
            idle_sample.state, load.cpu_percent, load.mem_used_percent, crit, meta, content
        ));
        update_status_queue_state(Some(depth), Some(workers));
        update_status_metrics(None);

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

/// Update helpers for the live scheduler view. Intended to be called by the real
/// scheduler loop / worker manager.
pub fn set_live_queue_counts(critical: usize, metadata: usize, content: usize) {
    let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);
    live.critical.store(critical, Ordering::Relaxed);
    live.metadata.store(metadata, Ordering::Relaxed);
    live.content.store(content, Ordering::Relaxed);
}

pub fn set_live_active_workers(active: u32) {
    let live = LIVE_STATE.get_or_init(SchedulerLiveState::default);
    live.active_workers.store(active, Ordering::Relaxed);
}
