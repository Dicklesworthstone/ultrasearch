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
    if let Some(p) = BASIC_PROVIDER.get() {
        p.update_metrics(metrics);
    }
}

pub fn update_status_queue_state(
    queue_depth: Option<u64>,
    active_workers: Option<u32>,
    content_enqueued: Option<u64>,
    content_dropped: Option<u64>,
) {
    if let Some(p) = BASIC_PROVIDER.get() {
        p.update_queue_state(queue_depth, active_workers, content_enqueued, content_dropped);
    }
}

pub fn update_status_last_commit(ts: Option<i64>) {
    if let Some(p) = BASIC_PROVIDER.get() {
        p.update_last_index_commit(ts);
    }
}

/// Basic in-memory status provider that other modules can update.
#[derive(Debug, Default)]
pub struct BasicStatusProvider {
    state: RwLock<StatusSnapshot>,
}

impl BasicStatusProvider {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(StatusSnapshot {
                volumes: Vec::new(),
                scheduler_state: "unknown".into(),
                metrics: global_metrics_snapshot(Some(0), Some(0), Some(0), Some(0)),
                last_index_commit_ts: None,
            }),
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
            })
    }
}
