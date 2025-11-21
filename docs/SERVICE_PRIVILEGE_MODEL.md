# Service Privilege Model & Hardening

## Service Account
The UltraSearch service (`ultrasearch-service.exe`) must run with high privileges to function correctly as a system-wide indexer.

### Required Privileges
*   **SeBackupPrivilege**: Required to open files and directories for indexing regardless of discretionary access control lists (DACLs). This allows indexing user content without changing permissions.
*   **SeRestorePrivilege**: Often paired with Backup; technically not needed for read-only indexing but standard for backup operators.
*   **SeManageVolumePrivilege**: Required for opening volume handles and utilizing USN journal controls (FSCTL_READ_USN_JOURNAL).
*   **SeDebugPrivilege**: Useful for obtaining process handles if needed for priority tuning, though typically not strictly required for file I/O.

**Recommended Account:** `LocalSystem` (NT AUTHORITY\SYSTEM).
*   Has all necessary privileges by default.
*   Has Full Control over the system volume.

*Alternative:* A dedicated Managed Service Account (MSA) or Virtual Account (e.g., `NT SERVICE\UltraSearch`) added to the **Backup Operators** group.

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
    sc.exe create "UltraSearch" binPath= "C:\Program Files\UltraSearch\ultrasearch-service.exe" start= auto type= own
    ```
3.  Ensure directory exists and ACLs are set:
    ```powershell
    $data = "C:\ProgramData\UltraSearch"
    New-Item -ItemType Directory -Force -Path $data
    $acl = Get-Acl $data
    # Disable inheritance and restrict to Admin/System if needed...
    ```