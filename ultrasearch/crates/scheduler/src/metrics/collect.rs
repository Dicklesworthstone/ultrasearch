use std::time::{Duration, Instant};
#[cfg(not(target_os = "windows"))]
use sysinfo::Disks;
use sysinfo::System;
#[cfg(target_os = "windows")]
use windows::{
    Win32::System::Performance::{
        PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
        PdhOpenQueryW,
    },
    core::w,
};

/// Snapshot of system load suitable for scheduling decisions.
#[derive(Debug, Clone, Copy)]
pub struct SystemLoad {
    pub cpu_percent: f32,
    pub mem_used_percent: f32,
    /// Aggregate disk throughput in bytes/sec since the previous sample.
    /// Zero is a placeholder when the sampler reports unavailable disk data.
    pub disk_bytes_per_sec: u64,
    /// Also true when disk data is unavailable: missing data never permits work.
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
    disk_available: Option<bool>,
    #[cfg(target_os = "windows")]
    disk_counter: Option<Box<dyn DiskCounter>>,
    #[cfg(target_os = "windows")]
    recreate_disk_counter: bool,
    #[cfg(target_os = "windows")]
    last_counter_attempt: Instant,
    #[cfg(not(target_os = "windows"))]
    disks: Disks,
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
        Self {
            system,
            disk_busy_threshold_bps,
            last_sample: Instant::now(),
            disk_available: None,
            #[cfg(target_os = "windows")]
            disk_counter,
            #[cfg(target_os = "windows")]
            recreate_disk_counter: true,
            #[cfg(target_os = "windows")]
            last_counter_attempt: Instant::now(),
            #[cfg(not(target_os = "windows"))]
            disks,
        }
    }

    #[cfg(target_os = "windows")]
    pub fn with_disk_counter(mut self, disk_counter: Option<Box<dyn DiskCounter>>) -> Self {
        self.disk_counter = disk_counter;
        // Never silently replace an injected counter (including None) with a
        // different native sensor. Failed injected counters recover in place.
        self.recreate_disk_counter = false;
        self.disk_available = None;
        self
    }

    /// Whether the most recent disk sample contained a usable observation.
    pub fn disk_sample_available(&self) -> bool {
        self.disk_available == Some(true)
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
        // Ensure we always record a sensible, non-zero sample duration; very fast
        // calls (sub-millisecond) can otherwise truncate to 0ms in tests/metrics.
        let elapsed = now
            .saturating_duration_since(self.last_sample)
            .max(Duration::from_millis(1));

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
        let _ = elapsed;

        #[cfg(target_os = "windows")]
        {
            if self.disk_counter.is_none()
                && self.recreate_disk_counter
                && counter_retry_due(self.last_counter_attempt, Instant::now())
            {
                // Initialization primes the rate counter. Wait for the next
                // sampling turn before asking for its second observation.
                self.disk_counter = PdhCounter::new_total_disk_bytes()
                    .ok()
                    .map(|counter| Box::new(counter) as Box<dyn DiskCounter>);
                self.last_counter_attempt = Instant::now();
                return self.finish_disk_sample(None);
            }
            let observed = self.disk_counter.as_mut().and_then(|counter| {
                counter
                    .sample_bytes_per_sec()
                    .inspect_err(|error| tracing::debug!(%error, "disk counter sample failed"))
                    .ok()
            });
            if observed.is_none() && self.recreate_disk_counter && self.disk_counter.is_some() {
                // Drop closes a damaged query before a replacement is opened.
                // At most one reinitialization is attempted per retry interval.
                self.disk_counter = None;
                self.last_counter_attempt = Instant::now();
            }
            self.finish_disk_sample(observed)
        }

        #[cfg(not(target_os = "windows"))]
        {
            // Use sysinfo disk usage deltas between refreshes.
            self.disks.refresh(false);

            // These are already interval deltas. Differencing them again
            // reports zero under sustained equal-rate I/O.
            let observed = interval_disk_rate(
                self.disks.list().iter().map(|disk| {
                    let usage = disk.usage();
                    (usage.read_bytes, usage.written_bytes)
                }),
                elapsed,
            );
            self.finish_disk_sample(observed)
        }
    }

    fn finish_disk_sample(&mut self, observed: Option<u64>) -> (u64, bool) {
        let available = observed.is_some();
        if self.disk_available != Some(available) {
            if !available {
                tracing::warn!("disk load unavailable; pausing disk-gated background work");
            } else if self.disk_available == Some(false) {
                tracing::info!("disk load sensing recovered");
            }
        }
        self.disk_available = Some(available);
        disk_admission(observed, self.disk_busy_threshold_bps)
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

fn disk_admission(observed: Option<u64>, threshold: u64) -> (u64, bool) {
    // Never publish the last successful rate as a fresh observation.
    (
        observed.unwrap_or(0),
        observed.is_none_or(|rate| rate >= threshold),
    )
}

#[cfg(any(not(windows), test))]
fn interval_disk_rate(
    usages: impl IntoIterator<Item = (u64, u64)>,
    elapsed: Duration,
) -> Option<u64> {
    let mut found = false;
    let mut bytes = 0u64;
    for (read, written) in usages {
        found = true;
        bytes = bytes.checked_add(read)?.checked_add(written)?;
    }
    if !found || elapsed.is_zero() {
        return None;
    }
    // Preserve inclusive threshold boundaries without floating-point rounding.
    // Reject overflow instead of wrapping into a quiet rate.
    let rate = u128::from(bytes) * 1_000_000_000 / elapsed.as_nanos();
    u64::try_from(rate).ok()
}

#[cfg(any(windows, test))]
fn checked_disk_rate(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0 && value < u64::MAX as f64).then_some(value as u64)
}

#[cfg(any(windows, test))]
fn counter_retry_due(previous: Instant, now: Instant) -> bool {
    now.checked_duration_since(previous)
        .is_some_and(|elapsed| elapsed >= Duration::from_secs(30))
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

            // Own the query before the next fallible operation: failed counter
            // registration or priming must not leak native handles on retries.
            let mut owned = Self { query, counter: 0 };
            pdh_ok(
                PdhAddEnglishCounterW(
                    query,
                    w!("\\PhysicalDisk(_Total)\\Disk Bytes/sec"),
                    0,
                    &mut owned.counter,
                ),
                "PdhAddEnglishCounterW",
            )?;
            pdh_ok(PdhCollectQueryData(query), "PdhCollectQueryData(init)")?;

            Ok(owned)
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
        formatted_disk_rate(&value)
    }
}

#[cfg(target_os = "windows")]
fn formatted_disk_rate(value: &PDH_FMT_COUNTERVALUE) -> windows::core::Result<u64> {
    // API success and counter-data success are separate PDH contracts.
    // Do not read the numeric union member unless CStatus is valid.
    if value.CStatus != PDH_CSTATUS_VALID_DATA as u32
        && value.CStatus != PDH_CSTATUS_NEW_DATA as u32
    {
        return Err(windows::core::Error::new(
            windows::core::HRESULT(0x80004005u32 as i32),
            format!("invalid disk counter data (status 0x{:08x})", value.CStatus).into(),
        ));
    }
    // SAFETY: our sole production caller requested PDH_FMT_DOUBLE and checked
    // the API result; CStatus was checked above. Tests initialize doubleValue.
    checked_disk_rate(unsafe { value.Anonymous.doubleValue }).ok_or_else(|| {
        windows::core::Error::new(
            windows::core::HRESULT(0x80004005u32 as i32),
            "disk counter returned a negative, non-finite, or unrepresentable rate".into(),
        )
    })
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
        let computed_flag = !sampler.disk_sample_available()
            || load.disk_bytes_per_sec >= sampler.disk_threshold();
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

    #[test]
    fn interval_deltas_measure_sustained_and_falling_load() {
        let elapsed = Duration::from_secs(1);
        let observed: Vec<_> = [(2_000, 1_000), (2_000, 1_000), (1_000, 500)]
            .into_iter()
            .map(|usage| interval_disk_rate([usage], elapsed))
            .collect();
        assert_eq!(observed, [Some(3_000), Some(3_000), Some(1_500)]);
        assert_eq!(
            interval_disk_rate([(2_000, 1_000), (500, 500)], Duration::from_secs(2)),
            Some(2_000)
        );
        assert_eq!(
            interval_disk_rate([(1, 0)], Duration::from_millis(250)),
            Some(4)
        );
    }

    #[test]
    fn interval_rates_reject_missing_data_and_overflow_without_rounding() {
        assert_eq!(interval_disk_rate([], Duration::from_secs(1)), None);
        assert_eq!(interval_disk_rate([(1, 1)], Duration::ZERO), None);
        assert_eq!(
            interval_disk_rate([(u64::MAX, 1)], Duration::from_secs(1)),
            None
        );
        assert_eq!(
            interval_disk_rate([(u64::MAX, 0), (1, 0)], Duration::from_secs(1)),
            None
        );
        assert_eq!(
            interval_disk_rate([(u64::MAX, 0)], Duration::from_nanos(1)),
            None
        );
        assert_eq!(
            interval_disk_rate([(u64::MAX, 0)], Duration::from_secs(1)),
            Some(u64::MAX)
        );
        assert_eq!(interval_disk_rate([(0, 0)], Duration::from_secs(1)), Some(0));
        assert_eq!(interval_disk_rate([(1, 0)], Duration::from_secs(3)), Some(0));
    }

    #[test]
    fn unavailable_disk_pauses_extraction_but_preserves_critical_cleanup() {
        use crate::{IdleState, Job, JobCategory, JobQueues, SchedulerConfig, select_jobs};

        let cfg = SchedulerConfig::default();
        let mut queues = JobQueues::default();
        let deleted = core_types::DocKey::from_parts(1, 10);
        queues.push(JobCategory::Critical, Job::Delete(deleted), 0);
        queues.push(
            JobCategory::Content,
            Job::ContentIndex(core_types::DocKey::from_parts(1, 11)),
            10,
        );
        let (rate, busy) = disk_admission(None, u64::MAX);
        let mut load = SystemLoad {
            cpu_percent: 0.0,
            mem_used_percent: 10.0,
            disk_bytes_per_sec: rate,
            disk_busy: busy,
            sample_duration: Duration::from_secs(1),
            on_battery: false,
            game_mode: false,
        };
        assert!(!crate::allow_metadata_jobs(IdleState::DeepIdle, load, &cfg));
        assert!(!crate::allow_content_jobs(IdleState::DeepIdle, load, &cfg));
        let selected = select_jobs(&mut queues, IdleState::DeepIdle, load, &cfg);
        assert!(matches!(selected.as_slice(), [Job::Delete(key)] if *key == deleted));
        assert_eq!(queues.counts(), (0, 0, 1));
        (load.disk_bytes_per_sec, load.disk_busy) = disk_admission(Some(0), 1_000);
        let selected = select_jobs(&mut queues, IdleState::DeepIdle, load, &cfg);
        assert!(matches!(selected.as_slice(), [Job::ContentIndex(_)]));
        assert!(queues.is_empty());
    }

    #[test]
    fn availability_and_live_thresholds_do_not_reuse_stale_rates() {
        let mut sampler = SystemLoadSampler::new(1_000);
        assert!(!sampler.disk_sample_available());
        assert_eq!(sampler.finish_disk_sample(Some(999)), (999, false));
        assert!(sampler.disk_sample_available());
        sampler.set_disk_threshold(999);
        assert_eq!(sampler.finish_disk_sample(Some(999)), (999, true));
        assert_eq!(sampler.finish_disk_sample(None), (0, true));
        assert!(!sampler.disk_sample_available());
        sampler.set_disk_threshold(u64::MAX);
        assert_eq!(sampler.finish_disk_sample(None), (0, true));
        assert_eq!(sampler.finish_disk_sample(Some(0)), (0, false));
        assert!(sampler.disk_sample_available());
        sampler.set_disk_threshold(0);
        assert_eq!(sampler.finish_disk_sample(Some(0)), (0, true));
    }

    #[test]
    fn native_rate_conversion_never_turns_invalid_values_into_quiet_io() {
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -0.1,
            u64::MAX as f64,
        ] {
            assert_eq!(checked_disk_rate(value), None, "value={value}");
        }
        for (value, expected) in [(0.0, 0), (-0.0, 0), (999.9, 999), (1_000.0, 1_000)] {
            assert_eq!(checked_disk_rate(value), Some(expected));
        }
    }

    #[test]
    fn failed_counter_recreation_has_a_monotonic_backoff() {
        let now = Instant::now();
        assert!(!counter_retry_due(now, now));
        assert!(!counter_retry_due(now, now + Duration::from_millis(29_999)));
        assert!(counter_retry_due(now, now + Duration::from_secs(30)));
        assert!(counter_retry_due(now, now + Duration::from_secs(60)));
        assert!(!counter_retry_due(now + Duration::from_secs(1), now));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn injected_counter_failures_remain_closed_until_a_valid_sample() {
        let mock = MockCounter {
            vals: vec![
                Ok(0),
                Err(windows::core::Error::new(
                    windows::core::HRESULT(0x80004005u32 as i32),
                    "synthetic counter failure".into(),
                )),
                Ok(2_000),
                Ok(0),
            ],
            idx: 0,
        };
        let mut sampler = SystemLoadSampler::new(1_000).with_disk_counter(Some(Box::new(mock)));
        for (expected, available) in [
            ((0, false), true),
            ((0, true), false),
            ((2_000, true), true),
            ((0, false), true),
        ] {
            assert_eq!(sampler.sample_disk(Duration::from_secs(1)), expected);
            assert_eq!(sampler.disk_sample_available(), available);
        }
        let mut missing = SystemLoadSampler::new(1_000).with_disk_counter(None);
        missing.last_counter_attempt -= Duration::from_secs(60);
        assert_eq!(missing.sample_disk(Duration::from_secs(1)), (0, true));
        assert!(missing.disk_counter.is_none());
        assert!(!missing.disk_sample_available());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn formatted_counter_requires_valid_status_and_value() {
        let mut value = PDH_FMT_COUNTERVALUE::default();
        value.Anonymous.doubleValue = 0.0;
        for status in [2, 0x800007d5, 0xc0000bc6, u32::MAX] {
            value.CStatus = status;
            assert!(formatted_disk_rate(&value).is_err());
        }
        for status in [PDH_CSTATUS_VALID_DATA as u32, PDH_CSTATUS_NEW_DATA as u32] {
            value.CStatus = status;
            value.Anonymous.doubleValue = 1_000.0;
            assert_eq!(formatted_disk_rate(&value).unwrap(), 1_000);
            value.Anonymous.doubleValue = f64::NAN;
            assert!(formatted_disk_rate(&value).is_err());
        }
    }
}
