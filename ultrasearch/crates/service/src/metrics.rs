use anyhow::Result;
use core_types::config::MetricsSection;
use once_cell::sync::Lazy;
use prometheus::{opts, Encoder, Histogram, HistogramOpts, IntCounter, Registry, TextEncoder};

/// Shared metrics handle for the service.
pub struct ServiceMetrics {
    pub registry: Registry,
    pub requests_total: IntCounter,
    pub request_latency: Histogram,
    pub worker_failures: IntCounter,
    pub worker_failure_threshold: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ServiceMetricsSnapshot {
    pub search_latency_ms_p50: Option<f64>,
    pub search_latency_ms_p95: Option<f64>,
    pub worker_failures: u64,
    pub queue_depth: Option<u64>,
    pub active_workers: Option<u32>,
}

impl ServiceMetrics {
    pub fn new(cfg: &MetricsSection) -> Result<Self> {
        let registry = Registry::new();

        let requests_total =
            IntCounter::with_opts(opts!("requests_total", "Total IPC requests served"))?;
        let mut hist_opts =
            HistogramOpts::new("request_latency_seconds", "IPC request latency in seconds");
        if !cfg.request_latency_buckets.is_empty() {
            hist_opts = hist_opts.buckets(cfg.request_latency_buckets.clone());
        }
        let request_latency = Histogram::with_opts(hist_opts)?;
        let worker_failures =
            IntCounter::with_opts(opts!("worker_failures_total", "Index worker failures"))?;

        registry.register(Box::new(requests_total.clone()))?;
        registry.register(Box::new(request_latency.clone()))?;
        registry.register(Box::new(worker_failures.clone()))?;

        Ok(Self {
            registry,
            requests_total,
            request_latency,
            worker_failures,
            worker_failure_threshold: cfg.worker_failure_threshold,
        })
    }

    /// Record a successful request with latency (seconds).
    pub fn record_request(&self, latency_secs: f64) {
        self.requests_total.inc();
        let _ = self.request_latency.observe(latency_secs);
    }

    /// Record a worker failure; returns true if the threshold has been met/exceeded.
    pub fn record_worker_failure(&self) -> bool {
        let failures = self.worker_failures.inc();
        failures >= self.worker_failure_threshold
    }

    pub fn snapshot_with_queue_state(
        &self,
        queue_depth: Option<u64>,
        active_workers: Option<u32>,
    ) -> ServiceMetricsSnapshot {
        ServiceMetricsSnapshot {
            search_latency_ms_p50: None,
            search_latency_ms_p95: None,
            worker_failures: self.worker_failures.get(),
            queue_depth,
            active_workers,
        }
    }

    /// Render a lightweight metrics snapshot for status reporting.
    /// Note: Prometheus crate does not expose quantiles; we return None for p50/p95 for now.
    pub fn snapshot(&self) -> ServiceMetricsSnapshot {
        ServiceMetricsSnapshot {
            search_latency_ms_p50: None,
            search_latency_ms_p95: None,
            worker_failures: self.worker_failures.get(),
            queue_depth: None,
            active_workers: None,
        }
    }
}

static ENCODER: Lazy<TextEncoder> = Lazy::new(TextEncoder::new);

pub fn init_metrics_from_config(cfg: &MetricsSection) -> Result<ServiceMetrics> {
    ServiceMetrics::new(cfg)
}

/// Encode all metrics in Prometheus text format.
pub fn scrape_metrics(metrics: &ServiceMetrics) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let metric_families = metrics.registry.gather();
    ENCODER.encode(&metric_families, &mut buffer)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_trips() {
        let cfg = MetricsSection {
            worker_failure_threshold: 2,
            ..Default::default()
        };
        let metrics = ServiceMetrics::new(&cfg).unwrap();
        assert!(!metrics.record_worker_failure());
        assert!(metrics.record_worker_failure()); // second one trips
    }

    #[test]
    fn request_latency_recorded() {
        let metrics = ServiceMetrics::new(&MetricsSection::default()).unwrap();
        metrics.record_request(0.01);
        assert!(metrics.requests_total.get() >= 1);
    }

    #[test]
    fn snapshot_with_queue_state_sets_fields() {
        let metrics = ServiceMetrics::new(&MetricsSection::default()).unwrap();
        let snap = metrics.snapshot_with_queue_state(Some(3), Some(2));
        assert_eq!(snap.queue_depth, Some(3));
        assert_eq!(snap.active_workers, Some(2));
    }
}
