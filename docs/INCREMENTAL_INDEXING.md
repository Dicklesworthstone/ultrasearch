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

Metadata access failures reject the whole bounded read, including any events
resolved before the failure. Only file/path-not-found means that an identity
disappeared; access denied, sharing violations, unavailable devices, and failed
path/exclusion probes cannot acknowledge deletion or silently omit a baseline
entry. Content refresh reasons include transacted changes and ordinary data
writes, including data reasons accumulated into close records. CLOSE alone
changes no indexed state.

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
Admitting any Reconcile job durably retains the volume's catch-up obligation:
a path can move while that job waits. A read taken before these workers cannot
certify their later filesystem observations. The service settles coverage only
after a subsequent caught-up read that admits no new Reconcile work; reconciliation
admitted from a journal batch requests an immediate follow-up turn.
Ignored output records and metadata-only cleanup can still
settle that verification read.

## Commit, visibility, and replay

The service processes one serialized ingestion lane with this ordering:

1. Coalesce events by complete document identity and persist a pending batch in
   `state_dir/ingestion-v2.json`. Hide its affected search entries before index
   mutation. Durable mutation batches contain at most 128 changes.
2. Wait for bounded scheduler admission and a successful real worker exit. The
   worker invalidates obsolete content, applies replacements/deletions, and
   stores an ingestion receipt in the Tantivy commit payload. Each receipt
   contains the durable batch UUID, a fresh physical commit UUID, and a
   complete/partial marker, and a bounded set of deferred file identities.
   Intermediate and failed worker commits are partial; every successful batch
   ends with a complete receipt. The dispatcher verifies
   the complete receipt after a successful exit before acknowledging the batch.
   Reconciliation jobs verify file identity and metadata around
   extraction. Windows captures bounded immutable bytes under an oplock on the
   verified file object, then releases its capture handles before extraction.
   The non-Windows test path reads the verified open file handle. A pathname
   swapped away and back cannot supply another file's contents. Windows checks
   the full FRN and volume serial; for production GUID paths it also resolves the
   handle's actual volume GUID to reject cross-volume junction substitutions.
3. Require a complete content receipt for the expected batch, then commit
   metadata with a complete receipt for that batch. Extracted files use the
   worker's committed metadata snapshot. Deferred files must have no content
   document, and their metadata is tombstoned as part of the same batch.
4. Require complete receipts in both indices for the pending batch, persist
   their exact physical commit UUIDs, the new checkpoint, and any deferred retry
   obligations, and clear pending intent, then reveal completed entries.
   Whole-volume reconciliation remains
   hidden until journal catch-up.

The two indices do not form one database transaction. Durable intent and search
visibility cover the interval between their commits. A restart replays pending
work before later journal records. Deletes and replacements are idempotent, so
replay does not append another result for the same key or retain obsolete body
terms. Both search readers reload while holding the visibility read guard.

Already durable intent replays even when its volume is offline or deselected,
including when no selected volume is mounted. Its recorded GUID paths and content
policy remain authoritative. Unavailable source files become durable deferred
outcomes, releasing the shared lane for other volumes; the unavailable volume
remains hidden. Mount selection still controls new reads and scheduled retries.

Fatal worker failure, failed persistence, or backpressure leaves the cursor behind
the uncompleted batch. The service retries that batch and does not silently drop
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
rescan triggers full-volume reconciliation. Link/reparse changes and ambiguous
directory ancestry also require it. Ordinary directory renames use the bounded
subtree repair described below because descendants can change paths without
receiving individual rename records. Ordinary non-reparse directory deletion is a precise
tombstone, including records with accumulated create, data, attribute, or close
reasons. Recursive removal supplies each child's deletion or move records; routine
temporary-directory cleanup no longer resets a volume. Directory deletion with
structural/reparse history remains conservative and requires reconciliation.
The scanner captures the journal head **before**
MFT enumeration, resets the volume through the worker lane, applies the snapshot,
and replays changes from that captured position. A journal gap during this work
requires another reconciliation.

Reconciliation pulls bounded MFT batches. Each active volume retains one reader
with a 256 KiB raw buffer and resolves at most 128 raw records per pull, including
excluded or disappeared entries. In each scheduling round, a volume performs one
reset, one MFT page, or validated EOF, or one bounded journal read. Other volumes
therefore receive turns between baseline pages. Active work continues after a
cooperative yield; the one-second poll applies when no continuation is pending.
The service commits the current mutation batch before requesting its next page;
paused worker admission cannot accumulate an entire volume snapshot in memory.
A page rejected for retry capacity remains attached to its reader until admitted,
so recovery neither drops it nor pulls ahead. An empty page is bounded progress
and still ends that volume's turn. Each pull and native EOF revalidate the original
journal cursor, so a journal that wraps during a worker pause invalidates the scan.

Reader/worker failures and cancellation keep the incomplete-scan marker and any
pending intent durable. Only a successful native EOF and durable checkpoint write
complete the baseline. A failed EOF save preserves both the in-memory and on-disk
incomplete checkpoint. Restart discards volatile readers, replays pending work,
and starts an incomplete baseline with a fresh reset and captured journal head.
The volume remains hidden until journal catch-up. Index writers, extraction,
and individual path lengths still contribute to memory use. Large volumes and
sustained churn can take substantial time; bounded batches do not establish a
maximum end-to-end catch-up time.

Metadata/content indices, state, jobs, logging directories, an existing semantic
index directory, and retained migration archives are excluded using canonical
GUID paths with case-insensitive path-boundary matching. The baseline skips
them; their journal records still consume the read budget and advance progress.
An indexed file moved into an excluded directory is removed from both views;
an indexed directory move requires descendant repair. Failure to resolve
mandatory output exclusions is an ingestion error.

### Directory moves without repeated volume resets

The watcher retains each directory rename record's complete file reference,
historical parent reference, and recorded name, together with identity-checked
current metadata. The service combines these records with indexed ancestry to
preserve old, intermediate, and current path prefixes. It durably records those
prefixes before changing descendants. Missing or ambiguous ancestry, excessive
history, journal gaps, and structural link/reparse changes fall back to full
reconciliation instead of accepting uncertain coverage.

A frozen metadata reader examines at most 128 raw document positions or segment
transitions per turn, including deleted, unrelated, and other-volume entries.
Matching descendants are resolved by full file reference against the original
journal position, then replaced or deleted through the existing dual-index
receipt lane. Deferred descendants are included even though their search rows
were removed. This can examine the whole stored metadata index in bounded pages;
it does not extract or replace unrelated files. Ordinary changes in the same
journal batch are also refreshed before application. Directory roots are updated
last, and only the final committed page advances the journal cursor.

Search masks use the volume identity and normalized, case-insensitive path
boundaries in both indices before result counts and pagination. A repair hides
its affected prefixes while unrelated results remain searchable. After restart,
pending work replays first and saved prefixes are reconstructed before repair
resumes; startup also retains its conservative volume mask until journal catch-up.
A failed file probe discards the volatile reader so the saved history can be
scanned again. Capacity rejection retains the already-read page without
reading ahead. Repair admission preserves space for existing retries; if history
prevents admission and no retry can release capacity, full reconciliation replaces
the repair obligation. Query execution and index commits still have their normal costs;
the bounded ingestion page is not an end-to-end latency guarantee.

A directory entering indexed coverage may contain unchanged children with no
individual journal event. Such moves also run bounded MFT discovery filtered to
the affected current prefixes. Discovery also covers prior baseline, edit, or
retry work whose queued pathnames may have disappeared before the worker opened
them. A successful obsolete-path tombstone can remove the only indexed child
identity even though the unchanged file survives under its parent's new name.
The retained catch-up obligation makes the first following directory move
discover that child again. This includes roots already observed by a baseline
after leaving an exclusion. Every repaired directory retains its coverage
obligation through the root commit and restart until fresh journal catch-up;
further ancestor or child moves during that interval require discovery again.
The filtered scan can resolve unrelated MFT metadata while enumerating the volume,
but only matching descendants enter mutation batches. It neither resets the
volume nor hides unrelated files.

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
verified independently. Win32 sharing and lock violations never qualify as
permanent permission-based omissions. Missing paths on a GUID volume require an
independently verified accessible volume root and a bounded reopen before being
accepted as obsolete; unavailable or mismatched roots retain a retry obligation.
Changed identities invalidate obsolete jobs. Unknown extraction/I/O errors and
files changing during extraction require deferred retry as described below.

Production's SimpleText and Noop backends support extraction from verified
snapshot bytes as well as the verified-handle path. Optional backends that require
reopening a pathname explicitly report
unsupported for reconciliation jobs, so these jobs retain metadata with empty
content and an omission warning. There is no pathname fallback for reconciliation.
Standalone strict Upsert jobs retain their existing path-based extractor contract.

Consequently, healthy journal ingestion does not promise that every file has
full-text content. Inspect worker omission warnings and extraction policy when a
filename is searchable but its body is absent.

### Coherent Windows content capture

Windows Reconcile jobs atomically open the file for a Read-Handle oplock while
allowing read, write, and delete sharing. The oplock request immediately follows
the open, before any metadata operation. A granted lease covers identity checks
and reads of at most 64 KiB at a time, with at most one byte beyond the persisted
content limit. A break before, during, or just after a read rejects the snapshot,
including a break racing EOF. Same-length overwrites cannot be accepted merely
because their final last-write timestamp has not yet appeared.

Capture cancellation closes the source handle promptly, then drains outstanding
I/O before releasing its buffers. A new share-incompatible application opener can
continue after that close. CPU extraction consumes only immutable captured bytes
after all capture handles close. The final identity/metadata probe requests only
attributes, so it does not acquire another data-read share during extraction.
Unsupported oplocks, read failures, and interrupted captures retain a deferred
retry obligation rather than publishing possibly mixed bytes. Recognized content
permission and size omissions retain their separate policy described above.

Each pending read waits up to five seconds before cancellation is requested.
This is not a universal wall-clock bound on filesystem operations or driver
cancellation. Changes after a completed capture are handled by the final metadata
check and subsequent journal replay; the index remains eventually consistent.

The native protocol follows Microsoft's [oplock overview](https://learn.microsoft.com/en-us/windows/win32/fileio/opportunistic-locks),
[FSCTL_REQUEST_OPLOCK contract](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_request_oplock),
and [asynchronous cancellation lifetime rules](https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-cancelioex).

## Deferred extraction without stopping journal progress

A single persistently locked or changing file must not make every volume repeat
one global failed worker batch. Version 4 service batches distinguish per-file
reconciliation failures from transaction failures. Filesystem probe failures,
extractor errors, and snapshot changes defer the affected Reconcile job. The
worker removes its old content, continues the other jobs, and commits the sorted
deferred-key set atomically with its final content outcome. Validation, index
mutation, persistence, and process failures still fail the entire transaction.
Standalone strict Upsert jobs retain their strict failure behavior.

The service validates that every deferred key belongs to the durable batch,
verifies that its content was tombstoned, and removes the corresponding metadata.
The checkpoint atomically retains the file snapshot, attempt count, and next retry
time together with both physical index commit identities and the journal cursor.
The file stays absent from all search modes until a later successful replacement;
obsolete content and metadata are not presented as current results. A crash in
either commit window replays the pending batch before publishing these obligations.

Retries start after five seconds and back off through 10, 20, 40, 80, and 160
seconds to a five-minute cap. A backwards wall-clock change cannot postpone an
obligation indefinitely. After fresh journal work, the service admits at most one
bounded retry batch of 128 files while content extraction is allowed. Due files
are ordered by deadline and document identity. A retry has no journal cursor and
cannot reset a volume; it uses the worker's current verified file snapshot for
replacement in both indices. Fresh changes supersede older obligations, and
deletion, exclusion, or a volume reset removes them. After pending work has
replayed, offline and deselected volumes exchange their per-file retries for a
durable full-volume reconciliation obligation. The volume stays hidden, and the
service persists that replacement obligation before releasing any retry capacity.
Remounting or selecting it again requires a fresh baseline and journal catch-up.
This prevents an unavailable volume from occupying the entire shared retry ledger
indefinitely while another volume waits to index new changes.
Retries follow successful bounded reads even when more journal records remain,
so sustained change load cannot starve a recovered file solely by preventing
full catch-up.

The ledger is bounded to 1,024 files and the state envelope to 32 MiB. Admission
reserves capacity for the worst possible deferred outcome before any index write,
including enough serialized headroom to retry an existing file. Oversized retry
groups are reduced to a fitting prefix. Near capacity, older obligations receive
priority; a volume whose new batch was rejected for capacity may retry older files
even while it has journal backlog. Rejection installs no global pending barrier
and consumes no journal position. A full ledger can still apply backpressure if
none of its failures recover; it does not discard obligations or claim healthy
idle operation.

Delete-only, volume-reset, and empty durable batches can dispatch while content
extraction is paused by idle/load policy. Batches containing extraction continue
to respect that policy and ordered durable intent. Previously queued legacy
extraction remains an ordering barrier. Failure to resolve native journal metadata
is still an explicit volume error; deferred extraction does not manufacture a
successful native read or conceal a fatal dependency failure.

## Health and operational recovery

| Condition | Behavior and operator action |
| --- | --- |
| `watching; last bounded journal read succeeded` | A real read succeeded and observed catch-up completed. This is distinct from scheduler idle or an extraction-coverage guarantee. |
| Reconciling/catching up | Allow the selected volume to rebuild and catch up. Repeated reconciliation can indicate a journal wrapping faster than processing completes. |
| Previously caught-up volume has new backlog | The next bounded read records catching-up state before mutation admission. Prior healthy status cannot survive a newly observed backlog. |
| Unsupported platform or no configured NTFS volume mounted | Ingestion reports unavailable. There is no active polling fallback. Correct the runtime, mount, or selected-volume configuration. |
| Backup privilege initialization failed | Windows ingestion requires `SeBackupPrivilege` already assigned to the dedicated service process token. Startup enables only that privilege and rejects incomplete assignment; correct the service account and restart. No account rights or ACLs are changed by ingestion. |
| Journal unavailable or access denied | Check the mounted volume, existing journal, and service account's access. Restore access and let retry/reconciliation proceed; no successful empty read is substituted. |
| Worker failure, full admission queue, or unwritable output | Retain state and job artifacts. Inspect logs, the worker executable/configuration, permissions, and free space. Restore the dependency; pending work is retried without advancing its checkpoint. |
| `degraded: N files deferred for extraction retry` | Other journal changes can progress. Pending file/byte totals include the retained obligations. Restore file access or release locks and allow the bounded retry schedule to run; readiness requires no selected-volume obligations. |
| Deferred extraction capacity reserved | New affected work waits before installing intent or moving its cursor. Due existing obligations receive priority so recovery can release capacity. Persistent failures or paused content admission can continue to apply backpressure. |
| Directory repair in progress | Affected path prefixes remain hidden until bounded descendant replacement completes. Unrelated files stay searchable. Discovery may also enumerate MFT metadata when unchanged children enter coverage. |
| Unavailable volume retained deferred work | Its per-file ledger is replaced transactionally by full-volume recovery, releasing shared capacity. Keep the saved checkpoint; remounting requires reconciliation before this volume becomes visible. |
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
Service worker batches use JSON version 4 with a required non-nil batch UUID.
Older workers reject the new version instead of acknowledging deferred work
without an outcome. The updated worker accepts version 3 strict durable input and
legacy version 1 Upsert-only/version 2 mutation input; the legacy commits are
untagged and cannot acknowledge durable service ingestion. Tantivy ingestion
payload version 2 adds the deferred-key set; readers also accept version 1
receipts as having no deferred obligations. Partial receipts cannot carry retries.

The state envelope is now version 4, retaining the `ingestion-v2.json` filename so
existing volume identities stay bound. Valid version 2 and 3 state is accepted
and upgraded with empty directory-repair history. An older service rejects the
new envelope instead of silently dropping recovery obligations. Deploy the updated
service and worker together. Exact index commit evidence remains required.

Both indices now require the normalized, untokenized `path_exact` field used for
precise subtree masking. An index created by the preceding schema therefore
undergoes the preserved, archived-index rebuild described above on its first
startup with this version. The state-format upgrade itself retains volume IDs;
the schema rebuild invalidates journal checkpoints so the new field is populated
for every document. Existing v2 checkpoints without commit evidence still require
reconciliation. Full NTFS document identities remain unchanged by this upgrade.

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

### Deferred extraction and incremental cleanup verification

The next implementation pass adds durable worker outcomes, per-file retry
recovery, bounded retry admission, metadata-only dispatch during content pauses,
and precise ordinary-directory deletion. The regression coverage includes both
split-commit restart windows, removal of stale results in all three search modes,
unrelated-file progress, retry/deletion supersession, stable identities, state
version upgrades, count and serialized-byte capacity, and scheduler replacement.
The final worker guard also checks bounded root verification and reopening before
accepting a missing GUID path as obsolete.

There were **125 distinct passing Linux tests** for the affected packages:
ntfs-watcher 26, content-index 14, index-worker 17, meta-index 13, and service 55.
All passed, with no failed or ignored tests in those suites. The Windows-only
integration target has no runnable tests on Linux. This is 20 additional host
regressions beyond the preceding 105-test affected-package run; repeated checks
are not counted as additional tests.

| Gate | Recorded result |
| --- | --- |
| Final Linux all-target check and Clippy for `service`, `ntfs-watcher`, `content-index`, `index-worker`, and `meta-index`, with `--locked --offline` and `-D warnings` for Clippy | Both passed against the final source, including the missing-volume guard. |
| Windows GNU all-target check and strict Clippy for `ntfs-watcher` and `ipc` | Both passed. |
| Final Windows GNU all-target check and strict Clippy for `service` and `index-worker`, with `service/e2e-windows` | Both passed, including the native sharing-lock/restart test and missing-GUID worker regression. |
| Workspace `cargo fmt --all -- --check` and Git whitespace checks | Passed. |
| Whole-workspace Linux check and Clippy | Both attempted and exited 101 in `glib-sys 0.18.1` because `pkg-config` is absent and `glib-2.0 >= 2.56` cannot be resolved. Neither whole-workspace gate passed. |
| Native Windows execution, installed MSI acceptance, and native performance qualification | Not run; no Windows runtime is available here. |

This pass used the pinned `nightly-2026-08-31` toolchain and locked dependencies,
with the same host linker/OpenSSL settings and official LLVM-MinGW Windows GNU
toolchain described above. Tests ran with one build job, one test thread, no
incremental compilation, and no debug symbols. No tracked dependency versions or
feature gates were changed. UBS and RCH were unavailable; no result from either
tool is claimed.

The native sources now include a real data-read sharing violation with unrelated
work and restart/recovery, a missing GUID root that must become deferred work,
and recursive directory deletion with a moved-out survivor. The missing-root
fixture does not mount, unmount, or alter a volume. These Windows-only tests still
require native execution before acceptance can be claimed.

### Fair volume progress and coherent Windows capture verification

The next pass makes baseline scans resumable between volume turns, retains a
capacity-rejected page, replays admitted work independently of source-volume
selection, and makes baseline EOF persistence transactional. It also rejects
partial native metadata resolution, handles transacted content changes, enables
the service token's already-held backup privilege, and captures immutable Windows
content under an atomic Read-Handle oplock before CPU extraction.

There were **154 distinct passing Linux tests** across the affected packages:
service 60, ntfs-watcher 30, index-worker 20, content-extractor 17, content-index
14, and meta-index 13. All passed, with zero failed or ignored tests. This includes
14 new host regressions; repeated extractor and worker runs are not additional
distinct tests. The new scanner regressions use the production bounded-turn and
pending-replay functions with real metadata/content index commits. Native calls
and offline worker outcomes modeled by those host tests remain separate from
Windows execution evidence.

The regressions verify other-volume create/edit/delete progress between MFT pages,
empty-page fairness, retaining a rejected page at the 1,024-file retry limit,
replaying a failed page commit before advancing the same reader, offline-volume
pending recovery and remount, healthy-to-backlog status, and failed EOF checkpoint
persistence. Watcher tests cover rejected metadata/exclusion probes and exact
replay, transacted changes, and complete privilege assignment. Worker/extractor
tests cover bounded snapshot reads, breaks racing reads and EOF, read errors,
verified bytes surviving path replacement, and content limits/omission policy.

| Gate | Recorded result |
| --- | --- |
| Linux tests for the six affected packages, with `--locked --offline -- --test-threads=1` | All 154 passed. The Windows-only service integration target ran zero tests on Linux. |
| Linux all-target check and strict Clippy for the same six packages | Both passed. The post-freeze worker test rerun passed all 20 tests again. |
| Windows GNU all-target check and strict Clippy for `ntfs-watcher` and `ipc` | Both passed, including the privilege helper and duplicate-token test targets. |
| Windows GNU all-target check and strict Clippy for `service`, `index-worker`, and `content-extractor`, with `service/e2e-windows` | Both passed, including the native oplock code, writer-contention tests, and NTFS integration target. These are compilation/lint results, not native execution. |
| Workspace `cargo fmt --all -- --check` and Git staged/unstaged whitespace checks | Passed. |
| Whole-workspace Linux check and Clippy | Both attempted and exited 101 in the unchanged UI dependency `glib-sys 0.18.1`: `pkg-config` is absent and `glib-2.0 >= 2.56` cannot be resolved. Neither whole-workspace gate passed. |
| Native Windows execution, installed MSI acceptance, and native performance qualification | Not run; no Windows runtime is available here. |

Verification used the pinned `nightly-2026-08-31` toolchain, locked dependencies,
the host linker/OpenSSL settings and official LLVM-MinGW Windows GNU toolchain
described above, one build job and test thread, no incremental compilation, and
no debug symbols. Host linking emitted the toolchain's existing gold-linker
deprecation notice. No dependency versions, feature gates, receipt formats, or
checkpoint schema versions changed in this pass. UBS and RCH were unavailable.

### Directory repair and unavailable-volume capacity verification

The 2026-10-11 pass replaces ordinary directory-triggered volume resets with
durable bounded descendant repair and filtered discovery, and reclaims unavailable
volumes' shared retry capacity only after persisting full-volume recovery. It
also preserves existing retry byte headroom when recording directory history.

There were **192 distinct passing Linux tests** across seven affected packages:
service 71, ntfs-watcher 36, core-types 16, meta-index 17, content-index 15,
index-worker 20, and content-extractor 17. All passed with zero failed or ignored
tests. This includes 23 new host regressions relative to the preceding versions
of these packages; repeated final runs are not additional distinct tests.

The new service regressions use real metadata/content index commits with modeled
native probes and worker outcomes. They cover more than one descendant page,
content-before-metadata failure and restart replay, retaining old and intermediate
directory paths, exclusion entry, discovery after an overlapping baseline,
subsequent ancestor and child moves, unchanged children entering coverage,
resolver failure, retained pages at retry saturation, and transactional history
admission near the byte limit. Two additional regressions force a parent move
after durable child admission and model the resulting successful obsolete-path
tombstone without deferred debt. They cover directory repair, initial MFT work,
and ordinary edits, then require recovery from directory events alone after
restart and a fresh post-worker read before healthy catch-up.
The tests also fill the retry ledger with an unavailable volume, require
unrelated-volume edit/delete progress, and verify that restart
preserves the unavailable volume's full-recovery obligation. Watcher and index
regressions cover raw rename history, bounded identity probes, unsupported hosts,
full reference reuse sequences, path boundaries, volume isolation, bounded scans,
and schema migration. Native kernel behavior remains outside these host tests.

| Gate | Recorded result |
| --- | --- |
| Linux tests for the seven affected packages, with `--locked --offline -- --test-threads=1` | All 192 passed. The final service suite passed all 71 tests, including the admitted-path races and fresh-read recovery assertions. |
| Final Linux all-target check and strict Clippy for the same seven packages | Both passed with `--locked --offline` and `-D warnings` for Clippy. |
| Windows GNU all-target check and strict Clippy for `ntfs-watcher` and `ipc` | Both passed, including the new bounded identity-probe and directory-history native test code. |
| Final Windows GNU all-target check and strict Clippy for `service`, `index-worker`, and `content-extractor`, with `service/e2e-windows` | Both passed, including the revised real-worker fixture that moves the directory after the child's intermediate path is durably admitted. Compilation/lint only. |
| Workspace `cargo fmt --all -- --check` and Git staged/unstaged whitespace checks | Passed. |
| Whole-workspace Linux check and strict Clippy | Both attempted and exited 101 in unchanged `glib-sys 0.18.1`: `pkg-config` is absent, so `glib-2.0 >= 2.56` cannot be located. Neither whole-workspace gate passed. |
| Native Windows execution, installed MSI acceptance, and native performance qualification | Not run; no Windows runtime is available here. |

Verification used the pinned `nightly-2026-08-31` toolchain and locked dependencies,
the official LLVM-MinGW 20261006 MSVCRT Linux x86-64 cross toolchain, and the host
linker/OpenSSL settings described above. Host linking emitted the existing gold
linker deprecation notice; no source lint failures remained. No tracked dependency
versions, feature gates, workflow files, worker batch formats, or receipt formats
changed. State v4 and the exact-path index fields require the migration described
above. UBS and RCH were unavailable. Native acceptance remains outstanding.

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
cargo test -p ntfs-watcher --target x86_64-pc-windows-msvc held_backup_privilege_is_enabled_idempotently_on_a_copied_token -- --ignored --nocapture --test-threads=1
cargo test -p ntfs-watcher --target x86_64-pc-windows-msvc -- --nocapture --test-threads=1
cargo test -p index-worker --bin index-worker --target x86_64-pc-windows-msvc -- --nocapture --test-threads=1
cargo test -p service --target x86_64-pc-windows-msvc --features e2e-windows --test ntfs_incremental -- --ignored --nocapture --test-threads=1
```

The low-level watcher test uses the Windows temporary directory, hence the
explicit `TEMP`/`TMP` setting. It checks real create/modify/attribute/rename/delete
records, GUID paths, identity, and cursor rejection. It also enumerates the
isolated volume through two-record MFT pulls, verifies the fixture's identity
and metadata, and replays a change made after the captured baseline head. It checks
same-length edits while a writer stays open and a later close-derived refresh.
The test explicitly enables its already-held backup privilege and fails when that
prerequisite is absent. Separate token regressions use duplicate tokens to test
activation, idempotence, and rejection when the privilege is genuinely absent;
they do not change the test process token or grant account rights. Recursive
directory removal verifies each deleted identity, a moved-out survivor, and replay
without a full-volume reset. Consecutive directory moves retain both recorded
names and current metadata while a bounded full-reference probe resolves an
unchanged child; exclusion replay retains the directory's historical anchors.
Worker component tests exercise actual atomic oplock capture with existing and
new fully shared writers, a same-length overwrite, and recovery after the broken
capture closes. Another test opens a share-incompatible writer on a separate
thread and requires the snapshot reader to release it after observing the break.
A real batch/extractor test holds an exclusive writer through CPU extraction and
the final attribute-only probe. These native component tests complement the
durable tombstone/restart service test; they do not by themselves establish an
end-to-end service race result or writable-memory-mapping qualification.
The service test uses
`ULTRASEARCH_NTFS_TEST_ROOT` and `ULTRASEARCH_WORKER_PATH`, creates files after
baseline completion, and verifies all three search modes, current metadata,
no duplicates, paused-worker backpressure, forced reconciliation, restart with
an offline edit, and deletion while a live sentinel remains searchable. It also
holds a real Windows sharing lock that blocks data reads while allowing metadata
access, verifies both-index tombstones and one durable retry obligation, and checks
unrelated create/edit/delete progress. The obligation must survive a restart with
the lock still held. After unlocking without another edit, the test permits up to
360 seconds for the persisted five-minute maximum backoff to restore one current
result under the original identity. The directory fixture pauses real dispatch
while retaining the scheduler's admission channel, renames a directory, waits for
the unchanged child's durable job at the intermediate path, and renames the
directory again before dispatch resumes. It requires descendant work without a volume
reset while an unrelated sentinel stays visible. After resuming, it checks stable
identities and removal of old and intermediate paths. Moving the tree into an
excluded output directory removes descendants from both search views; moving it
back out discovers unchanged children. The evidence records blocked intent and
the cursors for these phases. Healthy readiness requires an empty retry list,
no directory repair, and no unsettled discovery obligations.
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
