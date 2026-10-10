# Service Privilege Model & Hardening

## Service Account

The UltraSearch service (`ultrasearch-service.exe`) runs in its own process and
requires access to selected NTFS volumes and their existing USN journals.
The expected account is `LocalSystem` (`NT AUTHORITY\SYSTEM`). A dedicated
account must independently satisfy volume access and privilege requirements;
group membership alone is not evidence that native ingestion succeeds.

### Backup privilege activation

Windows ingestion calls `ntfs_watcher::enable_backup_privilege()` before the
durable ingestion lane discovers volumes or reads MFT/journal records. Bootstrap
may already have discovered volume names to initialize selection defaults.
The helper opens its own process token with
`TOKEN_ADJUST_PRIVILEGES` and enables only `SeBackupPrivilege`, which must already
be present. LocalSystem has this privilege disabled by default. The reader's
`FILE_FLAG_BACKUP_SEMANTICS` opens can override file read ACLs only when the
appropriate privilege is enabled.

`AdjustTokenPrivileges` cannot grant a missing privilege. The helper checks both
the API result and its immediately captured last error; a successful BOOL with
`ERROR_NOT_ALL_ASSIGNED` is a startup failure. Ingestion reports the failure and
keeps existing volume results hidden. It never substitutes a healthy empty scan
for inaccessible metadata, and it never changes account rights or file ACLs.
No restore, debug, or manage-volume privilege is enabled by this initialization.

Activation lasts for the dedicated service process lifetime. MFT readers move
between blocking worker threads, so restoring the shared process token after
individual calls would race other active reads. Ingestion threads must not
impersonate clients or later disable the privilege. Adding impersonation would
require scoped thread-token handling for each native batch; process-token
activation does not override an impersonation token.

This prerequisite does not guarantee access to every device or full-text content
for every file. Content reads retain their verification and omission/retry
policy; unresolved journal metadata remains an explicit volume error. See
[incremental indexing and recovery](INCREMENTAL_INDEXING.md) for operational
status, deferred work, and native acceptance commands.

Microsoft references:

- [LocalSystem account privileges](https://learn.microsoft.com/en-us/windows/win32/services/localsystem-account)
- [OpenFileById access and backup semantics](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-openfilebyid)
- [AdjustTokenPrivileges result and assignment semantics](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-adjusttokenprivileges)

## File System ACLs
The service stores data in `%PROGRAMDATA%\UltraSearch`.

### Security Posture
*   **Index Data:** Contains sensitive file metadata (names, paths, potentially snippets). Must be protected.
*   **Logs:** Operational logs.
*   **Config:** Service configuration.

### Recommended ACLs for `%PROGRAMDATA%\UltraSearch`
*   **SYSTEM**: Full Control
*   **Administrators**: Full Control
*   **UltraSearch Service Account**: Full Control
*   **Users**: **Read-Only** (or **No Access** if strict privacy is required).
    *   If `Users` have Read access, any local user can read the index and potentially infer file existence.
    *   Ideally, the IPC pipe enforces access control for search queries, and the raw index files are locked down (System/Admin only).

## Named Pipe Hardening
The IPC pipe `\\.\pipe\ultrasearch` is the primary attack surface for local privilege escalation or information disclosure.

### Access Control
The pipe security descriptor should allow:
*   **Connect/Read/Write:**
    *   SYSTEM
    *   Administrators
    *   Authenticated Users (if we allow any user to search).
*   **Deny:** Network access (unless explicitly configured).

### Validation
*   The service validates IPC requests.
*   Input sizes are capped (max frame size).
*   Deserialization is robust (bincode with limits).

## DLL Hijacking Prevention
*   The service executable should be installed in a secure location (e.g., `%ProgramFiles%\UltraSearch`).
*   ACLs on the install directory must prevent non-admins from writing/modifying files (standard Program Files behavior).
*   When loading DLLs (e.g., `extractous` dependencies), specify absolute paths or ensure the search order is safe.

## Installation Requirements
1.  Copy binaries to `%ProgramFiles%\UltraSearch`.
2.  Register service:
    ```powershell
    sc.exe create "UltraSearchService" binPath= "C:\Program Files\UltraSearch\ultrasearch-service.exe" start= auto type= own
    ```
3.  Ensure directory exists and ACLs are set:
    ```powershell
    $data = "C:\ProgramData\UltraSearch"
    New-Item -ItemType Directory -Force -Path $data
    $acl = Get-Acl $data
    # Disable inheritance and restrict to Admin/System if needed...
    ```
