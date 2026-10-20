use crate::{SchedulerConfig, metrics::SystemLoad};
use std::time::{Duration, Instant};

const CPU_SMOOTHING: f32 = 0.2;
const BATCH_SIZE_MIN: usize = 10;
const BATCH_SIZE_MAX: usize = 2000;
const CPU_THRESHOLD_MIN: f32 = 15.0;
const ADJUSTMENT_INTERVAL: Duration = Duration::from_secs(5);
const MAX_SAMPLE_GAP: Duration = Duration::from_secs(5);

/// Load feedback bounded by the operator's configured ceilings.
///
/// This policy only tunes fresh batch size and the content CPU ceiling. It
/// never relaxes idle, disk or power gates, changes worker concurrency, or
/// rewrites an already-admitted transaction. Callers must combine
/// `can_admit_content()` with the normal content admission predicate.
pub struct AdaptivePolicy {
    base: SchedulerConfig,
    config: SchedulerConfig,
    smoothed_cpu: f32,
    previous_sample: Option<Instant>,
    last_adjustment: Option<Instant>,
    sample_usable: bool,
    memory_pressure: bool,
}

fn batch_ceiling(config: &SchedulerConfig) -> usize {
    config.content_batch_size.clamp(1, BATCH_SIZE_MAX)
}

fn cpu_ceiling(config: &SchedulerConfig) -> f32 {
    if config.cpu_content_max.is_finite() {
        config.cpu_content_max.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

impl AdaptivePolicy {
    pub fn new(base: SchedulerConfig) -> Self {
        let mut config = base.clone();
        config.content_batch_size = batch_ceiling(&base);
        config.cpu_content_max = cpu_ceiling(&base);
        Self {
            base,
            config,
            smoothed_cpu: 0.0,
            previous_sample: None,
            last_adjustment: None,
            sample_usable: false,
            memory_pressure: false,
        }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Additional guard, not a substitute for idle/disk/CPU/power admission.
    pub fn can_admit_content(&self) -> bool {
        self.sample_usable && !self.memory_pressure
    }

    /// Apply a live base policy without erasing learned limits on every tick.
    /// Lower ceilings apply immediately. Raising them requires fresh usable
    /// observations before the learned values grow. Batch growth requires quiet
    /// load; CPU admission can also recover from a self-imposed stall once
    /// pressure has cleared. Non-adaptive fields are copied verbatim; no
    /// configuration is written back to the shared cache.
    pub fn reconfigure(&mut self, base: SchedulerConfig) {
        let changed = batch_ceiling(&base) != batch_ceiling(&self.base)
            || cpu_ceiling(&base) != cpu_ceiling(&self.base)
            || base.power_save_mode != self.base.power_save_mode;
        let batch = self.config.content_batch_size.min(batch_ceiling(&base));
        let cpu = self.config.cpu_content_max.min(cpu_ceiling(&base));
        self.config = base.clone();
        self.config.content_batch_size = batch;
        self.config.cpu_content_max = cpu;
        self.base = base;
        if changed {
            self.previous_sample = None;
            self.last_adjustment = None;
            self.sample_usable = false;
        }
    }

    /// Update from a fresh system observation and the monotonic clock.
    pub fn update(&mut self, load: &SystemLoad) {
        self.update_at(load, Instant::now());
    }

    /// Clock-injectable update for deterministic controllers and regressions.
    /// Duplicate times earn no feedback credit. Backwards time, stale samples
    /// and gaps restart the observation window instead of authorizing growth.
    pub fn update_at(&mut self, load: &SystemLoad, now: Instant) {
        self.sample_usable = load.cpu_percent.is_finite()
            && (0.0..=100.0).contains(&load.cpu_percent)
            && load.mem_used_percent.is_finite()
            && (0.0..=100.0).contains(&load.mem_used_percent)
            && self.base.cpu_content_max.is_finite()
            && (0.0..=100.0).contains(&self.base.cpu_content_max)
            && !load.sample_duration.is_zero()
            && load.sample_duration <= MAX_SAMPLE_GAP;
        self.memory_pressure = load.mem_used_percent >= 90.0;
        if !self.sample_usable {
            self.previous_sample = None;
            self.last_adjustment = None;
            return;
        }

        let elapsed = self.previous_sample.and_then(|previous| {
            now.checked_duration_since(previous)
                .filter(|elapsed| *elapsed <= MAX_SAMPLE_GAP)
        });
        self.previous_sample = Some(now);
        let Some(elapsed) = elapsed else {
            // Seed from the first real sample, not an invented 0% CPU reading.
            self.smoothed_cpu = load.cpu_percent;
            self.last_adjustment = Some(now);
            return;
        };
        if elapsed.is_zero() {
            return;
        }
        // Use elapsed time rather than call count; burst polling cannot make
        // repeated cached observations look like sustained quiet activity.
        let alpha = 1.0 - (1.0 - CPU_SMOOTHING).powf(elapsed.as_secs_f32());
        self.smoothed_cpu += alpha * (load.cpu_percent - self.smoothed_cpu);
        if !self.last_adjustment.is_some_and(|previous| {
            now.checked_duration_since(previous)
                .is_some_and(|elapsed| elapsed >= ADJUSTMENT_INTERVAL)
        }) {
            return;
        }

        let ceiling = cpu_ceiling(&self.base);
        let power_pause = self.base.power_save_mode && (load.on_battery || load.game_mode);
        let pressure = load.cpu_percent >= ceiling
            || self.smoothed_cpu > ceiling * 0.8
            || load.mem_used_percent >= 85.0
            || load.disk_busy
            || power_pause;
        let quiet = load.cpu_percent < 10.0
            && self.smoothed_cpu < 10.0
            && load.mem_used_percent < 75.0
            && !load.disk_busy
            && !power_pause;
        if pressure {
            // A previous, smaller operator ceiling may leave learned limits
            // below the usual floors after a reload. Pressure must never raise
            // those limits merely to reach a floor.
            let batch_floor = self
                .config
                .content_batch_size
                .min(BATCH_SIZE_MIN.min(batch_ceiling(&self.base)));
            let cpu_floor = self.config.cpu_content_max.min(CPU_THRESHOLD_MIN.min(ceiling));
            self.config.content_batch_size = self
                .config
                .content_batch_size
                .saturating_sub(100)
                .max(batch_floor);
            self.config.cpu_content_max = (self.config.cpu_content_max - 5.0).max(cpu_floor);
        } else if quiet {
            self.config.content_batch_size = self
                .config
                .content_batch_size
                .saturating_add(50)
                .min(batch_ceiling(&self.base));
            self.config.cpu_content_max = (self.config.cpu_content_max + 5.0).min(ceiling);
        } else if load.cpu_percent >= self.config.cpu_content_max {
            // Pressure has cleared, but the learned CPU ceiling itself may
            // still prevent every content job from running. Requiring <10%
            // absolute CPU to recover would strand a machine with a steady
            // 20% background load forever after a disk/CPU pressure episode.
            // Probe admission gradually using the existing adjustment cadence,
            // never above the operator ceiling. Do not grow the batch: spare
            // capacity to start a small batch is not evidence for larger ones.
            // Equality must recover too: allow_content_jobs uses strict `<`.
            self.config.cpu_content_max = (self.config.cpu_content_max + 5.0).min(ceiling);
        }
        self.last_adjustment = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_load(cpu: f32) -> SystemLoad {
        SystemLoad {
            cpu_percent: cpu,
            mem_used_percent: 50.0,
            disk_bytes_per_sec: 0,
            disk_busy: false,
            game_mode: false,
            on_battery: false,
            sample_duration: Duration::from_secs(1),
        }
    }

    fn observe_seconds(
        policy: &mut AdaptivePolicy,
        load: &SystemLoad,
        start: Instant,
        seconds: u64,
    ) {
        for second in 0..=seconds {
            policy.update_at(load, start + Duration::from_secs(second));
        }
    }

    #[test]
    fn batch_size_decreases_under_high_load() {
        let mut policy = AdaptivePolicy::new(SchedulerConfig::default());
        let initial_batch = policy.config().content_batch_size;
        let initial_cpu = policy.config().cpu_content_max;
        observe_seconds(&mut policy, &cpu_load(60.0), Instant::now(), 5);
        assert!(policy.config().content_batch_size < initial_batch);
        assert!(policy.config().cpu_content_max < initial_cpu);
    }

    #[test]
    fn moderate_load_recovers_queued_content_after_pressure_without_growing_batches() {
        use crate::{IdleState, Job, JobCategory, JobQueues};
        use core_types::DocKey;

        for (ceiling, ambient) in [(40.0, 20.0), (80.0, 35.0), (80.0, 60.0)] {
            let mut policy = AdaptivePolicy::new(SchedulerConfig {
                cpu_content_max: ceiling,
                ..Default::default()
            });
            let start = Instant::now();
            observe_seconds(&mut policy, &cpu_load(100.0), start, 100);
            assert_eq!(policy.config().cpu_content_max, CPU_THRESHOLD_MIN);
            assert_eq!(policy.config().content_batch_size, BATCH_SIZE_MIN);

            let key = DocKey::from_parts(7, 0xabcd_0000_0000_1234);
            let mut queue = JobQueues::default();
            queue.push(JobCategory::Content, Job::ContentIndex(key), 1024);
            let load = cpu_load(ambient);
            assert!(
                crate::select_jobs(&mut queue, IdleState::DeepIdle, load, policy.config()).is_empty()
            );
            let mut admitted = false;
            for second in 101..=220 {
                policy.update_at(&load, start + Duration::from_secs(second));
                assert!(policy.config().cpu_content_max <= ceiling);
                assert_eq!(policy.config().content_batch_size, BATCH_SIZE_MIN);
                assert!(!crate::allow_content_jobs(
                    IdleState::Active,
                    load,
                    policy.config()
                ));
                assert!(!crate::allow_content_jobs(
                    IdleState::WarmIdle,
                    load,
                    policy.config()
                ));
                if policy.can_admit_content() {
                    let selected =
                        crate::select_jobs(&mut queue, IdleState::DeepIdle, load, policy.config());
                    if !selected.is_empty() {
                        assert_eq!(selected.len(), 1);
                        assert!(matches!(&selected[0], Job::ContentIndex(doc) if *doc == key));
                        admitted = true;
                        break;
                    }
                }
            }
            assert!(
                admitted,
                "content stranded at ambient CPU {ambient}, ceiling {ceiling}"
            );
            assert!(queue.is_empty());
        }
    }

    #[test]
    fn recovery_probe_preserves_pressure_gates_and_does_not_grow_under_them() {
        for load in [
            cpu_load(100.0),
            cpu_load(35.0), // Still above the existing smoothed pressure boundary.
            SystemLoad {
                disk_busy: true,
                ..cpu_load(20.0)
            },
            SystemLoad {
                mem_used_percent: 95.0,
                ..cpu_load(20.0)
            },
            SystemLoad {
                on_battery: true,
                ..cpu_load(20.0)
            },
            SystemLoad {
                game_mode: true,
                ..cpu_load(20.0)
            },
        ] {
            let mut policy = AdaptivePolicy::new(SchedulerConfig::default());
            let start = Instant::now();
            observe_seconds(&mut policy, &cpu_load(100.0), start, 100);
            let reduced_cpu = policy.config().cpu_content_max;
            let reduced_batch = policy.config().content_batch_size;
            for second in 101..=160 {
                policy.update_at(&load, start + Duration::from_secs(second));
                assert!(policy.config().cpu_content_max <= reduced_cpu);
                assert_eq!(policy.config().content_batch_size, reduced_batch);
                assert!(!(policy.can_admit_content()
                    && crate::allow_content_jobs(crate::IdleState::DeepIdle, load, policy.config())));
            }
        }
    }

    #[test]
    fn moderate_recovery_requires_elapsed_observations_and_crosses_equal_cpu_limit() {
        let mut policy = AdaptivePolicy::new(SchedulerConfig {
            content_batch_size: 3,
            cpu_content_max: 5.0,
            ..Default::default()
        });
        policy.reconfigure(SchedulerConfig::default());
        let start = Instant::now();
        let load = cpu_load(20.0);
        observe_seconds(&mut policy, &load, start, 4);
        assert_eq!(policy.config().cpu_content_max, 5.0);
        policy.update_at(&load, start + Duration::from_secs(5));
        assert_eq!(policy.config().cpu_content_max, 10.0);
        for _ in 0..100 {
            policy.update_at(&load, start + Duration::from_secs(5));
        }
        assert_eq!(policy.config().cpu_content_max, 10.0);

        policy.update_at(&cpu_load(f32::NAN), start + Duration::from_secs(6));
        assert!(!policy.can_admit_content());
        observe_seconds(&mut policy, &load, start + Duration::from_secs(7), 4);
        assert_eq!(policy.config().cpu_content_max, 10.0);
        policy.update_at(&load, start + Duration::from_secs(12));
        assert_eq!(policy.config().cpu_content_max, 15.0);

        observe_seconds(&mut policy, &load, start + Duration::from_secs(60), 4);
        assert_eq!(policy.config().cpu_content_max, 15.0);
        policy.update_at(&load, start + Duration::from_secs(65));
        assert_eq!(policy.config().cpu_content_max, 20.0);
        assert!(!crate::allow_content_jobs(
            crate::IdleState::DeepIdle,
            load,
            policy.config()
        ));
        observe_seconds(&mut policy, &load, start + Duration::from_secs(66), 4);
        assert_eq!(policy.config().cpu_content_max, 25.0);
        assert!(
            policy.can_admit_content()
                && crate::allow_content_jobs(crate::IdleState::DeepIdle, load, policy.config())
        );
        observe_seconds(&mut policy, &load, start + Duration::from_secs(71), 10);
        assert_eq!(
            policy.config().cpu_content_max,
            25.0,
            "stop probing once admission recovers"
        );
        assert_eq!(
            policy.config().content_batch_size,
            3,
            "moderate load is not quiet growth"
        );
    }

    #[test]
    fn recovery_cannot_override_a_lowered_or_zero_operator_cpu_ceiling() {
        let mut policy = AdaptivePolicy::new(SchedulerConfig {
            cpu_content_max: 5.0,
            ..Default::default()
        });
        policy.reconfigure(SchedulerConfig::default());
        let start = Instant::now();
        let load = cpu_load(20.0);
        observe_seconds(&mut policy, &load, start, 10);
        assert_eq!(policy.config().cpu_content_max, 15.0);
        for (offset, ceiling) in [(11, 12.0), (22, 0.0)] {
            policy.reconfigure(SchedulerConfig {
                cpu_content_max: ceiling,
                ..Default::default()
            });
            for second in offset..=offset + 10 {
                policy.update_at(&load, start + Duration::from_secs(second));
                assert!(policy.config().cpu_content_max <= ceiling);
                assert!(!crate::allow_content_jobs(
                    crate::IdleState::DeepIdle,
                    load,
                    policy.config()
                ));
            }
        }
    }

    #[test]
    fn feedback_never_exceeds_operator_limits_or_overflows_small_and_huge_batches() {
        for (batch, cpu) in [
            (1, 5.0),
            (5, 0.0),
            (37, 12.0),
            (500, 40.0),
            (usize::MAX, 100.0),
        ] {
            let base = SchedulerConfig {
                content_batch_size: batch,
                cpu_content_max: cpu,
                ..Default::default()
            };
            let mut policy = AdaptivePolicy::new(base.clone());
            let start = Instant::now();
            for second in 0..=160 {
                let load = cpu_load(if second < 60 { 100.0 } else { 0.0 });
                policy.update_at(&load, start + Duration::from_secs(second));
                assert!((1..=batch_ceiling(&base)).contains(&policy.config().content_batch_size));
                assert!((0.0..=cpu).contains(&policy.config().cpu_content_max));
            }
        }
    }

    #[test]
    fn reloading_preserves_feedback_but_lower_caps_take_effect_immediately() {
        let base = SchedulerConfig::default();
        let mut policy = AdaptivePolicy::new(base.clone());
        let start = Instant::now();
        for second in 0..=5 {
            policy.reconfigure(base.clone());
            policy.update_at(&cpu_load(90.0), start + Duration::from_secs(second));
        }
        assert_eq!(policy.config().content_batch_size, base.content_batch_size - 100);
        let tuned_cpu = policy.config().cpu_content_max;
        policy.reconfigure(base.clone());
        assert_eq!(policy.config().cpu_content_max, tuned_cpu);
        let mut changed = base.clone();
        changed.content_batch_size = 3;
        changed.cpu_content_max = 7.0;
        changed.deep_idle = Duration::from_secs(240);
        changed.disk_busy_threshold_bps = 123;
        changed.power_save_mode = false;
        policy.reconfigure(changed);
        assert_eq!(policy.config().content_batch_size, 3);
        assert_eq!(policy.config().cpu_content_max, 7.0);
        assert_eq!(policy.config().deep_idle, Duration::from_secs(240));
        assert_eq!(policy.config().disk_busy_threshold_bps, 123);
        assert!(!policy.config().power_save_mode);
        policy.reconfigure(base);
        assert_eq!(policy.config().content_batch_size, 3);
        assert_eq!(policy.config().cpu_content_max, 7.0);
        assert!(!policy.can_admit_content());
        observe_seconds(&mut policy, &cpu_load(0.0), start + Duration::from_secs(6), 5);
        assert_eq!(policy.config().content_batch_size, 53);
        assert_eq!(policy.config().cpu_content_max, 12.0);
    }

    #[test]
    fn raising_operator_caps_cannot_raise_learned_limits_under_pressure() {
        let mut policy = AdaptivePolicy::new(SchedulerConfig {
            content_batch_size: 3,
            cpu_content_max: 7.0,
            ..Default::default()
        });
        policy.reconfigure(SchedulerConfig::default());
        observe_seconds(&mut policy, &cpu_load(90.0), Instant::now(), 10);
        assert_eq!(policy.config().content_batch_size, 3);
        assert_eq!(policy.config().cpu_content_max, 7.0);
    }

    #[test]
    fn invalid_observations_revoke_admission_without_poisoning_future_feedback() {
        let mut invalid = Vec::new();
        for value in [f32::NAN, f32::INFINITY, -1.0, 101.0] {
            invalid.push(cpu_load(value));
            invalid.push(SystemLoad {
                mem_used_percent: value,
                ..cpu_load(0.0)
            });
        }
        invalid.push(SystemLoad {
            sample_duration: Duration::ZERO,
            ..cpu_load(0.0)
        });
        invalid.push(SystemLoad {
            sample_duration: Duration::from_secs(6),
            ..cpu_load(0.0)
        });
        for load in invalid {
            let mut policy = AdaptivePolicy::new(SchedulerConfig::default());
            let start = Instant::now();
            observe_seconds(&mut policy, &cpu_load(90.0), start, 5);
            let reduced = policy.config().content_batch_size;
            policy.update_at(&load, start + Duration::from_secs(6));
            assert!(!policy.can_admit_content());
            assert_eq!(policy.config().content_batch_size, reduced);
            policy.update_at(&cpu_load(0.0), start + Duration::from_secs(7));
            assert!(policy.can_admit_content());
            assert_eq!(policy.config().content_batch_size, reduced);
            observe_seconds(&mut policy, &cpu_load(0.0), start + Duration::from_secs(8), 4);
            assert_eq!(policy.config().content_batch_size, reduced + 50);
        }
    }

    #[test]
    fn gaps_and_duplicate_samples_cannot_accelerate_recovery() {
        let mut policy = AdaptivePolicy::new(SchedulerConfig::default());
        let start = Instant::now();
        observe_seconds(&mut policy, &cpu_load(90.0), start, 5);
        let reduced = policy.config().content_batch_size;
        for _ in 0..100 {
            policy.update_at(&cpu_load(0.0), start + Duration::from_secs(5));
        }
        assert_eq!(policy.config().content_batch_size, reduced);
        policy.update_at(&cpu_load(0.0), start + Duration::from_secs(90));
        assert_eq!(policy.config().content_batch_size, reduced);
        // A backwards clock is another new observation window, not elapsed credit.
        policy.update_at(&cpu_load(0.0), start);
        assert_eq!(policy.config().content_batch_size, reduced);
        observe_seconds(&mut policy, &cpu_load(0.0), start + Duration::from_secs(1), 4);
        assert_eq!(policy.config().content_batch_size, reduced + 50);
    }

    #[test]
    fn disk_memory_and_power_pressure_never_authorize_content_or_quiet_growth() {
        let base = SchedulerConfig::default();
        for load in [
            SystemLoad {
                disk_busy: true,
                ..cpu_load(0.0)
            },
            SystemLoad {
                mem_used_percent: 95.0,
                ..cpu_load(0.0)
            },
            SystemLoad {
                on_battery: true,
                ..cpu_load(0.0)
            },
            SystemLoad {
                game_mode: true,
                ..cpu_load(0.0)
            },
        ] {
            let mut policy = AdaptivePolicy::new(base.clone());
            observe_seconds(&mut policy, &load, Instant::now(), 5);
            assert!(policy.config().content_batch_size < base.content_batch_size);
            assert!(!(policy.can_admit_content()
                && crate::allow_content_jobs(crate::IdleState::DeepIdle, load, policy.config())));
        }
    }
}
