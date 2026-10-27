# Windows startup and search troubleshooting

An installed service, a connected UI, and a searchable index are three different
conditions. Check them separately before changing configuration or reinstalling.

## Installed startup contract

1. The MSI installs `service.exe`, `index-worker.exe`, `ui.exe`, `cli.exe`, and
   `launcher.exe` into the same application directory. Both WiX definitions
   register the automatic, LocalSystem service named `UltraSearchService` and
   request that Windows start it during installation.
2. `service.exe` connects to the Windows Service Control Manager (SCM) before
   configuration IO, permission setup, or indexing. The service host registers
   its control handler and reports `START_PENDING` while initialization proceeds.
3. The worker loads the ProgramData configuration, validates settings, creates
   data directories, acquires ingestion ownership, and opens the indexes. The
   startup stages are `RuntimeReady`, `ConfigurationBound`, `IndexesReady`, and
   `EndpointReady`; their completion advances the SCM checkpoint.
4. The search readers and named-pipe endpoint `\\.\pipe\ultrasearch` must exist
   before the host reports `RUNNING`. Index scanning is subsequent background
   work. A service-registration check alone does not establish pipe readiness.
5. The UI immediately probes status, retries bounded connection failures, and
   distinguishes connection errors from query or indexing errors. A cold search
   receives its requested execution budget plus transport time. Retry restarts
   status polling immediately and retries the current query.
6. The ingestion coordinator validates volume identity, completes the filename
   baseline and journal catch-up, and publishes searchable coverage. The UI
   refreshes the current query when committed index data or visibility changes,
   including after reconnecting with old results already displayed.

The release MSI uses `ultrasearch/crates/ui/wix/main.wxs`; the local installer
script also has `ultrasearch/wix/main.wxs`. They agree on service identity and the
worker executable beside the service. The release shortcuts launch the UI; the
portable launcher starts a console service only when SCM explicitly says the
service is not registered. A stopped, starting, running, or inaccessible installed
service does not authorize a second index writer.

## Collect the first useful evidence

Open PowerShell. Adjust the application path if it was installed elsewhere:

```powershell
$app = Join-Path $env:ProgramFiles 'UltraSearch'
sc.exe qc UltraSearchService
sc.exe queryex UltraSearchService
& "$app\cli.exe" --version
& "$app\cli.exe" ping --json
& "$app\cli.exe" status --json
& "$app\cli.exe" search 'part-of-a-known-filename' --mode name --json
```

Keep the complete errors and service exit codes. Include the installer filename
and installed executable version in a bug report. A successful ping confirms a
responsive pipe; it does not prove that the first index is complete.

| Observation | Meaning and next step |
| --- | --- |
| Service does not exist (1060) | Check MSI installation logs and repair the installation. A portable directory should be started with `launcher.exe`. |
| Service is `STOPPED` | Record Windows and service-specific exit codes. Start it from Services or elevated PowerShell, then inspect the startup log if it stops again. |
| Service remains `START_PENDING` | Record checkpoint and wait hint. Inspect the last startup stage and configuration/index path failure. |
| Service is `RUNNING`, but the pipe is missing or busy | Preserve the exact pipe error. Confirm the installed binary path and inspect startup/runtime logs; do not start a competing console process. |
| Pipe access is denied | Run the same status request as an ordinary user and preserve the error. Inspect endpoint permissions and installation identity. Elevation is a diagnostic comparison, not the expected everyday UI mode. |
| Connected, with an initial-index or journal-reconciliation message | Filename coverage is incomplete. Check selected NTFS volumes and ingestion progress. The existing query refreshes when coverage becomes available. |
| Connected, with indexing degraded/unavailable/retrying | Inspect the actual ingestion error, worker executable location, selected mounted volumes, and logs. This is different from a connection failure. |
| Connected, with a rejected or timed-out search | Preserve the query error. A query failure does not establish that SCM stopped the service. |
| Connected, current coverage, completed search with no hits | Check query mode, selected volume scope, and exclusions. Only this case is an ordinary empty result. |
| Configuration error | Correct the named file or setting, then reload or restart as appropriate. The UI retains configuration errors instead of treating a failed load or apply as successful onboarding. |

The Service Health view separates filename-ingestion state from content-work
admission. Content admission may report AC power, Game Mode, deep-idle, CPU,
disk, or adaptive-policy restrictions. These messages explain why content jobs
are waiting instead of presenting a disconnected service.

## Filename coverage and deferred content

Filename mutations commit independently of content extraction. The coordinator
uses the existing paired transaction machinery: a content tombstone removes
stale body text and supplies a real worker receipt, then metadata is committed
and both physical receipts are checked before advancing the journal cursor.
Initial filenames still require MFT completion and valid journal catch-up;
battery or idle policy does not postpone that coverage.

Eligible metadata with no corresponding content document represents durable
work still to extract. A bounded scan finds that work when content admission
opens. It checks snapshot candidates against current metadata before dispatch,
so an old pathname cannot replace a newer journal observation. Content retries
retain their existing backoff.

If admission closes before an optional worker starts, the scheduler reports a
distinct deferred outcome. It is not a commit acknowledgment. The coordinator
must rewrite the intent and execute a real metadata-only transaction before
retiring it. The same rule applies during restart, preventing an old optional
content job from blocking filename recovery. An extraction failure retains
valid filename metadata while removing stale body text and recording retry debt.

The durable checkpoint format is version 5. Upgrade handling checks existing
physical receipts and migrates older pending work. Older binaries reject the
new semantics; a downgrade is not a substitute for diagnosing a startup failure.

## Start the installed service correctly

From an elevated PowerShell session:

```powershell
sc.exe start UltraSearchService
sc.exe queryex UltraSearchService
```

Launching `service.exe` or an older `ultrasearch-service.exe` directly without an
argument selects the SCM dispatcher. An interactive process was not started by
SCM, so Windows error **1063**, `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`, is
expected. That error alone says nothing about the health of an independently
installed service.

For intentional interactive diagnosis, stop the installed instance first and
confirm it is `STOPPED`. Then run the service in its explicitly supported mode:

```powershell
sc.exe stop UltraSearchService
sc.exe queryex UltraSearchService
& "$app\service.exe" --console
```

Keep the console output. Exit that instance before restarting the installed
service. Do not run both against the same index directories.

Microsoft documents the dispatcher requirement and error 1063 in
[StartServiceCtrlDispatcher](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-startservicectrldispatcherw).

## Configuration and startup logs

Default paths are:

```text
%PROGRAMDATA%\UltraSearch\config\config.toml
%PROGRAMDATA%\UltraSearch\log\startup.log
```

The startup log records PID, configuration path, initialization stages, and the
full failure chain even when TOML cannot be parsed or normal logging cannot
start. It has bounded rotation. Admission to its dedicated writer is nonblocking;
normal exit waits only a short bounded interval for prior records, within the
remaining service deadline. A full queue, blocked disk, or forced timeout may
prevent a final record, so retain SCM exit codes as well.

After configuration loads, runtime logs use the configured `logging.file` path.
Use the actual path from that configuration. Onboarding reports configuration
save, reload, and rescan failures and remains open until service acknowledgments
succeed.

Mixed `/` and `\` separators in a displayed ordinary Win32 path are not, by
themselves, evidence of a path failure. Check the actual operation and error. The
October 27 path-formatting report did not establish a causal connection to the
disconnected UI.

## Regression coverage and evidence limits

The independent `installed-startup` Windows CI job builds all five binaries,
tests IPC/UI/service code, packages the release WiX definition, and runs:

```powershell
./.github/scripts/windows-installed-startup.ps1 -DisposableHost `
  -MsiPath target/wix/installed-startup.msi -BinaryDir target/debug `
  -EvidenceDir "$env:RUNNER_TEMP/ultrasearch-installed-startup"
```

Use only an elevated, disposable Windows host with no existing UltraSearch
installation or ProgramData directory. The script refuses conflicting service,
installer, directory, and evidence state. It creates an isolated NTFS VHD and
USN journal, installs the real MSI, verifies service identity and installed
binary hashes, and searches for files created before and after service startup.
It also exercises an ordinary local user's IPC access, service restart, invalid
configuration, and the expected bare-executable 1063 case.

The installed test initially sets a closed deep-idle policy and requires both
baseline and journal filename hits while body search remains empty. It then
reloads an open policy and requires the unique body token to become searchable
without losing filename results. This is an actual idle-policy test when run
on Windows; it does not emulate physical battery hardware. Portable scheduler
regressions separately exercise battery observations and revoked admission.

Cleanup is limited to the test's own MSI, local-user SID, and attached VHD. It
preserves diagnostic evidence and reports cleanup failures. The GUI check covers
process liveness through restart. UI state tests and the real Windows pipe
adapter cover connection/readiness behavior; this is not a rendered-row GUI test.

A Linux compiler check or Windows cross-target compiler check is not native
Windows execution. The Windows branches and installed-MSI script count as
verified only when an actual Windows run completes successfully. Preserve its
result JSON, command outputs, MSI logs, and ProgramData logs.

## What the October 27 investigation established

- The published `v1.5.1` source performed initial scanning before binding its
  query endpoint. Current `main` had already moved endpoint readiness ahead of
  background ingestion before this investigation. The reports do not identify
  an exact installed build, so this is a relevant release difference, not proof
  of which source the reporter ran.
- The UI used a 750 ms transport budget for a request that allowed five seconds
  of query execution. It also delayed its first status poll, failed to reset
  backoff consistently, and did not refresh every already-entered query when
  index visibility changed. Those are concrete client defects.
- Initial filename reconciliation was coupled to content-worker admission.
  Battery or deep-idle restrictions could therefore leave known filenames
  unavailable even after a pipe was ready. Filename transactions and deferred
  content enrichment now have separate admission behavior, while retaining
  paired commit receipts and journal coverage checks.
- MSI service names, SCM host identity, and worker placement agree. No service
  registration mismatch was demonstrated by these reports.
- Direct interactive error 1063 is expected. It must not be treated as a
  reproduced installed-service failure.

Source commits on `main` do not update an already installed MSI. Diagnose the
installed version and test a build containing the fixes before declaring the
reporter's machine recovered.
