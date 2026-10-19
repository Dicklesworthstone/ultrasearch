# UltraSearch Configuration Reference

This document defines the configuration surface for the UltraSearch stack. Defaults favor safety and low resource use; advanced features are opt-in. Configuration is loaded via `dotenvy` from `.env` and merged with `config/config.toml`.

## Files and load order

1. `.env` (root of installation) -- loaded at process start via `dotenvy::dotenv()`.
2. `config/config.toml` -- default location `%PROGRAMDATA%/UltraSearch/config/config.toml` for service/worker/UI; falls back to local `config/config.toml` in developer builds.
3. Runtime overrides (future) -- optional IPC command to set overrides for the current session.

If a key is present in multiple sources, priority is: runtime overrides > config.toml > .env.

## Global keys

```toml
[app]
product_uid = "ultrasearch"
data_dir    = "%PROGRAMDATA%/UltraSearch"
```

- `product_uid` is used by Agent Mail/Beads integration and log/metrics namespacing.
- `data_dir` controls where state, indices, logs, and jobs are written.

## Logging & tracing (c00.8.2)

```toml
[logging]
level = "info"          # trace|debug|info|warn|error
format = "json"         # json|text
file   = "{data_dir}/log/searchd.log"
roll   = "daily"        # daily|hourly|size
max_size_mb = 100
retain = 7              # number of rolled files to keep
```

- All processes (service, index-worker, CLI, UI) use the same section; each process tags log entries with `process`.
- Defaults: JSON, info level, daily roll, 7 files retained. Console output remains colored for local dev.

## Metrics (c00.8.3)

```toml
[metrics]
enabled = false
bind    = "127.0.0.1:9310"   # HTTP /metrics (Prometheus text)
push_interval_secs = 10      # if future push gateway is enabled
sample_interval_secs = 10    # scheduler/system sampling
request_latency_buckets = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0]
worker_failure_threshold = 3
```

- When `enabled=false`, metrics are still logged periodically (summaries) but no HTTP server is started.
- Metrics namespace: `ultrasearch_*`. Key counters/histograms: files_indexed_total, search_latency_ms, worker_cpu_pct, worker_mem_bytes, usn_lag_seconds.
- Service stub (c00.8.3) exposes Prometheus registry with counters: requests_total, worker_failures_total; histogram: request_latency_seconds. `scrape_metrics()` returns text format for pipeline to expose via IPC/HTTP later.

## Extraction limits (c00.5)

```toml
[extract]
max_bytes_per_file = 16777216   # 16 MiB default
max_chars_per_file = 200000     # truncate beyond this for safety
ocr_enabled = false             # enable when Tesseract/OCR component is installed
```

- `max_bytes_per_file` stops runaway memory use on huge binaries.
- `max_chars_per_file` truncates text while marking `truncated=true` in results.
- `ocr_enabled` gates the OCR backend; when disabled the pipeline skips OCR-only formats.

## Scheduler knobs (c00.4.x)

```toml
[scheduler]
idle_warm_seconds = 15
idle_deep_seconds = 60
max_records_per_tick = 10000
usn_chunk_bytes = 1048576       # 1 MiB
cpu_soft_limit_pct = 50
cpu_hard_limit_pct = 80
```

- `idle_warm_seconds` / `idle_deep_seconds` classify conservative observations of interactive Windows sessions, including from service Session 0. Missing observations are Active. Reloads require `idle_deep_seconds >= idle_warm_seconds` and update the actual sensors before the next observation.
- `max_records_per_tick` and `usn_chunk_bytes` are declared reader tuning values. The production service currently uses fixed 128-record / 256-KiB reads; wiring these settings remains outstanding. They do not control worker batching.
- `cpu_*_pct` provide soft/hard cutoffs for deferring content indexing.

## Configuration reload (c00.8.1)

- Service watches for `ConfigReload` IPC command (from UI/CLI) and reloads config.toml; dotenv values are static until restart.
- On reload: validate new config, apply to schedulers and IPC, then ack success/failure via IPC/Status.
- UI exposes "Reload config" action; CLI: `search-cli config reload` (to be implemented).

## Status reporting (c00.8.4)

Status response fields (over IPC `StatusRequest`):
- volumes: list of GUID, enablement, last_usn, journal_id, index state, lag seconds
- indices: meta/content sizes, segment counts, last commit timestamps
- scheduler: state (Active/WarmIdle/DeepIdle), pending job counts by class, last worker result
- metrics snapshot: recent averages for search latency, worker CPU/mem

## Feature flags (c00.9.1)

```toml
[features]
multi_tier_index = false
delta_index      = false
adaptive_scheduler = false
doc_type_analyzers = false
semantic_search  = false
plugin_system    = false
log_dataset_mode = false
mem_opt_tuning   = false
auto_tuning      = false
```

- Flags default to `false`; enabling a flag requires that the underlying module is built and configured.
- Some flags imply others (e.g., `delta_index` requires `multi_tier_index`); validation should enforce combinations.

## Scheduler thresholds (references c00.4.x)

```toml
[scheduler]
idle_warm_seconds = 15
idle_deep_seconds = 60
cpu_soft_limit_pct = 50
cpu_hard_limit_pct = 80
disk_busy_bytes_per_s = 10485760  # 10 MB/s threshold
content_batch_size = 1000
power_save_mode = true
```

`content_batch_size` bounds newly assembled legacy worker batches, clamped to
1..=4096 before integer conversion. Each batch also stops before exceeding the
worker receipt's 1024 distinct deferrable full-document identities; repeated
observations of one identity count once. The remaining jobs retain FIFO order.
Already-admitted durable batches are never split or resized. Oversized durable
envelopes are rejected before scheduler admission, not partially acknowledged.

### Adaptive content scheduling

Set `features.adaptive_scheduler = true` to enable load feedback. The production
scheduler updates it on normal observation ticks and while a worker is pending.
It tunes fresh legacy batch sizes and the CPU admission ceiling for subsequent
content work, including durable journal work, without changing the worker lane,
in-flight transaction, journal cursor, or commit acknowledgement rules.

The configured content batch size and `cpu_hard_limit_pct` are ceilings, not
starting points that adaptation may exceed. The adaptive batch ceiling is also
limited to 2000 jobs; the 1024 distinct-deferral limit still applies when a new
legacy batch is assembled. Feedback uses a time-weighted CPU average and adjusts
at most once every five seconds of continuous usable observations. High CPU,
disk, memory, or configured power-saving pressure reduces limits. Quiet samples
recover them gradually. Counter failures cannot become quiet disk observations;
invalid CPU/memory values or stale/zero-duration load samples revoke adaptive
content admission. Memory use at or above 90% also pauses new content admission.
Existing idle, disk, and power gates still apply, including when adaptive limits
recover. Cleanup remains eligible under pressure, subject to older FIFO work.

Live reloads preserve learned limits, apply lower operator ceilings immediately,
and require new quiet observations to grow toward raised ceilings. Turning the
feature off discards its learned state and immediately restores the configured
policy on the next observation; turning it on starts fresh. The status text
reports `adaptive`, `cpu_cap`, and the effective fresh `batch` limit. These are
admission diagnostics, not ingestion-health or completed-work claims.

No adaptation forcibly cancels a running worker or a blocking native sensor.
Sampling and worker timeouts retain their existing limitations. This feature
does not provide parallel index writers or measured throughput guarantees.

## Index paths

```toml
[paths]
meta_index    = "{data_dir}/index/meta"
content_index = "{data_dir}/index/content"
state_dir     = "{data_dir}/volumes"
jobs_dir      = "{data_dir}/jobs"
```

## Security & privileges (c00.2.5)

- Service should run under a dedicated account with `SE_BACKUP_NAME`/`SE_RESTORE_NAME` as required for MFT/USN access.
- `data_dir` should inherit restrictive ACLs; binaries never added to global PATH.

## Defaults for extraction limits (c00.5.x)

```toml
[extract]
max_bytes_per_file = 16777216   # 16 MiB
max_chars          = 200000
ocr_enabled        = false
ocr_max_pages      = 10
```

## Semantic search (c00.9.7)

```toml
[semantic]
enabled   = false
model     = "all-minilm-l12-v2"
index_dir = "{data_dir}/index/semantic"
```

Only effective when `features.semantic_search=true`.

---

Future extensions: add per-volume overrides under `[volumes."\\\\?\\Volume{GUID}\\"]`, and per-filetype policies for extraction. Keep this file minimal; prefer sane defaults over complex matrices.
