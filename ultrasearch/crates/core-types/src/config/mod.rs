use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

/// Global configuration root loaded from `.env` + `config/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub app: AppSection,
    #[serde(default)]
    pub logging: LoggingSection,
    #[serde(default)]
    pub metrics: MetricsSection,
    #[serde(default)]
    pub features: FeaturesSection,
    #[serde(default)]
    pub scheduler: SchedulerSection,
    #[serde(default)]
    pub paths: PathsSection,
    #[serde(default)]
    pub extract: ExtractSection,
    #[serde(default)]
    pub semantic: SemanticSection,
}

/// Load config, creating a default config file if none exists at the target path.
pub fn load_or_create_config(path: Option<&Path>) -> Result<AppConfig> {
    let target: PathBuf = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("config/config.toml"));

    if !target.exists() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut cfg = AppConfig::default();
        apply_placeholders(&mut cfg);
        let toml = toml::to_string_pretty(&cfg)?;
        fs::write(&target, toml)?;
    }

    load_config(path)
}

#[allow(clippy::derivable_impls)]
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            app: AppSection::default(),
            logging: LoggingSection::default(),
            metrics: MetricsSection::default(),
            features: FeaturesSection::default(),
            scheduler: SchedulerSection::default(),
            paths: PathsSection::default(),
            extract: ExtractSection::default(),
            semantic: SemanticSection::default(),
        }
    }
}

/// Common app-wide metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSection {
    #[serde(default = "default_product_uid")]
    pub product_uid: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

impl Default for AppSection {
    fn default() -> Self {
        Self {
            product_uid: default_product_uid(),
            data_dir: default_data_dir(),
        }
    }
}

fn default_product_uid() -> String {
    "ultrasearch".into()
}

fn default_data_dir() -> String {
    "%PROGRAMDATA%/UltraSearch".into()
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingSection {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_format")]
    pub format: String, // "json" or "text"
    #[serde(default = "default_log_file")]
    pub file: String,
    #[serde(default = "default_log_roll")]
    pub roll: String, // "daily"|"hourly"|"size" (only "daily" handled initially)
    #[serde(default = "default_log_max_size")]
    pub max_size_mb: u64,
    #[serde(default = "default_log_retain")]
    pub retain: u32,
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
            file: default_log_file(),
            roll: default_log_roll(),
            max_size_mb: default_log_max_size(),
            retain: default_log_retain(),
        }
    }
}

fn default_log_level() -> String {
    "info".into()
}
fn default_log_format() -> String {
    "json".into()
}
fn default_log_file() -> String {
    "{data_dir}/log/searchd.log".into()
}
fn default_log_roll() -> String {
    "daily".into()
}
fn default_log_max_size() -> u64 {
    100
}
fn default_log_retain() -> u32 {
    7
}

/// Metrics configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_metrics_bind")]
    pub bind: String,
    #[serde(default = "default_push_interval")]
    pub push_interval_secs: u64,
    #[serde(default = "default_sample_interval")]
    pub sample_interval_secs: u64,
    #[serde(default = "default_latency_buckets")]
    pub request_latency_buckets: Vec<f64>,
    #[serde(default = "default_worker_failure_threshold")]
    pub worker_failure_threshold: u64,
}

impl Default for MetricsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_metrics_bind(),
            push_interval_secs: default_push_interval(),
            sample_interval_secs: default_sample_interval(),
            request_latency_buckets: default_latency_buckets(),
            worker_failure_threshold: default_worker_failure_threshold(),
        }
    }
}

fn default_metrics_bind() -> String {
    "127.0.0.1:9310".into()
}
fn default_push_interval() -> u64 {
    10
}
fn default_sample_interval() -> u64 {
    10
}
fn default_latency_buckets() -> Vec<f64> {
    vec![0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0]
}
fn default_worker_failure_threshold() -> u64 {
    3
}

/// Feature flags toggling advanced modules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeaturesSection {
    #[serde(default)]
    pub multi_tier_index: bool,
    #[serde(default)]
    pub delta_index: bool,
    #[serde(default)]
    pub adaptive_scheduler: bool,
    #[serde(default)]
    pub doc_type_analyzers: bool,
    #[serde(default)]
    pub semantic_search: bool,
    #[serde(default)]
    pub plugin_system: bool,
    #[serde(default)]
    pub log_dataset_mode: bool,
    #[serde(default)]
    pub mem_opt_tuning: bool,
    #[serde(default)]
    pub auto_tuning: bool,
}

#[allow(clippy::derivable_impls)]
impl Default for FeaturesSection {
    fn default() -> Self {
        Self {
            multi_tier_index: false,
            delta_index: false,
            adaptive_scheduler: false,
            doc_type_analyzers: false,
            semantic_search: false,
            plugin_system: false,
            log_dataset_mode: false,
            mem_opt_tuning: false,
            auto_tuning: false,
        }
    }
}

/// Scheduler thresholds (base values; may be tuned adaptively).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerSection {
    #[serde(default = "default_idle_warm")]
    pub idle_warm_seconds: u64,
    #[serde(default = "default_idle_deep")]
    pub idle_deep_seconds: u64,
    #[serde(default = "default_max_records_per_tick")]
    pub max_records_per_tick: u64,
    #[serde(default = "default_usn_chunk_bytes")]
    pub usn_chunk_bytes: u64,
    #[serde(default = "default_cpu_soft")]
    pub cpu_soft_limit_pct: u64,
    #[serde(default = "default_cpu_hard")]
    pub cpu_hard_limit_pct: u64,
    #[serde(default = "default_disk_busy")]
    pub disk_busy_bytes_per_s: u64,
    #[serde(default = "default_content_batch")]
    pub content_batch_size: u64,
}

impl Default for SchedulerSection {
    fn default() -> Self {
        Self {
            idle_warm_seconds: default_idle_warm(),
            idle_deep_seconds: default_idle_deep(),
            max_records_per_tick: default_max_records_per_tick(),
            usn_chunk_bytes: default_usn_chunk_bytes(),
            cpu_soft_limit_pct: default_cpu_soft(),
            cpu_hard_limit_pct: default_cpu_hard(),
            disk_busy_bytes_per_s: default_disk_busy(),
            content_batch_size: default_content_batch(),
        }
    }
}

fn default_idle_warm() -> u64 {
    15
}
fn default_idle_deep() -> u64 {
    60
}
fn default_max_records_per_tick() -> u64 {
    10_000
}
fn default_usn_chunk_bytes() -> u64 {
    1_048_576
}
fn default_cpu_soft() -> u64 {
    50
}
fn default_cpu_hard() -> u64 {
    80
}
fn default_disk_busy() -> u64 {
    10 * 1024 * 1024
}
fn default_content_batch() -> u64 {
    1000
}

/// Index and state paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathsSection {
    #[serde(default = "default_meta_index_path")]
    pub meta_index: String,
    #[serde(default = "default_content_index_path")]
    pub content_index: String,
    #[serde(default = "default_state_dir")]
    pub state_dir: String,
    #[serde(default = "default_jobs_dir")]
    pub jobs_dir: String,
}

impl Default for PathsSection {
    fn default() -> Self {
        Self {
            meta_index: default_meta_index_path(),
            content_index: default_content_index_path(),
            state_dir: default_state_dir(),
            jobs_dir: default_jobs_dir(),
        }
    }
}

fn default_meta_index_path() -> String {
    "{data_dir}/index/meta".into()
}
fn default_content_index_path() -> String {
    "{data_dir}/index/content".into()
}
fn default_state_dir() -> String {
    "{data_dir}/volumes".into()
}
fn default_jobs_dir() -> String {
    "{data_dir}/jobs".into()
}

/// Extraction limits and flags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractSection {
    #[serde(default = "default_max_bytes")]
    pub max_bytes_per_file: u64,
    #[serde(default = "default_max_chars", alias = "max_chars")]
    pub max_chars_per_file: u64,
    #[serde(default)]
    pub ocr_enabled: bool,
    #[serde(default = "default_ocr_max_pages")]
    pub ocr_max_pages: u64,
}

impl Default for ExtractSection {
    fn default() -> Self {
        Self {
            max_bytes_per_file: default_max_bytes(),
            max_chars_per_file: default_max_chars(),
            ocr_enabled: false,
            ocr_max_pages: default_ocr_max_pages(),
        }
    }
}

fn default_max_bytes() -> u64 {
    16 * 1024 * 1024
}
fn default_max_chars() -> u64 {
    200_000
}
fn default_ocr_max_pages() -> u64 {
    10
}

/// Semantic search configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_semantic_model")]
    pub model: String,
    #[serde(default = "default_semantic_index_dir")]
    pub index_dir: String,
}

impl Default for SemanticSection {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_semantic_model(),
            index_dir: default_semantic_index_dir(),
        }
    }
}

fn default_semantic_model() -> String {
    "all-minilm-l12-v2".into()
}
fn default_semantic_index_dir() -> String {
    "{data_dir}/index/semantic".into()
}

static CONFIG: Lazy<RwLock<AppConfig>> = Lazy::new(|| RwLock::new(AppConfig::default()));

/// Load configuration from .env and a TOML file (default: `config/config.toml`).
///
/// Returns a clone of the current configuration.
pub fn load_config(path: Option<&Path>) -> Result<AppConfig> {
    let _ = dotenvy::dotenv();
    // On first load, we might want to read from disk if not done yet.
    reload_config(path)
}

/// Force reload configuration from disk.
pub fn reload_config(path: Option<&Path>) -> Result<AppConfig> {
    let target = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("config/config.toml"));

    let mut lock = CONFIG.write().map_err(|_| anyhow::anyhow!("config lock poisoned"))?;

    if target.exists() {
        let raw = fs::read_to_string(&target)?;
        // Load into a temporary to validate
        let mut file_cfg: AppConfig = toml::from_str(&raw)?;
        apply_placeholders(&mut file_cfg);
        
        *lock = file_cfg.clone();
        Ok(file_cfg)
    } else {
        // If no file, return what we have (defaults or previous)
        Ok(lock.clone())
    }
}

/// Replace `{data_dir}` placeholder tokens with the configured data_dir.
fn apply_placeholders(cfg: &mut AppConfig) {
    let dd = cfg.app.data_dir.clone();
    cfg.logging.file = cfg.logging.file.replace("{data_dir}", &dd);
    cfg.paths.meta_index = cfg.paths.meta_index.replace("{data_dir}", &dd);
    cfg.paths.content_index = cfg.paths.content_index.replace("{data_dir}", &dd);
    cfg.paths.state_dir = cfg.paths.state_dir.replace("{data_dir}", &dd);
    cfg.paths.jobs_dir = cfg.paths.jobs_dir.replace("{data_dir}", &dd);
    cfg.semantic.index_dir = cfg.semantic.index_dir.replace("{data_dir}", &dd);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Merge `override_cfg` into `base`, taking any values present in `override_cfg`.
    fn merge(mut base: AppConfig, override_cfg: AppConfig) -> AppConfig {
        // Simple field replacement; nested structs are fully replaced.
        base.app = override_cfg.app;
        base.logging = override_cfg.logging;
        base.metrics = override_cfg.metrics;
        base.features = override_cfg.features;
        base.scheduler = override_cfg.scheduler;
        base.paths = override_cfg.paths;
        base.extract = override_cfg.extract;
        base.semantic = override_cfg.semantic;
        base
    }

    #[test]
    fn default_placeholders_expand() {
        let mut cfg = AppConfig::default();
        cfg.app.data_dir = "X:/UltraSearch".into();
        apply_placeholders(&mut cfg);
        assert_eq!(cfg.logging.file, "X:/UltraSearch/log/searchd.log");
        assert!(
            cfg.paths
                .meta_index
                .starts_with("X:/UltraSearch/index/meta")
        );
        assert!(
            cfg.semantic
                .index_dir
                .starts_with("X:/UltraSearch/index/semantic")
        );
    }

    #[test]
    fn merge_prefers_override() {
        let base = AppConfig::default();
        let mut override_cfg = AppConfig::default();
        override_cfg.logging.level = "debug".into();
        let merged = merge(base, override_cfg);
        assert_eq!(merged.logging.level, "debug");
    }

    #[test]
    fn metrics_defaults_include_buckets_and_threshold() {
        let cfg = AppConfig::default();
        assert!(!cfg.metrics.request_latency_buckets.is_empty());
        assert_eq!(cfg.metrics.worker_failure_threshold, 3);
    }

    #[test]
    fn scheduler_defaults_match_docs() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.scheduler.idle_warm_seconds, 15);
        assert_eq!(cfg.scheduler.idle_deep_seconds, 60);
        assert_eq!(cfg.scheduler.max_records_per_tick, 10_000);
        assert_eq!(cfg.scheduler.usn_chunk_bytes, 1_024 * 1_024);
        assert_eq!(cfg.scheduler.cpu_soft_limit_pct, 50);
        assert_eq!(cfg.scheduler.cpu_hard_limit_pct, 80);
    }

    #[test]
    fn extract_section_alias_for_max_chars() {
        // Ensure legacy "max_chars" still deserializes via alias.
        let toml_str = r#"""
            [extract]
            max_chars = 12345
        """#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.extract.max_chars_per_file, 12_345);
    }
}