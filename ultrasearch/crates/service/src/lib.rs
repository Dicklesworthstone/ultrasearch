//! Service support library: tracing/logging bootstrap and metrics helpers.

mod logging;
pub mod meta_ingest;
pub mod metrics;
pub mod priority;
pub mod scheduler_runtime;
pub mod search_handler;
pub mod status;
pub mod status_provider;

pub use logging::init_tracing;
pub use meta_ingest::{ingest_file_meta_batch, ingest_with_paths};
pub use metrics::{
    ServiceMetrics, ServiceMetricsSnapshot, init_metrics_from_config, scrape_metrics,
};
pub use priority::{ProcessPriority, set_process_priority};
pub use scheduler_runtime::SchedulerRuntime;
pub use search_handler::{SearchHandler, StubSearchHandler, search, set_search_handler};
pub use status_provider::{BasicStatusProvider, set_status_provider, status_snapshot};
