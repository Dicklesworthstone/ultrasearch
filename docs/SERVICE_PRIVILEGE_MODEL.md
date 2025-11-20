# UltraSearch Service Privilege Model & Hardening (c00.2.5)

## Goals
- Access NTFS MFT + USN journals efficiently while minimizing attack surface.
- Prevent DLL hijacking / path abuse for the service and worker binaries.
- Keep UI unprivileged; only the service holds elevated rights.

## Service identity
- Run `searchd` as **LocalSystem** or a dedicated service account granted only the required rights.
- Required privileges: `SE_BACKUP_NAME`, `SE_RESTORE_NAME` for raw volume access and change journal operations.
- UI (`search-ui`) and CLI run as the current user and communicate via named pipes; no elevation needed.

## Access control & filesystem layout
- Program data root: `%PROGRAMDATA%\\UltraSearch` with ACLs granting:
  - Service account: Full control.
  - Administrators: Full control.
  - Users: Read/execute on binaries only; no write on service/worker paths.
- Lock down plugin/component directories to avoid DLL hijacking:
  - Service/worker load paths are restricted to signed/owned locations under `%PROGRAMDATA%\\UltraSearch\\bin`.
  - Do not prepend/append user-writable paths to `PATH`.

## Service process hardening
- Set process priority: `NORMAL_PRIORITY_CLASS`; worker runs `BELOW_NORMAL` or `IDLE`.
- Consider job object caps for worker processes (memory/CPU) to contain runaway extraction workloads.
- Disable `PROCESS_MODE_BACKGROUND_BEGIN` to avoid working-set clamp per plan §7.4.

## Named pipe security
- Create pipes with ACLs granting:
  - Service account: Full control.
  - Authenticated Users: `READ`/`WRITE` only (no `CREATE`/`CHANGE_PERMISSIONS`), sufficient for client IPC.
  - Deny `Everyone`/`ANONYMOUS LOGON`.
- Enforce protocol version handshake; reject unknown versions early.
- Example SDDL for the pipe: `O:SYG:SYD:(A;;0x12019f;;;AU)(A;;FA;;;SY)` (system full control, authenticated users RW).

## Installation / deployment notes
- Service install must set the service account and privileges explicitly (no reliance on defaults).
- After install, verify:
  - ACLs on `%PROGRAMDATA%\\UltraSearch` (no user write to bin/components).
  - Service binary path and working directory are not user-writable.
  - Change journal operations succeed on NTFS volumes (journal ID check).
- Keep installer from altering global `PATH`; add private bin dir to the service process environment only.
- Record the service account SID and store it for ACL creation (pipes, programdata, components).

## Failure handling
- If privileges are insufficient (e.g., `ERROR_ACCESS_DENIED` from journal APIs):
  - Surface a clear status via IPC/metrics and the UI.
  - Do **not** attempt to auto-elevate; require admin action.
- On suspected DLL hijack risk (unexpected writable path in load order):
  - Log a high-severity event and fail fast rather than running with unsafe search paths.

## TODO (implementation)
- Add installer checks that assert required privileges and ACLs.
- Add runtime self-check to validate pipe ACLs and program data ACLs on startup.
- Add diagnostic status surface through IPC/metrics for privilege/ACL issues.
- Add a CLI `searchd doctor privileges` that:
  - Calls `OpenProcessToken`/`GetTokenInformation` to list effective privileges.
  - Attempts a harmless `FSCTL_QUERY_USN_JOURNAL` on a test volume and reports the result.
  - Prints expected vs actual SDDL for the pipe and programdata root.
