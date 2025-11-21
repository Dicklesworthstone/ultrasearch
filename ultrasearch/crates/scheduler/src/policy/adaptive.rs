use crate::{SchedulerConfig, SystemLoad};

/// Tuning logic for the scheduler.
pub struct AdaptivePolicy {
    base_config: SchedulerConfig,
}

impl AdaptivePolicy {
    pub fn new(base_config: SchedulerConfig) -> Self {
        Self { base_config }
    }

    /// Return a tuned config based on current load.
    pub fn tune(&self, load: &SystemLoad, queue_depth: usize) -> SchedulerConfig {
        let mut cfg = self.base_config.clone();

        // If queue is huge, relax CPU limits slightly to make progress
        if queue_depth > 1000 {
            // Backlog pressure: increase limits if system not melting.
            if load.cpu_percent < 80.0 {
                cfg.cpu_metadata_max = 85.0;
                cfg.cpu_content_max = 60.0;
                cfg.content_batch_size = (cfg.content_batch_size as f32 * 1.5) as usize;
            }
        } else if queue_depth < 100 {
            // Low pressure: save power.
            cfg.cpu_metadata_max = 40.0;
            cfg.cpu_content_max = 20.0;
        }

        // If disk is busy, throttle batch size heavily.
        if load.disk_busy {
            cfg.content_batch_size = 10;
        }

        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SystemLoad;
    use std::time::Duration;

    #[test]
    fn policy_tunes_up_on_backlog() {
        let base = SchedulerConfig::default();
        let policy = AdaptivePolicy::new(base.clone());
        
        let load = SystemLoad {
            cpu_percent: 50.0,
            mem_used_percent: 50.0,
            disk_busy: false,
            disk_bytes_per_sec: 0,
            sample_duration: Duration::from_secs(1),
        };

        let tuned = policy.tune(&load, 2000);
        assert!(tuned.cpu_metadata_max > base.cpu_metadata_max);
        assert!(tuned.content_batch_size > base.content_batch_size);
    }

    #[test]
    fn policy_throttles_on_disk_busy() {
        let base = SchedulerConfig::default();
        let policy = AdaptivePolicy::new(base);
        
        let load = SystemLoad {
            cpu_percent: 10.0,
            mem_used_percent: 10.0,
            disk_busy: true,
            disk_bytes_per_sec: 1000,
            sample_duration: Duration::from_secs(1),
        };

        let tuned = policy.tune(&load, 500);
        assert_eq!(tuned.content_batch_size, 10);
    }
}