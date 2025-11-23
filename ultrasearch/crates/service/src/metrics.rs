#![allow(dead_code)]

use anyhow::Result;
use core_types::config::MetricsSection;
use ipc::MetricsSnapshot;
use once_cell::sync::Lazy;
use prometheus::{Encoder, Histogram, HistogramOpts, IntCounter, Registry, TextEncoder, opts};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tracing::warn;

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
    pub content_enqueued: Option<u64>,
    pub content_dropped: Option<u64>,
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
        self.request_latency.observe(latency_secs);
    }

    /// Record a successful request with a Duration.
    pub fn record_request_duration(&self, duration: Duration) {
        self.record_request(duration.as_secs_f64());
    }

    /// Record a worker failure; returns true if the threshold has been met/exceeded.
    pub fn record_worker_failure(&self) -> bool {
        self.worker_failures.inc();
        let tripped = self.worker_failures.get() >= self.worker_failure_threshold;
        if tripped {
            warn!(
                count = self.worker_failures.get(),
                threshold = self.worker_failure_threshold,
                "worker failure threshold reached"
            );
        }
        tripped
    }

    /// Reset the worker failure counter (used after a healthy run).
    pub fn reset_worker_failures(&self) {
        self.worker_failures.reset();
    }

    pub fn snapshot_with_queue_state(
        &self,
        queue_depth: Option<u64>,
        active_workers: Option<u32>,
        content_enqueued: Option<u64>,
        content_dropped: Option<u64>,
    ) -> ServiceMetricsSnapshot {
        ServiceMetricsSnapshot {
            search_latency_ms_p50: None,
            search_latency_ms_p95: None,
            worker_failures: self.worker_failures.get(),
            queue_depth,
            active_workers,
            content_enqueued,
            content_dropped,
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
            content_enqueued: None,
            content_dropped: None,
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

static GLOBAL_METRICS: OnceLock<Arc<ServiceMetrics>> = OnceLock::new();

/// Set the global metrics handle so other modules (IPC/status) can emit snapshots.
pub fn set_global_metrics(metrics: Arc<ServiceMetrics>) {
    let _ = GLOBAL_METRICS.set(metrics);
}

pub fn with_global_metrics<R>(func: impl FnOnce(&ServiceMetrics) -> R) -> Option<R> {
    GLOBAL_METRICS.get().map(|m| func(m))
}

/// Render an IPC-facing metrics snapshot using the global handle, optionally annotating queue depth/active workers.
pub fn global_metrics_snapshot(
    queue_depth: Option<u64>,
    active_workers: Option<u32>,
    content_enqueued: Option<u64>,
    content_dropped: Option<u64>,
) -> Option<MetricsSnapshot> {
    with_global_metrics(|m| {
        let snap =
            m.snapshot_with_queue_state(queue_depth, active_workers, content_enqueued, content_dropped);
        MetricsSnapshot {
            search_latency_ms_p50: snap.search_latency_ms_p50,
            search_latency_ms_p95: snap.search_latency_ms_p95,
            worker_cpu_pct: None,
            worker_mem_bytes: None,
            queue_depth: snap.queue_depth,
            active_workers: snap.active_workers,
            content_enqueued: snap.content_enqueued,
            content_dropped: snap.content_dropped,
        }
    })
}

/// Record a single IPC request duration against the global metrics handle (no-op if uninitialized).
pub fn record_ipc_request(duration: Duration) {
    let _ = with_global_metrics(|m| m.record_request_duration(duration));
}

/// Record a worker failure and return true if the failure threshold was met; no-op if metrics unset.
pub fn record_worker_failure_global() -> Option<bool> {
    with_global_metrics(|m| m.record_worker_failure())
}

/// Scrape all metrics from the global handle in Prometheus text format.
pub fn global_scrape_metrics() -> Option<Vec<u8>> {
    with_global_metrics(|m| scrape_metrics(m).unwrap_or_default())
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

    #[test]
    fn reset_worker_failures_resets_counter() {
        let metrics = ServiceMetrics::new(&MetricsSection {
            worker_failure_threshold: 1,
            ..Default::default()
        })
        .unwrap();
        metrics.record_worker_failure();
        assert!(metrics.worker_failures.get() > 0);
        metrics.reset_worker_failures();
        assert_eq!(metrics.worker_failures.get(), 0);
    }
}
