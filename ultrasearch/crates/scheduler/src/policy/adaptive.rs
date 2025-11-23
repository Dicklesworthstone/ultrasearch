use crate::{SchedulerConfig, metrics::SystemLoad};
use std::time::Duration;

const CPU_SMOOTHING: f32 = 0.2;
const BATCH_SIZE_MIN: usize = 10;
const BATCH_SIZE_MAX: usize = 2000;
const CPU_THRESHOLD_MIN: f32 = 15.0;
const CPU_THRESHOLD_MAX: f32 = 60.0;

/// Dynamically adjusts scheduler config based on recent system load.
pub struct AdaptivePolicy {
    config: SchedulerConfig,
    smoothed_cpu: f32,
    last_adjustment: std::time::Instant,
}

impl AdaptivePolicy {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            smoothed_cpu: 0.0,
            last_adjustment: std::time::Instant::now(),
            config,
        }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Update internal state and adjust config if needed.
    pub fn update(&mut self, load: &SystemLoad) {
        // Smooth CPU load to avoid jerky reactions
        self.smoothed_cpu =
            self.smoothed_cpu * (1.0 - CPU_SMOOTHING) + load.cpu_percent * CPU_SMOOTHING;

        // Adjust every few seconds, not on every tick
        if self.last_adjustment.elapsed() < Duration::from_secs(5) {
            return;
        }

        // --- Batch Size Policy ---
        // If CPU is low, increase batch size. If high, decrease it.
        let batch_size = if self.smoothed_cpu < 20.0 {
            (self.config.content_batch_size + 50).min(BATCH_SIZE_MAX)
        } else if self.smoothed_cpu > 50.0 {
            (self.config.content_batch_size as i32 - 100).max(BATCH_SIZE_MIN as i32) as usize
        } else {
            self.config.content_batch_size
        };
        self.config.content_batch_size = batch_size;

        // --- CPU Threshold Policy ---
        // If CPU has been low for a while, we can be more aggressive (higher threshold).
        let cpu_threshold = if self.smoothed_cpu < 10.0 {
            (self.config.cpu_content_max + 5.0).min(CPU_THRESHOLD_MAX)
        } else if self.smoothed_cpu > 40.0 {
            (self.config.cpu_content_max - 5.0).max(CPU_THRESHOLD_MIN)
        } else {
            self.config.cpu_content_max
        };
        self.config.cpu_content_max = cpu_threshold;

        self.last_adjustment = std::time::Instant::now();
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

    #[test]
    fn batch_size_decreases_under_high_load() {
        let mut policy = AdaptivePolicy::new(SchedulerConfig::default());
        let initial_batch = policy.config().content_batch_size;

        policy.smoothed_cpu = 60.0; // pre-condition high load
        policy.last_adjustment -= Duration::from_secs(10); // allow update
        policy.update(&cpu_load(60.0));

        assert!(policy.config().content_batch_size < initial_batch);
    }
}
