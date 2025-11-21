//! Service support library: tracing/logging bootstrap and metrics helpers.

mod logging;
pub mod metrics;
pub mod priority;
pub mod status;

pub use logging::init_tracing;
pub use metrics::{
    ServiceMetrics, ServiceMetricsSnapshot, init_metrics_from_config, scrape_metrics,
};
pub use priority::{ProcessPriority, set_process_priority};
