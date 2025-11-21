//! Service support library: tracing/logging bootstrap and metrics helpers.

pub mod bootstrap;
pub mod dispatcher;
mod logging;
pub mod memory;
pub mod meta_ingest;
pub mod metrics;
pub mod planner;
pub mod priority;
pub mod scheduler_runtime;
pub mod search_handler;
pub mod status;
pub mod status_provider;

#[cfg(windows)]
pub mod windows;

pub mod ipc; // I forgot to add this!

pub use logging::{init_tracing, init_tracing_with_config};
pub use meta_ingest::{ingest_file_meta_batch, ingest_with_paths};
pub use metrics::{
    ServiceMetrics, ServiceMetricsSnapshot, init_metrics_from_config, scrape_metrics,
};
pub use priority::{ProcessPriority, set_process_priority};
pub use scheduler_runtime::{SchedulerRuntime, set_live_active_workers, set_live_queue_counts};
pub use search_handler::{
    SearchHandler, StubSearchHandler, UnifiedSearchHandler, search, set_search_handler,
};
pub use status_provider::{
    BasicStatusProvider, init_basic_status_provider, set_status_provider, status_snapshot,
};
