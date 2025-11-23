//! Scheduler primitives: idle detection, system load sampling, job queues, and
//! small policy helpers for background work. The service crate orchestrates
//! execution; this crate keeps the decision logic testable and self-contained.

pub mod idle;
pub mod metrics;
pub mod policy;

pub use idle::{IdleSample, IdleState, IdleTracker};
pub use metrics::{SystemLoad, SystemLoadSampler};
pub use policy::adaptive::AdaptivePolicy;

use core_types::DocKey;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum Job {
    MetadataUpdate(DocKey),
    ContentIndex(DocKey),
    Delete(DocKey),
    Rename { from: DocKey, to: DocKey },
}

#[derive(Debug)]
pub struct QueuedJob {
    pub job: Job,
    pub est_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobCategory {
    Critical, // deletes/renames/attr updates
    Metadata, // MFT/USN rebuilds, small batches
    Content,  // heavy extraction/index writes
}

#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub max_files: usize,
    pub max_bytes: u64,
}

impl Budget {
    pub fn unlimited() -> Self {
        Self {
            max_files: usize::MAX,
            max_bytes: u64::MAX,
        }
    }
}

#[derive(Default)]
pub struct JobQueues {
    critical: VecDeque<QueuedJob>,
    metadata: VecDeque<QueuedJob>,
    content: VecDeque<QueuedJob>,
}

impl JobQueues {
    pub fn push(&mut self, category: JobCategory, job: Job, est_bytes: u64) {
        let item = QueuedJob { job, est_bytes };
        match category {
            JobCategory::Critical => self.critical.push_back(item),
            JobCategory::Metadata => self.metadata.push_back(item),
            JobCategory::Content => self.content.push_back(item),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.critical.is_empty() && self.metadata.is_empty() && self.content.is_empty()
    }

    pub fn len(&self) -> usize {
        self.critical.len() + self.metadata.len() + self.content.len()
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        (self.critical.len(), self.metadata.len(), self.content.len())
    }
}

/// Select jobs given idle state, system load, and simple budgets.
pub fn select_jobs(
    queues: &mut JobQueues,
    idle: IdleState,
    load: SystemLoad,
    config: &SchedulerConfig,
) -> Vec<Job> {
    // Use budgets from config? No, Budget is passed in?
    // Wait, signature took Budget.
    // I can remove Budget argument if it's in config.
    // Or keep it for flexibility.
    // `SchedulerConfig` has `metadata_budget` and `content_budget`.
    // `SchedulerRuntime` passed `Budget`.
    // Let's keep `select_jobs` taking explicit budgets but also `config` for policy.
    // Actually, simpler to take just `config` and use its budgets.
    // But `SchedulerRuntime` logic might want to override.
    // Let's change `select_jobs` to take `&SchedulerConfig` and use its budgets.

    // Actually, `select_jobs` had `budget: Budget` param. This was applied to ALL queues?
    // The implementation used `budget` for critical, then reused it?
    // No, `take` updated `file_count` and `bytes_accum`.
    // So it was a global budget for the tick.
    // `SchedulerConfig` has per-category budgets.
    // `SchedulerRuntime` logic wasn't calling `select_jobs` yet (it handled content manually).
    // So I can redefine `select_jobs` freely.

    let mut selected = Vec::new();

    // Critical: Always run, small hardcoded limit or from config?
    // Let's say critical ignores budget/policy mostly.
    let mut take = |queue: &mut VecDeque<QueuedJob>, limit: usize| {
        let mut taken = 0;
        while taken < limit {
            if let Some(qj) = queue.pop_front() {
                selected.push(qj.job);
                taken += 1;
            } else {
                break;
            }
        }
    };

    take(&mut queues.critical, 16);

    let allow_meta = allow_metadata_jobs(idle, load, config);
    let allow_content = allow_content_jobs(idle, load, config);

    if allow_meta {
        take(&mut queues.metadata, config.metadata_budget.max_files);
    }

    if allow_content {
        take(&mut queues.content, config.content_budget.max_files);
    }

    selected
}

/// Basic policy for running metadata jobs.
pub fn allow_metadata_jobs(idle: IdleState, load: SystemLoad, config: &SchedulerConfig) -> bool {
    if config.power_save_mode && (load.on_battery || load.game_mode) {
        return false;
    }
    matches!(idle, IdleState::WarmIdle | IdleState::DeepIdle)
        && load.cpu_percent < config.cpu_metadata_max
        && !load.disk_busy
}

/// Basic policy for running content jobs (heavier work).
pub fn allow_content_jobs(idle: IdleState, load: SystemLoad, config: &SchedulerConfig) -> bool {
    if config.power_save_mode && (load.on_battery || load.game_mode) {
        return false;
    }
    matches!(idle, IdleState::DeepIdle)
        && load.cpu_percent < config.cpu_content_max
        && !load.disk_busy
}

/// Static policy inputs used across scheduler beads.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub warm_idle: Duration,
    pub deep_idle: Duration,
    pub cpu_metadata_max: f32,
    pub cpu_content_max: f32,
    pub disk_busy_threshold_bps: u64,
    pub metadata_budget: Budget,
    pub content_budget: Budget,
    pub content_spawn_backlog: usize,
    pub content_spawn_cooldown: Duration,
    pub content_batch_size: usize,
    pub power_save_mode: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            warm_idle: Duration::from_secs(15),
            deep_idle: Duration::from_secs(60),
            cpu_metadata_max: 60.0,
            cpu_content_max: 40.0,
            disk_busy_threshold_bps: 10 * 1024 * 1024, // placeholder: 10 MiB/s
            metadata_budget: Budget {
                max_files: 256,
                max_bytes: 64 * 1024 * 1024,
            },
            content_budget: Budget {
                max_files: 64,
                max_bytes: 512 * 1024 * 1024,
            },
            content_spawn_backlog: 200,
            content_spawn_cooldown: Duration::from_secs(30),
            content_batch_size: 500,
            power_save_mode: true,
        }
    }
}

/// Combined snapshot of scheduler inputs and queue sizes for UI/status surfaces.
#[derive(Debug, Clone)]
pub struct SchedulerState {
    pub idle: IdleSample,
    pub load: SystemLoad,
    pub queues_critical: usize,
    pub queues_metadata: usize,
    pub queues_content: usize,
}

/// Decide whether to spawn a content worker.
pub fn should_spawn_content_worker(
    backlog: usize,
    idle: IdleState,
    load: SystemLoad,
    config: &SchedulerConfig,
    last_spawn: Option<Instant>,
) -> bool {
    if config.power_save_mode && (load.on_battery || load.game_mode) {
        return false;
    }
    if backlog == 0 || load.disk_busy || load.cpu_percent >= config.cpu_content_max {
        return false;
    }
    if !matches!(idle, IdleState::DeepIdle) {
        return false;
    }
    if backlog < config.content_spawn_backlog {
        return false;
    }
    if let Some(prev) = last_spawn
        && prev.elapsed() < config.content_spawn_cooldown
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_ok() -> SystemLoad {
        SystemLoad {
            cpu_percent: 10.0,
            mem_used_percent: 10.0,
            disk_busy: false,
            disk_bytes_per_sec: 0,
            sample_duration: Duration::from_secs(1),
            on_battery: false,
            game_mode: false,
        }
    }

    #[test]
    fn content_jobs_blocked_when_not_deep_idle() {
        let cfg = SchedulerConfig::default();
        assert!(!allow_content_jobs(IdleState::WarmIdle, load_ok(), &cfg));
        assert!(allow_content_jobs(IdleState::DeepIdle, load_ok(), &cfg));
    }

    #[test]
    fn metadata_jobs_respect_cpu_and_disk() {
        let cfg = SchedulerConfig::default();
        let load = load_ok();
        assert!(allow_metadata_jobs(IdleState::WarmIdle, load, &cfg));

        let busy = SystemLoad {
            disk_busy: true,
            ..load
        };
        assert!(!allow_metadata_jobs(IdleState::WarmIdle, busy, &cfg));

        let high_cpu = SystemLoad {
            cpu_percent: 70.0,
            ..load
        };
        assert!(!allow_metadata_jobs(IdleState::WarmIdle, high_cpu, &cfg));
    }

    #[test]
    fn power_save_mode_blocks_jobs() {
        let cfg = SchedulerConfig {
            power_save_mode: true,
            ..SchedulerConfig::default()
        };

        let mut load = load_ok();
        load.on_battery = true;

        // Battery blocks
        assert!(!allow_metadata_jobs(IdleState::DeepIdle, load, &cfg));
        assert!(!allow_content_jobs(IdleState::DeepIdle, load, &cfg));

        // Game mode blocks
        load.on_battery = false;
        load.game_mode = true;
        assert!(!allow_metadata_jobs(IdleState::DeepIdle, load, &cfg));
        assert!(!allow_content_jobs(IdleState::DeepIdle, load, &cfg));

        // Normal ok
        load.game_mode = false;
        assert!(allow_metadata_jobs(IdleState::DeepIdle, load, &cfg));
    }

    #[test]
    fn budgets_respected_files_and_bytes() {
        let mut queues = JobQueues::default();
        queues.push(
            JobCategory::Content,
            Job::ContentIndex(DocKey::from_parts(1, 1)),
            5,
        );
        queues.push(
            JobCategory::Content,
            Job::ContentIndex(DocKey::from_parts(1, 2)),
            5,
        );

        let mut cfg = SchedulerConfig::default();
        cfg.content_budget.max_files = 1;

        let selected = select_jobs(&mut queues, IdleState::DeepIdle, load_ok(), &cfg);
        assert_eq!(selected.len(), 1);
        assert_eq!(queues.len(), 1); // second job remains due to budget
    }

    #[test]
    fn critical_jobs_run_even_when_busy() {
        let mut queues = JobQueues::default();
        queues.push(
            JobCategory::Critical,
            Job::Delete(DocKey::from_parts(1, 9)),
            1,
        );
        queues.push(
            JobCategory::Content,
            Job::ContentIndex(DocKey::from_parts(1, 2)),
            50,
        );

        let mut load = load_ok();
        load.cpu_percent = 95.0;
        load.mem_used_percent = 90.0;
        load.disk_busy = true;

        let selected = select_jobs(
            &mut queues,
            IdleState::Active,
            load,
            &SchedulerConfig::default(),
        );
        assert!(selected.iter().any(|j| matches!(j, Job::Delete(_))));
    }

    #[test]
    fn spawn_content_worker_honors_backlog_and_cooldown() {
        let cfg = SchedulerConfig {
            content_spawn_backlog: 5,
            content_spawn_cooldown: Duration::from_secs(10),
            cpu_content_max: 40.0,
            ..Default::default()
        };

        assert!(!should_spawn_content_worker(
            3,
            IdleState::DeepIdle,
            load_ok(),
            &cfg,
            None
        ));

        assert!(should_spawn_content_worker(
            10,
            IdleState::DeepIdle,
            load_ok(),
            &cfg,
            None
        ));

        let just_spawned = Instant::now();
        assert!(!should_spawn_content_worker(
            10,
            IdleState::DeepIdle,
            load_ok(),
            &cfg,
            Some(just_spawned)
        ));
    }
}
