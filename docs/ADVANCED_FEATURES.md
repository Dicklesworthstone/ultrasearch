# Advanced Features and Implementation Details

This document extends the base architecture with ten major enhancements:

1. Multi‑tier (hot/warm/cold) index layout.
2. In‑memory delta index for ultra‑hot data.
3. Document‑type‑aware analyzers and index strategies.
4. Query planner with AST rewrite and filter pushdown.
5. Adaptive, self‑tuning scheduler and concurrency control.
6. Hybrid semantic + lexical search.
7. Plugin architecture for extractors and index‑time transforms.
8. Specialized strategies for large append‑only logs.
9. Memory‑footprint optimization and allocator strategy.
10. Deep observability and auto‑tuning feedback loop.

Each section below specifies:

- Module / crate changes.
- Data structures and on‑disk layout.
- IPC / API changes where relevant.
- Configuration knobs and defaults.
- Error and migration behaviour.

The goal is that an implementation following this document requires minimal
architectural reinterpretation: all major moving parts and their interactions
are explicitly defined.

---

## 1. Multi‑tier hot/warm/cold index layout

### 1.1 Overview

Replace single `index-meta` and `index-content` instances with three tiers each:

- `meta_hot`, `content_hot`   – recent data, small indexes, ultra‑fast queries.
- `meta_warm`, `content_warm` – mid‑term history, balanced size vs performance.
- `meta_cold`, `content_cold` – long‑term archive, heavily compacted.

Routing is based primarily on `modified` timestamp, optionally on size or
per‑volume policy.

### 1.2 Directory layout and configuration

On disk under `%PROGRAMDATA%\UltraSearch/index`:

```text
/index/
  meta_hot/
  meta_warm/
  meta_cold/
  content_hot/
  content_warm/
  content_cold/
```

Configuration (in `config.toml`):

```toml
[index.tiers]
hot_days = 30
warm_days = 365

[index.tiers.meta]
enable_cold = true

[index.tiers.content]
enable_cold = true

[index.tiers.merge_policy.hot]
target_segment_size_mb = 64
max_merged_segment_size_mb = 256

[index.tiers.merge_policy.warm]
target_segment_size_mb = 128
max_merged_segment_size_mb = 512

[index.tiers.merge_policy.cold]
target_segment_size_mb = 256
max_merged_segment_size_mb = 1024
max_segments = 16
```

### 1.3 Routing rules

At index time for a document with `modified: UnixMillis`:

```text
age_days = (now_millis - modified) / (1000 * 60 * 60 * 24)

if age_days <= hot_days         => hot
else if age_days <= warm_days   => warm
else                             => cold
```

Separate routing tables for metadata and content:

- Metadata always routed based on modified time.
- Content additionally considers:
  - File size (very large may be forced to warm/cold even if “recent”).
  - Volume policy (e.g. removable media only in warm/cold).

### 1.4 Index management

`index-core` crate now manages six `IndexHandle`s:

```rust
struct TieredIndexSet {
    meta_hot: IndexHandle,
    meta_warm: IndexHandle,
    meta_cold: Option<IndexHandle>,
    content_hot: IndexHandle,
    content_warm: IndexHandle,
    content_cold: Option<IndexHandle>,
}

struct IndexHandle {
    path: PathBuf,
    reader: IndexReader,
}
```

- Readers are opened on service startup and kept for the life of `searchd`.
- Writers are created in the worker processes for individual tiers only when needed.

### 1.5 Query planner integration

The query planner (see §4) is responsible for selecting tiers to search:

- **Default mode**:
  - Search `*_delta` (if enabled, §2) and `*_hot` first.
  - If the user requests more results than available from those tiers or the
    query has explicit date constraints that include earlier periods, then
    include `*_warm`.
  - Only include `*_cold` when:
    - User explicitly selects “include archive” in the UI, or
    - Query explicitly spans dates older than `warm_days`.

Planned execution:

1. Execute query against hot + delta.
2. If `hits >= requested_limit` and no explicit cold coverage is requested:
   - Stop; report results.
3. Otherwise:
   - Execute against warm (and then cold if required).

Results from multiple tiers are merged by `doc_key` and score.

### 1.6 Migration and compaction

Periodic task (e.g. nightly, configurable) in `searchd`:

- **Promotion**: none (time only moves forward).
- **Demotion**:
  - Identify docs in hot whose `modified` is older than `hot_days`.
  - Background job to:
    - Reindex them into warm.
    - Delete them from hot.
  - Same logic from warm → cold.

Demotion is implemented as an offline worker job:

- Read candidate doc_keys and stored fields from source tier.
- Re‑add docs into target tier via worker.
- Delete from source tier.

This ensures that tier boundaries remain “clean” and that per‑tier size and
performance remain predictable.

---

## 2. In‑memory delta index for ultra‑hot data

### 2.1 Goals

- Provide near‑instant incorporation of filesystem changes into searches.
- Avoid frequent commits on disk‑based indices.
- Keep memory bounded and predictable.

### 2.2 Architecture

Add an in‑memory tier in `index-core` using Tantivy’s `RamDirectory`:

```rust
struct DeltaIndexes {
    meta: tantivy::Index,
    content: tantivy::Index,
}
```

These live entirely in RAM within `searchd` and hold:

- Metadata for recently changed/created files.
- Content for recently indexed files, within configurable limits.

### 2.3 Lifecycle and size limits

Config:

```toml
[delta]
enable_meta = true
enable_content = true
max_docs_meta = 50_000
max_docs_content = 10_000
max_total_bytes_content = 256_000_000  # approx
flush_interval_secs = 600
```

Behaviour:

- New/updated documents are inserted into delta meta/content first.
- When:
  - `max_docs_*` or `max_total_bytes_content` are exceeded, or
  - `flush_interval_secs` have elapsed since last flush,
  - the scheduler enqueues a “delta flush” worker job.

The flush job:

- Reads all docs in delta tier.
- Routes each doc into the corresponding disk tier (hot/warm/cold).
- Adds them via a disk‑based `IndexWriter`.
- Clears delta index by dropping and re‑creating it.

### 2.4 Querying with delta

Every query (metadata or content) is conceptually executed over:

1. Delta index.
2. Disk tiers (selected via planner).

Implementation strategy:

- Use the same schema in delta and non‑delta indices.
- Execute query independently against delta and disk indices.
- Merge results:
  - Deduplicate by `doc_key`, preferring delta versions (more recent).
  - Merge score lists for ranking.

### 2.5 Failure modes

- If flush job fails:
  - Delta index stays populated; service logs error and retries later.
  - On crash/restart, delta content is lost; it will be re‑indexed from the
    filesystem via USN and MFT. This is acceptable as a soft cache.

---

## 3. Document‑type‑aware analyzers and index strategies

### 3.1 Classification

Introduce a `DocKind` enum in `core-types`:

```rust
enum DocKind {
    Text,
    Code,
    Log,
    Binary,
}
```

Classification rules are applied at index time based on:

- File extension.
- Path (e.g. directories known to contain logs).
- Optional content sampling (e.g. detect if text looks like source code).

### 3.2 Schema extensions

Add logical fields to `content` schema:

- `content_text` – natural language text.
- `content_code` – code tokens and identifiers.
- `content_log` – log messages.

In practice, these may be separate Tantivy fields, or JSON fields with per‑field
analyzers, depending on Tantivy capabilities chosen at implementation time.

### 3.3 Analyzer manager

`index-core` registers analyzers with Tantivy:

- `text_en`, `text_de`, etc. – language‑specific analyzers.
- `code_generic` – splits on non‑identifier characters, keeps `foo::bar`.
- `log_generic` – tokenizes preserving timestamps, numbers, tokens; may
  normalize IP addresses and numeric IDs for better grouping.

The analyzer manager exposes:

```rust
fn analyzer_for_doc_kind(kind: DocKind, lang: Option<Lang>) -> &'static str;
```

This returns the analyzer name to use for the relevant `content_*` field.

### 3.4 Indexing strategy

Per document:

- For plain text docs:
  - `content_text` populated, analyzer selected by detected language.
- For code files:
  - `content_code` populated, `content_text` empty.
- For log files:
  - `content_log` populated.
- For binaries:
  - Content indexing may be disabled or limited based on extraction output.

This allows queries to:

- Target specific fields: `content_code:foo` vs `content_text:foo`.
- Use tailored scoring per field while reusing the same overall index.

### 3.5 Query side integration

The query planner (see §4) uses `DocKind` and filetype filters to decide which
fields to query:

- If the user is restricting search to code, target `content_code`.
- If searching logs, target `content_log`.
- Default content search:
  - Query both `content_text` and `content_code` with different boosts.

---

## 4. Query planner, AST rewrite, and filter pushdown

### 4.1 AST representation

`core-types` defines an AST for queries:

```rust
enum QueryExpr {
    Term(TermExpr),
    Range(RangeExpr),
    Not(Box<QueryExpr>),
    And(Vec<QueryExpr>),
    Or(Vec<QueryExpr>),
}

struct TermExpr {
    field: Option<FieldKind>,  // None => default
    value: String,
    modifiers: TermModifiers,  // phrase, fuzzy, prefix
}

struct RangeExpr {
    field: FieldKind,
    op: RangeOp,               // >, >=, <, <=, between
    value: RangeValue,         // numeric, timestamp, size
}

enum FieldKind {
    Name,
    Path,
    Ext,
    Content,
    Size,
    Modified,
    Created,
    Flags,
    Volume,
}
```

The parser produces this AST from user input.

### 4.2 Normalization and classification

A `QueryPlanner` struct performs:

1. **Normalization**:
   - Flatten nested AND/OR.
   - Apply De Morgan’s laws for `NOT`.
   - Deduplicate identical subtrees.
2. **Classification**:
   - Separates `filter_expr` (fast fields) and `score_expr` (content/name).
   - Recognizes pure metadata queries and can skip content indexes entirely.

### 4.3 Execution plan

Plan representation:

```rust
struct ExecutionPlan {
    filters: Vec<FilterClause>,
    scoring: Option<ScoringClause>,
    tiers: TierSelection,
}

enum FilterClause {
    FastFieldRange { field: FieldKind, range: RangeExpr },
    TermFilter { field: FieldKind, term: String },
}

enum ScoringClause {
    FilenameFirst { expr: QueryExpr },
    ContentFirst { expr: QueryExpr },
    Hybrid { name_expr: QueryExpr, content_expr: QueryExpr },
}

struct TierSelection {
    include_hot: bool,
    include_warm: bool,
    include_cold: bool,
}
```

### 4.4 Filter pushdown

The planner attempts to:

- Convert `RangeExpr` and simple `TermExpr` on fast fields into `FilterClause`.
- Push those into dedicated `RangeQuery` and term filters in Tantivy.

Execution pipeline:

1. Build a filter query using fast fields only.
2. Build a scoring query (BM25) limited by that filter.
3. Execute with BlockMax WAND where applicable to reduce scoring costs.

### 4.5 Caching

Implement a simple LRU cache keyed by normalized `filter_expr`:

- Value: `BitSet` or Tantivy `DocSet` representing candidate doc_ids per tier.
- On repeated queries with the same filter but different scoring clauses, reuse
  the filter set.

Cache limits:

- Maximum number of cached filters.
- Maximum total memory for filter bitsets.

---

## 5. Adaptive, self‑tuning scheduler and concurrency control

### 5.1 Metrics collection

`scheduler` crate maintains sampling of:

- CPU usage (`sysinfo`).
- Disk IO rates (per volume where available).
- Available RAM.
- Idle times from `GetLastInputInfo`.
- Worker job performance:
  - Files indexed per job.
  - Bytes indexed.
  - Job duration.

### 5.2 Dynamic budgets

Compute dynamic “budgets”:

- `max_index_bytes_per_minute`.
- `max_index_files_per_minute`.
- `max_concurrent_workers`.

Budgets are derived from:

- Configured soft limits.
- Observed ability of machine to process jobs without exceeding target CPU and IO thresholds.

### 5.3 Scheduling policy

Scheduler states remain `Active`, `WarmIdle`, `DeepIdle` but:

- In `Active`:
  - Only critical jobs (deletes, minimal metadata updates).
- In `WarmIdle`:
  - Metadata rebuilds within budget.
- In `DeepIdle`:
  - Full content indexing, but budget‑constrained.

For each tick:

1. Update metrics and budgets.
2. Estimate `allowed_bytes` and `allowed_files` for this window.
3. Select jobs whose estimated cost fits the budget.
4. Spawn worker processes with batch sizes sized to fill but not overshoot the budget.

### 5.4 Learning from history

Persist a small “scheduler state” file:

- Stores aggregated statistics per volume:
  - Typical throughput of worker jobs.
  - Average idle window durations.
- Stores last chosen budgets and their performance.

On startup:

- Use this state to initialize budgets rather than cold defaults.
- This allows the system to converge to a stable behaviour per machine over time.

---

## 6. Hybrid semantic + lexical search

### 6.1 Scope and constraints

Semantic search is optional and controlled by configuration:

- Disabled by default.
- When enabled, applied only to:
  - Documents with `DocKind::Text` or optionally `DocKind::Code`.
  - Volumes and paths marked as “semantic‑eligible”.

### 6.2 Vector index

Introduce a new crate `semantic-index`:

- Maintains an approximate nearest‑neighbour (ANN) index for embeddings:
  - `doc_key`.
  - Vector embedding (fixed dimension, e.g. 384 or 768).
- On disk:
  - `semantic/index.bin` per tier or global.

Implementation choices:

- Use a Rust HNSW implementation or a small wrapper around a C++ ANN library.
- The exact library is an implementation detail; the architecture assumes:
  - Insert, remove, and query operations.
  - Persistence and reload.

### 6.3 Embedding pipeline

In the index worker:

- For documents marked as semantic‑eligible:
  - After content extraction, send text through an embedding generator:
    - Could be a local model (e.g. via a separate process using ONNX or similar).
    - Or via a plugin if you want more flexibility.
  - Store embedding in the vector index with the `doc_key`.

Embedding generation may be:

- Synchronous, for small batches.
- Asynchronous via a queue if embedding cost is high; in that case, a separate
  embedding worker updates the vector index.

### 6.4 Semantic query path

At query time:

- If semantic search is enabled and the user selects it (or query type is inferred as semantic):
  1. Generate embedding for the query string.
  2. Query ANN index for top‑N nearest neighbours.
  3. Obtain candidate `doc_key`s.
  4. Run a restricted Tantivy lexical query against those doc_keys:
     - Use BM25 on `content` and `name`.
     - Optionally include filters.
  5. Combine lexical and semantic scores:
     - Weighted sum: `score = alpha * lexical + (1 - alpha) * semantic`.

The result set is then merged with pure lexical results if both modes are active.

### 6.5 Consistency and maintenance

- When a document is deleted or moved:
  - Remove its embedding from the ANN index.
- When re‑tiering (hot → warm, etc.):
  - Vector index may remain global if you store `doc_key` only.
  - Alternatively, maintain per‑tier ANN segments and reassign `doc_key`s there.

On rebuild:

- If vector index is missing or corrupt, it can be rebuilt from Tantivy content
  by re‑running embedding generation for all semantic‑eligible docs.

---

## 7. Plugin architecture for extractors and index‑time transforms

### 7.1 Goals

- Allow external modules to:
  - Extract text/metadata from proprietary formats.
  - Perform index‑time transformations (e.g. classify documents, add tags).
- Keep core binaries stable and minimal.

### 7.2 Plugin model

Plugins run only in the index worker process, not in the long‑lived service.

Two possible plugin types:

- **Native plugins**: dynamic libraries (DLL) loaded with `libloading`.
- **WASM plugins**: modules executed in a sandbox (e.g. `wasmtime`).

The architecture supports both, but initial implementation can pick one.

### 7.3 Plugin registry

Configuration section:

```toml
[plugins]
paths = [
  "C:\\ProgramData\\UltraSearch\\plugins\\native\\*.dll",
  "C:\\ProgramData\\UltraSearch\\plugins\\wasm\\*.wasm",
]
```

At worker startup:

- Discover plugin files.
- Load each and query metadata:
  - Supported extensions / MIME types.
  - Supported transform types.

### 7.4 Extractor plugin ABI (native)

For native (DLL) plugins, define a C‑ABI header, e.g.:

```c
typedef struct {
    const char* text;
    const char* metadata_json;
} us_extract_result_t;

typedef struct {
    const char* ext;
    const char* mime;
} us_file_info_t;

typedef struct {
    uint32_t api_version;
    const char* plugin_name;
} us_plugin_info_t;

typedef us_plugin_info_t (*us_plugin_init_fn)(void);
typedef int (*us_plugin_supports_fn)(const us_file_info_t*);
typedef int (*us_plugin_extract_fn)(const char* path, us_extract_result_t* out);
```

Plugins export:

- `us_plugin_init`
- `us_plugin_supports`
- `us_plugin_extract`

The worker:

- Calls `us_plugin_init` once per plugin.
- For each file:
  - Calls `us_plugin_supports`.
  - If true, calls `us_plugin_extract`.

### 7.5 Transform plugin ABI

Index‑time transforms operate on document metadata and extracted content:

- Input:
  - JSON representation of file metadata and extracted text.
- Output:
  - JSON with additional fields/tags to add to Tantivy documents.

ABI (native) example:

```c
typedef const char* (*us_plugin_transform_fn)(const char* json_in);
```

Worker:

- For each doc:
  - Build JSON input.
  - Call transform plugins in sequence.
  - Merge results into final Tantivy document.

### 7.6 Safety and limits

- All plugins are invoked with:
  - Timeouts.
  - Memory limits (via OS job objects or WASM sandboxing).
- On repeated failure:
  - Plugin is disabled for the remainder of the worker job.
  - Error is logged and surfaced in diagnostics.

---

## 8. Specialized strategies for large append‑only logs

### 8.1 Detection and classification

Log‑like files are detected via:

- Extension patterns: `.log`, `.jsonl`, `.ndl`, etc.
- Path patterns: `logs/`, `var\log\`, etc.
- Change patterns: file size only grows; USN events indicate data extension.

Such files are given a `DocKind::Log` classification.

### 8.2 Per‑file cursor

For each log file, maintain a cursor:

- Stored in a small local database (`log-index/state.rkyv`) keyed by `doc_key`.
- Tracks:
  - `last_offset_bytes`.
  - `last_timestamp` (optional).

On update:

- If file size > cursor offset:
  - Indexer reads content from `cursor` to end (or up to a max).
  - Parses lines/records and indexes them as separate docs.
  - Updates cursor to new file size.

### 8.3 Log record document schema

Separate schema for `index-logs` or reuse `index-content` with distinct fields:

- `doc_key` – file‑level key.
- `record_id` – sequential id per file (line number or event id).
- `timestamp` – parsed timestamp, if present.
- `severity` – if parseable (DEBUG/INFO/WARN/ERROR/FATAL).
- `message` – main text, indexed with `content_log` analyzer.
- Additional structured fields (e.g. `service`, `request_id`, `ip`).

### 8.4 Log query features

The query planner recognises log queries via:

- Explicit `kind:log` filter.
- UI “Logs” search mode.

Capabilities:

- Time‑range filtering on `timestamp`.
- Severity filtering.
- Aggregations:
  - Count by time bucket (minute/hour/day).
  - Count by severity.

These are implemented using Tantivy aggregations, limited to `index-logs`.

### 8.5 Performance

This design avoids re‑indexing entire log files on every change:

- Only appended bytes are processed.
- Cursor prevents re‑processing previously indexed records.

---

## 9. Memory‑footprint optimization and allocator strategy

### 9.1 Struct layout and packing

Guidelines:

- Order fields by descending alignment requirement.
- Use the smallest integer types that maintain correctness.
- For large collections (millions of entries), prefer:
  - `Vec<Struct>` over `Vec<Box<Struct>>` to avoid pointer chasing.

For `FileMeta`:

- Use `u64` for `doc_key`.
- Use `u64` for `size`.
- Use `u64` for timestamps (compressed epoch).
- Use `u32` or `u16` for flags and volume ids.

### 9.2 String interning and arenas

Implement:

- A global per‑volume string interner for filenames and extensions.
- A small `bumpalo` arena for short‑lived strings in worker processes.

Service:

- Stores interned strings as integer ids in `FileMeta`.
- Resolves them lazily via interner when needed for display.

Worker:

- Uses arenas for temporary token buffers and JSON building to avoid repeated
  heap allocations and to keep fragmentation low.

### 9.3 Zero‑copy serialization

State files and job descriptors:

- Use `rkyv` to serialize into mmappable buffers.
- Service:
  - Memory‑maps the state file and accesses fields without deserialization
    where possible.
- Worker:
  - Reads job descriptors directly from mapped memory with zero‑copy where
    feasible.

### 9.4 Allocator choice

For worker binaries:

- Optionally switch to an alternative allocator (e.g. `mimalloc` or similar)
  based on benchmarking for this specific workload.

Service binary:

- Conservative: default allocator or a carefully chosen alternatief if
  profiling demonstrates clear benefits.

### 9.5 Process lifetime strategy

Continue to rely on:

- Short‑lived worker processes that:
  - Do a bounded amount of work.
  - Release all heap fragmentation on exit.

Service:

- Avoids long‑lived heavy allocations:
  - Does not hold an `IndexWriter`.
  - Keeps caches small and bounded.

---

## 10. Observability and auto‑tuning feedback loop

### 10.1 Metrics

Measured metrics:

- Indexing:
  - Documents per job.
  - Bytes per job.
  - Job duration.
  - Tantivy commit and merge times.
- Query:
  - Latency per query class (filename‑only, content, logs, semantic).
  - Result counts and truncation rates.
- System:
  - CPU usage distribution during indexing.
  - Disk IO during indexing windows.
  - Peak RSS for service and worker processes.

Metrics export:

- Periodic log entries in a machine‑readable format (JSON).
- Optional lightweight HTTP endpoint (local only) for live inspection.

### 10.2 Tuning state

Persist `tuning_state.rkyv` containing:

- Effective Tantivy writer heap sizes.
- Effective worker thread counts.
- Observed good/bad outcomes:
  - E.g. “128 MB heap and 4 threads overshoot CPU budget”.
  - “64 MB heap and 2 threads meet target”.

This state is small and updated infrequently.

### 10.3 Auto‑tuning algorithm

At startup and periodically:

1. Read `tuning_state`.
2. Compare against current hardware:
   - CPU core count.
   - RAM.
3. For each tunable parameter:
   - If previous values yielded good performance, keep them.
   - If details are missing (new machine) or performance is poor:
     - Run micro‑benchmarks on a small test index or sample of docs:
       - Try candidate heap sizes (e.g. 32, 64, 128 MB).
       - Try candidate thread counts (1, 2, 4).
     - Record best configuration that meets latency/throughput goals.
4. Apply chosen parameters to new worker jobs.

Auto‑tuning respects safe bounds:

- Never exceeds user‑specified maximums.
- Never raises concurrency beyond what scheduler deems acceptable for system load.

### 10.4 User‑visible diagnostics

UI exposes:

- A compact diagnostics pane:
  - Current tier sizes.
  - Indexer activity.
  - Scheduler state (“active”, “warm idle”, “deep idle”).
  - Recent auto‑tuning decisions (“writer heap increased to 128 MB based on benchmarks”).

This is read‑only; changes are still made through configuration to ensure
repeatability.

---

## 11. Compatibility and feature gating

All advanced features are feature‑gated via configuration:

- `index.tiers.enable` – multi‑tier indices.
- `delta.enable_meta`, `delta.enable_content` – in‑memory delta indices.
- `semantic.enable` – semantic search.
- `plugins.enable` – plugin system.
- `logs.enable` – log‑specific indexing.

Default behaviour:

- If no advanced features are enabled, the system reduces to:
  - Single metadata and content indices.
  - No delta tiers.
  - Basic scheduler.
  - No semantic or plugin features.

This ensures that the advanced architecture can be rolled out progressively
without destabilizing the system for users who do not require the additional
power.

