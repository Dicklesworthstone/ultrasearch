use std::time::{Duration, Instant};
use sysinfo::System;
#[cfg(target_os = "windows")]
use windows::{
    Win32::System::Performance::{
        PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW,
        PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue, PdhOpenQueryW,
    },
    core::w,
};

/// Snapshot of system load suitable for scheduling decisions.
#[derive(Debug, Clone, Copy)]
pub struct SystemLoad {
    pub cpu_percent: f32,
    pub mem_used_percent: f32,
    /// Aggregate disk throughput in bytes/sec since the previous sample.
    pub disk_bytes_per_sec: u64,
    pub disk_busy: bool,
    /// Duration covered by this sample (useful for metrics surfaces).
    pub sample_duration: Duration,
}

pub struct SystemLoadSampler {
    system: System,
    disk_busy_threshold_bps: u64,
    last_sample: Instant,
    #[cfg(target_os = "windows")]
    disk_counter: Option<PdhCounter>,
}

impl SystemLoadSampler {
    /// Create a sampler with a busy threshold expressed in bytes/sec.
    pub fn new(disk_busy_threshold_bps: u64) -> Self {
        let mut system = System::new();
        system.refresh_cpu_all();
        system.refresh_memory();
        #[cfg(target_os = "windows")]
        let disk_counter = PdhCounter::new_total_disk_bytes().ok();

        Self {
            system,
            disk_busy_threshold_bps,
            last_sample: Instant::now(),
            #[cfg(target_os = "windows")]
            disk_counter,
        }
    }

    pub fn disk_threshold(&self) -> u64 {
        self.disk_busy_threshold_bps
    }

    pub fn set_disk_threshold(&mut self, disk_busy_threshold_bps: u64) {
        self.disk_busy_threshold_bps = disk_busy_threshold_bps;
    }

    /// Refresh system metrics and compute load figures.
    pub fn sample(&mut self) -> SystemLoad {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();

        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_sample);
        let elapsed = if elapsed.is_zero() {
            Duration::from_millis(1)
        } else {
            elapsed
        };

        let cpu_percent = self.system.global_cpu_usage();
        let total_mem = self.system.total_memory().max(1);
        let mem_used_percent = (self.system.used_memory() as f32 / total_mem as f32) * 100.0;

        let (disk_bytes_per_sec, disk_busy) = self.sample_disk(elapsed);

        self.last_sample = now;

        SystemLoad {
            cpu_percent,
            mem_used_percent,
            disk_bytes_per_sec,
            disk_busy,
            sample_duration: elapsed,
        }
    }

    fn sample_disk(&mut self, elapsed: Duration) -> (u64, bool) {
        #[cfg(target_os = "windows")]
        {
            if let Some(counter) = self.disk_counter.as_mut() {
                if let Ok(bytes_per_sec) = counter.sample_bytes_per_sec() {
                    let busy = bytes_per_sec >= self.disk_busy_threshold_bps;
                    return (bytes_per_sec, busy);
                }
            }
        }

        // Fallback when disk metrics unavailable (non-Windows builds).
        let bps = 0;
        let busy = false;
        let _ = elapsed; // keep signature consistent
        (bps, busy)
    }
}

#[cfg(target_os = "windows")]
struct PdhCounter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

#[cfg(target_os = "windows")]
impl PdhCounter {
    fn new_total_disk_bytes() -> windows::core::Result<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            PdhOpenQueryW(None, 0, &mut query).ok()?;

            let mut counter = PDH_HCOUNTER::default();
            PdhAddEnglishCounterW(
                query,
                w!("\\PhysicalDisk(_Total)\\Disk Bytes/sec"),
                0,
                &mut counter,
            )
            .ok()?;
            PdhCollectQueryData(query).ok()?;

            Ok(Self { query, counter })
        }
    }

    fn sample_bytes_per_sec(&mut self) -> windows::core::Result<u64> {
        unsafe {
            PdhCollectQueryData(self.query).ok()?;
            let mut value = PDH_FMT_COUNTERVALUE::default();
            PdhGetFormattedCounterValue(self.counter, PDH_FMT_DOUBLE, None, &mut value).ok()?;
            let v = value.Anonymous.doubleValue;
            Ok(if v.is_sign_negative() { 0 } else { v as u64 })
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for PdhCounter {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_busy_threshold_applied() {
        let mut sampler = SystemLoadSampler::new(1_000);
        let load = sampler.sample();
        let computed_flag = load.disk_bytes_per_sec >= sampler.disk_threshold();
        assert_eq!(load.disk_busy, computed_flag);
        assert!(load.sample_duration.as_millis() > 0);
    }
}
