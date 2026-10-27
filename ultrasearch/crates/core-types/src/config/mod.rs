use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

/// Global configuration root loaded from `.env` + `config/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub content_index_volumes: Vec<String>,
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
    #[serde(default, alias = "indexing")]
    pub extract: ExtractSection,
    #[serde(default)]
    pub semantic: SemanticSection,
}

/// Load config, creating a default config file if none exists at the target path.
pub fn load_or_create_config(path: Option<&Path>) -> Result<AppConfig> {
    // PROGRAMDATA may come from .env. Resolve the target only after loading it,
    // and pass that exact target onward instead of selecting a second default.
    let _ = dotenvy::dotenv();
    let target: PathBuf = path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path);
    create_default_config(&target)?;
    reload_config(Some(&target))
}

fn create_default_config(target: &Path) -> Result<()> {
    if target.try_exists()? {
        return Ok(());
    }
    if let Some(parent) = target.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    // Keep portable templates on disk. Expanding them before serialization
    // bakes one process's environment into every later service invocation.
    let template = toml::to_string_pretty(&AppConfig::default())?;
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
    {
        Ok(file) => file,
        // Another startup may have created the configuration after try_exists.
        // Never truncate its file. The subsequent load must still validate it.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error).context("cannot create default configuration"),
    };
    file.write_all(template.as_bytes())?;
    file.sync_all()?;
    Ok(())
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
            volumes: Vec::new(),
            content_index_volumes: Vec::new(),
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
    if cfg!(windows) {
        "%PROGRAMDATA%/UltraSearch".into()
    } else {
        // Fallback for Linux/macOS (XDG-ish)
        "$HOME/.local/share/ultrasearch".into()
    }
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
    2
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
    #[serde(default)]
    pub telemetry_opt_in: bool,
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
            telemetry_opt_in: false,
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
    #[serde(default)]
    pub power_save_mode: bool,
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
            power_save_mode: true, // Default to enabled
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

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// The source and runtime binding change under the same lock as the value.
/// A failed reload cannot redirect the next attempt or expose a partial update.
#[derive(Default)]
struct ConfigState {
    value: AppConfig,
    source: Option<PathBuf>,
    runtime_bound: bool,
    revision: u64,
}

impl ConfigState {
    fn bind_runtime(&mut self, cfg: AppConfig) -> Result<()> {
        cfg.validate()?;
        let revision = self.next_revision()?;
        self.value = cfg;
        self.runtime_bound = true;
        self.revision = revision;
        Ok(())
    }

    fn next_revision(&self) -> Result<u64> {
        self.revision
            .checked_add(1)
            .context("configuration revision exhausted")
    }

    fn prepare_reload(&self, path: Option<&Path>) -> ReloadRequest {
        ReloadRequest {
            target: path
                .map(Path::to_path_buf)
                .or_else(|| self.source.clone())
                .unwrap_or_else(default_config_path),
            revision: self.revision,
        }
    }

    fn publish_reload(&mut self, loaded: LoadedConfig) -> Result<AppConfig> {
        ensure!(
            loaded.revision == self.revision,
            "configuration changed during reload; retry against the current configuration"
        );
        if self.runtime_bound {
            ensure_runtime_paths_unchanged(&self.value, &loaded.value)?;
        }
        let revision = self.next_revision()?;
        self.value = loaded.value;
        self.source = Some(loaded.source);
        self.revision = revision;
        Ok(self.value.clone())
    }

    #[cfg(test)]
    fn reload(&mut self, path: Option<&Path>) -> Result<AppConfig> {
        let loaded = self.prepare_reload(path).load()?;
        self.publish_reload(loaded)
    }
}

struct ReloadRequest {
    target: PathBuf,
    revision: u64,
}

struct LoadedConfig {
    value: AppConfig,
    source: PathBuf,
    revision: u64,
}

impl ReloadRequest {
    fn load(self) -> Result<LoadedConfig> {
        // Anchor a relative --config once without resolving symlinks. Reopen
        // the chosen path after an editor replaces it or a symlink is retargeted.
        let target = std::path::absolute(self.target)?;
        let file = fs::File::open(&target)
            .with_context(|| format!("cannot open configuration {}", target.display()))?;
        ensure!(
            file.metadata()?.is_file(),
            "configuration must be a regular file"
        );
        let mut candidate = parse_config(file)
            .with_context(|| format!("cannot load configuration {}", target.display()))?;
        apply_placeholders(&mut candidate);
        candidate.validate()?;
        Ok(LoadedConfig {
            value: candidate,
            source: target,
            revision: self.revision,
        })
    }
}

fn reload_state(
    state: &RwLock<ConfigState>,
    path: Option<&Path>,
    load: impl FnOnce(ReloadRequest) -> Result<LoadedConfig>,
) -> Result<AppConfig> {
    let request = {
        let state = state
            .read()
            .map_err(|_| anyhow::anyhow!("config lock poisoned"))?;
        state.prepare_reload(path)
    };
    // Slow/offline storage must not hold the lock read by every ingestion and
    // scheduler tick. Publication revalidates the revision after all file IO,
    // parsing and expansion; a late reload cannot undo a startup or newer load.
    let loaded = load(request)?;
    state
        .write()
        .map_err(|_| anyhow::anyhow!("config lock poisoned"))?
        .publish_reload(loaded)
}

fn parse_config(source: impl Read) -> Result<AppConfig> {
    // Bound the actual read, including growth after open. A one-byte lookahead
    // distinguishes an exactly sized document from a truncated configuration.
    let mut raw = String::new();
    source.take(MAX_CONFIG_BYTES + 1).read_to_string(&mut raw)?;
    ensure!(
        raw.len() as u64 <= MAX_CONFIG_BYTES,
        "configuration exceeds 1 MiB"
    );
    toml::from_str(&raw).context("invalid configuration TOML")
}

fn ensure_runtime_paths_unchanged(active: &AppConfig, candidate: &AppConfig) -> Result<()> {
    // These destinations are already owned by live readers, writers, leases,
    // logging and exclusion masks. Reject the entire reload before publication;
    // the watcher must never observe paths that would stop its ingestion lane.
    let changed: Vec<_> = [
        (
            "paths.meta_index",
            &active.paths.meta_index,
            &candidate.paths.meta_index,
        ),
        (
            "paths.content_index",
            &active.paths.content_index,
            &candidate.paths.content_index,
        ),
        (
            "paths.state_dir",
            &active.paths.state_dir,
            &candidate.paths.state_dir,
        ),
        (
            "paths.jobs_dir",
            &active.paths.jobs_dir,
            &candidate.paths.jobs_dir,
        ),
        (
            "logging.file",
            &active.logging.file,
            &candidate.logging.file,
        ),
        (
            "semantic.index_dir",
            &active.semantic.index_dir,
            &candidate.semantic.index_dir,
        ),
    ]
    .into_iter()
    .filter_map(|(name, old, new)| (old != new).then_some(name))
    .collect();
    ensure!(
        changed.is_empty(),
        "configuration changes to {} require a service restart; active configuration unchanged",
        changed.join(", ")
    );
    Ok(())
}

static CONFIG: Lazy<RwLock<ConfigState>> = Lazy::new(|| RwLock::new(ConfigState::default()));

/// Get a clone of the currently loaded configuration.
pub fn get_current_config() -> AppConfig {
    CONFIG.read().expect("config lock poisoned").value.clone()
}

/// Publish the effective startup configuration to runtime consumers without
/// reloading or writing a configuration file. Validate before replacing it so
/// rejected changes leave the running configuration intact. Bind this before
/// starting workers: later disk reloads cannot change destinations they own.
/// Calling it again is an explicit startup/reinitialization, not a disk reload.
pub fn set_current_config(cfg: AppConfig) -> Result<()> {
    let mut lock = CONFIG
        .write()
        .map_err(|_| anyhow::anyhow!("config lock poisoned"))?;
    lock.bind_runtime(cfg)
}

/// Load configuration from .env and a TOML file. With no explicit path, reuse
/// the last successful source, or select default_config_path on the first load.
///
/// Returns a clone of the current configuration.
pub fn load_config(path: Option<&Path>) -> Result<AppConfig> {
    let _ = dotenvy::dotenv();
    reload_config(path)
}

/// Reload the last successfully loaded file, or an explicitly supplied path.
/// Missing, unreadable, invalid or incompatible files are errors; none replace
/// the running value or its source. Use load_or_create_config for first startup.
pub fn reload_config(path: Option<&Path>) -> Result<AppConfig> {
    let _ = dotenvy::dotenv();
    reload_state(&CONFIG, path, ReloadRequest::load)
}

impl AppConfig {
    /// Validate configuration constraints.
    pub fn validate(&self) -> Result<()> {
        // IdleTracker requires an ordered pair at construction and on live
        // threshold updates. Reject disk reloads and startup bindings before
        // publication, rather than panicking the scheduler after a success ACK.
        ensure!(
            self.scheduler.idle_deep_seconds >= self.scheduler.idle_warm_seconds,
            "scheduler.idle_deep_seconds must be greater than or equal to scheduler.idle_warm_seconds"
        );
        if self.features.delta_index && !self.features.multi_tier_index {
            return Err(anyhow::anyhow!(
                "Feature 'delta_index' requires 'multi_tier_index' to be enabled"
            ));
        }
        // Semantic search requires model to be specified (default has one, but if user clears it?)
        if self.features.semantic_search && self.semantic.model.is_empty() {
            return Err(anyhow::anyhow!(
                "Feature 'semantic_search' requires a valid model configuration"
            ));
        }
        Ok(())
    }
}

/// Replace `{data_dir}` placeholder tokens with the configured data_dir,
/// and expand environment variables (e.g. `%PROGRAMDATA%` or `$HOME`).
fn apply_placeholders(cfg: &mut AppConfig) {
    cfg.app.data_dir = expand_env_vars(&cfg.app.data_dir);
    let data_dir = cfg.app.data_dir.clone();
    for path in [
        &mut cfg.logging.file,
        &mut cfg.paths.meta_index,
        &mut cfg.paths.content_index,
        &mut cfg.paths.state_dir,
        &mut cfg.paths.jobs_dir,
        &mut cfg.semantic.index_dir,
    ] {
        *path = expand_tokens(path, Some(&data_dir), true, |name| std::env::var(name).ok());
    }
}

/// Expand whole `$VAR`/`${VAR}` tokens and `%VAR%` tokens on every platform.
/// Never scan replacement values again: environment iteration order cannot
/// redirect data. `expand_tokens` can still disable percent tokens.
fn expand_env_vars(input: &str) -> String {
    expand_tokens(input, None, true, |name| std::env::var(name).ok())
}

/// Testable expansion over an explicit environment. Percent tokens are
/// case-insensitive. `$HOME_CUSTOM` is one identifier, not a prefix of `$HOME`.
#[cfg(test)]
fn expand_env_vars_with<I>(input: &str, vars: I) -> String
where
    I: IntoIterator<Item = (String, String)>,
{
    let pairs: Vec<(String, String)> = vars.into_iter().collect();
    expand_tokens(input, None, true, |name| {
        pairs
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    })
}

fn expand_tokens(
    input: &str,
    data_dir: Option<&str>,
    windows_tokens: bool,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> String {
    fn starts_name(byte: u8) -> bool {
        byte.is_ascii_alphabetic() || byte == b'_'
    }
    fn continues_name(byte: u8) -> bool {
        starts_name(byte) || byte.is_ascii_digit()
    }

    let mut out = String::with_capacity(input.len());
    let mut remaining = input;
    while !remaining.is_empty() {
        if let Some(value) = data_dir
            && let Some(suffix) = remaining.strip_prefix("{data_dir}")
        {
            out.push_str(value);
            remaining = suffix;
            continue;
        }
        let token = if remaining.starts_with("${") {
            // Preserve malformed/unknown tokens exactly instead of partially
            // replacing a valid prefix of the variable name.
            let Some(end) = remaining.find('}') else {
                out.push_str(remaining);
                break;
            };
            let name = &remaining[2..end];
            let valid = name
                .as_bytes()
                .first()
                .is_some_and(|byte| starts_name(*byte))
                && name.bytes().all(continues_name);
            Some((end + 1, valid.then_some(name.to_string())))
        } else if let Some(suffix) = remaining.strip_prefix('$') {
            let length = suffix
                .bytes()
                .take_while(|byte| continues_name(*byte))
                .count();
            let valid = suffix
                .as_bytes()
                .first()
                .is_some_and(|byte| starts_name(*byte));
            Some((length + 1, valid.then(|| suffix[..length].to_string())))
        } else if windows_tokens && remaining.starts_with('%') {
            remaining[1..].find('%').map(|end| {
                let name = &remaining[1..end + 1];
                (
                    end + 2,
                    (!name.is_empty()).then(|| name.to_ascii_uppercase()),
                )
            })
        } else {
            None
        };
        if let Some((length, name)) = token {
            match name.as_deref().and_then(&mut lookup) {
                Some(value) => out.push_str(&value),
                None => out.push_str(&remaining[..length]),
            }
            remaining = &remaining[length..];
        } else {
            // Only slice at Unicode boundaries, including around unknown names.
            let character = remaining.chars().next().expect("nonempty remainder");
            out.push(character);
            remaining = &remaining[character.len_utf8()..];
        }
    }
    out
}

/// Default configuration path for installed binaries: `%PROGRAMDATA%/UltraSearch/config/config.toml`.
/// Falls back to a relative `config/config.toml` if PROGRAMDATA is missing (developer runs).
pub fn default_config_path() -> PathBuf {
    std::env::var("PROGRAMDATA")
        .map(|pd| {
            PathBuf::from(pd)
                .join("UltraSearch")
                .join("config")
                .join("config.toml")
        })
        .unwrap_or_else(|_| PathBuf::from("config/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Retain these small fixtures for inspection. Local ConfigState instances
    // exercise the production loader without racing process-wide settings or
    // changing environment variables/current_dir in the parallel test suite.
    fn config_fixture() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ultrasearch-config-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    fn fixture_config(root: &Path) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.app.data_dir = root.to_str().unwrap().into();
        cfg
    }

    fn save_fixture(path: &Path, cfg: &AppConfig) {
        fs::write(path, toml::to_string_pretty(cfg).unwrap()).unwrap();
    }

    fn config_json(cfg: &AppConfig) -> serde_json::Value {
        serde_json::to_value(cfg).unwrap()
    }

    #[test]
    fn custom_source_survives_runtime_binding_and_atomic_file_replacement() {
        let root = config_fixture();
        let source = root.join("custom.toml");
        let mut cfg = fixture_config(&root);
        cfg.volumes = vec!["X:\\".into()];
        save_fixture(&source, &cfg);
        let mut state = ConfigState::default();
        let initial = state.reload(Some(&source)).unwrap();
        state.bind_runtime(initial.clone()).unwrap();

        cfg.extract.max_bytes_per_file = 1234;
        cfg.volumes.push("Y:\\".into());
        let replacement = root.join("replacement.toml");
        save_fixture(&replacement, &cfg);
        fs::rename(replacement, &source).unwrap();
        let updated = state.reload(None).unwrap();
        assert_eq!(state.source.as_deref(), Some(source.as_path()));
        assert_eq!(updated.extract.max_bytes_per_file, 1234);
        assert_eq!(updated.volumes, ["X:\\", "Y:\\"]);
        ensure_runtime_paths_unchanged(&initial, &updated).unwrap();
    }

    #[test]
    fn all_live_destinations_reject_the_entire_reload_before_publication() {
        let root = config_fixture();
        let source = root.join("custom.toml");
        let alternate = root.join("rejected.toml");
        save_fixture(&source, &fixture_config(&root));
        let mut state = ConfigState::default();
        let active = state.reload(Some(&source)).unwrap();
        state.bind_runtime(active.clone()).unwrap();
        for field in [
            "paths.meta_index",
            "paths.content_index",
            "paths.state_dir",
            "paths.jobs_dir",
            "logging.file",
            "semantic.index_dir",
        ] {
            let mut candidate = active.clone();
            candidate.extract.max_bytes_per_file = 1;
            candidate.volumes = vec!["Z:\\".into()];
            let destination = match field {
                "paths.meta_index" => &mut candidate.paths.meta_index,
                "paths.content_index" => &mut candidate.paths.content_index,
                "paths.state_dir" => &mut candidate.paths.state_dir,
                "paths.jobs_dir" => &mut candidate.paths.jobs_dir,
                "logging.file" => &mut candidate.logging.file,
                "semantic.index_dir" => &mut candidate.semantic.index_dir,
                _ => unreachable!(),
            };
            destination.push_str("-relocated");
            save_fixture(&alternate, &candidate);
            let error = state.reload(Some(&alternate)).unwrap_err();
            assert!(error.to_string().contains(field), "{error:#}");
            assert!(error.to_string().contains("restart"));
            assert_eq!(config_json(&state.value), config_json(&active));
            assert_eq!(state.source.as_deref(), Some(source.as_path()));
            // A rejected alternate source cannot redirect a subsequent IPC
            // reload(None), even though the alternate file remains present.
            assert_eq!(
                config_json(&state.reload(None).unwrap()),
                config_json(&active)
            );
        }
    }

    #[test]
    fn missing_invalid_and_failed_validation_reload_preserve_value_and_source() {
        let root = config_fixture();
        let source = root.join("active.toml");
        save_fixture(&source, &fixture_config(&root));
        let mut state = ConfigState::default();
        let active = state.reload(Some(&source)).unwrap();
        state.bind_runtime(active.clone()).unwrap();
        assert!(state.reload(Some(&root.join("missing.toml"))).is_err());
        assert!(state.reload(Some(&root)).is_err());
        for raw in [
            "[extract]\nmax_bytes_per_file =",
            "[features]\ndelta_index = true\nmulti_tier_index = false\n",
        ] {
            fs::write(&source, raw).unwrap();
            assert!(state.reload(None).is_err());
            assert_eq!(config_json(&state.value), config_json(&active));
            assert_eq!(state.source.as_deref(), Some(source.as_path()));
        }
        fs::rename(&source, root.join("retained-invalid.toml")).unwrap();
        assert!(
            state.reload(None).is_err(),
            "missing is not successful idle reload"
        );
        assert_eq!(config_json(&state.value), config_json(&active));
        save_fixture(&source, &active);
        assert_eq!(
            config_json(&state.reload(None).unwrap()),
            config_json(&active)
        );
    }

    #[test]
    fn oversized_configuration_is_bounded_and_does_not_replace_active_state() {
        use std::io::Cursor;
        let exact = vec![b' '; MAX_CONFIG_BYTES as usize];
        parse_config(exact.as_slice()).unwrap();
        let mut too_large = Cursor::new(vec![b' '; MAX_CONFIG_BYTES as usize + 4096]);
        let error = parse_config(&mut too_large).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
        assert_eq!(too_large.position(), MAX_CONFIG_BYTES + 1);

        let root = config_fixture();
        let source = root.join("active.toml");
        save_fixture(&source, &fixture_config(&root));
        let mut state = ConfigState::default();
        let active = state.reload(Some(&source)).unwrap();
        fs::write(&source, too_large.into_inner()).unwrap();
        assert!(state.reload(None).is_err());
        assert_eq!(config_json(&state.value), config_json(&active));
        assert_eq!(state.source.as_deref(), Some(source.as_path()));
    }

    #[test]
    fn read_errors_and_invalid_utf8_cannot_publish_a_valid_prefix() {
        struct BrokenTail(std::io::Cursor<&'static [u8]>);
        impl Read for BrokenTail {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.0.position() as usize == self.0.get_ref().len() {
                    return Err(std::io::Error::other("fixture read failure"));
                }
                self.0.read(buffer)
            }
        }
        let valid_prefix = &b"[extract]\nmax_bytes_per_file = 1\n"[..];
        assert!(parse_config(BrokenTail(std::io::Cursor::new(valid_prefix))).is_err());
        assert!(parse_config(&b"# valid TOML prefix\n\xff"[..]).is_err());
    }

    #[test]
    fn default_creation_preserves_templates_and_never_overwrites_existing_files() {
        let root = config_fixture();
        let source = root.join("config").join("config.toml");
        create_default_config(&source).unwrap();
        let raw = fs::read_to_string(&source).unwrap();
        let defaults: AppConfig = toml::from_str(&raw).unwrap();
        assert_eq!(defaults.paths.meta_index, "{data_dir}/index/meta");
        assert_eq!(defaults.app.data_dir, default_data_dir());
        fs::write(&source, "# explicitly retained user configuration\n").unwrap();
        create_default_config(&source).unwrap();
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "# explicitly retained user configuration\n"
        );
    }

    #[test]
    fn explicit_startup_rebinding_allows_new_destinations_but_validates_first() {
        let mut state = ConfigState::default();
        let mut first = AppConfig::default();
        first.paths.meta_index = "first-generation".into();
        state.bind_runtime(first.clone()).unwrap();
        let mut invalid = first.clone();
        invalid.paths.meta_index = "must-not-publish".into();
        invalid.features.delta_index = true;
        invalid.features.multi_tier_index = false;
        assert!(state.bind_runtime(invalid).is_err());
        assert_eq!(config_json(&state.value), config_json(&first));
        first.paths.meta_index = "restarted-generation".into();
        state.bind_runtime(first.clone()).unwrap();
        assert_eq!(config_json(&state.value), config_json(&first));
        assert!(state.runtime_bound);
    }

    #[test]
    fn environment_tokens_do_not_confuse_variable_name_prefixes() {
        let lookup = |name: &str| match name {
            "HOME" => Some("short".into()),
            "HOME_DIR" => Some("long/λ".into()),
            _ => None,
        };
        assert_eq!(
            expand_tokens("$HOME_DIR/${HOME}/$HOME_SUFFIX/🥁", None, false, lookup),
            "long/λ/short/$HOME_SUFFIX/🥁"
        );
        assert_eq!(
            expand_tokens("%home_dir%/%HOME%/%%/%missing%", None, true, lookup),
            "long/λ/short/%%/%missing%"
        );
        assert_eq!(expand_tokens("%HOME%", None, false, lookup), "%HOME%");
    }

    #[test]
    fn expanded_values_and_data_directory_are_never_reinterpreted_as_tokens() {
        let lookup = |name: &str| match name {
            "BASE" => Some("literal$NEXT%NEXT%{data_dir}".into()),
            "NEXT" => panic!("inserted values must never be expanded recursively"),
            _ => None,
        };
        assert_eq!(
            expand_tokens(
                "{data_dir}/$BASE/${BASE}",
                Some("dir$NEXT%NEXT%{data_dir}"),
                true,
                lookup,
            ),
            "dir$NEXT%NEXT%{data_dir}/literal$NEXT%NEXT%{data_dir}/literal$NEXT%NEXT%{data_dir}"
        );
    }

    #[test]
    fn unknown_and_malformed_expansion_tokens_are_preserved_verbatim() {
        for input in [
            "$",
            "$9",
            "${}",
            "${MISSING}",
            "${MISSING",
            "%",
            "%%",
            "%UNFINISHED",
            "%MISSING%",
            "λ/${HOME:-fallback}/🥁",
        ] {
            assert_eq!(expand_tokens(input, None, true, |_| None), input);
        }
        assert_eq!(
            expand_tokens("${HOME:-fallback}", None, true, |_| Some("wrong".into())),
            "${HOME:-fallback}"
        );
    }

    #[test]
    fn indexing_alias_loads_real_limits_and_rejects_ambiguous_duplicate_sections() {
        let cfg =
            parse_config(&b"[indexing]\nmax_bytes_per_file = 1234\nmax_chars = 45\n"[..]).unwrap();
        assert_eq!(cfg.extract.max_bytes_per_file, 1234);
        assert_eq!(cfg.extract.max_chars_per_file, 45);
        assert!(parse_config(&b"[indexing]\n[extract]\n"[..]).is_err());
    }

    #[test]
    fn stale_reload_cannot_undo_a_new_source_or_runtime_binding() {
        let root = config_fixture();
        let first = root.join("first.toml");
        let second = root.join("second.toml");
        let original = fixture_config(&root);
        save_fixture(&first, &original);
        let mut newer = original.clone();
        newer.extract.max_bytes_per_file = 4321;
        save_fixture(&second, &newer);
        for restart in [false, true] {
            let mut state = ConfigState::default();
            state.reload(Some(&first)).unwrap();
            let stale = state.prepare_reload(Some(&first)).load().unwrap();
            let active = if restart {
                apply_placeholders(&mut newer);
                state.bind_runtime(newer.clone()).unwrap();
                newer.clone()
            } else {
                state.reload(Some(&second)).unwrap()
            };
            let source = state.source.clone();
            let revision = state.revision;
            let error = state.publish_reload(stale).unwrap_err();
            assert!(error.to_string().contains("changed during reload"));
            assert_eq!(config_json(&state.value), config_json(&active));
            assert_eq!(state.source, source);
            assert_eq!(state.revision, revision);
        }
    }

    #[test]
    fn delayed_reload_leaves_configuration_readers_and_startup_unblocked() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;

        let root = config_fixture();
        let source = root.join("active.toml");
        save_fixture(&source, &fixture_config(&root));
        let state = Arc::new(RwLock::new(ConfigState::default()));
        let original = reload_state(&state, Some(&source), ReloadRequest::load).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let loading = Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            reload_state(&loading, None, |request| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                request.load()
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let visible = state.try_read().ok().map(|guard| config_json(&guard.value));
        let mut restarted = original.clone();
        restarted.paths.meta_index.push_str("-restarted");
        let rebound = state
            .try_write()
            .ok()
            .map(|mut guard| guard.bind_runtime(restarted.clone()));
        // Release and join before assertions so a regression cannot strand a
        // test thread. The loader performs the real disk read after release.
        release_tx.send(()).unwrap();
        let result = worker.join().unwrap();
        assert_eq!(visible, Some(config_json(&original)));
        assert!(rebound.is_some_and(|result| result.is_ok()));
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("changed during reload")
        );
        assert_eq!(
            config_json(&state.read().unwrap().value),
            config_json(&restarted)
        );
    }

    #[test]
    fn rejected_updates_do_not_consume_or_wrap_configuration_revisions() {
        let root = config_fixture();
        let source = root.join("active.toml");
        save_fixture(&source, &fixture_config(&root));
        let mut state = ConfigState::default();
        let active = state.reload(Some(&source)).unwrap();
        state.revision = u64::MAX;
        let source = state.source.clone();
        assert!(
            state
                .reload(None)
                .unwrap_err()
                .to_string()
                .contains("revision exhausted")
        );
        assert!(state.bind_runtime(active.clone()).is_err());
        assert_eq!(state.revision, u64::MAX);
        assert_eq!(state.source, source);
        assert_eq!(config_json(&state.value), config_json(&active));
        assert!(!state.runtime_bound);
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
        base.volumes = override_cfg.volumes;
        base.content_index_volumes = override_cfg.content_index_volumes;
        base
    }

    #[test]
    fn invalid_idle_thresholds_never_publish_on_startup_or_reload() {
        let root = config_fixture();
        let source = root.join("active.toml");
        let alternate = root.join("invalid-idle.toml");
        let cfg = fixture_config(&root);
        let mut invalid = cfg.clone();
        invalid.scheduler.idle_warm_seconds = 61;
        invalid.scheduler.idle_deep_seconds = 60;
        invalid.extract.max_bytes_per_file = 123;
        save_fixture(&alternate, &invalid);

        let mut state = ConfigState::default();
        let initial = config_json(&state.value);
        for from_disk in [true, false] {
            let error = if from_disk {
                state.reload(Some(&alternate)).unwrap_err()
            } else {
                state.bind_runtime(invalid.clone()).unwrap_err()
            };
            assert!(error.to_string().contains("scheduler.idle_deep_seconds"));
            assert_eq!(config_json(&state.value), initial);
            assert!(state.source.is_none());
            assert!(!state.runtime_bound);
            assert_eq!(state.revision, 0);
        }

        save_fixture(&source, &cfg);
        let active = state.reload(Some(&source)).unwrap();
        state.bind_runtime(active.clone()).unwrap();
        let revision = state.revision;
        // Keep destinations identical: these attempts must fail on the idle
        // invariant itself, not merely on the live-path relocation guard.
        invalid = active.clone();
        invalid.scheduler.idle_warm_seconds = 61;
        invalid.scheduler.idle_deep_seconds = 60;
        invalid.extract.max_bytes_per_file = 123;
        save_fixture(&alternate, &invalid);
        for from_disk in [true, false] {
            let error = if from_disk {
                state.reload(Some(&alternate)).unwrap_err()
            } else {
                state.bind_runtime(invalid.clone()).unwrap_err()
            };
            assert!(error.to_string().contains("scheduler.idle_warm_seconds"));
            assert_eq!(config_json(&state.value), config_json(&active));
            assert_eq!(state.source.as_deref(), Some(source.as_path()));
            assert!(state.runtime_bound);
            assert_eq!(state.revision, revision);
        }
        assert_eq!(
            config_json(&state.reload(None).unwrap()),
            config_json(&active)
        );

        // A corrected file is accepted without restarting or reinitializing
        // the state, and all of its settings are published together.
        invalid.scheduler.idle_warm_seconds = 15;
        invalid.scheduler.idle_deep_seconds = 30;
        save_fixture(&alternate, &invalid);
        let corrected = state.reload(Some(&alternate)).unwrap();
        assert_eq!(corrected.scheduler.idle_deep_seconds, 30);
        assert_eq!(corrected.extract.max_bytes_per_file, 123);
        assert_eq!(state.source.as_deref(), Some(alternate.as_path()));
    }

    #[test]
    fn ordered_idle_threshold_boundaries_remain_valid() {
        let mut cfg = AppConfig::default();
        for (warm, deep) in [(0, 0), (0, 60), (60, 60), (u64::MAX, u64::MAX)] {
            cfg.scheduler.idle_warm_seconds = warm;
            cfg.scheduler.idle_deep_seconds = deep;
            cfg.validate().unwrap();
        }
        for (warm, deep) in [(1, 0), (61, 60), (u64::MAX, u64::MAX - 1)] {
            cfg.scheduler.idle_warm_seconds = warm;
            cfg.scheduler.idle_deep_seconds = deep;
            assert!(cfg.validate().is_err());
        }
    }

    #[test]
    fn validation_rejects_delta_without_multitier() {
        let mut cfg = AppConfig::default();
        cfg.features.delta_index = true;
        cfg.features.multi_tier_index = false;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validation_accepts_valid_combo() {
        let mut cfg = AppConfig::default();
        cfg.features.delta_index = true;
        cfg.features.multi_tier_index = true;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn effective_config_updates_memory_and_rejects_invalid_changes() {
        let original = get_current_config();
        let mut configured = AppConfig::default();
        configured.paths.meta_index = "explicit-startup-index".into();
        set_current_config(configured.clone()).unwrap();
        assert_eq!(
            get_current_config().paths.meta_index,
            "explicit-startup-index"
        );

        configured.features.delta_index = true;
        configured.features.multi_tier_index = false;
        assert!(set_current_config(configured).is_err());
        assert_eq!(
            get_current_config().paths.meta_index,
            "explicit-startup-index"
        );
        set_current_config(original).unwrap();
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
        let toml_str = r#"
            [extract]
            max_chars = 12345
        "#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.extract.max_chars_per_file, 12_345);
    }

    #[test]
    fn indexing_section_alias_and_env_var_expansion_work_deterministically() {
        let toml_str = r#"
            [indexing]
            max_bytes_per_file = 4194304
            ocr_enabled = true
        "#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.extract.max_bytes_per_file, 4_194_304);
        assert!(cfg.extract.ocr_enabled);

        let expanded = expand_env_vars_with(
            "$HOME_CUSTOM/sub/%PROGRAMDATA%/end",
            vec![
                ("HOME".to_string(), "/short".to_string()),
                ("HOME_CUSTOM".to_string(), "/longer/root".to_string()),
                ("ProgramData".to_string(), "C:/ProgramData".to_string()),
            ],
        );
        assert_eq!(expanded, "/longer/root/sub/C:/ProgramData/end");
    }
}
