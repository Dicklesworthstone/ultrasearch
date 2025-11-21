At a high level you’re building:

**Progress log**
- 2025-11-21 (LilacCat): c00.1.2 manifests aligned to nightly policy — workspace edition 2024, `windows` dep wildcard, workspace `hnsw_rs` removed (scoped only to semantic-index); lockfile removed; cargo fmt/check/clippy pass on nightly. 

* an **NTFS + USN–driven catalog** for filenames and metadata (Everything‑style),
* a **Tantivy‑based full‑text engine** for contents,
* a **minimal resident service + bursty worker**,
* and a **GPUI desktop client** that feels instant but keeps memory under control.

Below is an implementation plan that walks every subsystem, which crate does what, and how to shape it for low memory + high performance using current Rust tooling.

### Progress log
- 2025-11-20 (RedSnow): Onboarded, reviewed docs/Beads. Began c00.1.x scaffolding: added workspace Cargo + crate stubs under `ultrasearch/`. Waiting on LilacCat’s reservation before touching root `Cargo.toml`/`rust-toolchain.toml`.

---

## 0. Design philosophy

Constraints to keep in mind while designing everything:

1. **Always‑on but tiny**

   * Long‑lived Windows service must stay in the tens of MB of RSS.
   * Heavy code (Tantivy writer, Extractous, OCR, etc.) lives in a **separate worker process** that only runs when needed.

2. **Maximal leverage of OS / FS primitives**

   * NTFS MFT enumeration + USN journal for changes, using `usn-journal-rs` ≥ 0.4.0.([Docs.rs][1])
   * No recursive `FindFirstFile` crawlers.

3. **Single source of truth for search**

   * Filename, metadata, and content all live in **Tantivy 0.24.x** indices.([Crates.io][2])
   * No ad‑hoc custom DBs if we can get away with an index + a small mapping.

4. **Background respect**

   * Indexing only during user+system idle using `GetLastInputInfo` and `sysinfo` for CPU/IO.([Leapcell][3])
   * Low process + thread priorities, but **no `PROCESS_MODE_BACKGROUND_BEGIN`** because of the ~32 MiB working‑set clamp and thrash documented by others.

5. **Predictable memory use**

   * Memory‑mapped storage for big datasets via `memmap2`.([Docs.rs][4])
   * Zero‑copy and compact serialization via `rkyv` / `zerocopy` for metadata snapshots and logs.([Docs.rs][5])

---

## 1. Top‑level architecture

### 1.1 Processes

**1. Windows service: `searchd`**

* Runs at boot, minimal dependency set.
* Responsibilities:

  * Discover NTFS volumes, open MFT + USN journals (`usn-journal-rs`).([Docs.rs][1])
  * Keep `last_usn` state per volume and publish change events.
  * Maintain **lightweight in‑memory metadata cache** (just enough to build paths and small stats).
  * Answer filename/metadata queries via the Tantivy *metadata index* (read‑only).
  * Schedule content indexing batches based on idle state and spawn workers.
  * Provide IPC endpoint (Windows named pipes) for UI and CLI.

**2. Index worker: `search-index-worker`**

* Short‑lived process spawned by `searchd` with a batch of files to index.
* Responsibilities:

  * Run heavy extraction stack (Extractous, IFilter COM, OCR).
  * Build `Tantivy::IndexWriter` for **content index** and process the batch.
  * Commit, close, exit to release memory.

**3. UI process: `search-ui`**

* GPUI desktop app (using `gpui` + `gpui-component`).([Gpui][6])
* Responsibilities:

  * Search box, filters, keybindings, results table with virtual scrolling.
  * Incremental query experience (Everything‑like for names).
  * File preview (text, markdown, some docs).
  * Communicate with `searchd` via named pipes in a background thread.

**4. Optional CLI: `search-cli`**

* Simple tool for scripting and debugging.
* Same IPC protocol as UI.

### 1.2 Cargo workspace layout

Workspace `Cargo.toml`:

* `core-types` – shared types, IDs, config, IPC messages.
* `core-serialization` – rkyv / bincode wrappers and schemas.
* `ntfs-watcher` – MFT + USN access, volume abstraction.
* `meta-index` – metadata Tantivy index management.
* `content-index` – content Tantivy index management.
* `content-extractor` – Extractous/IFilter/OCR.
* `scheduler` – idle + system load heuristics.
* `service` – Windows service host for `searchd`.
* `index-worker` – batch worker binary.
* `ipc` – named pipe client/server helpers.
* `ui` – gpui app.

---

## 2. Core data model

### 2.1 Identifiers

* `VolumeId`

  * A small integer (u16) assigned at runtime that maps to:

    * Volume GUID path (e.g. `\\?\Volume{...}\`).
    * Drive letter(s), if any.
* `FileId`

  * 64‑bit NTFS File Reference Number (FRN).
* `DocKey`

  * Composite key `(VolumeId, FileId)` packed into u64:

    * e.g. top bits = volume, low bits = FRN.
* `IndexDocId`

  * Tantivy internal doc ID; *never stored long‑term*, only transient.

### 2.2 Documents

Two logical document types:

1. **Metadata doc** (in `index-meta`):

   * Fields: `doc_key`, `volume`, `path`, `name`, `ext`, `size`, `created`, `modified`, `flags`.
   * No `content` field.
   * Indexed with analyzers optimized for prefix/name search.

2. **Content doc** (in `index-content`):

   * `doc_key` (fast field).
   * Same base metadata subset.
   * `content` (full text).
   * `content_lang` (optional).
   * Stored `path` for quick display (or reconstruct from metadata if you want to save space).

### 2.3 On‑disk layout and persistence

Under `%PROGRAMDATA%\UltraSearch`:

* `/volumes/{volume_guid}/state.rkyv`

  * rkyv archive containing:

    * `last_mft_scan_generation`
    * `last_usn` + `journal_id`
    * volume‑level settings (include/exclude, content indexing toggle).
* `/index/meta/`

  * Tantivy index for metadata.
* `/index/content/`

  * Tantivy index for content.
* `/config/config.toml`

  * User and system config.
* `/log/*.log`

  * Structured logs from `tracing`.

Use `rkyv` archives + `memmap2` so the service can read state with **zero‑copy** where appropriate.([Docs.rs][5])

---

## 3. NTFS integration and change tracking

### 3.1 Volume discovery

Crate: `windows` / `windows-sys` for Win32; `usn-journal-rs` for NTFS.([Microsoft Learn][7])

Process:

1. Enumerate local volumes using `GetLogicalDrives` / `GetVolumeInformationW`.
2. For each NTFS volume:

   * Resolve volume GUID path (`GetVolumeNameForVolumeMountPointW`).
   * Open handle with `FILE_READ_ATTRIBUTES | FILE_READ_DATA | FILE_LIST_DIRECTORY` and `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`.([Microsoft Learn][8])
3. Initialize a `Volume` object from `usn-journal-rs` bound to this handle.

### 3.2 Initial MFT enumeration

Use `usn-journal-rs` MFT iterator:

* It exposes an iterator over MFT records and also utilities to resolve `FileReferenceNumber` to full paths.([Docs.rs][1])
* For each record:

  * Filter out inaccessible/special system files if desired (configurable).
  * Collect:

    * FRN, parent FRN.
    * File name.
    * Flags (directory, hidden, system, reparse).
    * Size and timestamps (from record).
  * Immediately construct a **metadata document** and pass to `meta-index` builder (streaming, not buffering everything).

Memory angle:

* Don’t build a huge in‑memory directory tree; rely on `usn-journal-rs` path resolution only during the initial build.
* Keep only a **tiny LRU cache** of `(FRN -> path)` to accelerate repeated queries; backing store is the index itself.

### 3.3 USN journal tailing

`usn-journal-rs` provides iterators over change journal records and tracks `USN_JOURNAL_DATA` (including `NextUsn` and `UsnJournalID`).([Docs.rs][1])

Per‑volume worker (in service):

* Opens the journal with `CreateFileW` on `\\?\Volume{GUID}` and `FSCTL_QUERY_USN_JOURNAL` / `FSCTL_READ_USN_JOURNAL` under the hood (encapsulated by `usn-journal-rs`).
* Maintains `last_usn` and `journal_id`.
* Loop:

  1. Read a chunk (`READ_USN_JOURNAL_DATA_V1`) asynchronously.
  2. Convert records to internal `FileEvent`:

     * Created, Deleted, Modified (data), Renamed, BasicInfoChanged.
  3. Push `FileEvent`s into a crossbeam channel to the scheduler.

Change gap handling:

* On start, fetch current `USN_JOURNAL_DATA` and compare stored `journal_id` / `FirstUsn` / `NextUsn`.([Microsoft Learn][9])
* If `last_usn` lies outside `[FirstUsn, NextUsn]` or `journal_id` changed → mark volume as **stale** and schedule a full incremental walkthrough (like a lighter MFT scan) to re‑sync.

### 3.4 Minimal metadata cache

Even though Tantivy holds metadata on disk, we want minimal in‑memory structures for speed:

* Data structure: `slotmap::SlotMap<FileKey, FileMeta>` (stable handles and good perf, maintained unlike `generational-arena`).([Docs.rs][10])
* `FileMeta` includes:

  * `doc_key`, `flags`, `size`, `timestamps`, plus short name.
* Name + path caching:

  * **String interner** (e.g. `lasso` or a custom interner) for filenames.
  * Very small LRU path cache: `lru::LruCache<DocKey, Arc<str>>` with cap ~50k entries.

Crucially, this cache is **optional**; it accelerates operations but the source of truth is the metadata index. You can rebuild it on restart by scanning the `index-meta` store.

---

## 4. Metadata and filename index

### 4.1 Tantivy `index-meta` schema

Use `tantivy` 0.24.x (CompactDoc, memory optimizations).([Crates.io][2])

Fields:

* `doc_key: u64` – FAST + STORED, primary key for delete/update.
* `volume: u16` – FAST to filter queries per volume.
* `name: TEXT` – indexed; no storing required for search, though you may store for display.
* `path: TEXT` – optional; you can reconstruct from directory tree or store for convenience.
* `ext: STRING` – keyword, FAST.
* `size: u64` – FAST.
* `created`, `modified: i64` – FAST for range queries.
* `flags: u32` – FAST (bitfield for is_dir, hidden, system, etc.).

Tokenizers:

* Default tokenizer for `name` with separators on `[\\, /, '.', '-', '_']`.
* Raw keyword analyzer for `ext`.
* Optional path tokenizer for `path` splitting on directory separators.

Memory tuning:

* Index writer for `index-meta` is only heavily used during:

  * Initial build.
  * Rare rebuilds.
* For normal operation, service only holds an `IndexReader` with:

  * Small docstore cache and segment cache:

    * Configure via `IndexReaderBuilder::reload_policy` and `with_num_warmers(0)`/small.([Quickwit][11])
* Readers are cheap; OS page cache handles bulk.

### 4.2 Initial build

Pipeline:

1. MFT enumerator streams `FileMetaSeed` (volume, FRN, path, etc.).
2. `meta-index` builder threads:

   * Build Tantivy doc on the fly.
   * Add batches to writer.
3. Commit periodically:

   * E.g. every 100k docs or every 30 s.
   * On commit, record progress in `state.rkyv` (last FRN or internal cursor).

Tantivy writer configuration for initial build:

* `heap_size` ~ 512 MB if machine has plenty of RAM.
* `num_threads` = min(8, n_cpus).
* Merge policy tuned to create bigger segments, reducing later merges.

### 4.3 Name search query path

On user typing a query (e.g. `"foo bar"`):

1. Parse query into AST (see §5.4) but default to:

   * `name` prefix matches (high boost).
   * `name` full‑token matches.
2. Build `BooleanQuery` with:

   * `PrefixQuery` on `name`.
   * `TermQuery` / `PhraseQuery` on `name`.
3. Apply optional filters:

   * `size` range, `modified` range.
   * `flags` filter (is_dir / hidden etc.).
4. Limit to 500–2k docs for UI.

This yields Everything‑like filename search entirely through Tantivy, benefiting from tuned indexing and OS caches.([GitHub][12])

### 4.4 Optional FST accelerator

For ultra‑fast prefix lookup on large datasets, layer an `fst::Map` or `fst::Set` over filenames.([Docs.rs][13])

* Keys: normalized (lowercased) `name`.
* Values: serialized `doc_key` (u64) or small integer index.

Properties:

* FST backed by memory‑mapped file via `memmap2` to keep RAM constant.([Docs.rs][4])
* Constructed on initial build and after major compactions; not updated per change.
* Maintain a small in‑memory delta index (hash map) for recent changes.

Query path when FST enabled:

* For short prefix queries, use FST to get candidate doc_keys and then fetch docs from `index-meta` with fast field lookups.
* For more complex queries (regex, multiple terms), fall back to Tantivy.

---

## 5. Full‑text content indexing

### 5.1 Separate `index-content`

Design choice: keep content in a **different index** from `index-meta`:

* `index-content` is often significantly larger and more volatile.
* Service can run without ever opening it (only the worker and search requests that use `content:` need it).
* Allows per‑volume or per‑path **content indexing policy** independent of metadata.

### 5.2 `index-content` schema

Fields:

* `doc_key: u64` – FAST + STORED.
* `volume: u16` – FAST.
* `name: TEXT` – optional; factor into ranking.
* `path: TEXT | STORED` – for snippet context.
* `ext: STRING`.
* `size: u64` – FAST.
* `modified: i64` – FAST.
* `content_lang: STRING` – to choose analyzer.
* `content: TEXT` – main field.

Analyzer strategy:

* Default analyzer for English (tokenization + stopwords + stemming).
* Optionally per‑language analyzers (Tantivy 0.24 JSON fields / per‑field combinations can help, but keep it simple initially).([Quickwit][14])

### 5.3 Tantivy writer tuning for low memory

Worker process uses `IndexWriter` configured for “bursty but bounded” memory:

* `heap_size` = 64–256 MB depending on machine and user setting.
* `num_threads` = 2–4 (Tantivy uses Rayon internally so you get parallel indexing).([GitHub][15])
* Merge policy:

  * `LogMergePolicy` tuned to keep segments modestly sized (e.g. 128–256 MB).
  * Cap number of open segments to avoid huge memory spikes during merges.([GitHub][16])

Key point: The **writer exists only in the worker process**. When the worker exits, all its allocations go away. The service never holds writer‑specific state.

### 5.4 Query orchestrator and scoring

When a query hit comes from UI:

1. Parse query string into AST:

   * Terms without prefix/default field go to:

     * `OR(name:term, content:term)` with field boosts.
   * Recognize `name:`, `path:`, `ext:`, `content:`, `size:` and date ranges.
   * Recognize `is:dir`, `is:hidden`, `is:system`.

2. Build a compound Tantivy query:

   * `BooleanQuery` with subqueries for structured parts.
   * Range queries for size/time: `RangeQuery::new_i64/i64`.
   * For fuzzy names, use `FuzzyTermQuery`.

3. Ranking:

   * Primary: Tantivy’s BM25 for `content` field.([GitHub][12])
   * Extra manual boosts:

     * Exact name match > prefix match > substring.
     * More recent modified time.

4. Execution:

   * Query `index-meta` only for pure filename/metadata queries.
   * Query both `index-meta` and `index-content` for mixed queries:

     * Union/merge results based on `doc_key`.
     * This can be done either:

       * In service (preferred: no UI direct disk access).
       * Or, optionally, in UI for advanced offline modes.

---

## 6. Content extraction subsystem

### 6.1 Extractor architecture

Crate: `content-extractor`.

Core traits:

* `Extractor` (for a single extractor implementation).
* `ExtractorStack` (ordered list with fallback semantics).

Input:

* `FileDescriptor` (doc_key, path, ext, size, maybe MIME).

Output:

* `ExtractedContent`:

  * `text: String` (or segmented pages).
  * `lang: Option<String>`.
  * `metadata: SmallMap<String, String>`.

### 6.2 Extractous integration

Use `extractous` crate (Rust implementation; Apache Tika formats) at latest version.([Docs.rs][17])

Properties:

* Supports PDF, Word, Excel, PowerPoint, HTML, CSV, Markdown, EPUB, and many others.([Docs.rs][17])
* Designed for high‑performance extraction with **low memory** and multi‑threading; 10x–18x speedups vs some Python alternatives and ~11x lower memory in benchmarks.([Reddit][18])
* Internal use of Tika bits with GraalVM, but as a Rust crate you call it via safe APIs.

Usage pattern:

* Build a single `ExtractousEngine` per worker process.
* For each file:

  * Detect type (Extractous does magic; but you can hint by extension).
  * Stream output text:

    * For big PDFs, use page‑by‑page extraction to avoid huge `String`s.

### 6.3 IFilter integration (optional)

When you hit file types Extractous doesn’t support or where a system‑installed filter has better fidelity, call COM `IFilter`:

* Via `windows` crate, using the `Filter` COM interfaces (`IFilter`, `IPersistFile`).
* Pattern:

  * Call `LoadIFilter` for file path or MIME.
  * Iterate chunks of text via `GetChunk`/`GetText`.
* Keep this behind a feature flag; it introduces COM/STA requirements but buys you compatibility with e.g. proprietary Office filters.

### 6.4 OCR integration

For scanned PDFs/images:

* Use `tesseract` / `tesseract-rs` or `rusty-tesseract` on Windows, pointing at a bundled Tesseract binary.([Crates.io][19])
* Extraction strategy:

  * Detect if doc is image‑only (no text from Extractous).
  * Run Tesseract on each page up to a limit (configurable per doc type).
  * Merge recognized text and mark `content_lang`.

### 6.5 Auto‑installation: component manager

User requirement: “auto installs behind the scenes”.

Design a `ComponentManager` (part of worker):

* Manifest stored in `core-types` or `config/components.toml`:

  * `id`, `version`, `platform`, `url`, `sha256`.
* If the worker starts and a component (e.g. Tesseract) is required:

  * Check presence in `%LOCALAPPDATA%\UltraSearch\bin\{id}\{version}`.
  * If missing, download via `reqwest` and validate `sha256`.
  * Unpack ZIP or run silent installer to a private directory.
* Use absolute paths to these binaries; do **not** modify global PATH (security and isolation).

### 6.6 Resource limits and streaming

To maintain low memory:

* Document limits:

  * `max_bytes_per_file` (e.g. 16–32 MiB by default).
  * `max_chars` / `max_tokens` (e.g. 100–200k).
* Archive policies:

  * Only index whitelisted formats inside ZIPs.
  * Limit recursion depth.

Implementation details:

* Where Extractous or backends support it, stream pages instead of concatenating everything.
* Use `String` buffers per file with reserved capacity up to `max_chars`; beyond that, discard remainder or treat as truncated.
* Run extraction tasks in a small `rayon` thread pool within worker (or just rely on Tantivy’s internal parallelization; you can choose one concurrency layer).

---

## 7. Scheduler & background execution

### 7.1 User idle detection

Use `GetLastInputInfo` via `windows` crate.([Leapcell][3])

* Periodically (e.g. every 1 s) get:

  * Current tick via `GetTickCount64`.
  * Last input tick via `GetLastInputInfo`.
* Idle duration = difference.

State machine:

* `Active`: idle < 15s.
* `WarmIdle`: 15–60s.
* `DeepIdle`: >60s.

### 7.2 System load sampling

Crate: `sysinfo` ≥ 0.37.2.([Docs.rs][20])

* Maintain a `System` object; refresh CPU, disks, memory every 5–10 s.
* Thresholds:

  * CPU < 20% → good for content indexing in DeepIdle.
  * CPU 20–50% → metadata only.
  * CPU > 50% → pause indexing.
  * Disk busy flagged if read/write bytes per second exceed some baseline.

### 7.3 Job categories and priorities

Three categories:

1. **Critical updates** (cheap):

   * Deletion events.
   * Simple renames / attribute changes.
   * Should be processed quickly even in `Active` state, but very small resource usage.

2. **Metadata rebuilds** (moderate):

   * Volume rescan after USN gap.
   * Reindex of a directory tree after config change.
   * Only in `WarmIdle` or better.

3. **Content indexing** (heavy):

   * New or changed files requiring full extraction.
   * Only in `DeepIdle` and when CPU/disk thresholds are low.

Scheduler algorithm:

* Maintain per‑category queues in `crossbeam-channel` or a custom priority queue.
* In each scheduler tick:

  * Check current state.
  * Pop jobs from allowed categories up to a per‑tick budget (e.g. N files or M MB).
  * When there’s a backlog of content jobs and DeepIdle persists for more than X seconds, spawn a worker with a batch (size tunable, e.g. 500–2000 files).

### 7.4 Process and thread priorities

For `searchd`:

* Normal process priority (`NORMAL_PRIORITY_CLASS`).
* Worker threads for USN reading / scheduler occasionally at `THREAD_PRIORITY_BELOW_NORMAL` to reduce jitter.

For `index-worker`:

* Use `SetPriorityClass` to `IDLE_PRIORITY_CLASS` or `BELOW_NORMAL_PRIORITY_CLASS`.([Crates.io][21])
* Optionally reduce I/O priority at the file handle level if you use undocumented NT I/O priority APIs, but not necessary if scheduler thresholds are correct.
* Avoid `PROCESS_MODE_BACKGROUND_BEGIN` / `END` to skip the working‑set clamp that has been shown to cause severe paging and performance collapses.

You can also use a Windows **job object** for worker processes to cap total memory and CPU if you want an extra layer of safety.

---

## 8. Service, worker & IPC

### 8.1 Windows service host

Use `windows-services` crate (thin, recent, built for Windows services).([Docs.rs][22])

* `service` crate binary:

  * Implements main entrypoint registering a service with SCM.
  * On `SERVICE_START`, initializes:

    * A `tokio` runtime (multi‑threaded or current‑thread).
    * Volume manager + USN watchers.
    * Scheduler loop.
    * IPC server.

Context:

* Windows docs for Rust + `windows` crate show this pattern is standard for calling Win32 from Rust.([Microsoft Learn][7])

Alternative: `windows-service` crate if you prefer more examples / ecosystem; architecture is the same.([Crates.io][21])

### 8.2 Worker process contract

Worker (binary `index-worker`) is invoked with:

* A path to a **job file** (rkyv/bincode) in `%PROGRAMDATA%\UltraSearch\jobs/`.
* Job file includes:

  * Batch of `FileDescriptor` items.
  * Index path.
  * Extractor configuration (max sizes, filetype filters).

Worker steps:

1. Initialize Extractous + optional IFilter/OCR.
2. Open `index-content` Tantivy index.
3. Build `IndexWriter` with configured memory/threads.
4. For each file:

   * Extract text (respecting limits).
   * Add document.
5. Commit index and update per‑volume `last_content_indexed_usn` if needed.
6. Write `JobResult` file.
7. Exit.

Service monitors workers:

* Job queue → worker spawns with limited concurrency (e.g. 1–2 at a time).
* If worker crashes, log error and back‑off the offending files.

### 8.3 IPC protocol: named pipes + bincode

Crates:

* `tokio::net::windows::named_pipe` on the service side.([Docs.rs][23])
* `named_pipe` or same `tokio` type wrapped on the client side for synchronous use.([Crates.io][24])
* Serialization: `bincode` or `rmp-serde` for compact binary.

Framing:

* Each message: `[u32 length][payload bytes]`.
* Requests:

  * `SearchRequest { id, query_ast, limit, mode }`
  * `StatusRequest`
  * `ConfigGet/Set`
* Responses:

  * `SearchResponse { id, results, truncated }`
  * `StatusResponse { volumes, index_stats, scheduler_state }`
  * `ErrorResponse { message }`

Concurrency:

* Service:

  * Accepts multiple pipe connections.
  * Each connection handled by a dedicated task reading requests, running appropriate queries, and sending responses.
* UI:

  * Hidden `ipc_thread` that does blocking read/write and posts results into GPUI’s `AppContext` (so you don’t fight with async in the UI layer).

Versioning:

* Include `protocol_version` field in hello handshake to allow future incompatible changes.

---

## 9. UI architecture with GPUI

### 9.1 Ownership and model

GPUI’s core idea: all state lives in an `AppContext`, and views/models are accessed via handles into that context.([Zed][25])

Design:

* `SearchAppModel`:

  * Fields:

    * `query_state` (current text, parsed AST, mode).
    * `results: Vec<ResultRowHandle>` – handles to result rows in a separate store.
    * `selected_row`.
    * `status` (indexing progress, volumes).
* `ResultsStore`:

  * Owned by GPUI, holds `ResultRow { doc_key, name, path, size, modified, score }`.
  * Kept small; each query truncates to N rows.

GPUI components:

* Top search bar view implementing `Render` with a `TextInput` and query mode toggles.
* Main `Table` component from `gpui-component` with virtual scrolling.([Crates.io][26])
* Right‑side preview pane.

### 9.2 Virtualized results table

Use `gpui-component`’s `Table` or `VirtualList`:

* These are designed for high‑performance rendering of large datasets by only rendering visible items and supporting variable row heights.([Longbridge][27])
* Columns:

  * Name, Path, Type (ext), Size, Modified, Score.
* Features:

  * Sorting by column (client‑side within the current page).
  * Keyboard navigation (Up/Down, Enter to open, Ctrl+C path).

Memory behavior:

* Only store basic scalar fields in memory; no content blobs.
* For preview, request snippet for a single row from service when the row is selected (lazy and on‑demand).

### 9.3 Query UX

Behaviour:

* Debounce keystrokes (50–150 ms).
* For each new query:

  * Cancel, or simply ignore, outstanding requests if results for older `query_id` come back.
  * Start with metadata‑only search.
  * When the user hits a “content” toggle or prefix `content:`, query `index-content` too.

Visual response:

* Show count (`X results (Y shown)`), but keep actual rows truncated to configured `max_rows`.
* Display search latency from service for debugging/tuning.

### 9.4 Preview pane

Features:

* Text preview:

  * For text files and code, render direct from disk, syntax highlighting optional.
* Document preview:

  * Use `content-extractor` in a lightweight mode or call a `Preview` API on the service that returns a small snippet, not full content.
* Path context:

  * Show directory path with clickable segments to open Explorer or filter by folder.

Rendering is all GPU‑accelerated through GPUI; Zed’s architecture shows this achieves near‑game‑like performance at 120 FPS by rendering via custom shaders.([Medium][28])

---

## 10. Configuration, logging, metrics

### 10.1 Config model

Crates: `serde`, `toml`.

Config file:

* Global:

  * Enabled volumes.
  * Excluded paths / patterns.
  * Default filetype policies.
  * Max index sizes and doc limits.
* Indexer:

  * Tantivy writer `heap_size_mb`, `threads`.
  * Scheduler thresholds (idle times, CPU%, IO).
* UI:

  * Theme, keybindings, columns.

Reload:

* `searchd` supports SIGHUP‑equivalent via control channel (or manual “Reload config” from UI).

### 10.2 Logging

Crates: `tracing`, `tracing-subscriber`.

* Service logs:

  * Volume discovery, USN state, index commits, worker spawn/exit, errors.
* Worker logs:

  * Extraction errors per file, commit latencies.
* UI logs:

  * Query requests and latencies.

Logs are in JSON or structured text for easy analysis.

### 10.3 Metrics

Expose an optional low‑overhead HTTP endpoint or just log periodic metrics:

* Number of files indexed (meta + content).
* Index size on disk.
* Average search latency by query type.
* Worker CPU/memory usage (via `sysinfo` as sample).([Docs.rs][20])

---

## 11. Security & reliability

### 11.1 Privilege model

To access MFT and USN journals efficiently you typically need administrative rights or a service with privileges similar to Everything’s service.([Voidtools][29])

* Run `searchd` as a service under `LocalSystem` or a dedicated service account with:

  * `SE_BACKUP_NAME` and `SE_RESTORE_NAME` privileges as needed to read raw volumes; change journal docs highlight that creating/modifying journals requires admin rights.([Microsoft Learn][30])
* UI runs as normal user and talks to the service; doesn’t need elevation.

Hardening:

* Apply strong ACLs to program files and `%PROGRAMDATA%\UltraSearch` to avoid DLL hijacking issues similar to those found in Everything’s service.([Cymaera][31])
* Ensure worker processes run with the same restricted token and do not expose any network service.

### 11.2 Resilience

* Index corruption:

  * Tantivy is designed for durability: commits are atomic; an index is either in previous or new state.([GitHub][12])
  * On startup, if `index-content` or `index-meta` fails validation, rename to `*.broken` and trigger a rebuild from MFT/USN.
* Journal wrap:

  * Detect via `journal_id` comparison and `FirstUsn`/`NextUsn` range; schedule volume rescan.([Microsoft Learn][9])
* Power loss mid‑build:

  * Safe because indexing is append + commit; worst case you redo last batch.

---

## 12. Implementation roadmap (high‑level)

Given the architecture, the practical rollout looks like:

1. **Core types + service skeleton**

   * `core-types`, `core-serialization` with IDs, config, IPC messages.
   * `service` using `windows-services` that starts, logs, and shuts down cleanly.([Docs.rs][22])

2. **NTFS integration and metadata index**

   * `ntfs-watcher` hooking `usn-journal-rs` for MFT enumeration + journal tail.([Docs.rs][1])
   * `meta-index` with Tantivy 0.24.x and schema tuned for filenames.([Crates.io][2])
   * Build metadata index only; search CLI.

3. **Scheduler + background state**

   * Implement idle and system load detection with `GetLastInputInfo` and `sysinfo`.([Leapcell][3])
   * Add basic job queues and worker launching (no content yet).

4. **Content extraction + content index**

   * Integrate Extractous; verify doc coverage and memory behaviour.([Docs.rs][17])
   * Add `index-content` and worker pipeline for a few filetypes.
   * Add filetype policies and limits.

5. **IPC + UI (GPUI)**

   * Implement named pipe IPC for search requests/responses.([Docs.rs][23])
   * Build GPUI app with search box + results table using `gpui` and `gpui-component`.([Gpui][6])

6. **Polish + optimization**

   * FST accelerator for filenames (`fst`), if needed for very large datasets.([Docs.rs][13])
---
   * Tune Tantivy writer and reader caches for your dataset using metrics.
   * Add per‑volume and per‑filetype dashboards in UI.

## 13. Advanced features overview

The baseline design provides:

- NTFS‑driven metadata catalog (MFT + USN).
- Single‑tier metadata and content indices (Tantivy).
- A conservative scheduler and a short‑lived index worker.
- A GPUI desktop client with a fast results table.

This document is extended by `ADVANCED_FEATURES.md`, which defines ten major enhancements:

1. Multi‑tier (hot/warm/cold) index layout for both metadata and content.
2. In‑memory delta index for ultra‑hot data and near‑instant updates.
3. Document‑type‑aware analyzers and indexing strategies for text, code, and logs.
4. A query planner with AST rewrite and aggressive filter pushdown.
5. An adaptive, feedback‑driven scheduler and concurrency controller.
6. Hybrid semantic + lexical search using a vector index.
7. A plugin architecture for custom extractors and index‑time transforms.
8. Specialized handling for large append‑only logs and similar datasets.
9. System‑wide memory‑footprint optimization and allocator strategy.
10. Deep observability and auto‑tuning feedback loops.

Each of these features is specified as:

- New or modified modules/crates.
- Data and index layout changes.
- API and IPC extensions.
- Configuration options and defaults.
- Failure and migration behaviour.

The advanced features are additive:

- If all advanced features are disabled, the system behaves exactly like the base design.
- Each feature can be enabled independently via configuration, except where explicitly noted
  (e.g. semantic search depends on the vector index).

Refer to:

- `ADVANCED_FEATURES.md` for the full specification of advanced capabilities.
- `CONFIG_REFERENCE.md` (to be added) for the complete configuration surface,
  including per-feature toggles and tuning parameters.

Implementation strategy:

- Introduce the advanced modules behind feature flags and configuration gates.
- Roll them out in this order:
  1. Multi-tier layout + delta index.
  2. Query planner + doc-type analyzers.
  3. Adaptive scheduler.
  4. Log indexing.
  5. Plugin system.
  6. Semantic search.
  7. Memory and allocator tuning.
  8. Observability and auto-tuning.

This staged approach keeps the system shippable at each step while steadily increasing
performance, versatility, and power without regressing footprint or reliability.

---

### Progress log — 2025-11-21

- Workspace fix in progress (c00.1.2): root members updated to `ultrasearch/crates/*`; optional `hnsw_rs` removed from workspace deps (now crate-level optional in `semantic-index`); `ultrasearch/Cargo.toml` neutralized to avoid nested workspace.
- Toolchain set to nightly per new policy; workspace deps switched to wildcards to track latest crates pending stabilization.
- Recent scaffolding (for traceability): meta-index add_batch helper + RamDirectory test; ntfs-watcher trait/cursor; content-index schema/helpers; IPC query AST; scheduler idle/load + queues scaffolding; content-extractor limits/fields adjusted.

---

That gives you a complete, tightly integrated design: each Rust crate and process has a clear role; heavy tasks are isolated into short‑lived workers; and all indexing/searching leans hard on modern, well‑maintained libraries like `usn-journal-rs`, `tantivy` 0.24+, `extractous`, `sysinfo`, `gpui`, and `gpui-component` to hit both performance and memory targets.

[1]: https://docs.rs/usn-journal-rs?utm_source=chatgpt.com "usn_journal_rs - Rust"
[2]: https://crates.io/crates/tantivy/0.24.2?utm_source=chatgpt.com "tantivy - crates.io: Rust Package Registry"
[3]: https://leapcell.io/blog/rust-without-the-standard-library-a-deep-dive-into-no-std-development?utm_source=chatgpt.com "Rust Without the Standard Library A Deep Dive into no_std ..."
[4]: https://docs.rs/memmap2?utm_source=chatgpt.com "memmap2 - Rust"
[5]: https://docs.rs/rkyv?utm_source=chatgpt.com "rkyv - Rust"
[6]: https://www.gpui.rs/?utm_source=chatgpt.com "gpui"
[7]: https://learn.microsoft.com/en-us/windows/dev-environment/rust/rust-for-windows?utm_source=chatgpt.com "Rust for Windows, and the windows crate"
[8]: https://learn.microsoft.com/en-us/windows/win32/fileio/file-security-and-access-rights?utm_source=chatgpt.com "File Security and Access Rights - Win32 apps"
[9]: https://learn.microsoft.com/en-us/windows/win32/fileio/using-the-change-journal-identifier?utm_source=chatgpt.com "Using the Change Journal Identifier - Win32 apps"
[10]: https://docs.rs/slotmap/latest/slotmap/?utm_source=chatgpt.com "slotmap - Rust"
[11]: https://quickwit.io/blog?utm_source=chatgpt.com "Blog | Quickwit, Tantivy, Rust, and more."
[12]: https://github.com/quickwit-oss/tantivy?utm_source=chatgpt.com "Tantivy is a full-text search engine library inspired ..."
[13]: https://docs.rs/fst?utm_source=chatgpt.com "Crate fst - Rust"
[14]: https://quickwit.io/blog/tantivy-0.24?utm_source=chatgpt.com "Tantivy 0.24"
[15]: https://github.com/quickwit-oss/tantivy/releases?utm_source=chatgpt.com "Releases · quickwit-oss/tantivy"
[16]: https://github.com/microsoft/windows-rs?utm_source=chatgpt.com "microsoft/windows-rs: Rust for Windows"
[17]: https://docs.rs/extractous?utm_source=chatgpt.com "extractous - Rust"
[18]: https://www.reddit.com/r/Python/comments/1gqi6bg/extractous_fast_data_extraction_with_a_rust_core/?utm_source=chatgpt.com "fast data extraction with a rust core + tika native libs ..."
[19]: https://crates.io/crates/mmap-sync?utm_source=chatgpt.com "mmap-sync - crates.io: Rust Package Registry"
[20]: https://docs.rs/sysinfo/?utm_source=chatgpt.com "sysinfo - Rust"
[21]: https://crates.io/crates/windows-service?utm_source=chatgpt.com "windows-service - crates.io: Rust Package Registry"
[22]: https://docs.rs/windows-services?utm_source=chatgpt.com "windows_services - Rust"
[23]: https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/struct.NamedPipeServer.html?utm_source=chatgpt.com "NamedPipeServer in tokio::net::windows::named_pipe - Rust"
[24]: https://crates.io/crates/named_pipe?utm_source=chatgpt.com "named_pipe - crates.io: Rust Package Registry"
[25]: https://zed.dev/blog/gpui-ownership?utm_source=chatgpt.com "Ownership and data flow in GPUI — Zed's Blog"
[26]: https://crates.io/crates/gpui-component?utm_source=chatgpt.com "gpui-component - crates.io: Rust Package Registry"
[27]: https://longbridge.github.io/gpui-component/docs/components/virtual-list?utm_source=chatgpt.com "VirtualList | GPUI Component - GitHub Pages"
[28]: https://beckmoulton.medium.com/gpui-a-technical-overview-of-the-high-performance-rust-ui-framework-powering-zed-ac65975cda9f?utm_source=chatgpt.com "GPUI: A Deep Dive into the High-Performance Rust UI ..."
[29]: https://www.voidtools.com/support/everything/installing_everything/?utm_source=chatgpt.com "Installing Everything"
[30]: https://learn.microsoft.com/en-us/windows/win32/fileio/creating-modifying-and-deleting-a-change-journal?utm_source=chatgpt.com "Creating, Modifying, and Deleting a Change Journal"
[31]: https://www.cymaera.com/articles/everything.html?utm_source=chatgpt.com "Voidtools Everything Service - DLL hijacking and ... - Cymæra"
