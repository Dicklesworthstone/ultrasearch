use crate::metrics::global_metrics_snapshot;
use ipc::{MetricsSnapshot, VolumeStatus};
use std::sync::{Arc, OnceLock, RwLock};

/// Snapshot of service status used by IPC responses.
#[derive(Debug, Clone, Default)]
pub struct StatusSnapshot {
    pub volumes: Vec<VolumeStatus>,
    pub scheduler_state: String,
    pub metrics: Option<MetricsSnapshot>,
    pub last_index_commit_ts: Option<i64>,
    pub content_jobs_total: Option<u64>,
    pub content_jobs_remaining: Option<u64>,
    pub content_bytes_total: Option<u64>,
    pub content_bytes_remaining: Option<u64>,
}

pub trait StatusProvider: Send + Sync {
    fn snapshot(&self) -> StatusSnapshot;
}

static PROVIDER: OnceLock<Arc<dyn StatusProvider>> = OnceLock::new();
static BASIC_PROVIDER: OnceLock<Arc<BasicStatusProvider>> = OnceLock::new();

/// Install a process-wide status provider.
pub fn set_status_provider(provider: Arc<dyn StatusProvider>) {
    let _ = PROVIDER.set(provider);
}

/// Initialize and register a BasicStatusProvider; returns the handle for direct updates.
pub fn init_basic_status_provider() -> Arc<BasicStatusProvider> {
    let basic = Arc::new(BasicStatusProvider::new());
    let _ = BASIC_PROVIDER.set(basic.clone());
    set_status_provider(basic.clone());
    basic
}

/// Fetch the current snapshot from the registered provider (or a default stub).
pub fn status_snapshot() -> StatusSnapshot {
    if let Some(provider) = PROVIDER.get() {
        return provider.snapshot();
    }

    StatusSnapshot {
        volumes: Vec::new(),
        scheduler_state: "initializing".to_string(),
        metrics: global_metrics_snapshot(Some(0), Some(0), Some(0), Some(0)),
        last_index_commit_ts: None,
        content_jobs_total: None,
        content_jobs_remaining: None,
        content_bytes_total: None,
        content_bytes_remaining: None,
    }
}

/// Update helpers routed to the BasicStatusProvider if registered.
pub fn update_status_volumes(volumes: Vec<VolumeStatus>) {
    if let Some(p) = BASIC_PROVIDER.get() {
        p.update_volumes(volumes);
    }
}

pub fn update_status_scheduler_state(state: impl Into<String>) {
    if let Some(p) = BASIC_PROVIDER.get() {
        p.update_scheduler_state(state);
    }
}

pub fn update_status_metrics(metrics: Option<MetricsSnapshot>) {
    if let Some(p) = BASIC_PROVIDER.get()
        && let Some(m) = metrics
    {
        p.update_metrics(Some(m));
    }
}

pub fn update_status_queue_state(
    queue_depth: Option<u64>,
    active_workers: Option<u32>,
    content_enqueued: Option<u64>,
    content_dropped: Option<u64>,
) {
    if let Some(p) = BASIC_PROVIDER.get() {
        p.update_queue_state(
            queue_depth,
            active_workers,
            content_enqueued,
            content_dropped,
        );
    }
}

pub fn update_status_last_commit(ts: Option<i64>) {
    if let Some(p) = BASIC_PROVIDER.get() {
        p.update_last_index_commit(ts);
    }
}

pub fn update_content_plan(total_jobs: u64, total_bytes: u64) {
    if let Some(p) = BASIC_PROVIDER.get() {
        p.update_content_plan(total_jobs, total_bytes);
    }
}

pub fn increment_content_plan(new_jobs: u64, new_bytes: u64) {
    if let Some(p) = BASIC_PROVIDER.get() {
        p.increment_content_plan(new_jobs, new_bytes);
    }
}

pub fn update_content_remaining(queue_depth: u64, active_workers: u32) {
    if let Some(p) = BASIC_PROVIDER.get() {
        p.update_content_remaining(queue_depth, active_workers);
    }
}

/// Basic in-memory status provider that other modules can update.
#[derive(Debug, Default)]
pub struct BasicStatusProvider {
    state: RwLock<StatusSnapshot>,
    avg_content_job_bytes: RwLock<Option<u64>>,
}

impl BasicStatusProvider {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(StatusSnapshot {
                volumes: Vec::new(),
                scheduler_state: "unknown".into(),
                metrics: global_metrics_snapshot(Some(0), Some(0), Some(0), Some(0)),
                last_index_commit_ts: None,
                content_jobs_total: None,
                content_jobs_remaining: None,
                content_bytes_total: None,
                content_bytes_remaining: None,
            }),
            avg_content_job_bytes: RwLock::new(None),
        }
    }

    pub fn update_volumes(&self, volumes: Vec<VolumeStatus>) {
        if let Ok(mut guard) = self.state.write() {
            guard.volumes = volumes;
        }
    }

    pub fn update_scheduler_state(&self, state: impl Into<String>) {
        if let Ok(mut guard) = self.state.write() {
            guard.scheduler_state = state.into();
        }
    }

    pub fn update_metrics(&self, metrics: Option<MetricsSnapshot>) {
        if let Ok(mut guard) = self.state.write() {
            guard.metrics = metrics;
        }
    }

    pub fn update_queue_state(
        &self,
        queue_depth: Option<u64>,
        active_workers: Option<u32>,
        content_enqueued: Option<u64>,
        content_dropped: Option<u64>,
    ) {
        if let Ok(mut guard) = self.state.write() {
            let mut snap = guard.metrics.take().unwrap_or(MetricsSnapshot {
                search_latency_ms_p50: None,
                search_latency_ms_p95: None,
                worker_cpu_pct: None,
                worker_mem_bytes: None,
                queue_depth: None,
                active_workers: None,
                content_enqueued: None,
                content_dropped: None,
            });
            snap.queue_depth = queue_depth;
            snap.active_workers = active_workers;
            snap.content_enqueued = content_enqueued;
            snap.content_dropped = content_dropped;
            guard.metrics = Some(snap);
        }
    }

    pub fn update_last_index_commit(&self, ts: Option<i64>) {
        if let Ok(mut guard) = self.state.write() {
            guard.last_index_commit_ts = ts;
        }
    }

    pub fn update_content_plan(&self, total_jobs: u64, total_bytes: u64) {
        if let Ok(mut guard) = self.state.write() {
            guard.content_jobs_total = Some(total_jobs);
            guard.content_bytes_total = Some(total_bytes);
            guard.content_jobs_remaining = guard.content_jobs_remaining.or(Some(total_jobs));
            guard.content_bytes_remaining = guard.content_bytes_remaining.or(Some(total_bytes));
        }
        if let Ok(mut avg) = self.avg_content_job_bytes.write()
            && total_jobs > 0
        {
            *avg = Some(total_bytes / total_jobs.max(1));
        }
    }

    pub fn increment_content_plan(&self, new_jobs: u64, new_bytes: u64) {
        if new_jobs == 0 && new_bytes == 0 {
            return;
        }
        let (total_jobs, total_bytes) = if let Ok(mut guard) = self.state.write() {
            let total_jobs = guard.content_jobs_total.unwrap_or(0) + new_jobs;
            let total_bytes = guard.content_bytes_total.unwrap_or(0) + new_bytes;
            guard.content_jobs_total = Some(total_jobs);
            guard.content_bytes_total = Some(total_bytes);
            guard.content_jobs_remaining =
                Some(guard.content_jobs_remaining.unwrap_or(0) + new_jobs);
            guard.content_bytes_remaining =
                Some(guard.content_bytes_remaining.unwrap_or(0) + new_bytes);
            (Some(total_jobs), Some(total_bytes))
        } else {
            (None, None)
        };

        if let Ok(mut avg) = self.avg_content_job_bytes.write()
            && let (Some(jobs), Some(bytes)) = (total_jobs, total_bytes)
            && jobs > 0
        {
            *avg = Some(bytes / jobs.max(1));
        }
    }

    pub fn update_content_remaining(&self, queue_depth: u64, active_workers: u32) {
        let remaining_jobs = queue_depth + active_workers as u64;
        let avg_bytes = self.avg_content_job_bytes.read().ok().and_then(|v| *v);

        if let Ok(mut guard) = self.state.write() {
            guard.content_jobs_remaining = Some(remaining_jobs);
            if let Some(avg) = avg_bytes {
                guard.content_bytes_remaining = Some(remaining_jobs.saturating_mul(avg));
            }
            if let Some(total) = guard.content_jobs_total
                && total < remaining_jobs
            {
                guard.content_jobs_total = Some(remaining_jobs);
            }
            if let (Some(total_jobs), Some(avg)) = (guard.content_jobs_total, avg_bytes) {
                guard.content_bytes_total = Some(total_jobs.saturating_mul(avg));
            }
        }
    }
}

impl StatusProvider for BasicStatusProvider {
    fn snapshot(&self) -> StatusSnapshot {
        self.state
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|_| StatusSnapshot {
                volumes: Vec::new(),
                scheduler_state: "initializing".into(),
                metrics: global_metrics_snapshot(Some(0), Some(0), Some(0), Some(0)),
                last_index_commit_ts: None,
                content_jobs_total: None,
                content_jobs_remaining: None,
                content_bytes_total: None,
                content_bytes_remaining: None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_state_updates_metrics_fields() {
        let provider = init_basic_status_provider();
        provider.update_queue_state(Some(5), Some(2), Some(10), Some(1));
        let snap = provider.snapshot();
        let metrics = snap.metrics.unwrap();
        assert_eq!(metrics.queue_depth, Some(5));
        assert_eq!(metrics.active_workers, Some(2));
        assert_eq!(metrics.content_enqueued, Some(10));
        assert_eq!(metrics.content_dropped, Some(1));
    }

    #[test]
    fn update_metrics_none_does_not_clear_queue_state() {
        let provider = init_basic_status_provider();
        provider.update_queue_state(Some(3), Some(1), Some(4), Some(0));
        update_status_metrics(None);
        let snap = provider.snapshot();
        let metrics = snap.metrics.unwrap();
        assert_eq!(metrics.queue_depth, Some(3));
        assert_eq!(metrics.active_workers, Some(1));
    }
}
