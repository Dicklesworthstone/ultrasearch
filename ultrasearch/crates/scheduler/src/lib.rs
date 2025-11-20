//! Scheduler primitives: idle detection and system load sampling.
//!
//! This crate provides lightweight building blocks for the service:
//! - `IdleTracker`: classifies user activity into Active/WarmIdle/DeepIdle using
//!   GetLastInputInfo on Windows with configurable thresholds.
//! - `SystemLoadSampler`: periodically samples CPU/memory/disk load via `sysinfo`.
//! - Stubs for job selection that will later consume queues and thresholds.
//!
//! The actual scheduling loop lives in the service crate; this crate just owns
//! reusable sampling logic.

use core_types::DocKey;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleState {
    Active,
    WarmIdle,
    DeepIdle,
}

/// Tracks user idle time based on GetLastInputInfo (Windows).
pub struct IdleTracker {
    warm_idle_ms: u64,
    deep_idle_ms: u64,
}

impl IdleTracker {
    /// Create a tracker with thresholds in milliseconds.
    pub fn new(warm_idle_ms: u64, deep_idle_ms: u64) -> Self {
        Self {
            warm_idle_ms,
            deep_idle_ms,
        }
    }

    /// Sample current idle state.
    pub fn sample(&self) -> IdleState {
        match idle_elapsed_ms() {
            None => IdleState::Active,
            Some(elapsed) if elapsed >= self.deep_idle_ms => IdleState::DeepIdle,
            Some(elapsed) if elapsed >= self.warm_idle_ms => IdleState::WarmIdle,
            _ => IdleState::Active,
        }
    }
}

/// System load snapshot.
#[derive(Debug, Clone, Copy)]
pub struct SystemLoad {
    pub cpu_percent: f32,
    pub mem_used_percent: f32,
    pub disk_busy: bool,
}

pub struct SystemLoadSampler {
    sys: sysinfo::System,
    /// Bytes/sec threshold to consider disk busy.
    pub disk_busy_threshold: u64,
    last_read_bytes: u64,
    last_write_bytes: u64,
    last_sample: Instant,
}

impl SystemLoadSampler {
    pub fn new(disk_busy_threshold: u64) -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        sys.refresh_cpu();
        sys.refresh_disks_list();
        sys.refresh_disks();

        let (read, write) = disk_totals(&sys);

        Self {
            sys,
            disk_busy_threshold,
            last_read_bytes: read,
            last_write_bytes: write,
            last_sample: Instant::now(),
        }
    }

    pub fn sample(&mut self) -> SystemLoad {
        self.sys.refresh_cpu();
        self.sys.refresh_memory();
        self.sys.refresh_disks();

        let now = Instant::now();
        let dt = now.saturating_duration_since(self.last_sample).max(Duration::from_millis(1));
        let secs = dt.as_secs_f64();

        let cpu_percent = self.sys.global_cpu_info().cpu_usage();
        let total = self.sys.total_memory().max(1);
        let mem_used_percent = (self.sys.used_memory() as f32 / total as f32) * 100.0;

        let (read, write) = disk_totals(&self.sys);
        let read_bps =
            ((read.saturating_sub(self.last_read_bytes)) as f64 / secs).round() as u64;
        let write_bps =
            ((write.saturating_sub(self.last_write_bytes)) as f64 / secs).round() as u64;

        self.last_sample = now;
        self.last_read_bytes = read;
        self.last_write_bytes = write;

        let disk_busy = read_bps >= self.disk_busy_threshold || write_bps >= self.disk_busy_threshold;

        SystemLoad {
            cpu_percent,
            mem_used_percent,
            disk_busy,
        }
    }
}

fn disk_totals(sys: &sysinfo::System) -> (u64, u64) {
    sys.disks().iter().fold((0, 0), |(r_acc, w_acc), d| {
        (r_acc + d.total_read_bytes(), w_acc + d.total_written_bytes())
    })
}

#[derive(Debug)]
pub enum Job {
    MetadataUpdate(DocKey),
    ContentIndex(DocKey),
    Delete(DocKey),
    Rename { from: DocKey, to: DocKey },
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
    critical: VecDeque<Job>,
    metadata: VecDeque<Job>,
    content: VecDeque<Job>,
}

impl JobQueues {
    pub fn push(&mut self, category: JobCategory, job: Job) {
        match category {
            JobCategory::Critical => self.critical.push_back(job),
            JobCategory::Metadata => self.metadata.push_back(job),
            JobCategory::Content => self.content.push_back(job),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.critical.is_empty() && self.metadata.is_empty() && self.content.is_empty()
    }

    pub fn len(&self) -> usize {
        self.critical.len() + self.metadata.len() + self.content.len()
    }
}

/// Select jobs given idle state, system load, and simple budgets.
pub fn select_jobs(
    queues: &mut JobQueues,
    idle: IdleState,
    load: SystemLoad,
    budget: Budget,
) -> Vec<Job> {
    let mut selected = Vec::new();
    let mut file_count = 0usize;
    let mut bytes_accum = 0u64;

    let mut take = |queue: &mut VecDeque<Job>, limit: usize| {
        for _ in 0..limit {
            if file_count >= budget.max_files {
                break;
            }
            if let Some(job) = queue.pop_front() {
                selected.push(job);
                file_count += 1;
            } else {
                break;
            }
        }
    };

    // Always process some critical jobs regardless of load.
    take(&mut queues.critical, 16);

    // Gate metadata/content on idle state and load thresholds.
    let allow_metadata = matches!(idle, IdleState::WarmIdle | IdleState::DeepIdle)
        && load.cpu_percent < 60.0
        && !load.disk_busy;

    let allow_content = matches!(idle, IdleState::DeepIdle)
        && load.cpu_percent < 40.0
        && !load.disk_busy;

    if allow_metadata {
        take(&mut queues.metadata, 256);
    }

    if allow_content {
        take(&mut queues.content, 64);
    }

    selected
}

#[cfg(target_os = "windows")]
fn idle_elapsed_ms() -> Option<u64> {
    use windows::Win32::UI::WindowsAndMessaging::GetLastInputInfo;
    use windows::Win32::UI::WindowsAndMessaging::LASTINPUTINFO;
    use windows::Win32::Foundation::GetTickCount;

    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        if GetLastInputInfo(&mut info).as_bool() {
            let now = GetTickCount() as u64;
            let last = info.dwTime as u64;
            return Some(now.saturating_sub(last));
        }
    }
    warn!("GetLastInputInfo failed; treating as active");
    None
}

#[cfg(not(target_os = "windows"))]
fn idle_elapsed_ms() -> Option<u64> {
    // Non-Windows placeholder; treat as always active for now.
    None
}
