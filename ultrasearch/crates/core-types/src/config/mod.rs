use std::{fs, path::Path};

use anyhow::Result;
use once_cell::sync::OnceCell;
use serde::Deserialize;

/// Global configuration root loaded from `.env` + `config/config.toml`.
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
pub struct MetricsSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_metrics_bind")]
    pub bind: String,
    #[serde(default = "default_push_interval")]
    pub push_interval_secs: u64,
    #[serde(default = "default_sample_interval")]
    pub sample_interval_secs: u64,
}

impl Default for MetricsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_metrics_bind(),
            push_interval_secs: default_push_interval(),
            sample_interval_secs: default_sample_interval(),
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

/// Feature flags toggling advanced modules.
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerSection {
    #[serde(default = "default_idle_warm")]
    pub idle_warm_seconds: u64,
    #[serde(default = "default_idle_deep")]
    pub idle_deep_seconds: u64,
    #[serde(default = "default_cpu_low")]
    pub cpu_low_pct: u64,
    #[serde(default = "default_cpu_mid")]
    pub cpu_mid_pct: u64,
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
            cpu_low_pct: default_cpu_low(),
            cpu_mid_pct: default_cpu_mid(),
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
fn default_cpu_low() -> u64 {
    20
}
fn default_cpu_mid() -> u64 {
    50
}
fn default_disk_busy() -> u64 {
    10 * 1024 * 1024
}
fn default_content_batch() -> u64 {
    1000
}

/// Index and state paths.
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
pub struct ExtractSection {
    #[serde(default = "default_max_bytes")]
    pub max_bytes_per_file: u64,
    #[serde(default = "default_max_chars")]
    pub max_chars: u64,
    #[serde(default)]
    pub ocr_enabled: bool,
    #[serde(default = "default_ocr_max_pages")]
    pub ocr_max_pages: u64,
}

impl Default for ExtractSection {
    fn default() -> Self {
        Self {
            max_bytes_per_file: default_max_bytes(),
            max_chars: default_max_chars(),
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
#[derive(Debug, Clone, Deserialize)]
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

static CONFIG: OnceCell<AppConfig> = OnceCell::new();

/// Load configuration from .env and a TOML file (default: `config/config.toml`).
///
/// The first successful call caches the config globally for reuse.
pub fn load_config(path: Option<&Path>) -> Result<&'static AppConfig> {
    // Safe to call multiple times; dotenvy ignores missing file.
    let _ = dotenvy::dotenv();

    CONFIG.get_or_try_init(|| {
        let mut cfg = AppConfig::default();
        if let Some(path) = path {
            if path.exists() {
                let raw = fs::read_to_string(path)?;
                let file_cfg: AppConfig = toml::from_str(&raw)?;
                cfg = merge(cfg, file_cfg);
            }
        } else if Path::new("config/config.toml").exists() {
            let raw = fs::read_to_string("config/config.toml")?;
            let file_cfg: AppConfig = toml::from_str(&raw)?;
            cfg = merge(cfg, file_cfg);
        }
        apply_placeholders(&mut cfg);
        Ok(cfg)
    })
}

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
