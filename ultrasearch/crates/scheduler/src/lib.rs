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
    budget: Budget,
) -> Vec<Job> {
    if budget.max_files == 0 || budget.max_bytes == 0 {
        return Vec::new();
    }

    let mut selected = Vec::new();
    let mut file_count = 0usize;
    let mut bytes_accum = 0u64;

    let mut take = |queue: &mut VecDeque<QueuedJob>, limit: usize| {
        for _ in 0..limit {
            if file_count >= budget.max_files {
                break;
            }
            if let Some(qj) = queue.pop_front() {
                if bytes_accum + qj.est_bytes > budget.max_bytes {
                    // stop taking from this queue to respect byte budget
                    queue.push_front(qj);
                    break;
                }
                selected.push(qj.job);
                file_count += 1;
                bytes_accum += qj.est_bytes;
            } else {
                break;
            }
        }
    };

    // Always process some critical jobs regardless of load.
    take(&mut queues.critical, 16);

    // Gate metadata/content on idle state and load thresholds.
    let allow_metadata = allow_metadata_jobs(idle, load);
    let allow_content = allow_content_jobs(idle, load);

    if allow_metadata {
        take(&mut queues.metadata, 256);
    }

    if allow_content {
        take(&mut queues.content, 64);
    }

    selected
}

/// Basic policy for running metadata jobs.
pub fn allow_metadata_jobs(idle: IdleState, load: SystemLoad) -> bool {
    matches!(idle, IdleState::WarmIdle | IdleState::DeepIdle)
        && load.cpu_percent < 60.0
        && !load.disk_busy
}

/// Basic policy for running content jobs (heavier work).
pub fn allow_content_jobs(idle: IdleState, load: SystemLoad) -> bool {
    matches!(idle, IdleState::DeepIdle) && load.cpu_percent < 40.0 && !load.disk_busy
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
        }
    }

    #[test]
    fn content_jobs_blocked_when_not_deep_idle() {
        assert!(!allow_content_jobs(IdleState::WarmIdle, load_ok()));
        assert!(allow_content_jobs(IdleState::DeepIdle, load_ok()));
    }

    #[test]
    fn metadata_jobs_respect_cpu_and_disk() {
        let load = load_ok();
        assert!(allow_metadata_jobs(IdleState::WarmIdle, load));

        let busy = SystemLoad {
            disk_busy: true,
            ..load
        };
        assert!(!allow_metadata_jobs(IdleState::WarmIdle, busy));

        let high_cpu = SystemLoad {
            cpu_percent: 70.0,
            ..load
        };
        assert!(!allow_metadata_jobs(IdleState::WarmIdle, high_cpu));
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

        let selected = select_jobs(
            &mut queues,
            IdleState::DeepIdle,
            load_ok(),
            Budget {
                max_files: 1,
                max_bytes: 8,
            },
        );
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
            Budget {
                max_files: 10,
                max_bytes: 1_000,
            },
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
