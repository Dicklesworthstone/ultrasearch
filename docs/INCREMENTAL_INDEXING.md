# Incremental indexing and recovery

UltraSearch follows the initial MFT scan with production NTFS USN journal reads.
An initial scan alone does not establish that later changes are indexed. The
service reports journal ingestion health separately from scheduler idle/load
state, and coordinates metadata and content replacement through durable intent.

The implementation is in the [NTFS reader](../ultrasearch/crates/ntfs-watcher/src/lib.rs),
[scanner](../ultrasearch/crates/service/src/scanner.rs),
[checkpoint store](../ultrasearch/crates/service/src/scanner/state.rs), and
[worker](../ultrasearch/crates/index-worker/src/main.rs).

## Journal and document identity

Each production tail call issues at most one nonwaiting
`FSCTL_READ_USN_JOURNAL` request. The service uses a 256 KiB buffer and consumes at
most 1,024 raw records per volume per iteration. Ignored and excluded records
count against that budget. If a buffer contains more records than the budget
allows, the returned cursor points to the first unconsumed record.

An `ERROR_MORE_DATA` partial result is accepted only when the byte count fits
the fixed buffer, every returned record is complete and valid, and the result
makes forward progress. Empty or truncated overflow results fail without
advancing the checkpoint. Handling partial output does not grow the buffer or
perform additional kernel reads in the same call.

`JournalCursor.last_usn` is the **next unread USN**, despite its historical name.
It is not the last processed record's USN and must not be incremented manually.
The reader validates the journal ID and available USN range before and after
reading/resolving metadata. A replaced journal, expired position, or position
beyond the journal head requires reconciliation. Malformed records and
unsupported record versions produce errors; the implemented reader requests
USN version 2 records.

The volume GUID and journal ID have different purposes. GUIDs bind persisted
volume IDs to mounted volumes; journal IDs identify journal incarnations on
those volumes. A document key preserves the volume ID and all 64 bits of the
NTFS file reference number, including its reuse sequence. Renaming a file
preserves its key; reusing an MFT slot for another file does not. Metadata and
worker paths are anchored to volume GUIDs, so drive-letter reassignment cannot
redirect an admitted job to another volume.

`JournalBatch.caught_up` compares progress with the head observed before that
read. Checkpoint/index writes can generate more USN records themselves, so
absolute filesystem inactivity is not a prerequisite for reporting catch-up.

## Commit, visibility, and replay

The service processes one serialized ingestion lane with this ordering:

1. Coalesce events by complete document identity and persist a pending batch in
   `state_dir/ingestion-v2.json`. Hide its affected search entries before index
   mutation. Durable mutation batches contain at most 128 changes.
2. Wait for bounded scheduler admission and a successful real worker exit. The
   worker invalidates obsolete content, applies replacements/deletions, and
   stores an ingestion receipt in the Tantivy commit payload. Each receipt
   contains the durable batch UUID, a fresh physical commit UUID, and a
   complete/partial marker. Intermediate and failed worker commits are partial;
   every successful batch ends with a complete receipt. The dispatcher verifies
   the complete receipt after a successful exit before acknowledging the batch.
   Reconciliation jobs verify file identity and metadata around
   extraction and read content from the verified open file handle. A pathname
   swapped away and back cannot supply another file's contents. Windows checks
   the full FRN and volume serial; for production GUID paths it also resolves the
   handle's actual volume GUID to reject cross-volume junction substitutions.
3. Require a complete content receipt for the expected batch, then commit
   metadata with a complete receipt for that batch. Extracted files use the
   worker's committed metadata snapshot.
4. Require complete receipts in both indices for the pending batch, persist
   their exact physical commit UUIDs and the new checkpoint, and clear pending
   intent, then reveal completed entries. Whole-volume reconciliation remains
   hidden until journal catch-up.

The two indices do not form one database transaction. Durable intent and search
visibility cover the interval between their commits. A restart replays pending
work before later journal records. Deletes and replacements are idempotent, so
replay does not append another result for the same key or retain obsolete body
terms. Both search readers reload while holding the visibility read guard.

Worker failure, failed persistence, or backpressure leaves the cursor behind the
uncompleted batch. The service retries that batch and does not silently drop
it. Actionless records may advance an in-memory cursor without index commits;
periodic persistence avoids a checkpoint-write feedback loop, and their replay
after restart is harmless. A state-file lock prevents two service instances
from owning the same ingestion state. Admitted workers and blocking metadata
writes retain a shared lease on that lock until they actually finish, including
when their async caller is cancelled. A restarted scanner cannot overtake a
write that outlived its original task. Worker dispatch also shares one execution
permit across scheduler instances. A detached child retains that permit until
it has exited and been reaped, so replacing only the scheduler cannot run a retry
or later batch ahead of an old worker.

On startup, both index receipts are checked against the exact completed commit
UUIDs and any pending batch. With pending work, the reachable content/metadata
pairs are completed/completed, pending/completed, and pending/pending. Pending
content may be partial; metadata must have a complete receipt. Every pending
batch is replayed, even if both indices already contain complete receipts for
it. A receipt cannot replace successful worker execution during replay.

The reverse completed/pending pair, unrelated or missing receipts, and older
state without commit evidence require preserved-index reconciliation. Checking
physical commit UUIDs also detects an older partial or complete attempt of the
same batch restored underneath a newer checkpoint. The batch UUID alone cannot
distinguish those attempts. New empty indices receive complete receipts for a
shared seed batch and retain their individual physical commit identities.

## Reconciliation and exclusions

A new index, incompatible index generation/schema, journal gap, or requested
rescan triggers full-volume reconciliation. Directory rename/deletion and link
changes also trigger it because descendants can change paths without receiving
individual rename records. The scanner captures the journal head **before**
MFT enumeration, resets the volume through the worker lane, applies the snapshot,
and replays changes from that captured position. A journal gap during this work
requires another reconciliation.

Reconciliation pulls bounded MFT batches. One scanner retains a 256 KiB raw
buffer and resolves at most 128 raw records per pull, including excluded or
disappeared entries. The service commits the current mutation batch before
requesting the next one; paused worker admission cannot accumulate an entire
volume snapshot in memory. An empty batch represents bounded progress and does
not end the scan. Each pull and native EOF revalidate the original journal
cursor, so a journal that wraps during a worker pause invalidates the scan.

Reader/worker failures and cancellation keep the incomplete-scan marker and any
pending intent durable. Only a successful native EOF completes the baseline,
and the volume remains hidden until journal catch-up. Index writers, extraction,
and individual path lengths still contribute to memory use. Large volumes and
sustained churn can take substantial time; bounded batches do not establish a
maximum end-to-end catch-up time.

Metadata/content indices, state, jobs, logging directories, an existing semantic
index directory, and retained migration archives are excluded using canonical
GUID paths with case-insensitive path-boundary matching. The baseline skips
them; their journal records still consume the read budget and advance progress.
An indexed file moved into an excluded directory is removed from both views;
an indexed directory move requires descendant reconciliation. Failure to resolve
mandatory output exclusions is an ingestion error.

## Content omissions and errors

`volumes` selects the volumes whose metadata is indexed;
`content_index_volumes` independently enables extraction on selected drive roots
(case-insensitively). Explicitly selected volumes with an empty content list are
metadata-only. When both lists are empty, the automatic all-volume default also
enables content. Disabling content removes previously indexed body text while
preserving filename indexing.

Each volume checkpoint records its content selection and byte/character limits.
When the scanner observes a changed policy, or loads an older checkpoint without
one, it hides the volume and schedules full reconciliation. This also covers
unchanged files after a settings change or restart. Already-admitted durable
batches retain their recorded operations and limits; after they finish, a reset
and scan apply the new policy before revealing the volume again.

Directories, reparse points, offline/system files, files exceeding the configured
size limit, and entries without usable paths do not receive extraction jobs.
Their eligible metadata updates can remain searchable while obsolete content is
removed. This decision is stored with the batch, so a configuration change during
replay cannot reinterpret its original metadata-only policy.

A file can also become unsupported or too large while queued. For production
reconciliation jobs, recognized unsupported/size-limit outcomes produce a
metadata snapshot with empty, marked-truncated content and a warning. Denied
content reads can use this path only when metadata and identity can still be
verified independently. Missing files and changed identities invalidate obsolete
jobs. Unknown extraction/I/O errors and files changing during extraction remain
failures requiring retry.

Production's SimpleText and Noop backends support extraction from the verified
handle. Optional backends that require reopening a pathname explicitly report
unsupported for reconciliation jobs, so these jobs retain metadata with empty
content and an omission warning. There is no pathname fallback for reconciliation.
Standalone strict Upsert jobs retain their existing path-based extractor contract.

Consequently, healthy journal ingestion does not promise that every file has
full-text content. Inspect worker omission warnings and extraction policy when a
filename is searchable but its body is absent.

## Health and operational recovery

| Condition | Behavior and operator action |
| --- | --- |
| `watching; last bounded journal read succeeded` | A real read succeeded and observed catch-up completed. This is distinct from scheduler idle or an extraction-coverage guarantee. |
| Reconciling/catching up | Allow the selected volume to rebuild and catch up. Repeated reconciliation can indicate a journal wrapping faster than processing completes. |
| Unsupported platform or no configured NTFS volume mounted | Ingestion reports unavailable. There is no active polling fallback. Correct the runtime, mount, or selected-volume configuration. |
| Journal unavailable or access denied | Check the mounted volume, existing journal, and service account's access. Restore access and let retry/reconciliation proceed; no successful empty read is substituted. |
| Worker failure, full admission queue, or unwritable output | Retain state and job artifacts. Inspect logs, the worker executable/configuration, permissions, and free space. Restore the dependency; pending work is retried without advancing its checkpoint. |
| Invalid/unsupported state or duplicate volume identities | Startup refuses to guess identities or checkpoints. Preserve the failed state and recover a coherent index/state set as described below. |

Before recovery, stop the service and preserve its configuration, both indices,
state, pending job artifacts, and logs. Restore a matched backup made with the
service stopped, including generation markers and pending intent. If no valid
backup exists, provision fresh metadata/content/state/jobs directories and let
the service perform initial reconciliation; keep the old data for diagnosis
outside the selected indexing scope. Do not hand-edit USNs or volume IDs, or
clear/reset the NTFS journal to repair an index/checkpoint problem.

Schema, generation, or commit-identity mismatch preserves readable existing index directories as
siblings named with `before-ingestion-v2-<uuid>`, records/excludes those archives,
creates new index-generation markers, and invalidates old cursors for rebuilding.
Corrupt unreadable indices may instead fail startup and require the recovery
procedure above. Retained archives consume disk space.

Deploy matching service, worker, and client builds. IPC uses an explicit v2
magic/version/message-kind envelope; old formats are rejected before decoding
full-reference document responses. Update the UI/CLI and service together.
Service worker batches use JSON version 3 with a required non-nil batch UUID.
Workers that cannot persist commit identities reject the new version. The new
worker still accepts legacy version 1 Upsert-only and version 2 mutation input,
but those commits are untagged and cannot acknowledge durable service ingestion.
Deploy the updated service and worker together. Existing v2 checkpoints without
commit evidence trigger an archived-index rebuild on this upgrade. The
document-key representation and index schema also changed to preserve the
complete NTFS identity.

## Deterministic regressions and quality gates

Run from the repository root using the pinned `rust-toolchain.toml` toolchain:

```sh
cargo test -p core-types -p core-serialization -p meta-index -p content-index -p ntfs-watcher -p ipc
cargo test -p content-extractor
cargo test -p index-worker --bin index-worker
cargo test -p semantic-index --features hnsw_rs --lib
cargo test -p service --lib -- --test-threads=1
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
```

These regressions cover cursor bounds/record-budget continuation, event replay,
full file references, replacement/deletion, partial commits and restart,
metadata-only policy, exclusions, and scheduler admission/restart. They can
validate deterministic processing without establishing native journal behavior.

### Verification of the initial implementation

Verification of commit `b767a0a07539c45b6ea78b4cd6d6493a8a49c993` on
2026-10-10 used the pinned `nightly-2026-08-31` toolchain.
There were **131 distinct passing Linux tests**: core-types 15,
core-serialization 5, content-extractor 15, index-worker 9, meta-index 13,
content-index 7, IPC 17, ntfs-watcher 18, service 31, and feature-enabled
semantic-index 1. The final service and watcher rerun passed all 49 tests again;
those repetitions are not additional distinct tests.

| Gate | Recorded result |
| --- | --- |
| Targeted Linux service check, all targets | Passed. |
| Linux Clippy, all targets of the ten packages above, with `semantic-index/hnsw_rs` and `-D warnings` | Passed without diagnostics. |
| Windows-target watcher and IPC check and Clippy, all targets, `-D warnings` for Clippy | Passed without diagnostics. These are compilation/lint results. |
| Workspace `cargo fmt --all -- --check` and Git whitespace checks | Passed. |
| Whole-workspace Linux check and Clippy | Both stopped in UI dependency `glib-sys 0.18.1` because `pkg-config` and GLib development metadata are unavailable. Neither gate passed. |
| Windows service/worker/e2e check | Stopped in `zstd-sys` because the MSVC native toolchain, including `lib.exe`, is unavailable. |
| Native NTFS execution | Not run; there is no Windows runtime in this environment. |

Linux verification used command-only overrides for the available `cc`/gold
linker, one Cargo build job, one test thread, disabled incremental compilation,
and omitted debug symbols. System OpenSSL headers/libraries were selected
explicitly. Windows checks used the repository's Windows target flags.
The environment did not provide UBS or RCH; no results from those tools are
claimed.

### Bounded baseline and commit-recovery verification

The follow-up changes use the same pinned toolchain and locked dependencies.
The source adds regressions for bounded MFT pulls, filtered progress versus EOF,
commit-before-next-pull backpressure, cancelled baselines, reader failure,
partial worker receipts, restored attempts of the same batch, exact checkpoint
commit identities, and ingestion lock ownership after async cancellation.
The child cancellation regression also checks that a successor waits for the
shared worker execution permit until the original child has been reaped.

There were **105 distinct passing Linux tests** for the five affected packages:
ntfs-watcher 24, content-index 12, index-worker 12, meta-index 13, and service 44.
This includes 27 added regressions. Unchanged packages from the initial
131-test run retain their separately recorded verification above.

| Gate | Recorded result |
| --- | --- |
| Linux all-target check for `service`, `ntfs-watcher`, `content-index`, `index-worker`, and `meta-index`, with `--locked --offline` | Passed without source diagnostics, including the new test targets. |
| Linux tests for the five affected packages | All 105 passed, including the final dispatcher cancellation and successor-ordering regression. The Windows-only integration target ran zero tests on Linux. |
| Linux Clippy, all targets of the five affected packages, with `--locked --offline -- -D warnings` | Passed without diagnostics after the final dispatcher change. |
| Windows GNU all-target check and Clippy for `ntfs-watcher` and `ipc`, with `--locked --offline` and `-D warnings` for Clippy | Both passed without diagnostics, including the native MFT reader and Windows test code. |
| Windows GNU all-target check and Clippy for `service` and `index-worker`, with `service/e2e-windows`, `--locked --offline`, and `-D warnings` for Clippy | Both passed after the final Windows test cleanup, including production Windows service/worker code and the native integration test target. |
| Workspace `cargo fmt --all -- --check` and staged Git whitespace checks | Passed. |
| Whole-workspace Linux check and Clippy | Both attempted against the final source and stopped in UI dependency `glib-sys 0.18.1`: the `pkg-config` command needed to locate `glib-2.0 >= 2.56` is absent. Both exited 101; neither gate passed. |
| Native NTFS and service lifecycle execution | Not run; no Windows runtime is available in this environment. |

The follow-up Windows checks used `x86_64-pc-windows-gnu`, the pinned Rust
toolchain and target standard library, and the official LLVM-MinGW 20261006
MSVCRT Linux x86-64 bundle (Clang 23.1.3), with target-specific compiler,
archiver, and linker variables and `RUSTFLAGS='-Z threads=1'`. Host OpenSSL
paths were unset for the cross commands. The broader check compiled the Windows
C dependencies as well as the Windows Rust code; it did not substitute
dependency stubs or change tracked dependency versions or feature gates. These
are compilation results, not native execution results.

## Native NTFS acceptance

Use an **elevated Windows developer PowerShell** with an isolated mounted NTFS
volume, an already active USN journal, the Windows build dependencies, and a real
worker built from the same checkout. The service test indexes the entire selected
volume; do not point it at a normal system/data volume. In this example the
isolated volume is `V:`. Adjust the worker path if using `CARGO_TARGET_DIR`.

```powershell
cargo build -p index-worker --target x86_64-pc-windows-msvc
$env:ULTRASEARCH_NTFS_TEST_ROOT = 'V:\'
$env:ULTRASEARCH_WORKER_PATH = (Resolve-Path '.\target\x86_64-pc-windows-msvc\debug\index-worker.exe').Path
$env:TEMP = 'V:\UltraSearchNativeTemp'
$env:TMP = $env:TEMP
New-Item -ItemType Directory -Force -Path $env:TEMP | Out-Null
fsutil usn queryjournal V:

cargo test -p ntfs-watcher --target x86_64-pc-windows-msvc native_ntfs_lifecycle -- --ignored --nocapture --test-threads=1
cargo test -p service --target x86_64-pc-windows-msvc --features e2e-windows --test ntfs_incremental -- --ignored --nocapture --test-threads=1
```

The low-level watcher test uses the Windows temporary directory, hence the
explicit `TEMP`/`TMP` setting. It checks real create/modify/attribute/rename/delete
records, GUID paths, identity, and cursor rejection. It also enumerates the
isolated volume through two-record MFT pulls, verifies the fixture's identity
and metadata, and replays a change made after the captured baseline head.
The service test uses
`ULTRASEARCH_NTFS_TEST_ROOT` and `ULTRASEARCH_WORKER_PATH`, creates files after
baseline completion, and verifies all three search modes, current metadata,
no duplicates, paused-worker backpressure, forced reconciliation, restart with
an offline edit, and deletion while a live sentinel remains searchable.
It retains a uniquely named fixture directory and writes
`data/log/native-ingestion-evidence.json` only after success. Missing prerequisites
fail an explicitly selected native test; they do not return an early success.

Windows configuration/type checks can also be requested without native execution:

```sh
rustup target add x86_64-pc-windows-msvc
cargo check -p ntfs-watcher -p ipc --all-targets --target x86_64-pc-windows-msvc
cargo check -p service --features e2e-windows --test ntfs_incremental --target x86_64-pc-windows-msvc
```

**Validation limitation:** this implementation work was performed in a Linux
environment. The follow-up Windows GNU all-target checks and strict Clippy
passed for the watcher, IPC, service, worker, and native integration test target.
The initial broader Windows MSVC attempt
stopped in `zstd-sys` because the MSVC native toolchain, including `lib.exe`, is
unavailable. The subsequent GNU check reached and checked the service and
worker's Windows Rust code, but does not establish an MSVC build or Windows
runtime acceptance.

No successful native Windows journal or native service lifecycle run has been
established there. Windows-gated tests are absent from Linux test runs;
cross-compilation cannot validate NTFS permissions, kernel journal behavior,
worker launch, or restart behavior on a real volume. Record native commands,
host/volume details, and retained evidence before claiming Windows qualification;
the available tests are not exhaustive filesystem qualification.
