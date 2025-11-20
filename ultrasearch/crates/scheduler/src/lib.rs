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
}

impl SystemLoadSampler {
    pub fn new(disk_busy_threshold: u64) -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        sys.refresh_cpu();
        sys.refresh_disks_list();
        sys.refresh_disks();
        Self {
            sys,
            disk_busy_threshold,
        }
    }

    pub fn sample(&mut self) -> SystemLoad {
        self.sys.refresh_cpu();
        self.sys.refresh_memory();
        self.sys.refresh_disks();

        let cpu_percent = self.sys.global_cpu_info().cpu_usage();
        let total = self.sys.total_memory().max(1);
        let mem_used_percent = (self.sys.used_memory() as f32 / total as f32) * 100.0;

        let disk_busy = self
            .sys
            .disks()
            .iter()
            .any(|d| d.total_written_bytes_per_second() >= self.disk_busy_threshold
                || d.total_read_bytes_per_second() >= self.disk_busy_threshold);

        SystemLoad {
            cpu_percent,
            mem_used_percent,
            disk_busy,
        }
    }
}

#[derive(Debug)]
pub enum Job {
    MetadataUpdate(DocKey),
    ContentIndex(DocKey),
}

pub fn select_jobs(_state: IdleState) -> Vec<Job> {
    // TODO(c00.4.3): integrate queues, budgets, and thresholds.
    Vec::new()
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
