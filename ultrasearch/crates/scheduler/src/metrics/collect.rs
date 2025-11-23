use std::time::{Duration, Instant};
#[cfg(not(target_os = "windows"))]
use sysinfo::Disks;
use sysinfo::System;
#[cfg(target_os = "windows")]
use windows::{
    Win32::System::Performance::{
        PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PdhAddEnglishCounterW, PdhCloseQuery,
        PdhCollectQueryData, PdhGetFormattedCounterValue, PdhOpenQueryW,
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
    /// True if the system is running on battery power.
    pub on_battery: bool,
    /// True if a full-screen application (game/presentation) is active.
    pub game_mode: bool,
}

pub struct SystemLoadSampler {
    system: System,
    disk_busy_threshold_bps: u64,
    last_sample: Instant,
    #[cfg(target_os = "windows")]
    disk_counter: Option<Box<dyn DiskCounter>>,
    #[cfg(not(target_os = "windows"))]
    disks: Disks,
    #[cfg(not(target_os = "windows"))]
    prev_disk_bytes: u64,
}

impl SystemLoadSampler {
    /// Create a sampler with a busy threshold expressed in bytes/sec.
    pub fn new(disk_busy_threshold_bps: u64) -> Self {
        let mut system = System::new();
        system.refresh_cpu_all();
        system.refresh_memory();
        #[cfg(target_os = "windows")]
        let disk_counter = PdhCounter::new_total_disk_bytes()
            .ok()
            .map(|c| Box::new(c) as Box<dyn DiskCounter>);
        #[cfg(not(target_os = "windows"))]
        let disks = Disks::new_with_refreshed_list();
        #[cfg(not(target_os = "windows"))]
        let prev_disk_bytes = disks
            .list()
            .iter()
            .map(|d| {
                let usage = d.usage();
                usage.read_bytes.saturating_add(usage.written_bytes)
            })
            .sum();

        Self {
            system,
            disk_busy_threshold_bps,
            last_sample: Instant::now(),
            #[cfg(target_os = "windows")]
            disk_counter,
            #[cfg(not(target_os = "windows"))]
            disks,
            #[cfg(not(target_os = "windows"))]
            prev_disk_bytes,
        }
    }

    #[cfg(target_os = "windows")]
    pub fn with_disk_counter(mut self, disk_counter: Option<Box<dyn DiskCounter>>) -> Self {
        self.disk_counter = disk_counter;
        self
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
        let on_battery = self.sample_power();
        let game_mode = self.sample_game_mode();

        self.last_sample = now;

        SystemLoad {
            cpu_percent,
            mem_used_percent,
            disk_bytes_per_sec,
            disk_busy,
            sample_duration: elapsed,
            on_battery,
            game_mode,
        }
    }

    fn sample_disk(&mut self, elapsed: Duration) -> (u64, bool) {
        #[cfg(target_os = "windows")]
        {
            if let Some(counter) = self.disk_counter.as_mut()
                && let Ok(bytes_per_sec) = counter.sample_bytes_per_sec()
            {
                let busy = bytes_per_sec >= self.disk_busy_threshold_bps;
                return (bytes_per_sec, busy);
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            // Use sysinfo disk usage deltas between refreshes.
            self.disks.refresh();

            let delta_bytes: u64 = self
                .disks
                .list()
                .iter()
                .map(|d| {
                    let usage = d.usage();
                    usage.read_bytes.saturating_add(usage.written_bytes)
                })
                .sum::<u64>()
                .saturating_sub(self.prev_disk_bytes);
            self.prev_disk_bytes = self
                .disks
                .list()
                .iter()
                .map(|d| {
                    let usage = d.usage();
                    usage.read_bytes.saturating_add(usage.written_bytes)
                })
                .sum();

            let secs = elapsed.as_secs_f64().max(0.001);
            let bytes_per_sec = (delta_bytes as f64 / secs) as u64;
            let busy = bytes_per_sec >= self.disk_busy_threshold_bps;
            return (bytes_per_sec, busy);
        }

        // Fallback when disk metrics unavailable.
        let bps = 0;
        let busy = false;
        let _ = elapsed; // keep signature consistent
        (bps, busy)
    }
    fn sample_power(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
            let mut status = SYSTEM_POWER_STATUS::default();
            if unsafe { GetSystemPowerStatus(&mut status) }.is_ok() {
                // ACLineStatus: 0 = Offline (Battery), 1 = Online, 255 = Unknown.
                // We assume on battery if AC is offline (0).
                return status.ACLineStatus == 0;
            }
        }
        false
    }

    fn sample_game_mode(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::UI::Shell::{
                QUNS_BUSY, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
            };
            if let Ok(state) = unsafe { SHQueryUserNotificationState() } {
                return state == QUNS_BUSY || state == QUNS_RUNNING_D3D_FULL_SCREEN;
            }
        }
        false
    }
}

#[cfg(target_os = "windows")]
pub trait DiskCounter: Send {
    fn sample_bytes_per_sec(&mut self) -> windows::core::Result<u64>;
}

#[cfg(target_os = "windows")]
struct PdhCounter {
    query: isize,
    counter: isize,
}

#[cfg(target_os = "windows")]
impl DiskCounter for PdhCounter {
    fn sample_bytes_per_sec(&mut self) -> windows::core::Result<u64> {
        pdh_collect_and_sample(self.query, self.counter)
    }
}

#[cfg(target_os = "windows")]
impl PdhCounter {
    fn new_total_disk_bytes() -> windows::core::Result<Self> {
        fn pdh_ok(status: u32, ctx: &str) -> windows::core::Result<()> {
            if status == 0 {
                Ok(())
            } else {
                Err(windows::core::Error::new(
                    windows::core::HRESULT(status as i32),
                    format!("{ctx} failed (status 0x{status:08x})").into(),
                ))
            }
        }

        unsafe {
            let mut query: isize = 0;
            pdh_ok(PdhOpenQueryW(None, 0, &mut query), "PdhOpenQueryW")?;

            let mut counter: isize = 0;
            pdh_ok(
                PdhAddEnglishCounterW(
                    query,
                    w!("\\PhysicalDisk(_Total)\\Disk Bytes/sec"),
                    0,
                    &mut counter,
                ),
                "PdhAddEnglishCounterW",
            )?;
            pdh_ok(PdhCollectQueryData(query), "PdhCollectQueryData(init)")?;

            Ok(Self { query, counter })
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

#[cfg(target_os = "windows")]
fn pdh_collect_and_sample(query: isize, counter: isize) -> windows::core::Result<u64> {
    fn pdh_ok(status: u32, ctx: &str) -> windows::core::Result<()> {
        if status == 0 {
            Ok(())
        } else {
            Err(windows::core::Error::new(
                windows::core::HRESULT(status as i32),
                format!("{ctx} failed (status 0x{status:08x})").into(),
            ))
        }
    }

    unsafe {
        pdh_ok(PdhCollectQueryData(query), "PdhCollectQueryData")?;
        let mut value = PDH_FMT_COUNTERVALUE::default();
        pdh_ok(
            PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut value),
            "PdhGetFormattedCounterValue",
        )?;
        let v = value.Anonymous.doubleValue;
        Ok(if v.is_sign_negative() { 0 } else { v as u64 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "windows")]
    struct MockCounter {
        vals: Vec<windows::core::Result<u64>>,
        idx: usize,
    }

    #[cfg(target_os = "windows")]
    impl DiskCounter for MockCounter {
        fn sample_bytes_per_sec(&mut self) -> windows::core::Result<u64> {
            let out = self.vals.get(self.idx).cloned().unwrap_or(Ok(0));
            self.idx += 1;
            out
        }
    }

    #[test]
    fn disk_busy_threshold_applied() {
        let mut sampler = SystemLoadSampler::new(1_000);
        let load = sampler.sample();
        let computed_flag = load.disk_bytes_per_sec >= sampler.disk_threshold();
        assert_eq!(load.disk_busy, computed_flag);
        assert!(load.sample_duration.as_millis() > 0);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn disk_busy_mock_counter() {
        let mock = MockCounter {
            vals: vec![Ok(2_000)],
            idx: 0,
        };
        let mut sampler = SystemLoadSampler::new(1_000).with_disk_counter(Some(Box::new(mock)));
        let (_, busy) = sampler.sample_disk(Duration::from_secs(1));
        assert!(busy);
    }
}
