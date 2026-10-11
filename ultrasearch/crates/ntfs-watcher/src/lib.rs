//! NTFS integration layer: volume discovery, MFT enumeration, and USN tailing.
//!
//! Each journal call reads at most one bounded buffer and returns a cursor for
//! the first unread record. Callers must durably apply the returned events
//! before saving that cursor. Unsupported platforms and unavailable journals
//! return errors, rather than appearing to be healthy, idle watchers.

use core_types::{DocKey, FileMeta, VolumeId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type Usn = u64;

/// Static information about a mounted NTFS volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Small runtime identifier assigned by the service.
    pub id: VolumeId,
    /// Volume GUID path such as `\\?\Volume{...}\`.
    pub guid_path: String,
    /// Optional drive letters currently mapped to the volume.
    pub drive_letters: Vec<char>,
}

/// Stream of logical file-system events derived from the USN journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileEvent {
    Created(FileMeta),
    Deleted(DocKey),
    Modified(FileMeta),
    Renamed {
        from: DocKey,
        to: FileMeta,
    },
    /// An ordinary directory rename can invalidate descendants without giving
    /// them individual USN records. Both rename halves retain their recorded
    /// parent and name; these are historical components, not a resolved path.
    /// `current` is the directory's latest identity-checked metadata, or None
    /// when the record proves deletion or that full identity no longer exists.
    /// Reconcile affected descendants before replacing the directory itself,
    /// retaining indexed path anchors until every bounded page is committed.
    /// This event survives exclusions so moves into service-owned directories
    /// still remove previously indexed descendants.
    DirectoryRenamed {
        doc: DocKey,
        parent: DocKey,
        name: String,
        current: Option<FileMeta>,
    },
    AttributesChanged(FileMeta),
    /// Directory link/reparse history cannot be resolved as an ordinary rename.
    /// Reconcile the volume from a new pre-scan cursor.
    RescanRequired {
        doc: DocKey,
    },
    /// Ignore never-indexed service outputs, but remove an existing document
    /// (or reconcile descendants) when it moves into an excluded directory.
    Excluded {
        doc: DocKey,
        is_dir: bool,
    },
}

/// Configuration knobs for NTFS/USN access.
#[derive(Debug, Clone)]
pub struct ReaderConfig {
    /// Bytes in one kernel read. Must be between 4 KiB and 16 MiB.
    pub chunk_size: usize,
    /// Maximum raw records consumed, including records with no index action.
    pub max_records_per_tick: usize,
    /// Existing service-owned directories, canonicalized to volume GUID paths.
    /// Their journal records still advance the cursor but create no index work.
    pub exclude_paths: Vec<String>,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            chunk_size: 1 << 20,          // 1 MiB read buffer
            max_records_per_tick: 10_000, // reasonable default for service loop
            exclude_paths: Vec::new(),
        }
    }
}

impl ReaderConfig {
    #[cfg(any(windows, test))]
    fn validate(&self) -> Result<(), NtfsError> {
        if !(4096..=16 * 1024 * 1024).contains(&self.chunk_size) || self.max_records_per_tick == 0 {
            return Err(NtfsError::Journal(
                "chunk_size must be 4096..=16777216 and max_records_per_tick must be positive"
                    .into(),
            ));
        }
        if self.exclude_paths.iter().any(|path| path.trim().is_empty()) {
            return Err(NtfsError::Journal("empty exclusion path".into()));
        }
        Ok(())
    }
}

/// Cursor for resuming USN processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalCursor {
    /// The next unread USN, as returned by FSCTL_READ_USN_JOURNAL. This is not
    /// the USN of the last applied record and must never be incremented by one.
    pub last_usn: Usn,
    pub journal_id: u64,
}

/// A bounded read with an explicit observed-head indication. Checkpoint writes
/// can themselves append to the journal, so absolute inactivity is not a valid
/// condition for deciding whether an index has caught up.
#[derive(Debug, Clone)]
pub struct JournalBatch {
    pub events: Vec<FileEvent>,
    pub cursor: JournalCursor,
    /// All records through the head captured before this read were consumed.
    pub caught_up: bool,
}

/// A pull-based baseline scan bound to one volume handle and journal incarnation.
/// The caller must apply each returned batch before requesting another one and
/// keep the volume hidden until enumeration and journal catch-up both complete.
#[derive(Debug)]
pub struct MftScan {
    cursor: JournalCursor,
    #[cfg(windows)]
    native: native::MftScan,
}

impl MftScan {
    /// The journal head captured before the first MFT read. Replay from this
    /// position after the complete baseline has been committed.
    pub fn journal_cursor(&self) -> JournalCursor {
        self.cursor
    }

    /// Consume at most the configured number of raw MFT records, issuing no
    /// more than one fixed-size kernel read. An empty `Some` batch is progress
    /// through excluded or disappeared files; only `None` means validated EOF.
    ///
    /// Errors invalidate this scan. Start a new reconciliation rather than
    /// treating an error or cancellation as successful partial enumeration.
    #[cfg(windows)]
    pub fn next_batch(&mut self) -> Result<Option<Vec<FileMeta>>, NtfsError> {
        self.native.next_batch(self.cursor)
    }

    #[cfg(not(windows))]
    pub fn next_batch(&mut self) -> Result<Option<Vec<FileMeta>>, NtfsError> {
        Err(NtfsError::NotSupported)
    }
}

/// Errors that can surface while interacting with NTFS / USN APIs.
#[derive(Debug, Error)]
pub enum NtfsError {
    #[error("NTFS privilege initialization failed: {0}")]
    Privilege(String),
    #[error("volume discovery failed: {0}")]
    Discovery(String),
    #[error("usn journal error: {0}")]
    Journal(String),
    #[error("usn gap detected")]
    GapDetected,
    #[error("mft enumeration failed: {0}")]
    Mft(String),
    #[error("operation not supported on this platform")]
    NotSupported,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Enable the backup privilege already assigned to this process's token.
///
/// Call once before starting production ingestion in the dedicated service
/// process. LocalSystem holds this privilege but starts with it disabled;
/// `FILE_FLAG_BACKUP_SEMANTICS` alone cannot bypass file read ACLs. This never
/// grants a privilege, changes an ACL, or enables restore/debug privileges.
/// Missing privileges are an initialization error, not partial scan coverage.
///
/// Activation intentionally lasts for the process lifetime. MFT scans move
/// between blocking threads, so enabling/disabling the shared process token
/// around individual calls would race other scans. Callers must not impersonate
/// clients on ingestion threads or later disable the privilege. Supporting such
/// token changes would require scoped thread tokens on every native batch;
/// merely enabling this process token would not affect an impersonation token.
#[cfg(windows)]
pub fn enable_backup_privilege() -> Result<(), NtfsError> {
    native::enable_process_backup_privilege()
}

#[cfg(not(windows))]
pub fn enable_backup_privilege() -> Result<(), NtfsError> {
    Err(NtfsError::NotSupported)
}

#[cfg(any(windows, test))]
fn check_backup_privilege_adjustment(succeeded: bool, last_error: u32) -> Result<(), NtfsError> {
    // AdjustTokenPrivileges can return TRUE while changing no privilege at all.
    // ERROR_NOT_ALL_ASSIGNED must not be treated as successful initialization.
    const ERROR_SUCCESS: u32 = 0;
    const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;
    if last_error == ERROR_NOT_ALL_ASSIGNED {
        return Err(NtfsError::Privilege(
            "SeBackupPrivilege is absent from the access token (Win32 error 1300)".into(),
        ));
    }
    if !succeeded || last_error != ERROR_SUCCESS {
        return Err(NtfsError::Privilege(format!(
            "AdjustTokenPrivileges(SeBackupPrivilege) failed (Win32 error {last_error})"
        )));
    }
    Ok(())
}

/// Trait abstraction to make the platform-specific implementation swap-able in tests.
pub trait NtfsWatcher {
    /// Discover NTFS volumes.
    fn discover_volumes(&self) -> Result<Vec<VolumeInfo>, NtfsError>;

    /// Return an in-memory MFT fixture. Production enumeration uses `MftScan`.
    fn enumerate_mft(&self, volume: &VolumeInfo) -> Result<Vec<FileMeta>, NtfsError>;

    /// Tail the USN journal starting at the given cursor.
    fn tail_usn(
        &self,
        volume: &VolumeInfo,
        cursor: JournalCursor,
    ) -> Result<(Vec<FileEvent>, JournalCursor), NtfsError>;
}

/// Discover NTFS volumes available on the machine.
#[cfg(windows)]
pub fn discover_volumes() -> Result<Vec<VolumeInfo>, NtfsError> {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    use tracing::warn;
    use windows::Win32::Storage::FileSystem::{
        GetLogicalDrives, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    };
    use windows::core::PCWSTR;

    let mut map: HashMap<String, Vec<char>> = HashMap::new();
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Err(NtfsError::Discovery("GetLogicalDrives returned 0".into()));
    }

    for i in 0..26 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let mut root_wide: Vec<u16> = OsString::from(&root).encode_wide().collect();
        root_wide.push(0);

        let mut fs_name = [0u16; 32];
        let mut serial = 0u32;
        let mut max_comp = 0u32;
        let mut flags = 0u32;
        let vol_info = unsafe {
            GetVolumeInformationW(
                PCWSTR(root_wide.as_ptr()),
                None,
                Some(&mut serial as *mut _),
                Some(&mut max_comp as *mut _),
                Some(&mut flags as *mut _),
                Some(&mut fs_name),
            )
        };
        if let Err(e) = vol_info {
            warn!("GetVolumeInformationW failed for {root}: {e}");
            continue;
        }
        let fs = String::from_utf16_lossy(&fs_name)
            .trim_end_matches('\0')
            .to_string();
        if !fs.eq_ignore_ascii_case("ntfs") {
            continue;
        }

        let mut guid_buf = [0u16; 64];
        let ok =
            unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root_wide.as_ptr()), &mut guid_buf) };
        if let Err(e) = ok {
            warn!("GetVolumeNameForVolumeMountPointW failed for {root}: {e}");
            continue;
        }
        let guid = String::from_utf16_lossy(&guid_buf)
            .trim_end_matches('\0')
            .to_string();

        map.entry(guid).or_default().push(letter);
    }

    // Ensure stable ordering of volume IDs by sorting GUID path before assigning IDs.
    let mut entries: Vec<(String, Vec<char>)> = map.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let vols: Vec<VolumeInfo> = entries
        .into_iter()
        .enumerate()
        .map(|(idx, (guid_path, mut drive_letters))| {
            drive_letters.sort_unstable();
            VolumeInfo {
                id: (idx + 1) as VolumeId,
                guid_path,
                drive_letters,
            }
        })
        .collect();
    Ok(vols)
}

#[cfg(not(windows))]
pub fn discover_volumes() -> Result<Vec<VolumeInfo>, NtfsError> {
    Err(NtfsError::NotSupported)
}

/// Open a volume handle with read access and permissive sharing (Windows only).
#[cfg(windows)]
pub fn open_volume_handle(
    volume: &VolumeInfo,
) -> Result<std::os::windows::io::OwnedHandle, NtfsError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::core::PCWSTR;

    // A trailing slash opens the volume's root directory, not the volume
    // device required by FSCTL_QUERY_USN_JOURNAL / FSCTL_READ_USN_JOURNAL.
    let mut path_w: Vec<u16> = OsString::from(volume.guid_path.trim_end_matches('\\'))
        .encode_wide()
        .collect();
    path_w.push(0);

    let handle = unsafe {
        CreateFileW(
            PCWSTR(path_w.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(|e| {
        NtfsError::Discovery(format!("CreateFileW failed for {}: {e}", volume.guid_path))
    })?;

    let raw: RawHandle = handle.0 as RawHandle;
    // SAFETY: handle is valid (error already handled) and ownership is transferred.
    let owned = unsafe { OwnedHandle::from_raw_handle(raw) };
    Ok(owned)
}

/// Begin a bounded baseline scan, capturing the journal head on the same GUID
/// handle before any MFT enumeration. No file metadata is materialized here.
#[cfg(windows)]
pub fn begin_mft_scan(volume: &VolumeInfo, config: &ReaderConfig) -> Result<MftScan, NtfsError> {
    config.validate()?;
    let handle = open_volume_handle(volume)?;
    let state = native::query_state(&handle)?;
    Ok(MftScan {
        cursor: JournalCursor {
            last_usn: state.next_usn,
            journal_id: state.journal_id,
        },
        native: native::MftScan::new(handle, volume.id, config.clone()),
    })
}

#[cfg(not(windows))]
pub fn begin_mft_scan(_volume: &VolumeInfo, _config: &ReaderConfig) -> Result<MftScan, NtfsError> {
    Err(NtfsError::NotSupported)
}

/// Resolve an existing file or directory to its current volume GUID path.
/// The result remains bound to the volume if drive letters are reassigned.
#[cfg(windows)]
pub fn canonical_path(path: &std::path::Path) -> Result<String, NtfsError> {
    native::canonical_path(path)
}

#[cfg(not(windows))]
pub fn canonical_path(_path: &std::path::Path) -> Result<String, NtfsError> {
    Err(NtfsError::NotSupported)
}

/// Resolve one bounded set of full file identities to their current metadata.
/// Results preserve input order. A missing identity or currently excluded path
/// produces None; access failures and other unresolved state return an error
/// for the entire batch. No cursor is advanced by this read-only operation.
///
/// The caller must bind `volume` to the saved cursor's GUID. The same volume
/// handle validates journal identity and retention before and after resolution;
/// a gap requires a fresh baseline. Parent identities are left unknown because
/// these probes have no historical parent record. Callers must commit each page
/// before resolving another, including descendant repairs after a rename.
#[cfg(windows)]
pub fn resolve_file_ids(
    volume: &VolumeInfo,
    cursor: JournalCursor,
    keys: &[DocKey],
    config: &ReaderConfig,
) -> Result<Vec<Option<FileMeta>>, NtfsError> {
    journal::validate_file_ids(volume.id, keys, config)?;
    let handle = open_volume_handle(volume)?;
    journal::validate_cursor(cursor, native::query_state(&handle)?)?;
    let resolved = journal::resolve_file_ids(volume.id, keys, config, |key| {
        native::resolve_metadata(
            &handle,
            volume.id,
            &journal::Record {
                frn: key.file_id(),
                parent_frn: 0,
                usn: cursor.last_usn,
                reason: 0,
                attributes: 0,
                name: String::new(),
            },
        )
    })?;
    journal::validate_cursor(cursor, native::query_state(&handle)?)?;
    Ok(resolved)
}

#[cfg(not(windows))]
pub fn resolve_file_ids(
    _volume: &VolumeInfo,
    _cursor: JournalCursor,
    _keys: &[DocKey],
    _config: &ReaderConfig,
) -> Result<Vec<Option<FileMeta>>, NtfsError> {
    Err(NtfsError::NotSupported)
}

/// Capture the current journal identity and end position **before** an MFT
/// scan. Replaying from this cursor afterward covers changes during the scan.
#[cfg(windows)]
pub fn query_journal(volume: &VolumeInfo) -> Result<JournalCursor, NtfsError> {
    let handle = open_volume_handle(volume)?;
    let state = native::query_state(&handle)?;
    Ok(JournalCursor {
        last_usn: state.next_usn,
        journal_id: state.journal_id,
    })
}

#[cfg(not(windows))]
pub fn query_journal(_volume: &VolumeInfo) -> Result<JournalCursor, NtfsError> {
    Err(NtfsError::NotSupported)
}

/// Tail one bounded, non-waiting USN batch using the default reader limits.
pub fn tail_usn(
    volume: &VolumeInfo,
    cursor: JournalCursor,
) -> Result<(Vec<FileEvent>, JournalCursor), NtfsError> {
    tail_usn_with_config(volume, cursor, &ReaderConfig::default())
}

/// Read at most `chunk_size` bytes and consume at most
/// `max_records_per_tick` raw records. An empty batch can still advance the
/// cursor past records that do not affect indexed state.
///
/// The volume must be the same GUID that was bound to the saved cursor. No
/// journal is created or reset by this read-only API. Gaps and journal identity
/// changes require a new baseline; errors never acknowledge any records.
pub fn tail_usn_with_config(
    volume: &VolumeInfo,
    cursor: JournalCursor,
    config: &ReaderConfig,
) -> Result<(Vec<FileEvent>, JournalCursor), NtfsError> {
    let batch = tail_usn_batch_with_config(volume, cursor, config)?;
    Ok((batch.events, batch.cursor))
}

/// Read one bounded batch and report whether its starting journal head was
/// reached. Excluded raw records count toward the tick limit and cursor.
#[cfg(windows)]
pub fn tail_usn_batch_with_config(
    volume: &VolumeInfo,
    cursor: JournalCursor,
    config: &ReaderConfig,
) -> Result<JournalBatch, NtfsError> {
    config.validate()?;
    let handle = open_volume_handle(volume)?;
    let before = native::query_state(&handle)?;
    journal::validate_cursor(cursor, before)?;
    let bytes = native::read_batch(&handle, cursor, config.chunk_size)?;
    let (events, next) = journal::resolve_batch(
        volume.id,
        &bytes,
        cursor,
        config.max_records_per_tick,
        |record| native::resolve_metadata(&handle, volume.id, record),
        |record, event| {
            native::excluded_event(&handle, volume.id, record, event, &config.exclude_paths)
        },
    )?;
    // A reset/wrap during metadata resolution must not be acknowledged as a
    // complete batch. Validate the original position, not only the new one.
    let after = native::query_state(&handle)?;
    journal::validate_cursor(cursor, after)?;
    journal::validate_cursor(next, after)?;
    Ok(JournalBatch {
        events,
        cursor: next,
        caught_up: next.last_usn >= before.next_usn,
    })
}

#[cfg(not(windows))]
pub fn tail_usn_batch_with_config(
    _volume: &VolumeInfo,
    _cursor: JournalCursor,
    _config: &ReaderConfig,
) -> Result<JournalBatch, NtfsError> {
    Err(NtfsError::NotSupported)
}

#[cfg(any(windows, test))]
mod journal {
    use super::*;

    pub(super) const REASON_CREATE: u32 = 0x0000_0100;
    pub(super) const REASON_DELETE: u32 = 0x0000_0200;
    pub(super) const REASON_RENAME_OLD: u32 = 0x0000_1000;
    pub(super) const REASON_RENAME_NEW: u32 = 0x0000_2000;
    pub(super) const REASON_HARD_LINK: u32 = 0x0001_0000;
    pub(super) const REASON_REPARSE: u32 = 0x0010_0000;
    pub(super) const REASON_TRANSACTED: u32 = 0x0040_0000;
    // TxF commits can identify changed stream data with TRANSACTED_CHANGE;
    // consuming that record without refreshing content loses the committed edit.
    // https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v2
    pub(super) const REASON_CONTENT: u32 = 0x0020_0077 | REASON_TRANSACTED;
    pub(super) const REASON_ATTRIBUTES: u32 = 0x001F_CC00;
    #[cfg(test)]
    pub(super) const REASON_CLOSE: u32 = 0x8000_0000;
    pub(super) const ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    pub(super) const ATTRIBUTE_REPARSE: u32 = 0x0000_0400;

    pub(super) fn excluded_path(path: &str, exclusions: &[String]) -> bool {
        let path = path.replace('/', "\\").to_lowercase();
        exclusions.iter().any(|excluded| {
            let excluded = excluded.replace('/', "\\").to_lowercase();
            let excluded = excluded.trim_end_matches('\\');
            !excluded.is_empty()
                && (path == excluded
                    || path
                        .strip_prefix(excluded)
                        .is_some_and(|suffix| suffix.starts_with('\\')))
        })
    }

    #[derive(Debug, Clone, Copy)]
    pub(super) struct JournalState {
        pub journal_id: u64,
        pub first_usn: Usn,
        pub lowest_valid_usn: Usn,
        pub next_usn: Usn,
    }

    pub(super) fn validate_cursor(
        cursor: JournalCursor,
        state: JournalState,
    ) -> Result<(), NtfsError> {
        if cursor.journal_id != state.journal_id
            || cursor.last_usn < state.first_usn.max(state.lowest_valid_usn)
            || cursor.last_usn > state.next_usn
        {
            return Err(NtfsError::GapDetected);
        }
        Ok(())
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Record {
        pub frn: u64,
        pub parent_frn: u64,
        pub usn: Usn,
        pub reason: u32,
        pub attributes: u32,
        pub name: String,
    }

    fn malformed(reason: &str) -> NtfsError {
        NtfsError::Journal(format!("invalid USN buffer: {reason}"))
    }

    fn bytes<const N: usize>(buffer: &[u8], offset: usize) -> Result<[u8; N], NtfsError> {
        buffer
            .get(offset..offset.saturating_add(N))
            .and_then(|part| part.try_into().ok())
            .ok_or_else(|| malformed("truncated field"))
    }

    fn usn(buffer: &[u8], offset: usize) -> Result<Usn, NtfsError> {
        u64::try_from(i64::from_le_bytes(bytes(buffer, offset)?))
            .map_err(|_| malformed("negative USN"))
    }

    pub(super) fn parse_record(buffer: &[u8]) -> Result<(Record, usize), NtfsError> {
        // USN_RECORD_V2 has a 60-byte fixed prefix. Parse fields as little-
        // endian bytes instead of casting an unaligned kernel buffer.
        let length = u32::from_le_bytes(bytes(buffer, 0)?) as usize;
        if length < 60 || !length.is_multiple_of(8) || length > buffer.len() {
            return Err(malformed("invalid record length"));
        }
        let record = &buffer[..length];
        if u16::from_le_bytes(bytes(record, 4)?) != 2 {
            return Err(malformed("unsupported record version; expected V2"));
        }
        let name_len = u16::from_le_bytes(bytes(record, 56)?) as usize;
        let name_offset = u16::from_le_bytes(bytes(record, 58)?) as usize;
        if !name_len.is_multiple_of(2) || name_offset < 60 || !name_offset.is_multiple_of(2) {
            return Err(malformed("invalid filename bounds"));
        }
        let name_bytes = record
            .get(name_offset..name_offset.saturating_add(name_len))
            .ok_or_else(|| malformed("filename extends beyond record"))?;
        let name_utf16: Vec<u16> = name_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let name = String::from_utf16(&name_utf16)
            .map_err(|_| malformed("filename cannot be represented as Unicode"))?;
        Ok((
            Record {
                frn: u64::from_le_bytes(bytes(record, 8)?),
                parent_frn: u64::from_le_bytes(bytes(record, 16)?),
                usn: usn(record, 24)?,
                reason: u32::from_le_bytes(bytes(record, 40)?),
                attributes: u32::from_le_bytes(bytes(record, 52)?),
                name,
            },
            length,
        ))
    }

    pub(super) fn parse_batch(
        buffer: &[u8],
        cursor: JournalCursor,
        max_records: usize,
    ) -> Result<(Vec<Record>, JournalCursor), NtfsError> {
        if max_records == 0 {
            return Err(malformed("zero record limit"));
        }
        let mut next_usn = usn(buffer, 0)?;
        if next_usn < cursor.last_usn {
            return Err(malformed("next USN moved backwards"));
        }
        let mut offset = 8;
        let mut records = Vec::new();
        let mut previous = None;
        while offset < buffer.len() {
            let (record, length) = parse_record(&buffer[offset..])?;
            if record.usn < cursor.last_usn
                || record.usn >= next_usn
                || previous.is_some_and(|old| record.usn <= old)
            {
                return Err(malformed("record USNs are outside the batch or unordered"));
            }
            if records.len() == max_records {
                // The kernel's header points past *all* records in its buffer.
                // Use the first record we did not consume so limiting a tick
                // never discards the unread suffix or half of a rename pair.
                next_usn = record.usn;
                break;
            }
            previous = Some(record.usn);
            records.push(record);
            offset += length;
        }
        Ok((
            records,
            JournalCursor {
                last_usn: next_usn,
                journal_id: cursor.journal_id,
            },
        ))
    }

    /// A partial IOCTL result must contain complete records throughout its
    /// fixed-size buffer. A truncated suffix has no safe resume position.
    pub(super) fn validate_partial_output(
        buffer: &[u8],
        cursor: JournalCursor,
    ) -> Result<(), NtfsError> {
        if buffer.len() <= 8 {
            return Err(malformed("overflow returned no complete journal records"));
        }
        // Every record occupies multiple bytes, so this validates the entire
        // already bounded buffer before applying the caller's record budget.
        let (records, next) = parse_batch(buffer, cursor, buffer.len())?;
        if records.is_empty() || next.last_usn <= cursor.last_usn {
            return Err(malformed("overflow made no journal progress"));
        }
        Ok(())
    }

    /// Resolve a bounded raw batch atomically from the caller's perspective.
    /// A failed metadata or exclusion probe yields neither a partial event set
    /// nor a cursor that could acknowledge an unresolved file. The caller can
    /// replay the same input position after access is restored.
    pub(super) fn resolve_batch(
        volume: VolumeId,
        buffer: &[u8],
        cursor: JournalCursor,
        max_records: usize,
        mut metadata: impl FnMut(&Record) -> Result<Option<FileMeta>, NtfsError>,
        mut excluded: impl FnMut(&Record, &FileEvent) -> Result<bool, NtfsError>,
    ) -> Result<(Vec<FileEvent>, JournalCursor), NtfsError> {
        let (records, next) = parse_batch(buffer, cursor, max_records)?;
        let mut events = Vec::with_capacity(records.len());
        for record in records {
            if let Some(event) = event_from_record(volume, &record, || metadata(&record))? {
                // An excluded destination cannot erase a rename's historical
                // path anchors: previously indexed descendants still need
                // removal. The service resolves exclusions during repair.
                if !matches!(&event, FileEvent::DirectoryRenamed { .. })
                    && excluded(&record, &event)?
                {
                    events.push(FileEvent::Excluded {
                        doc: DocKey::from_parts(volume, record.frn),
                        is_dir: record.attributes & ATTRIBUTE_DIRECTORY != 0,
                    });
                } else {
                    events.push(event);
                }
            }
        }
        Ok((events, next))
    }

    pub(super) fn event_from_record(
        volume: VolumeId,
        record: &Record,
        metadata: impl FnOnce() -> Result<Option<FileMeta>, NtfsError>,
    ) -> Result<Option<FileEvent>, NtfsError> {
        let doc = DocKey::from_parts(volume, record.frn);
        let directory = record.attributes & ATTRIBUTE_DIRECTORY != 0;
        let directory_rename =
            directory && record.reason & (REASON_RENAME_OLD | REASON_RENAME_NEW) != 0;
        if directory
            && (record.reason & (REASON_HARD_LINK | REASON_REPARSE) != 0
                || ((directory_rename || record.reason & REASON_DELETE != 0)
                    && record.attributes & ATTRIBUTE_REPARSE != 0))
        {
            return Ok(Some(FileEvent::RescanRequired { doc }));
        }
        // Reasons accumulate until close. Deletion wins over create/modify in
        // a combined record; a historical create must never resurrect a file.
        // Ordinary directory removal deletes an empty directory. Recursive
        // removal produces each child's own deletion/move records, so the
        // directory's tombstone must not reset an otherwise valid volume scan.
        // Keep structural/reparse histories conservative in the branch above.
        // https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-removedirectoryw
        if !directory_rename && record.reason & REASON_DELETE != 0 {
            return Ok(Some(FileEvent::Deleted(doc)));
        }
        let indexed_reasons =
            REASON_CREATE | REASON_RENAME_NEW | REASON_CONTENT | REASON_ATTRIBUTES;
        if !directory_rename && record.reason & indexed_reasons == 0 {
            // A file's lone OLD_NAME is completed by NEW_NAME across ticks.
            // A directory's OLD_NAME must retain its descendant path anchor.
            // CLOSE alone, without an accumulated reason, changes no state.
            return Ok(None);
        }
        let current = if record.reason & REASON_DELETE != 0 {
            None
        } else {
            metadata()?
        };
        if let Some(meta) = &current {
            validate_metadata_identity(meta, doc)?;
        }
        if directory_rename {
            if let Some(meta) = &current {
                if !meta.flags.contains(core_types::FileFlags::IS_DIR) {
                    return Err(NtfsError::Journal(
                        "resolved directory identity is not a directory".into(),
                    ));
                }
                if meta.flags.contains(core_types::FileFlags::REPARSE) {
                    return Ok(Some(FileEvent::RescanRequired { doc }));
                }
            }
            // Rename records carry old/new parent and name components, while
            // OpenFileById observes the current namespace. Preserve both; do
            // not invent a historical path from a parent's present-day path.
            // https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v2
            return Ok(Some(FileEvent::DirectoryRenamed {
                doc,
                parent: DocKey::from_parts(volume, record.parent_frn),
                name: record.name.clone(),
                current,
            }));
        }
        let Some(meta) = current else {
            // The record may outlive its file. Missing metadata means the full
            // identity no longer resolves, not that access happened to fail.
            // Access failures remain errors so this cursor cannot silently
            // discard a still-existing file or rely on another security event.
            return Ok(Some(FileEvent::Deleted(doc)));
        };
        if record.reason & REASON_RENAME_NEW != 0 {
            Ok(Some(FileEvent::Renamed {
                from: doc,
                to: meta,
            }))
        } else if record.reason & REASON_CREATE != 0 {
            Ok(Some(FileEvent::Created(meta)))
        } else if record.reason & REASON_CONTENT != 0 {
            Ok(Some(FileEvent::Modified(meta)))
        } else {
            Ok(Some(FileEvent::AttributesChanged(meta)))
        }
    }

    fn validate_metadata_identity(meta: &FileMeta, key: DocKey) -> Result<(), NtfsError> {
        if meta.key != key || meta.volume != key.volume() {
            return Err(NtfsError::Journal(
                "resolved metadata identity does not match requested file reference".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn validate_file_ids(
        volume: VolumeId,
        keys: &[DocKey],
        config: &ReaderConfig,
    ) -> Result<(), NtfsError> {
        config.validate()?;
        if keys.len() > config.max_records_per_tick {
            return Err(NtfsError::Journal(
                "file identity resolution exceeds the record limit".into(),
            ));
        }
        if keys.iter().any(|key| key.volume() != volume) {
            return Err(NtfsError::Journal(
                "file identity resolution spans multiple volumes".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn resolve_file_ids(
        volume: VolumeId,
        keys: &[DocKey],
        config: &ReaderConfig,
        mut metadata: impl FnMut(DocKey) -> Result<Option<FileMeta>, NtfsError>,
    ) -> Result<Vec<Option<FileMeta>>, NtfsError> {
        validate_file_ids(volume, keys, config)?;
        let mut resolved = Vec::with_capacity(keys.len());
        for &key in keys {
            let Some(mut meta) = metadata(key)? else {
                resolved.push(None);
                continue;
            };
            validate_metadata_identity(&meta, key)?;
            let path = meta.path.as_deref().ok_or_else(|| {
                NtfsError::Journal("resolved file identity has no current path".into())
            })?;
            if excluded_path(path, &config.exclude_paths) {
                resolved.push(None);
            } else {
                // The identity probe has no USN parent record. Do not expose
                // its placeholder as a real parent in descendant metadata.
                meta.parent = None;
                resolved.push(Some(meta));
            }
        }
        Ok(resolved)
    }
}

#[cfg(any(windows, test))]
mod mft {
    use super::*;

    #[derive(Debug)]
    struct Page {
        bytes: Vec<u8>,
        offset: usize,
        next_start: u64,
    }

    impl Page {
        fn new(bytes: Vec<u8>, start: u64) -> Result<Self, NtfsError> {
            let header = bytes
                .get(..8)
                .ok_or_else(|| NtfsError::Mft("missing next MFT position".into()))?;
            let next_start = u64::from_le_bytes(
                header
                    .try_into()
                    .map_err(|_| NtfsError::Mft("invalid next MFT position".into()))?,
            );
            if next_start <= start {
                return Err(NtfsError::Mft("MFT enumeration made no progress".into()));
            }
            // This header is an opaque enumeration ordinal, not a document
            // identity. Never derive a DocKey from it or replace full FRNs.
            Ok(Self {
                bytes,
                offset: 8,
                next_start,
            })
        }
    }

    fn parse_record(bytes: &[u8]) -> Result<(journal::Record, usize), NtfsError> {
        journal::parse_record(bytes)
            .map_err(|error| NtfsError::Mft(format!("invalid MFT record: {error}")))
    }

    /// Validate partial IOCTL output completely before retaining its next
    /// ordinal. MFT records are not ordered by their last-change USN or by the
    /// sequence bits in their full file references.
    pub(super) fn validate_partial_output(bytes: &[u8], start: u64) -> Result<(), NtfsError> {
        if bytes.len() <= 8 {
            return Err(NtfsError::Mft(
                "overflow returned no complete MFT records".into(),
            ));
        }
        let header = u64::from_le_bytes(
            bytes[..8]
                .try_into()
                .map_err(|_| NtfsError::Mft("invalid next MFT position".into()))?,
        );
        if header <= start {
            return Err(NtfsError::Mft("MFT enumeration made no progress".into()));
        }
        let mut offset = 8;
        while offset < bytes.len() {
            let (_, length) = parse_record(&bytes[offset..])?;
            offset += length;
        }
        Ok(())
    }

    /// Holds only one bounded raw page. Pulling the next batch never reads
    /// ahead of the current page or resolves beyond the raw-record budget.
    #[derive(Debug, Default)]
    pub(super) struct State {
        next_start: u64,
        page: Option<Page>,
        finished: bool,
        failed: bool,
    }

    impl State {
        pub(super) fn poison(&mut self) {
            self.failed = true;
        }

        pub(super) fn next_batch(
            &mut self,
            volume: VolumeId,
            config: &ReaderConfig,
            read: impl FnOnce(u64) -> Result<Option<Vec<u8>>, NtfsError>,
            resolve: impl FnMut(&journal::Record) -> Result<Option<FileMeta>, NtfsError>,
        ) -> Result<Option<Vec<FileMeta>>, NtfsError> {
            if self.failed {
                return Err(NtfsError::Mft(
                    "scan failed; a new baseline is required".into(),
                ));
            }
            let result = self.advance(volume, config, read, resolve);
            if result.is_err() {
                self.poison();
            }
            result
        }

        fn advance(
            &mut self,
            volume: VolumeId,
            config: &ReaderConfig,
            read: impl FnOnce(u64) -> Result<Option<Vec<u8>>, NtfsError>,
            mut resolve: impl FnMut(&journal::Record) -> Result<Option<FileMeta>, NtfsError>,
        ) -> Result<Option<Vec<FileMeta>>, NtfsError> {
            config.validate()?;
            if self.finished {
                return Ok(None);
            }
            if self.page.is_none() {
                let Some(bytes) = read(self.next_start)? else {
                    self.finished = true;
                    return Ok(None);
                };
                if bytes.len() > config.chunk_size {
                    return Err(NtfsError::Mft("MFT output exceeds its fixed buffer".into()));
                }
                self.page = Some(Page::new(bytes, self.next_start)?);
            }
            let page = self
                .page
                .as_mut()
                .ok_or_else(|| NtfsError::Mft("MFT page is missing".into()))?;
            let mut out = Vec::new();
            let mut consumed = 0;
            while page.offset < page.bytes.len() && consumed < config.max_records_per_tick {
                let (record, length) = parse_record(&page.bytes[page.offset..])?;
                if let Some(meta) = resolve(&record)? {
                    if meta.key != DocKey::from_parts(volume, record.frn) || meta.volume != volume {
                        return Err(NtfsError::Mft(
                            "resolved metadata identity does not match MFT record".into(),
                        ));
                    }
                    if !meta
                        .path
                        .as_deref()
                        .is_some_and(|path| journal::excluded_path(path, &config.exclude_paths))
                    {
                        out.push(meta);
                    }
                }
                page.offset += length;
                consumed += 1;
            }
            if page.offset == page.bytes.len() {
                self.next_start = page.next_start;
                self.page = None;
            }
            Ok(Some(out))
        }
    }
}

#[cfg(windows)]
mod native {
    use super::journal::{JournalState, Record};
    use super::*;
    use core_types::FileFlags;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use windows::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF, ERROR_JOURNAL_ENTRY_DELETED, ERROR_MORE_DATA,
        ERROR_PATH_NOT_FOUND, FILETIME, HANDLE,
    };
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_DESCRIPTOR,
        FILE_ID_DESCRIPTOR_0, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileIdType, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        OPEN_EXISTING, OpenFileById, VOLUME_NAME_GUID,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
        READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0,
    };
    use windows::core::{Error as WindowsError, HRESULT, PCWSTR};

    fn raw(handle: &OwnedHandle) -> HANDLE {
        HANDLE(handle.as_raw_handle() as isize)
    }

    pub(super) fn captured_last_error_code(result: windows::core::Result<()>) -> u32 {
        // windows 0.52 projects GetLastError as Result<(), Error>, converting
        // nonzero Win32 codes to HRESULTs. Unwrap that projection without
        // making another Windows call or turning an unknown failure into zero.
        match result {
            Ok(()) => 0,
            Err(error) => {
                let code = error.code().0 as u32;
                if code & 0xffff_0000 == 0x8007_0000 && code & 0xffff != 0 {
                    code & 0xffff
                } else {
                    code.max(1)
                }
            }
        }
    }

    pub(super) fn enable_process_backup_privilege() -> Result<(), NtfsError> {
        use windows::Win32::Security::TOKEN_ADJUST_PRIVILEGES;
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        let mut token = HANDLE::default();
        // SAFETY: this opens only our own process token, with the access needed
        // to enable an existing privilege. No previous-state query is requested.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token) }
            .map_err(|error| {
                NtfsError::Privilege(format!("OpenProcessToken for SeBackupPrivilege: {error}"))
            })?;
        // SAFETY: OpenProcessToken succeeded and ownership transfers exactly once.
        let token = unsafe { OwnedHandle::from_raw_handle(token.0 as RawHandle) };
        enable_backup_privilege_on_token(&token)
    }

    pub(super) fn enable_backup_privilege_on_token(token: &OwnedHandle) -> Result<(), NtfsError> {
        use windows::Win32::Foundation::{GetLastError, LUID};
        use windows::Win32::Security::{
            AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_BACKUP_NAME,
            SE_PRIVILEGE_ENABLED, TOKEN_PRIVILEGES,
        };

        let mut luid = LUID::default();
        // SAFETY: SE_BACKUP_NAME is terminated and the output LUID is writable.
        unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_BACKUP_NAME, &mut luid) }.map_err(
            |error| {
                NtfsError::Privilege(format!("LookupPrivilegeValueW(SeBackupPrivilege): {error}"))
            },
        )?;
        let requested = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        // SAFETY: the token is owned, and the one-entry request lives through
        // this synchronous call. Capture GetLastError before any logging or
        // other Windows call; a successful BOOL is insufficient on its own.
        let (succeeded, last_error) = unsafe {
            let result = AdjustTokenPrivileges(raw(token), false, Some(&requested), 0, None, None);
            let last_error = GetLastError();
            (result.is_ok(), last_error)
        };
        check_backup_privilege_adjustment(succeeded, captured_last_error_code(last_error))
    }

    pub(super) fn canonical_path(path: &std::path::Path) -> Result<String, NtfsError> {
        let encoded: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the input path is NUL-terminated and lives through the call.
        // Follow reparse points here: exclusions must name the actual output
        // directory even if its configured path traverses a junction.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(encoded.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_NO_RECALL,
                None,
            )
        }
        .map_err(|error| journal_error("canonical path CreateFileW", error))?;
        // SAFETY: this new valid handle is transferred to a single owner.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0 as RawHandle) };
        path_from_handle(&handle)?.ok_or_else(|| {
            NtfsError::Journal(format!("cannot resolve canonical path {}", path.display()))
        })
    }

    fn journal_error(operation: &str, error: WindowsError) -> NtfsError {
        if error.code() == HRESULT::from_win32(ERROR_JOURNAL_ENTRY_DELETED.0) {
            NtfsError::GapDetected
        } else {
            NtfsError::Journal(format!("{operation}: {error}"))
        }
    }

    pub(super) fn query_state(handle: &OwnedHandle) -> Result<JournalState, NtfsError> {
        let mut data = USN_JOURNAL_DATA_V0::default();
        let mut returned = 0;
        // SAFETY: the handle is owned and valid for this call, output points
        // to an initialized, properly aligned structure of the supplied size.
        unsafe {
            DeviceIoControl(
                raw(handle),
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some((&mut data as *mut USN_JOURNAL_DATA_V0).cast()),
                size_of::<USN_JOURNAL_DATA_V0>() as u32,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|error| journal_error("FSCTL_QUERY_USN_JOURNAL", error))?;
        if returned < size_of::<USN_JOURNAL_DATA_V0>() as u32 {
            return Err(NtfsError::Journal("truncated journal information".into()));
        }
        let state = JournalState {
            journal_id: data.UsnJournalID,
            first_usn: u64::try_from(data.FirstUsn)
                .map_err(|_| NtfsError::Journal("negative FirstUsn".into()))?,
            lowest_valid_usn: u64::try_from(data.LowestValidUsn)
                .map_err(|_| NtfsError::Journal("negative LowestValidUsn".into()))?,
            next_usn: u64::try_from(data.NextUsn)
                .map_err(|_| NtfsError::Journal("negative NextUsn".into()))?,
        };
        if state.first_usn.max(state.lowest_valid_usn) > state.next_usn {
            return Err(NtfsError::Journal("invalid journal USN range".into()));
        }
        Ok(state)
    }

    pub(super) fn read_batch(
        handle: &OwnedHandle,
        cursor: JournalCursor,
        chunk_size: usize,
    ) -> Result<Vec<u8>, NtfsError> {
        let request = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: i64::try_from(cursor.last_usn).map_err(|_| NtfsError::GapDetected)?,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            // A nonzero value with Timeout=0 waits indefinitely at the tail.
            // Zero makes an idle call return its next cursor immediately.
            BytesToWaitFor: 0,
            UsnJournalID: cursor.journal_id,
            // NTFS uses 64-bit FRNs. Do not truncate ReFS 128-bit identities
            // or interpret V3/V4 bytes as V2 records.
            MinMajorVersion: 2,
            MaxMajorVersion: 2,
        };
        let mut buffer = vec![0u8; chunk_size];
        let mut returned = 0;
        // SAFETY: request and buffer remain alive until this synchronous call
        // completes. Their byte counts match their allocations.
        let result = unsafe {
            DeviceIoControl(
                raw(handle),
                FSCTL_READ_USN_JOURNAL,
                Some((&request as *const READ_USN_JOURNAL_DATA_V1).cast()),
                size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                Some(buffer.as_mut_ptr().cast()),
                chunk_size as u32,
                Some(&mut returned),
                None,
            )
        };
        // DeviceIoControl permits ERROR_MORE_DATA with partial output and a
        // valid byte count. Keep the same buffer bound; never grow or drain in
        // a loop. The entire returned prefix must validate before acceptance.
        // https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-deviceiocontrol
        let partial = match result {
            Ok(()) => false,
            Err(error) if error.code() == HRESULT::from_win32(ERROR_MORE_DATA.0) => true,
            Err(error) => {
                // A reset between QUERY and READ may report a generic parameter
                // error. Re-query to classify identity/range failures as gaps.
                if let Ok(state) = query_state(handle) {
                    journal::validate_cursor(cursor, state)?;
                }
                return Err(journal_error("FSCTL_READ_USN_JOURNAL", error));
            }
        };
        if returned as usize > buffer.len() {
            return Err(NtfsError::Journal("invalid journal byte count".into()));
        }
        buffer.truncate(returned as usize);
        if partial {
            journal::validate_partial_output(&buffer, cursor)?;
        }
        Ok(buffer)
    }

    #[derive(Debug)]
    pub(super) struct MftScan {
        handle: OwnedHandle,
        volume: VolumeId,
        config: ReaderConfig,
        state: mft::State,
    }

    impl MftScan {
        pub(super) fn new(handle: OwnedHandle, volume: VolumeId, config: ReaderConfig) -> Self {
            Self {
                handle,
                volume,
                config,
                state: mft::State::default(),
            }
        }

        pub(super) fn next_batch(
            &mut self,
            cursor: JournalCursor,
        ) -> Result<Option<Vec<FileMeta>>, NtfsError> {
            let handle = &self.handle;
            let config = &self.config;
            let volume = self.volume;
            let result = (|| {
                // Slow worker admission can outlive journal retention. Check
                // the original pre-scan head on every pull, including EOF, so
                // an invalid baseline is never published as complete.
                journal::validate_cursor(cursor, query_state(handle)?)?;
                let batch = self.state.next_batch(
                    volume,
                    config,
                    |start| read_mft_batch(handle, start, config.chunk_size),
                    |record| resolve_metadata(handle, volume, record),
                )?;
                journal::validate_cursor(cursor, query_state(handle)?)?;
                Ok(batch)
            })();
            if result.is_err() {
                self.state.poison();
            }
            result
        }
    }

    fn read_mft_batch(
        handle: &OwnedHandle,
        start: u64,
        chunk_size: usize,
    ) -> Result<Option<Vec<u8>>, NtfsError> {
        let request = MFT_ENUM_DATA_V0 {
            // Use the opaque ordinal returned by the previous completed page.
            // https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-mft_enum_data_v0
            StartFileReferenceNumber: start,
            LowUsn: 0,
            HighUsn: i64::MAX,
        };
        let mut buffer = vec![0u8; chunk_size];
        let mut returned = 0;
        // SAFETY: valid owned volume handle, initialized request, and live
        // output allocation, with exact byte counts for each.
        let result = unsafe {
            DeviceIoControl(
                raw(handle),
                FSCTL_ENUM_USN_DATA,
                Some((&request as *const MFT_ENUM_DATA_V0).cast()),
                size_of::<MFT_ENUM_DATA_V0>() as u32,
                Some(buffer.as_mut_ptr().cast()),
                buffer.len() as u32,
                Some(&mut returned),
                None,
            )
        };
        let partial = match result {
            Ok(()) => false,
            Err(error) if error.code() == HRESULT::from_win32(ERROR_HANDLE_EOF.0) => {
                if returned != 0 {
                    return Err(NtfsError::Mft("unexpected MFT output at EOF".into()));
                }
                return Ok(None);
            }
            Err(error) if error.code() == HRESULT::from_win32(ERROR_MORE_DATA.0) => true,
            Err(error) => {
                return Err(NtfsError::Mft(format!("FSCTL_ENUM_USN_DATA: {error}")));
            }
        };
        if returned as usize > buffer.len() {
            return Err(NtfsError::Mft("invalid MFT byte count".into()));
        }
        buffer.truncate(returned as usize);
        if partial {
            mft::validate_partial_output(&buffer, start)?;
        }
        Ok(Some(buffer))
    }

    pub(super) fn excluded_event(
        handle: &OwnedHandle,
        volume: VolumeId,
        record: &Record,
        event: &FileEvent,
        exclusions: &[String],
    ) -> Result<bool, NtfsError> {
        if exclusions.is_empty() {
            return Ok(false);
        }
        let meta = match event {
            FileEvent::Created(meta)
            | FileEvent::Modified(meta)
            | FileEvent::AttributesChanged(meta)
            | FileEvent::Renamed { to: meta, .. } => Some(meta),
            FileEvent::DirectoryRenamed { .. } => return Ok(false),
            FileEvent::Deleted(_)
            | FileEvent::RescanRequired { .. }
            | FileEvent::Excluded { .. } => None,
        };
        if let Some(meta) = meta {
            return Ok(meta
                .path
                .as_deref()
                .is_some_and(|path| journal::excluded_path(path, exclusions)));
        }
        // A deleted file cannot be opened by ID. Its live parent normally can,
        // including persistent index/state/jobs directories. If the parent is
        // also gone, retain the deletion conservatively instead of dropping an
        // event whose former path is unknown.
        let parent_record = Record {
            frn: record.parent_frn,
            parent_frn: 0,
            usn: record.usn,
            reason: 0,
            attributes: journal::ATTRIBUTE_DIRECTORY,
            name: String::new(),
        };
        let parent = resolve_metadata(handle, volume, &parent_record)?;
        Ok(parent.and_then(|meta| meta.path).is_some_and(|parent| {
            let path = format!("{}\\{}", parent.trim_end_matches('\\'), record.name);
            journal::excluded_path(&path, exclusions)
        }))
    }

    pub(super) fn missing_file(error: &WindowsError) -> bool {
        // An access-denied probe does not prove deletion. It can be caused by
        // the token, an ancestor's access policy, or transient filesystem state
        // without a later per-file USN record that would restore this document.
        [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
            .iter()
            .any(|code| error.code() == HRESULT::from_win32(code.0))
    }

    pub(super) fn resolve_metadata(
        volume_handle: &OwnedHandle,
        volume: VolumeId,
        record: &Record,
    ) -> Result<Option<FileMeta>, NtfsError> {
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 {
                // FileId is a signed LARGE_INTEGER but represents all 64 FRN
                // bits, including the sequence number in the upper 16 bits.
                FileId: i64::from_ne_bytes(record.frn.to_ne_bytes()),
            },
        };
        // SAFETY: the descriptor matches FileIdType and lives through the
        // call. Opening by the full reference cannot select a reused MFT slot.
        let handle = match unsafe {
            OpenFileById(
                raw(volume_handle),
                &descriptor,
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                FILE_FLAG_BACKUP_SEMANTICS
                    | FILE_FLAG_OPEN_REPARSE_POINT
                    | FILE_FLAG_OPEN_NO_RECALL,
            )
        } {
            Ok(handle) => handle,
            Err(error) if missing_file(&error) => return Ok(None),
            Err(error) => return Err(journal_error("OpenFileById", error)),
        };
        // SAFETY: OpenFileById returned a new valid handle and ownership is
        // transferred exactly once. OwnedHandle closes it on every exit path.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0 as RawHandle) };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle remains valid and info is a live output struct.
        if let Err(error) = unsafe { GetFileInformationByHandle(raw(&handle), &mut info) } {
            if missing_file(&error) {
                return Ok(None);
            }
            return Err(journal_error("GetFileInformationByHandle", error));
        }
        let actual_frn = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        if actual_frn != record.frn {
            return Err(NtfsError::Journal(
                "file reference changed during resolution".into(),
            ));
        }
        let Some(path) = path_from_handle(&handle)? else {
            return Ok(None);
        };
        let name = path
            .rsplit('\\')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(&record.name)
            .to_string();
        Ok(Some(FileMeta::new(
            DocKey::from_parts(volume, record.frn),
            volume,
            Some(DocKey::from_parts(volume, record.parent_frn)),
            name,
            Some(path),
            (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
            timestamp(info.ftCreationTime),
            timestamp(info.ftLastWriteTime),
            flags(info.dwFileAttributes),
        )))
    }

    fn path_from_handle(handle: &OwnedHandle) -> Result<Option<String>, NtfsError> {
        let mut buffer = vec![0u16; 1024];
        for _ in 0..2 {
            // SAFETY: buffer is writable for its full length, and the handle
            // stays open while Windows resolves its current full path.
            let length =
                unsafe { GetFinalPathNameByHandleW(raw(handle), &mut buffer, VOLUME_NAME_GUID) };
            if length == 0 {
                let error = WindowsError::from_win32();
                if missing_file(&error) {
                    return Ok(None);
                }
                return Err(journal_error("GetFinalPathNameByHandleW", error));
            }
            if length as usize >= buffer.len() {
                if length >= 32768 {
                    return Err(NtfsError::Journal(
                        "resolved path exceeds Windows limit".into(),
                    ));
                }
                buffer.resize(length as usize + 1, 0);
                continue;
            }
            let path = String::from_utf16(&buffer[..length as usize])
                .map_err(|_| NtfsError::Journal("resolved path is not valid Unicode".into()))?;
            // Worker opens must remain bound to this volume even if Windows
            // reassigns drive letters between journal read and extraction.
            return Ok(Some(path));
        }
        Err(NtfsError::Journal(
            "file path changed repeatedly during resolution".into(),
        ))
    }

    fn timestamp(time: FILETIME) -> i64 {
        let ticks = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        // A Windows FILETIME counts 100 ns ticks since 1601-01-01.
        (ticks / 10_000_000) as i64 - 11_644_473_600
    }

    fn flags(attributes: u32) -> FileFlags {
        let mut result = FileFlags::empty();
        for (attribute, flag) in [
            (0x10, FileFlags::IS_DIR),
            (0x02, FileFlags::HIDDEN),
            (0x04, FileFlags::SYSTEM),
            (0x20, FileFlags::ARCHIVE),
            (0x400, FileFlags::REPARSE),
            (0x1000, FileFlags::OFFLINE),
            (0x100, FileFlags::TEMPORARY),
        ] {
            if attributes & attribute != 0 {
                result |= flag;
            }
        }
        result
    }
}

/// Simple in-memory watcher useful for tests and higher-level components.
pub struct InMemoryWatcher {
    vols: Vec<VolumeInfo>,
    mft: Vec<FileMeta>,
    events: Vec<FileEvent>,
}

impl InMemoryWatcher {
    pub fn new(vols: Vec<VolumeInfo>, mft: Vec<FileMeta>, events: Vec<FileEvent>) -> Self {
        Self { vols, mft, events }
    }
}

impl NtfsWatcher for InMemoryWatcher {
    fn discover_volumes(&self) -> Result<Vec<VolumeInfo>, NtfsError> {
        Ok(self.vols.clone())
    }

    fn enumerate_mft(&self, _volume: &VolumeInfo) -> Result<Vec<FileMeta>, NtfsError> {
        Ok(self.mft.clone())
    }

    fn tail_usn(
        &self,
        _volume: &VolumeInfo,
        cursor: JournalCursor,
    ) -> Result<(Vec<FileEvent>, JournalCursor), NtfsError> {
        Ok((self.events.clone(), cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::FileFlags;

    #[test]
    fn doc_key_round_trip() {
        let doc = DocKey::from_parts(42, 1_234_567_890);
        let (vol, frn) = doc.into_parts();
        assert_eq!(vol, 42);
        assert_eq!(frn, 1_234_567_890);
    }

    #[test]
    fn reader_config_defaults_are_sane() {
        let cfg = ReaderConfig::default();
        assert_eq!(cfg.chunk_size, 1 << 20);
        assert_eq!(cfg.max_records_per_tick, 10_000);
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_unbounded_or_zero_work_configuration() {
        for cfg in [
            ReaderConfig {
                chunk_size: 0,
                max_records_per_tick: 1,
                ..ReaderConfig::default()
            },
            ReaderConfig {
                chunk_size: usize::MAX,
                max_records_per_tick: 1,
                ..ReaderConfig::default()
            },
            ReaderConfig {
                chunk_size: 4096,
                max_records_per_tick: 0,
                ..ReaderConfig::default()
            },
        ] {
            assert!(cfg.validate().is_err());
        }
        assert!(
            ReaderConfig {
                exclude_paths: vec![String::new()],
                ..ReaderConfig::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn output_exclusions_match_case_insensitively_at_path_boundaries() {
        let excluded = vec![r"\\?\Volume{abc}\ProgramData\UltraSearch\state\".into()];
        for path in [
            r"\\?\Volume{ABC}\programdata\ULTRASEARCH\STATE",
            r"\\?\Volume{abc}\ProgramData\UltraSearch\state\ingestion-v2.json",
            r"\\?\Volume{abc}\ProgramData\UltraSearch\state\jobs\worker.json",
        ] {
            assert!(journal::excluded_path(path, &excluded), "{path}");
        }
        for path in [
            r"\\?\Volume{abc}\ProgramData\UltraSearch\state-backup\notes.txt",
            r"\\?\Volume{different}\ProgramData\UltraSearch\state\ingestion-v2.json",
            r"\\?\Volume{abc}\ProgramData\UltraSearch\states",
        ] {
            assert!(!journal::excluded_path(path, &excluded), "{path}");
        }
        assert!(!journal::excluded_path(r"\\?\Volume{abc}\notes.txt", &[]));
    }

    #[test]
    fn excluded_records_still_consume_the_tick_budget_and_advance_cursor() {
        let records = vec![
            record(100, 10, journal::REASON_CREATE, "ingestion-v2.json"),
            record(180, 11, journal::REASON_CREATE, "worker.json"),
            record(272, 12, journal::REASON_CREATE, "visible.txt"),
        ];
        let (limited, next) =
            journal::parse_batch(&encoded_batch(360, &records), cursor(100), 2).unwrap();
        let exclusions = vec![
            r"C:\fixtures\ingestion-v2.json".into(),
            r"C:\fixtures\worker.json".into(),
        ];
        assert!(limited.iter().all(|record| {
            journal::excluded_path(resolved_meta(record).path.as_deref().unwrap(), &exclusions)
        }));
        assert_eq!(next, cursor(272));
        let (last, end) =
            journal::parse_batch(&encoded_batch(360, &records[2..]), next, 2).unwrap();
        assert_eq!(last[0].name, "visible.txt");
        assert!(!journal::excluded_path(
            resolved_meta(&last[0]).path.as_deref().unwrap(),
            &exclusions
        ));
        assert_eq!(end, cursor(360));
    }

    fn cursor(usn: Usn) -> JournalCursor {
        JournalCursor {
            last_usn: usn,
            journal_id: 7,
        }
    }

    fn record(usn: Usn, frn: u64, reason: u32, name: &str) -> journal::Record {
        journal::Record {
            frn,
            parent_frn: 0x0002_0000_0000_0005,
            usn,
            reason,
            attributes: 0x20,
            name: name.into(),
        }
    }

    fn encoded_record(record: &journal::Record) -> Vec<u8> {
        let name: Vec<u16> = record.name.encode_utf16().collect();
        let name_bytes = name.len() * 2;
        let length = (60 + name_bytes + 7) & !7;
        let mut bytes = vec![0u8; length];
        bytes[..4].copy_from_slice(&(length as u32).to_le_bytes());
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        bytes[8..16].copy_from_slice(&record.frn.to_le_bytes());
        bytes[16..24].copy_from_slice(&record.parent_frn.to_le_bytes());
        bytes[24..32].copy_from_slice(&record.usn.to_le_bytes());
        bytes[40..44].copy_from_slice(&record.reason.to_le_bytes());
        bytes[52..56].copy_from_slice(&record.attributes.to_le_bytes());
        bytes[56..58].copy_from_slice(&(name_bytes as u16).to_le_bytes());
        bytes[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, unit) in name.iter().enumerate() {
            bytes[60 + 2 * i..62 + 2 * i].copy_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    fn encoded_batch(next: Usn, records: &[journal::Record]) -> Vec<u8> {
        let mut result = next.to_le_bytes().to_vec();
        for record in records {
            result.extend(encoded_record(record));
        }
        result
    }

    fn resolved_meta(record: &journal::Record) -> FileMeta {
        FileMeta::new(
            DocKey::from_parts(42, record.frn),
            42,
            Some(DocKey::from_parts(42, record.parent_frn)),
            record.name.clone(),
            Some(format!(r"C:\fixtures\{}", record.name)),
            1234,
            1_700_000_000,
            1_700_000_100,
            FileFlags::ARCHIVE,
        )
    }

    #[test]
    fn mft_pulls_bound_raw_work_and_retain_the_unread_page() {
        let config = ReaderConfig {
            chunk_size: 4096,
            max_records_per_tick: 2,
            exclude_paths: vec![r"C:\fixtures\excluded.txt".into()],
        };
        let records = [
            record(900, 0x4321_0000_0000_0020, 0, "first.txt"),
            record(100, 0x0123_0000_0000_0021, 0, "excluded.txt"),
            record(800, 0x1234_0000_0000_0022, 0, "gone.txt"),
            record(200, 0x0001_0000_0000_0023, 0, "last.txt"),
        ];
        let mut scan = mft::State::default();
        let mut resolved = Vec::new();
        let first = scan
            .next_batch(
                42,
                &config,
                |start| {
                    assert_eq!(start, 0);
                    Ok(Some(encoded_batch(36, &records)))
                },
                |record| {
                    resolved.push(record.frn);
                    Ok(Some(resolved_meta(record)))
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(resolved, [records[0].frn, records[1].frn]);
        assert_eq!(first, [resolved_meta(&records[0])]);

        // No second IOCTL and no metadata look-ahead are permitted while the
        // first caller has not pulled the unconsumed raw suffix.
        let last = scan
            .next_batch(
                42,
                &config,
                |_| panic!("the buffered page must be consumed before another kernel read"),
                |record| {
                    resolved.push(record.frn);
                    Ok((record.name != "gone.txt").then(|| resolved_meta(record)))
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(last, [resolved_meta(&records[3])]);
        assert_eq!(resolved, records.map(|record| record.frn));
        assert_eq!(first[0].key.file_id(), 0x4321_0000_0000_0020);
        assert!(
            scan.next_batch(
                42,
                &config,
                |start| {
                    // This is the kernel's ordinal, not a masked file key or
                    // the USN of the last consumed MFT record.
                    assert_eq!(start, 36);
                    Ok(None)
                },
                |_| panic!("EOF resolves no metadata"),
            )
            .unwrap()
            .is_none()
        );
        assert!(
            scan.next_batch(
                42,
                &config,
                |_| panic!("a completed scan does not issue more reads"),
                |_| panic!("a completed scan resolves no metadata"),
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn mft_empty_filtered_pages_are_progress_and_not_eof() {
        let config = ReaderConfig {
            chunk_size: 4096,
            max_records_per_tick: 1,
            ..ReaderConfig::default()
        };
        let mut scan = mft::State::default();
        let empty = scan
            .next_batch(
                42,
                &config,
                |_| Ok(Some(encoded_batch(5, &[]))),
                |_| panic!("header-only page resolves no metadata"),
            )
            .unwrap();
        assert_eq!(empty, Some(Vec::new()));
        let missing = record(100, 0x0100_0000_0000_0010, 0, "disappeared.txt");
        let skipped = scan
            .next_batch(
                42,
                &config,
                |start| {
                    assert_eq!(start, 5);
                    Ok(Some(encoded_batch(17, &[missing])))
                },
                |_| Ok(None),
            )
            .unwrap();
        assert_eq!(skipped, Some(Vec::new()));
        assert!(
            scan.next_batch(
                42,
                &config,
                |start| {
                    assert_eq!(start, 17);
                    Ok(None)
                },
                |_| panic!("EOF resolves no metadata"),
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn mft_malformed_and_oversized_pages_invalidate_the_scan() {
        let config = ReaderConfig {
            chunk_size: 4096,
            max_records_per_tick: 1,
            ..ReaderConfig::default()
        };
        let entry = record(100, 11, 0, "a.txt");
        let mut truncated = encoded_batch(12, &[entry]);
        truncated.pop();
        for invalid in [
            vec![],
            vec![0; 7],
            encoded_batch(0, &[]),
            vec![0; 4097],
            truncated,
        ] {
            let mut scan = mft::State::default();
            assert!(
                scan.next_batch(
                    42,
                    &config,
                    |_| Ok(Some(invalid)),
                    |record| { Ok(Some(resolved_meta(record))) }
                )
                .is_err()
            );
            assert!(
                scan.next_batch(
                    42,
                    &config,
                    |_| panic!("invalid scan must not read again"),
                    |_| panic!("invalid scan must not resolve again"),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn mft_failure_after_a_committed_prefix_never_becomes_completion() {
        let config = ReaderConfig {
            chunk_size: 4096,
            max_records_per_tick: 1,
            ..ReaderConfig::default()
        };
        let entries = [
            record(100, 11, 0, "first.txt"),
            record(200, 12, 0, "broken.txt"),
        ];
        let mut truncated = encoded_batch(13, &entries);
        truncated.pop();
        let mut scan = mft::State::default();
        assert_eq!(
            scan.next_batch(
                42,
                &config,
                |_| Ok(Some(truncated)),
                |record| { Ok(Some(resolved_meta(record))) }
            )
            .unwrap(),
            Some(vec![resolved_meta(&entries[0])])
        );
        assert!(
            scan.next_batch(
                42,
                &config,
                |_| panic!("the invalid suffix remains buffered"),
                |_| panic!("truncated record must fail before metadata resolution"),
            )
            .is_err()
        );
        assert!(
            scan.next_batch(42, &config, |_| Ok(None), |_| Ok(None))
                .is_err()
        );
    }

    #[test]
    fn mft_resolution_and_journal_failures_cannot_acknowledge_eof() {
        let config = ReaderConfig::default();
        let entry = record(100, 0x0100_0000_0000_0010, 0, "a.txt");
        for wrong_volume in [false, true] {
            let mut scan = mft::State::default();
            let mut wrong = resolved_meta(&entry);
            if wrong_volume {
                wrong.volume += 1;
            } else {
                wrong.key = DocKey::from_parts(42, entry.frn + 1);
            }
            assert!(
                scan.next_batch(
                    42,
                    &config,
                    |_| Ok(Some(encoded_batch(17, std::slice::from_ref(&entry)))),
                    |_| Ok(Some(wrong.clone())),
                )
                .is_err()
            );
            assert!(
                scan.next_batch(42, &config, |_| Ok(None), |_| Ok(None))
                    .is_err()
            );
        }
        let mut scan = mft::State::default();
        assert!(matches!(
            scan.next_batch(42, &config, |_| Err(NtfsError::GapDetected), |_| Ok(None)),
            Err(NtfsError::GapDetected)
        ));
        assert!(
            scan.next_batch(42, &config, |_| Ok(None), |_| Ok(None))
                .is_err()
        );

        // A failed post-read journal validation also overrides a provisional
        // EOF. This is the native wrapper's failure path after a journal reset.
        let mut scan = mft::State::default();
        assert!(
            scan.next_batch(42, &config, |_| Ok(None), |_| Ok(None))
                .unwrap()
                .is_none()
        );
        scan.poison();
        assert!(
            scan.next_batch(42, &config, |_| Ok(None), |_| Ok(None))
                .is_err()
        );
    }

    #[test]
    fn mft_access_denial_never_becomes_an_omitted_file_or_successful_eof() {
        let config = ReaderConfig {
            max_records_per_tick: 2,
            ..ReaderConfig::default()
        };
        let records = [
            record(100, 10, 0, "before-denied.txt"),
            record(180, 11, 0, "temporarily-denied.txt"),
            record(260, 12, 0, "after-denied.txt"),
        ];
        let mut scan = mft::State::default();
        let mut resolved = Vec::new();
        let denied = scan.next_batch(
            42,
            &config,
            |_| Ok(Some(encoded_batch(13, &records))),
            |record| {
                resolved.push(record.frn);
                if record.frn == records[1].frn {
                    Err(NtfsError::Io(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "metadata access temporarily denied",
                    )))
                } else {
                    Ok(Some(resolved_meta(record)))
                }
            },
        );
        assert!(matches!(
            denied,
            Err(NtfsError::Io(ref error))
                if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
        assert_eq!(resolved, [10, 11]);
        assert!(
            scan.next_batch(
                42,
                &config,
                |_| panic!("a failed scan must not request or acknowledge EOF"),
                |_| panic!("access restoration requires a fresh baseline"),
            )
            .is_err()
        );

        // Once access is restored, a new scan includes the denied identity and
        // resumes its raw page without skipping the bounded unread suffix.
        let mut recovered = mft::State::default();
        let first = recovered
            .next_batch(
                42,
                &config,
                |_| Ok(Some(encoded_batch(13, &records))),
                |record| Ok(Some(resolved_meta(record))),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            first,
            records[..2].iter().map(resolved_meta).collect::<Vec<_>>()
        );
        let last = recovered
            .next_batch(
                42,
                &config,
                |_| panic!("the unread MFT suffix stays in the same bounded buffer"),
                |record| Ok(Some(resolved_meta(record))),
            )
            .unwrap()
            .unwrap();
        assert_eq!(last, vec![resolved_meta(&records[2])]);
        assert!(
            recovered
                .next_batch(42, &config, |_| Ok(None), |_| Ok(None))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mft_partial_output_validates_complete_records_without_usn_order_assumptions() {
        let records = [
            record(900, 0x4321_0000_0000_0020, 0, "first.txt"),
            record(100, 0x0123_0000_0000_0021, 0, "last.txt"),
        ];
        let bytes = encoded_batch(34, &records);
        mft::validate_partial_output(&bytes, 0).unwrap();
        assert!(mft::validate_partial_output(&bytes[..bytes.len() - 1], 0).is_err());
        assert!(mft::validate_partial_output(&bytes[..7], 0).is_err());
        assert!(mft::validate_partial_output(&encoded_batch(34, &[]), 0).is_err());
        assert!(mft::validate_partial_output(&bytes, 34).is_err());
    }

    #[test]
    fn validates_journal_identity_wrap_and_future_cursor() {
        let state = journal::JournalState {
            journal_id: 7,
            first_usn: 100,
            lowest_valid_usn: 200,
            next_usn: 900,
        };
        for position in [200, 400, 900] {
            journal::validate_cursor(cursor(position), state).unwrap();
        }
        for position in [0, 99, 100, 199, 901, u64::MAX] {
            assert!(matches!(
                journal::validate_cursor(cursor(position), state),
                Err(NtfsError::GapDetected)
            ));
        }
        assert!(matches!(
            journal::validate_cursor(
                JournalCursor {
                    journal_id: 8,
                    ..cursor(300)
                },
                state
            ),
            Err(NtfsError::GapDetected)
        ));
    }

    #[test]
    fn record_limit_resumes_first_unread_record_without_loss() {
        let expected = vec![
            record(
                100,
                0x1234_0000_0000_0020,
                journal::REASON_CREATE,
                "音楽🥁.txt",
            ),
            record(
                180,
                0x1234_0000_0000_0020,
                journal::REASON_RENAME_OLD,
                "音楽🥁.txt",
            ),
            record(
                272,
                0x1234_0000_0000_0020,
                journal::REASON_RENAME_NEW,
                "new.txt",
            ),
        ];
        let (first, next) =
            journal::parse_batch(&encoded_batch(368, &expected), cursor(100), 2).unwrap();
        assert_eq!(first, expected[..2]);
        assert_eq!(next, cursor(272));
        let (rest, end) =
            journal::parse_batch(&encoded_batch(368, &expected[2..]), next, 2).unwrap();
        assert_eq!(rest, expected[2..]);
        assert_eq!(end, cursor(368));
        assert_eq!(first[0].frn, 0x1234_0000_0000_0020);
        assert_eq!(first[0].parent_frn, 0x0002_0000_0000_0005);
    }

    #[test]
    fn partial_output_requires_complete_records_and_keeps_the_unconsumed_suffix() {
        let records = vec![
            record(100, 10, journal::REASON_CREATE, "a.txt"),
            record(180, 11, journal::REASON_CONTENT, "b.txt"),
            record(260, 12, journal::REASON_DELETE, "c.txt"),
        ];
        let buffer = encoded_batch(340, &records);
        journal::validate_partial_output(&buffer, cursor(100)).unwrap();
        let (first, next) = journal::parse_batch(&buffer, cursor(100), 1).unwrap();
        assert_eq!(first, records[..1]);
        assert_eq!(next, cursor(180));
        let (remaining, end) =
            journal::parse_batch(&encoded_batch(340, &records[1..]), next, 10).unwrap();
        assert_eq!(remaining, records[1..]);
        assert_eq!(end, cursor(340));

        // Validate even the suffix beyond the caller's consumption budget.
        assert!(
            journal::validate_partial_output(&buffer[..buffer.len() - 1], cursor(100)).is_err()
        );
        assert!(journal::validate_partial_output(&buffer[..7], cursor(100)).is_err());
        assert!(journal::validate_partial_output(&encoded_batch(340, &[]), cursor(100)).is_err());
        assert!(journal::validate_partial_output(&buffer, cursor(340)).is_err());
    }

    #[test]
    fn denied_metadata_and_exclusion_probes_replay_the_entire_unacknowledged_batch() {
        let records = [
            record(100, 10, 1, "before-denied.txt"),
            record(180, 11, 1, "temporarily-denied.txt"),
            record(260, 12, 1, "after-denied.txt"),
        ];
        let bytes = encoded_batch(340, &records);
        for denied_exclusion in [false, true] {
            let mut resolved = Vec::new();
            let failed = journal::resolve_batch(
                42,
                &bytes,
                cursor(100),
                2,
                |record| {
                    resolved.push(record.frn);
                    if record.frn == 11 && !denied_exclusion {
                        Err(NtfsError::Io(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "metadata access denied",
                        )))
                    } else {
                        Ok(Some(resolved_meta(record)))
                    }
                },
                |record, _| {
                    if record.frn == 11 && denied_exclusion {
                        Err(NtfsError::Io(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "parent path access denied",
                        )))
                    } else {
                        Ok(false)
                    }
                },
            );
            assert!(matches!(
                failed,
                Err(NtfsError::Io(ref error))
                    if error.kind() == std::io::ErrorKind::PermissionDenied
            ));
            assert_eq!(resolved, [10, 11]);

            // No returned cursor can acknowledge either member of the failed
            // batch. A successful retry replays its prefix, including the file
            // whose probe failed, then continues at the first unread record.
            let (replayed, next) = journal::resolve_batch(
                42,
                &bytes,
                cursor(100),
                2,
                |record| Ok(Some(resolved_meta(record))),
                |_, _| Ok(false),
            )
            .unwrap();
            assert_eq!(
                replayed,
                records[..2]
                    .iter()
                    .map(|record| FileEvent::Modified(resolved_meta(record)))
                    .collect::<Vec<_>>()
            );
            assert_eq!(next, cursor(260));
            let (last, end) = journal::resolve_batch(
                42,
                &encoded_batch(340, &records[2..]),
                next,
                2,
                |record| Ok(Some(resolved_meta(record))),
                |_, _| Ok(false),
            )
            .unwrap();
            assert_eq!(last, vec![FileEvent::Modified(resolved_meta(&records[2]))]);
            assert_eq!(end, cursor(340));
        }
    }

    #[test]
    fn transactional_stream_changes_refresh_content_across_bounded_replay() {
        let frn = 0xFEDC_0000_0000_0020;
        let records = [
            record(100, frn, journal::REASON_TRANSACTED, "transactional.txt"),
            record(
                180,
                frn,
                journal::REASON_TRANSACTED | journal::REASON_CLOSE,
                "transactional.txt",
            ),
            record(
                260,
                frn,
                journal::REASON_TRANSACTED | journal::REASON_DELETE | journal::REASON_CLOSE,
                "transactional.txt",
            ),
        ];
        for _ in 0..2 {
            let mut position = cursor(100);
            for (number, record) in records.iter().enumerate() {
                let (events, next) = journal::resolve_batch(
                    42,
                    &encoded_batch(340, &records[number..]),
                    position,
                    1,
                    |record| {
                        assert_eq!(record.reason & journal::REASON_DELETE, 0);
                        Ok(Some(resolved_meta(record)))
                    },
                    |_, _| Ok(false),
                )
                .unwrap();
                let expected = if number == 2 {
                    FileEvent::Deleted(DocKey::from_parts(42, frn))
                } else {
                    FileEvent::Modified(resolved_meta(record))
                };
                assert_eq!(events, [expected]);
                assert_eq!(next, cursor([180, 260, 340][number]));
                position = next;
            }
        }
    }

    #[test]
    fn idle_and_ignored_records_have_correct_next_cursor() {
        let (events, next) =
            journal::parse_batch(&encoded_batch(100, &[]), cursor(100), 1).unwrap();
        assert!(events.is_empty());
        assert_eq!(next, cursor(100));
        let close = record(100, 10, journal::REASON_CLOSE, "closed.txt");
        let (records, next) =
            journal::parse_batch(&encoded_batch(180, &[close]), cursor(100), 1).unwrap();
        assert_eq!(next, cursor(180));
        assert!(
            journal::event_from_record(42, &records[0], || panic!("CLOSE resolves no metadata"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_records_fail_without_a_cursor() {
        let original = encoded_batch(180, &[record(100, 10, journal::REASON_CREATE, "a.txt")]);
        for size in [0, 1, 7, 9, 12, 30, original.len() - 1] {
            assert!(journal::parse_batch(&original[..size], cursor(100), 1).is_err());
        }
        for (offset, replacement) in [
            (8, 0u32.to_le_bytes().to_vec()),
            (8, u32::MAX.to_le_bytes().to_vec()),
            (12, 3u16.to_le_bytes().to_vec()),
            (64, 3u16.to_le_bytes().to_vec()),
            (66, 0u16.to_le_bytes().to_vec()),
            (66, u16::MAX.to_le_bytes().to_vec()),
            (32, (-1i64).to_le_bytes().to_vec()),
            (0, (-1i64).to_le_bytes().to_vec()),
        ] {
            let mut invalid = original.clone();
            invalid[offset..offset + replacement.len()].copy_from_slice(&replacement);
            assert!(
                journal::parse_batch(&invalid, cursor(100), 1).is_err(),
                "offset {offset}"
            );
        }
    }

    #[test]
    fn rejects_backward_duplicate_and_out_of_batch_usns() {
        let first = record(100, 10, journal::REASON_CREATE, "a.txt");
        for second_usn in [99, 100, 300, 301] {
            let second = record(second_usn, 11, journal::REASON_CREATE, "b.txt");
            assert!(
                journal::parse_batch(
                    &encoded_batch(300, &[first.clone(), second]),
                    cursor(100),
                    10
                )
                .is_err()
            );
        }
        assert!(journal::parse_batch(&encoded_batch(99, &[]), cursor(100), 1).is_err());
    }

    #[test]
    fn all_index_changes_carry_current_metadata_and_preserve_identity() {
        for reason in [
            journal::REASON_CREATE,
            1,
            2,
            4,
            0x8000,
            journal::REASON_RENAME_NEW,
        ] {
            let change = record(100, 0xFFFF_0000_0000_0020, reason, "current.txt");
            let meta = resolved_meta(&change);
            let event = journal::event_from_record(42, &change, || Ok(Some(meta.clone())))
                .unwrap()
                .unwrap();
            let actual = match event {
                FileEvent::Created(meta) => {
                    assert_eq!(reason, journal::REASON_CREATE);
                    meta
                }
                FileEvent::Modified(meta) => {
                    assert!(reason & 7 != 0);
                    meta
                }
                FileEvent::AttributesChanged(meta) => {
                    assert_eq!(reason, 0x8000);
                    meta
                }
                FileEvent::Renamed { from, to } => {
                    assert_eq!(from, meta.key);
                    to
                }
                other => panic!("unexpected event: {other:?}"),
            };
            assert_eq!(actual, meta);
            assert_eq!(actual.key.file_id(), change.frn);
        }
    }

    #[test]
    fn accumulated_delete_and_missing_files_cannot_resurrect_documents() {
        let delete = record(
            100,
            10,
            journal::REASON_CREATE | journal::REASON_DELETE | 1,
            "gone.txt",
        );
        assert_eq!(
            journal::event_from_record(42, &delete, || panic!("deleted files need no path"))
                .unwrap(),
            Some(FileEvent::Deleted(DocKey::from_parts(42, 10)))
        );
        let historical_create = record(100, 11, journal::REASON_CREATE, "already-gone.txt");
        assert_eq!(
            journal::event_from_record(42, &historical_create, || Ok(None)).unwrap(),
            Some(FileEvent::Deleted(DocKey::from_parts(42, 11)))
        );
        assert!(
            journal::event_from_record(42, &historical_create, || {
                Err(NtfsError::Journal("sharing violation".into()))
            })
            .is_err()
        );
    }

    #[test]
    fn rename_pair_can_be_split_and_replayed_without_changing_document_key() {
        let old = record(
            100,
            0x0008_0000_0000_0010,
            journal::REASON_RENAME_OLD,
            "old.txt",
        );
        let new = record(180, old.frn, journal::REASON_RENAME_NEW, "new.txt");
        assert!(
            journal::event_from_record(42, &old, || panic!("old name is not a new document"))
                .unwrap()
                .is_none()
        );
        let meta = resolved_meta(&new);
        let expected = Some(FileEvent::Renamed {
            from: meta.key,
            to: meta.clone(),
        });
        for _ in 0..2 {
            assert_eq!(
                journal::event_from_record(42, &new, || Ok(Some(meta.clone()))).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn ordinary_directory_deletion_is_precise_with_accumulated_reasons() {
        for reason in [
            journal::REASON_DELETE,
            journal::REASON_DELETE | journal::REASON_CREATE,
            journal::REASON_DELETE | journal::REASON_CLOSE,
            journal::REASON_DELETE | journal::REASON_CREATE | 0x8000 | 1 | journal::REASON_CLOSE,
        ] {
            let mut change = record(128, 0xFFFF_0000_0000_0030, reason, "removed-directory");
            change.attributes = journal::ATTRIBUTE_DIRECTORY | 0x20;
            assert_eq!(
                journal::event_from_record(42, &change, || panic!("a tombstone needs no path"))
                    .unwrap(),
                Some(FileEvent::Deleted(DocKey::from_parts(42, change.frn)))
            );
        }
    }

    #[test]
    fn mixed_recursive_deletion_replays_with_bounded_progress_and_preserves_moved_files() {
        let root_frn = 0x0003_0000_0000_0030;
        let nested_frn = 0x0004_0000_0000_0031;
        let deleted_frn = 0x0007_0000_0000_0032;
        let moved_frn = 0x0009_0000_0000_0033;
        let replacement_frn = 0x0005_0000_0000_0030;
        let mut old_name = record(128, moved_frn, journal::REASON_RENAME_OLD, "survivor.txt");
        old_name.parent_frn = nested_frn;
        let new_name = record(256, moved_frn, journal::REASON_RENAME_NEW, "survived.txt");
        let mut deleted = record(384, deleted_frn, journal::REASON_DELETE, "deleted.txt");
        deleted.parent_frn = nested_frn;
        let mut nested = record(512, nested_frn, journal::REASON_DELETE, "nested");
        nested.parent_frn = root_frn;
        nested.attributes = journal::ATTRIBUTE_DIRECTORY;
        let mut root = record(640, root_frn, journal::REASON_DELETE, "removed-tree");
        root.attributes = journal::ATTRIBUTE_DIRECTORY;
        let mut replacement = record(768, replacement_frn, journal::REASON_CREATE, "new-tree");
        replacement.attributes = journal::ATTRIBUTE_DIRECTORY;
        let moved_meta = resolved_meta(&new_name);
        let mut replacement_meta = resolved_meta(&replacement);
        replacement_meta.flags = FileFlags::IS_DIR;
        let records = [old_name, new_name, deleted, nested, root, replacement];
        let expected = vec![
            FileEvent::Renamed {
                from: DocKey::from_parts(42, moved_frn),
                to: moved_meta.clone(),
            },
            FileEvent::Deleted(DocKey::from_parts(42, deleted_frn)),
            FileEvent::Deleted(DocKey::from_parts(42, nested_frn)),
            FileEvent::Deleted(DocKey::from_parts(42, root_frn)),
            FileEvent::Created(replacement_meta.clone()),
        ];

        // A one-record budget splits the rename pair and yields an actionless
        // first tick. Replaying each partition must preserve the same events,
        // including different generations of the removed directory's MFT slot.
        for budget in [1, 2, 3] {
            for _ in 0..2 {
                let mut position = cursor(128);
                let mut consumed = 0;
                let mut events = Vec::new();
                while consumed < records.len() {
                    let (batch, next) = journal::parse_batch(
                        &encoded_batch(896, &records[consumed..]),
                        position,
                        budget,
                    )
                    .unwrap();
                    assert!(batch.len() <= budget);
                    assert!(next.last_usn > position.last_usn);
                    assert_eq!(next.journal_id, position.journal_id);
                    consumed += batch.len();
                    for record in batch {
                        let event = journal::event_from_record(42, &record, || {
                            if record.frn == moved_frn {
                                Ok(Some(moved_meta.clone()))
                            } else if record.frn == replacement_frn {
                                Ok(Some(replacement_meta.clone()))
                            } else {
                                panic!("deleted files and directories must not require resolution")
                            }
                        })
                        .unwrap();
                        events.extend(event);
                    }
                    position = next;
                }
                assert_eq!(position, cursor(896));
                assert_eq!(events, expected);
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, FileEvent::RescanRequired { .. }))
                );
                assert!(!events.contains(&FileEvent::Deleted(moved_meta.key)));
                assert!(!events.contains(&FileEvent::Deleted(replacement_meta.key)));
                assert_eq!(
                    journal::parse_batch(&encoded_batch(896, &[]), position, budget).unwrap(),
                    (Vec::new(), position)
                );
            }
        }
    }

    #[test]
    fn directory_rename_history_survives_bounded_replay_and_excluded_destinations() {
        let frn = 0xABCD_0000_0000_0040;
        let old_parent = 0x1234_0000_0000_0020;
        let new_parent = 0x5678_0000_0000_0030;
        let mut records = [
            record(128, frn, journal::REASON_RENAME_OLD, "original"),
            record(256, frn, journal::REASON_RENAME_NEW, "intermediate"),
            record(384, frn, journal::REASON_RENAME_OLD, "intermediate"),
            record(
                512,
                frn,
                journal::REASON_RENAME_NEW | journal::REASON_CLOSE,
                "final",
            ),
        ];
        for (index, record) in records.iter_mut().enumerate() {
            record.parent_frn = if index == 0 { old_parent } else { new_parent };
            record.attributes = journal::ATTRIBUTE_DIRECTORY;
        }
        let mut current = resolved_meta(&records[3]);
        current.flags = FileFlags::IS_DIR;
        current.path = Some(r"\\?\Volume{fixture}\service-output\final".into());
        let expected: Vec<_> = records
            .iter()
            .map(|record| FileEvent::DirectoryRenamed {
                doc: current.key,
                parent: DocKey::from_parts(42, record.parent_frn),
                name: record.name.clone(),
                current: Some(current.clone()),
            })
            .collect();

        // Each replay sees the latest namespace, not a fabricated intermediate
        // pathname. Historical components survive one-record ticks and retain
        // both parents even when the final destination is excluded.
        for budget in [1, 2, 3] {
            for _ in 0..2 {
                let mut position = cursor(128);
                let mut observed = Vec::new();
                while observed.len() < records.len() {
                    let remaining = &records[observed.len()..];
                    let (events, next) = journal::resolve_batch(
                        42,
                        &encoded_batch(640, remaining),
                        position,
                        budget,
                        |_| Ok(Some(current.clone())),
                        |_, _| panic!("directory repair anchors must bypass exclusion filtering"),
                    )
                    .unwrap();
                    assert_eq!(events.len(), remaining.len().min(budget));
                    assert!(next.last_usn > position.last_usn);
                    assert_eq!(next.journal_id, position.journal_id);
                    observed.extend(events);
                    position = next;
                }
                assert_eq!(observed, expected);
                assert_eq!(position, cursor(640));
            }
        }
        let (new_half, next) = journal::resolve_batch(
            42,
            &encoded_batch(640, &records[1..]),
            cursor(256),
            1,
            |_| Ok(Some(current.clone())),
            |_, _| panic!("a separately read new half still needs descendant repair"),
        )
        .unwrap();
        assert_eq!(new_half, expected[1..2]);
        assert_eq!(next, cursor(384));
    }

    #[test]
    fn missing_or_deleted_renamed_directories_retain_descendant_anchors() {
        for reason in [
            journal::REASON_RENAME_OLD,
            journal::REASON_RENAME_NEW,
            journal::REASON_RENAME_OLD | journal::REASON_RENAME_NEW | journal::REASON_CLOSE,
            journal::REASON_DELETE | journal::REASON_RENAME_NEW | journal::REASON_CLOSE,
        ] {
            let mut change = record(128, 0xFEDC_0000_0000_0040, reason, "gone-directory");
            change.attributes = journal::ATTRIBUTE_DIRECTORY;
            assert_eq!(
                journal::event_from_record(42, &change, || {
                    assert_eq!(reason & journal::REASON_DELETE, 0);
                    Ok(None)
                })
                .unwrap(),
                Some(FileEvent::DirectoryRenamed {
                    doc: DocKey::from_parts(42, change.frn),
                    parent: DocKey::from_parts(42, change.parent_frn),
                    name: "gone-directory".into(),
                    current: None,
                })
            );
        }
    }

    #[test]
    fn inaccessible_directory_rename_replays_without_acknowledging_a_prefix() {
        let before = record(128, 10, journal::REASON_CREATE, "before.txt");
        let mut rename = record(256, 11, journal::REASON_RENAME_OLD, "old-directory");
        rename.attributes = journal::ATTRIBUTE_DIRECTORY;
        let after = record(384, 12, journal::REASON_CREATE, "after.txt");
        let records = [before, rename, after];
        let bytes = encoded_batch(512, &records);
        let mut probes = Vec::new();
        let failed = journal::resolve_batch(
            42,
            &bytes,
            cursor(128),
            2,
            |record| {
                probes.push(record.frn);
                if record.frn == 11 {
                    Err(NtfsError::Io(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "directory metadata denied",
                    )))
                } else {
                    Ok(Some(resolved_meta(record)))
                }
            },
            |_, _| Ok(false),
        );
        assert!(matches!(failed, Err(NtfsError::Io(_))));
        assert_eq!(probes, [10, 11]);

        let mut current = resolved_meta(&records[1]);
        current.flags = FileFlags::IS_DIR;
        current.name = "new-directory".into();
        current.path = Some(r"\\?\Volume{fixture}\new-directory".into());
        let (replayed, next) = journal::resolve_batch(
            42,
            &bytes,
            cursor(128),
            2,
            |record| {
                Ok(Some(if record.frn == 11 {
                    current.clone()
                } else {
                    resolved_meta(record)
                }))
            },
            |_, _| Ok(false),
        )
        .unwrap();
        assert_eq!(
            replayed,
            [
                FileEvent::Created(resolved_meta(&records[0])),
                FileEvent::DirectoryRenamed {
                    doc: current.key,
                    parent: DocKey::from_parts(42, records[1].parent_frn),
                    name: "old-directory".into(),
                    current: Some(current),
                },
            ]
        );
        assert_eq!(next, cursor(384));
    }

    #[test]
    fn identity_resolution_rejects_unbounded_or_cross_volume_work_before_probing() {
        let config = ReaderConfig {
            chunk_size: 4096,
            max_records_per_tick: 2,
            ..ReaderConfig::default()
        };
        let keys = [
            DocKey::from_parts(42, 10),
            DocKey::from_parts(42, 11),
            DocKey::from_parts(42, 12),
        ];
        assert!(
            journal::resolve_file_ids(42, &keys, &config, |_| panic!("over-budget probe")).is_err()
        );
        assert!(
            journal::resolve_file_ids(
                42,
                &[keys[0], DocKey::from_parts(43, 11)],
                &config,
                |_| panic!("cross-volume probe"),
            )
            .is_err()
        );
        assert!(
            journal::resolve_file_ids(
                42,
                &keys[..1],
                &ReaderConfig {
                    max_records_per_tick: 0,
                    ..config.clone()
                },
                |_| panic!("invalid configuration probe"),
            )
            .is_err()
        );
        assert!(
            journal::resolve_file_ids(42, &[], &config, |_| panic!("empty batch probe"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn descendant_resolution_keeps_identity_and_purges_missing_or_excluded_paths() {
        let records = [
            record(128, 0x1111_0000_0000_0020, 0, "visible.txt"),
            record(256, 0x2222_0000_0000_0020, 0, "gone.txt"),
            record(384, 0x3333_0000_0000_0020, 0, "excluded.txt"),
            record(512, 0x4444_0000_0000_0020, 0, "sibling.txt"),
        ];
        let keys: Vec<_> = records
            .iter()
            .map(|record| DocKey::from_parts(42, record.frn))
            .collect();
        let config = ReaderConfig {
            max_records_per_tick: 4,
            exclude_paths: vec![r"\\?\Volume{fixture}\service".into()],
            ..ReaderConfig::default()
        };
        let mut visible = resolved_meta(&records[0]);
        visible.path = Some(r"\\?\Volume{fixture}\new-directory\visible.txt".into());
        let mut excluded = resolved_meta(&records[2]);
        excluded.path = Some(r"\\?\Volume{fixture}\SERVICE\excluded.txt".into());
        let mut sibling = resolved_meta(&records[3]);
        sibling.path = Some(r"\\?\Volume{fixture}\service-sibling\sibling.txt".into());
        let current = [
            Some(visible.clone()),
            None,
            Some(excluded),
            Some(sibling.clone()),
        ];
        for _ in 0..2 {
            let mut calls = 0;
            let resolved = journal::resolve_file_ids(42, &keys, &config, |key| {
                assert_eq!(key, keys[calls]);
                let meta = current[calls].clone();
                calls += 1;
                Ok(meta)
            })
            .unwrap();
            assert_eq!(calls, keys.len());
            let mut expected_visible = visible.clone();
            expected_visible.parent = None;
            let mut expected_sibling = sibling.clone();
            expected_sibling.parent = None;
            assert_eq!(
                resolved,
                [Some(expected_visible), None, None, Some(expected_sibling)]
            );
        }
    }

    #[test]
    fn failed_descendant_resolution_cannot_return_a_partial_replacement_page() {
        let records = [
            record(128, 10, 0, "first.txt"),
            record(256, 11, 0, "second.txt"),
        ];
        let keys = [DocKey::from_parts(42, 10), DocKey::from_parts(42, 11)];
        let config = ReaderConfig::default();
        for failure in 0..4 {
            let mut probes = 0;
            let failed = journal::resolve_file_ids(42, &keys, &config, |key| {
                assert_eq!(key, keys[probes]);
                let mut meta = resolved_meta(&records[probes]);
                probes += 1;
                if key == keys[1] {
                    match failure {
                        0 => {
                            return Err(NtfsError::Io(std::io::Error::new(
                                std::io::ErrorKind::PermissionDenied,
                                "descendant metadata denied",
                            )));
                        }
                        1 => meta.key = DocKey::from_parts(42, 0x0001_0000_0000_000B),
                        2 => meta.volume = 43,
                        3 => meta.path = None,
                        _ => unreachable!(),
                    }
                }
                Ok(Some(meta))
            });
            assert!(failed.is_err());
            assert_eq!(probes, 2);
        }
        let replayed = journal::resolve_file_ids(42, &keys, &config, |key| {
            let record = &records[usize::from(key == keys[1])];
            Ok(Some(resolved_meta(record)))
        })
        .unwrap();
        assert_eq!(
            replayed
                .iter()
                .map(|meta| meta.as_ref().unwrap().key)
                .collect::<Vec<_>>(),
            keys
        );
    }

    #[test]
    fn directory_link_and_reparse_history_still_require_full_reconciliation() {
        for reason in [
            journal::REASON_HARD_LINK,
            journal::REASON_REPARSE,
            journal::REASON_RENAME_OLD | journal::REASON_HARD_LINK,
            journal::REASON_RENAME_NEW | journal::REASON_REPARSE,
            journal::REASON_DELETE | journal::REASON_REPARSE,
        ] {
            let mut change = record(100, 10, reason, "directory");
            change.attributes = journal::ATTRIBUTE_DIRECTORY;
            assert_eq!(
                journal::event_from_record(42, &change, || panic!("requires a new baseline"))
                    .unwrap(),
                Some(FileEvent::RescanRequired {
                    doc: DocKey::from_parts(42, 10)
                })
            );
        }
        for reason in [
            journal::REASON_DELETE,
            journal::REASON_RENAME_OLD,
            journal::REASON_RENAME_NEW,
        ] {
            let mut reparse = record(100, 10, reason, "junction");
            reparse.attributes = journal::ATTRIBUTE_DIRECTORY | journal::ATTRIBUTE_REPARSE;
            assert_eq!(
                journal::event_from_record(42, &reparse, || panic!("requires reconciliation"))
                    .unwrap(),
                Some(FileEvent::RescanRequired {
                    doc: DocKey::from_parts(42, 10)
                })
            );
            if reason != journal::REASON_DELETE {
                // Reparse state may have changed since the historical record.
                let mut current = resolved_meta(&reparse);
                current.flags = FileFlags::IS_DIR | FileFlags::REPARSE;
                reparse.attributes = journal::ATTRIBUTE_DIRECTORY;
                assert!(matches!(
                    journal::event_from_record(42, &reparse, || Ok(Some(current))).unwrap(),
                    Some(FileEvent::RescanRequired { .. })
                ));
            }
        }
    }

    #[test]
    fn rejects_metadata_for_another_file_or_volume() {
        let change = record(100, 10, journal::REASON_CREATE, "a.txt");
        let mut wrong = resolved_meta(&change);
        wrong.key = DocKey::from_parts(42, 11);
        assert!(journal::event_from_record(42, &change, || Ok(Some(wrong))).is_err());
        let mut wrong = resolved_meta(&change);
        wrong.volume = 43;
        assert!(journal::event_from_record(42, &change, || Ok(Some(wrong))).is_err());
        let mut directory = record(100, 10, journal::REASON_RENAME_OLD, "directory");
        directory.attributes = journal::ATTRIBUTE_DIRECTORY;
        for failure in 0..3 {
            let mut wrong = resolved_meta(&directory);
            wrong.flags = FileFlags::IS_DIR;
            match failure {
                0 => wrong.key = DocKey::from_parts(42, 11),
                1 => wrong.volume = 43,
                2 => wrong.flags = FileFlags::empty(),
                _ => unreachable!(),
            }
            assert!(journal::event_from_record(42, &directory, || Ok(Some(wrong))).is_err());
        }
    }

    #[test]
    fn backup_privilege_requires_both_api_success_and_complete_assignment() {
        assert!(check_backup_privilege_adjustment(true, 0).is_ok());
        for (succeeded, last_error) in [
            (false, 0),
            (false, 5),
            (true, 5),
            (false, 1300),
            (true, 1300),
        ] {
            let error = check_backup_privilege_adjustment(succeeded, last_error)
                .expect_err("an unavailable privilege must not authorize native ingestion");
            assert!(matches!(error, NtfsError::Privilege(_)));
            assert!(error.to_string().contains("SeBackupPrivilege"));
            if last_error == 1300 {
                assert!(error.to_string().contains("absent"));
            }
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_platform_never_reports_an_idle_watcher() {
        let volume = VolumeInfo {
            id: 1,
            guid_path: "unsupported".into(),
            drive_letters: vec![],
        };
        assert!(matches!(
            enable_backup_privilege(),
            Err(NtfsError::NotSupported)
        ));
        assert!(matches!(discover_volumes(), Err(NtfsError::NotSupported)));
        assert!(matches!(
            begin_mft_scan(&volume, &ReaderConfig::default()),
            Err(NtfsError::NotSupported)
        ));
        assert!(matches!(
            query_journal(&volume),
            Err(NtfsError::NotSupported)
        ));
        assert!(matches!(
            tail_usn(&volume, cursor(100)),
            Err(NtfsError::NotSupported)
        ));
        assert!(matches!(
            resolve_file_ids(
                &volume,
                cursor(100),
                &[DocKey::from_parts(1, 10)],
                &ReaderConfig::default(),
            ),
            Err(NtfsError::NotSupported)
        ));
    }

    #[cfg(windows)]
    mod backup_privilege_tokens {
        use super::*;
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows::Win32::Foundation::{
            BOOL, ERROR_NOT_ALL_ASSIGNED, ERROR_SUCCESS, GetLastError, HANDLE, LUID,
        };
        use windows::Win32::Security::{
            AdjustTokenPrivileges, DuplicateTokenEx, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
            PRIVILEGE_SET, PrivilegeCheck, SE_BACKUP_NAME, SE_PRIVILEGE_REMOVED,
            SecurityImpersonation, TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE, TOKEN_PRIVILEGES,
            TOKEN_PRIVILEGES_ATTRIBUTES, TOKEN_QUERY, TokenImpersonation,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        use windows::core::PCWSTR;

        fn raw(token: &OwnedHandle) -> HANDLE {
            HANDLE(token.as_raw_handle() as isize)
        }

        fn copied_token() -> OwnedHandle {
            let mut source = HANDLE::default();
            // SAFETY: the real process token is opened only for query/duplicate,
            // never adjustment. The owned handle closes on every exit path.
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_QUERY | TOKEN_DUPLICATE,
                    &mut source,
                )
            }
            .expect("open test process token for duplication");
            let source = unsafe { OwnedHandle::from_raw_handle(source.0 as _) };
            let mut copy = HANDLE::default();
            // SAFETY: create a separate token object. It is never assigned to a
            // process or thread, and only this disposable copy will be changed.
            unsafe {
                DuplicateTokenEx(
                    raw(&source),
                    TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                    None,
                    SecurityImpersonation,
                    TokenImpersonation,
                    &mut copy,
                )
            }
            .expect("duplicate test token");
            unsafe { OwnedHandle::from_raw_handle(copy.0 as _) }
        }

        fn backup_entry(attributes: TOKEN_PRIVILEGES_ATTRIBUTES) -> LUID_AND_ATTRIBUTES {
            let mut luid = LUID::default();
            unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_BACKUP_NAME, &mut luid) }
                .expect("look up backup privilege for fixture");
            LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: attributes,
            }
        }

        fn adjust_fixture(token: &OwnedHandle, attributes: TOKEN_PRIVILEGES_ATTRIBUTES) -> u32 {
            let request = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [backup_entry(attributes)],
            };
            // SAFETY: this disposable token and one-entry request are valid
            // throughout the synchronous call. No ambient token is modified.
            let (result, last_error) = unsafe {
                let result =
                    AdjustTokenPrivileges(raw(token), false, Some(&request), 0, None, None);
                let last_error = GetLastError();
                (result, last_error)
            };
            result.expect("adjust disposable fixture token");
            native::captured_last_error_code(last_error)
        }

        fn backup_enabled(token: &OwnedHandle) -> bool {
            let mut requested = PRIVILEGE_SET {
                PrivilegeCount: 1,
                Control: 0,
                Privilege: [backup_entry(TOKEN_PRIVILEGES_ATTRIBUTES::default())],
            };
            let mut enabled = BOOL::default();
            // SAFETY: this is a query-only check of the duplicate impersonation
            // token; both output structures are valid for this call.
            unsafe { PrivilegeCheck(raw(token), &mut requested, &mut enabled) }
                .expect("query backup privilege on fixture token");
            enabled.as_bool()
        }

        #[test]
        fn absent_backup_privilege_cannot_be_added_to_a_copied_token() {
            let token = copied_token();
            // SE_PRIVILEGE_REMOVED genuinely removes the entry, rather than
            // merely disabling it. An already absent entry is also acceptable.
            let removed = adjust_fixture(&token, SE_PRIVILEGE_REMOVED);
            assert!(removed == ERROR_SUCCESS.0 || removed == ERROR_NOT_ALL_ASSIGNED.0);
            assert!(!backup_enabled(&token));

            let error = native::enable_backup_privilege_on_token(&token)
                .expect_err("enabling must never add a privilege absent from the token");
            assert!(matches!(error, NtfsError::Privilege(_)));
            assert!(error.to_string().contains("absent"));
            assert!(error.to_string().contains("1300"));
            assert!(!backup_enabled(&token));
        }

        #[test]
        #[ignore = "requires SeBackupPrivilege already present in the test process token"]
        fn held_backup_privilege_is_enabled_idempotently_on_a_copied_token() {
            let token = copied_token();
            assert_eq!(
                adjust_fixture(&token, TOKEN_PRIVILEGES_ATTRIBUTES::default()),
                ERROR_SUCCESS.0,
                "fixture requires an already assigned backup privilege"
            );
            assert!(!backup_enabled(&token));
            native::enable_backup_privilege_on_token(&token)
                .expect("enable existing disabled backup privilege");
            assert!(backup_enabled(&token));
            native::enable_backup_privilege_on_token(&token)
                .expect("enabling an already enabled privilege is idempotent");
            assert!(backup_enabled(&token));
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_metadata_probe_errors_distinguish_missing_files_from_unavailable_access() {
        use windows::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_INVALID_PARAMETER,
            ERROR_LOCK_VIOLATION, ERROR_NOT_READY, ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION,
        };
        use windows::core::{Error as WindowsError, HRESULT};

        for code in [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND] {
            let error = WindowsError::from(HRESULT::from_win32(code.0));
            assert!(native::missing_file(&error));
        }
        for code in [
            ERROR_ACCESS_DENIED,
            ERROR_SHARING_VIOLATION,
            ERROR_LOCK_VIOLATION,
            ERROR_NOT_READY,
            ERROR_GEN_FAILURE,
            ERROR_INVALID_PARAMETER,
        ] {
            let error = WindowsError::from(HRESULT::from_win32(code.0));
            assert!(
                !native::missing_file(&error),
                "Win32 error {} must not acknowledge deletion or omit a baseline file",
                code.0
            );
        }
    }

    /// Run explicitly on an elevated Windows host whose temp directory is on
    /// an isolated NTFS volume with an existing USN journal. This test also
    /// enumerates the whole volume through the bounded MFT reader:
    /// `cargo test -p ntfs-watcher native_ntfs_lifecycle -- --ignored --nocapture`
    /// A missing journal, insufficient privileges, or unsupported filesystem
    /// is a test failure, never a reported native success via an early return.
    #[cfg(windows)]
    #[test]
    #[ignore = "requires elevated Windows, an isolated NTFS temp volume, and an enabled USN journal"]
    fn native_ntfs_lifecycle() {
        use std::io::{Seek, SeekFrom, Write};
        use std::time::{Duration, Instant};

        fn wait_for(
            volume: &VolumeInfo,
            position: &mut JournalCursor,
            predicate: impl Fn(&FileEvent) -> bool,
        ) -> FileEvent {
            let deadline = Instant::now() + Duration::from_secs(20);
            let config = ReaderConfig {
                chunk_size: 4096,
                max_records_per_tick: 128,
                ..ReaderConfig::default()
            };
            loop {
                let (events, next) = tail_usn_with_config(volume, *position, &config)
                    .expect("native journal read must succeed");
                assert!(events.len() <= config.max_records_per_tick);
                assert_eq!(next.journal_id, position.journal_id);
                assert!(next.last_usn >= position.last_usn);
                *position = next;
                if let Some(event) = events.into_iter().find(&predicate) {
                    return event;
                }
                assert!(
                    Instant::now() < deadline,
                    "expected native USN event was not observed"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn read_to_head(volume: &VolumeInfo, position: &mut JournalCursor) -> Vec<FileEvent> {
            let head = query_journal(volume).expect("native journal head must be available");
            assert_eq!(head.journal_id, position.journal_id);
            assert!(head.last_usn >= position.last_usn);
            let deadline = Instant::now() + Duration::from_secs(20);
            let config = ReaderConfig {
                chunk_size: 4096,
                max_records_per_tick: 2,
                ..ReaderConfig::default()
            };
            let mut observed = Vec::new();
            while position.last_usn < head.last_usn {
                let before = *position;
                let batch = tail_usn_batch_with_config(volume, before, &config)
                    .expect("bounded native journal read must succeed");
                assert!(batch.events.len() <= config.max_records_per_tick);
                assert_eq!(batch.cursor.journal_id, before.journal_id);
                assert!(batch.cursor.last_usn >= before.last_usn);
                observed.extend(batch.events);
                *position = batch.cursor;
                assert!(
                    Instant::now() < deadline,
                    "native journal did not reach its observed head"
                );
                if *position == before {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            observed
        }

        fn created_key(events: &[FileEvent], path: &std::path::Path) -> DocKey {
            let expected_path = canonical_path(path).expect("fixture must have a GUID path");
            let keys: std::collections::BTreeSet<_> = events
                .iter()
                .filter_map(|event| match event {
                    FileEvent::Created(meta)
                        if meta.path.as_deref() == Some(expected_path.as_str()) =>
                    {
                        Some(meta.key)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                keys.len(),
                1,
                "fixture create records must preserve one full identity: {expected_path}"
            );
            *keys.first().unwrap()
        }

        enable_backup_privilege()
            .expect("native NTFS test requires SeBackupPrivilege already assigned to its token");
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().to_str().expect("Unicode temp directory");
        let drive = directory
            .strip_prefix(r"\\?\")
            .unwrap_or(directory)
            .chars()
            .next()
            .unwrap()
            .to_ascii_uppercase();
        let volume = discover_volumes()
            .expect("native volume discovery")
            .into_iter()
            .find(|volume| volume.drive_letters.contains(&drive))
            .expect("the temp directory must reside on an NTFS volume");
        let mut position = query_journal(&volume)
            .expect("an existing USN journal and administrative access are required");
        let old_path = dir.path().join("usn-original.txt");
        let new_path = dir.path().join("usn-renamed.txt");
        let directory_relative = directory
            .strip_prefix(r"\\?\")
            .unwrap_or(directory)
            .split_once(':')
            .expect("temp directory has a drive letter")
            .1;
        let expected_old_path = format!(
            "{}{}\\usn-original.txt",
            volume.guid_path.trim_end_matches('\\'),
            directory_relative.trim_end_matches('\\')
        );
        let expected_new_path = format!(
            "{}{}\\usn-renamed.txt",
            volume.guid_path.trim_end_matches('\\'),
            directory_relative.trim_end_matches('\\')
        );
        assert!(
            canonical_path(dir.path())
                .unwrap()
                .starts_with(&volume.guid_path)
        );
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&old_path)
            .unwrap();
        file.write_all(b"before").unwrap();
        file.sync_all().unwrap();
        drop(file);

        let created = wait_for(
            &volume,
            &mut position,
            |event| matches!(event, FileEvent::Created(meta) if meta.path.as_deref() == Some(expected_old_path.as_str())),
        );
        let FileEvent::Created(meta) = created else {
            unreachable!()
        };
        assert_eq!(meta.size, 6);
        assert!(meta.created > 0 && meta.modified > 0);
        let key = meta.key;

        let excluded = dir.path().join("excluded-output");
        std::fs::create_dir(&excluded).unwrap();
        std::fs::write(excluded.join("ignored.txt"), b"not indexed").unwrap();
        let scan_config = ReaderConfig {
            chunk_size: 4096,
            max_records_per_tick: 2,
            exclude_paths: vec![canonical_path(&excluded).unwrap()],
        };
        let mut scan = begin_mft_scan(&volume, &scan_config)
            .expect("native MFT scan must capture its pre-enumeration journal head");
        let baseline_cursor = scan.journal_cursor();
        assert_eq!(baseline_cursor.journal_id, position.journal_id);

        // The file changes after journal capture but before any enumeration.
        // Its MFT snapshot and subsequent journal replay must keep the key.
        std::fs::write(&old_path, b"after, with a different length").unwrap();
        let scan_deadline = Instant::now() + Duration::from_secs(120);
        let mut found = 0;
        while let Some(batch) = scan
            .next_batch()
            .expect("native MFT page and original journal cursor must remain valid")
        {
            assert!(batch.len() <= scan_config.max_records_per_tick);
            for entry in batch {
                assert_eq!(entry.key.volume(), volume.id);
                assert!(!journal::excluded_path(
                    entry.path.as_deref().expect("GUID path"),
                    &scan_config.exclude_paths,
                ));
                if entry.key == key {
                    found += 1;
                    assert_eq!(entry.path.as_deref(), Some(expected_old_path.as_str()));
                    assert_eq!(entry.size, 30);
                }
            }
            assert!(
                Instant::now() < scan_deadline,
                "bounded native MFT test requires a small isolated NTFS volume"
            );
        }
        assert_eq!(
            found, 1,
            "native MFT enumeration must find the fixture exactly once"
        );
        position = baseline_cursor;
        wait_for(
            &volume,
            &mut position,
            |event| matches!(event, FileEvent::Modified(meta) if meta.key == key && meta.size == 30),
        );

        // Native NTFS records can arrive while a data writer is still open.
        // Repeated writes may reuse the accumulated reason until the final
        // CLOSE record. Both the early and final refresh must retain the same
        // full identity, even for same-length in-place overwrites.
        position = query_journal(&volume).unwrap();
        let mut active_writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&old_path)
            .unwrap();
        active_writer.write_all(&[b'a'; 30]).unwrap();
        active_writer.sync_all().unwrap();
        wait_for(
            &volume,
            &mut position,
            |event| matches!(event, FileEvent::Modified(meta) if meta.key == key && meta.size == 30),
        );
        active_writer.seek(SeekFrom::Start(0)).unwrap();
        active_writer.write_all(&[b'b'; 30]).unwrap();
        active_writer.sync_all().unwrap();
        // Capture after the second write: observing the later event requires
        // the native close, not a replay of the first write's journal record.
        position = query_journal(&volume).unwrap();
        drop(active_writer);
        wait_for(
            &volume,
            &mut position,
            |event| matches!(event, FileEvent::Modified(meta) if meta.key == key && meta.size == 30),
        );
        assert_eq!(std::fs::read(&old_path).unwrap(), [b'b'; 30]);

        let original_permissions = std::fs::metadata(&old_path).unwrap().permissions();
        let mut permissions = original_permissions.clone();
        permissions.set_readonly(true);
        std::fs::set_permissions(&old_path, permissions).unwrap();
        wait_for(
            &volume,
            &mut position,
            |event| matches!(event, FileEvent::AttributesChanged(meta) if meta.key == key),
        );
        std::fs::set_permissions(&old_path, original_permissions).unwrap();

        std::fs::rename(&old_path, &new_path).unwrap();
        wait_for(&volume, &mut position, |event| {
            matches!(event, FileEvent::Renamed { from, to }
                if *from == key && to.key == key && to.path.as_deref() == Some(expected_new_path.as_str()))
        });
        std::fs::remove_file(&new_path).unwrap();
        wait_for(
            &volume,
            &mut position,
            |event| matches!(event, FileEvent::Deleted(doc) if *doc == key),
        );

        // Directory moves preserve historical path components and let callers
        // refresh descendants by FRN without another whole-volume MFT scan.
        let rename_source = dir.path().join("rename-source");
        let rename_nested = rename_source.join("nested");
        let rename_child = rename_nested.join("unchanged-child.txt");
        let rename_parent = dir.path().join("rename-parent");
        std::fs::create_dir_all(&rename_nested).unwrap();
        std::fs::create_dir(&rename_parent).unwrap();
        std::fs::write(&rename_child, b"child content survives ancestor moves").unwrap();
        let rename_created = read_to_head(&volume, &mut position);
        let rename_key = created_key(&rename_created, &rename_source);
        let rename_child_key = created_key(&rename_created, &rename_child);
        let rename_parent_key = created_key(&rename_created, &rename_parent);
        let before_rename = position;
        let rename_intermediate = rename_parent.join("intermediate");
        let rename_final = rename_parent.join("final");
        std::fs::rename(&rename_source, &rename_intermediate).unwrap();
        std::fs::rename(&rename_intermediate, &rename_final).unwrap();
        let current_directory_path = canonical_path(&rename_final).unwrap();
        let current_child_path =
            canonical_path(&rename_final.join("nested").join("unchanged-child.txt")).unwrap();
        let renamed = read_to_head(&volume, &mut position);
        let mut replay_position = before_rename;
        let replayed = read_to_head(&volume, &mut replay_position);
        for events in [&renamed, &replayed] {
            let mut historical_names = std::collections::BTreeSet::new();
            let mut new_parent_seen = false;
            for event in events {
                match event {
                    FileEvent::DirectoryRenamed {
                        doc,
                        parent,
                        name,
                        current,
                    } if *doc == rename_key => {
                        let current = current.as_ref().expect("renamed directory still exists");
                        assert_eq!(current.key, rename_key);
                        assert_eq!(
                            current.path.as_deref(),
                            Some(current_directory_path.as_str())
                        );
                        assert!(current.flags.contains(FileFlags::IS_DIR));
                        historical_names.insert(name.as_str());
                        new_parent_seen |= *parent == rename_parent_key;
                    }
                    FileEvent::RescanRequired { doc } => {
                        assert_ne!(
                            *doc, rename_key,
                            "ordinary rename must not reset the volume"
                        );
                    }
                    _ => {}
                }
            }
            assert!(historical_names.contains("rename-source"));
            assert!(historical_names.contains("intermediate"));
            assert!(historical_names.contains("final"));
            assert!(new_parent_seen);
        }
        let resolve_config = ReaderConfig {
            chunk_size: 4096,
            max_records_per_tick: 2,
            ..ReaderConfig::default()
        };
        let descendants = resolve_file_ids(
            &volume,
            before_rename,
            &[rename_child_key, rename_key],
            &resolve_config,
        )
        .expect("bounded native identity resolution must succeed");
        let child = descendants[0].as_ref().expect("unchanged child survives");
        assert_eq!(child.key, rename_child_key);
        assert_eq!(child.path.as_deref(), Some(current_child_path.as_str()));
        assert_eq!(child.parent, None);
        assert_eq!(descendants[1].as_ref().unwrap().key, rename_key);
        let excluded_config = ReaderConfig {
            exclude_paths: vec![canonical_path(&rename_parent).unwrap()],
            ..resolve_config.clone()
        };
        assert_eq!(
            resolve_file_ids(
                &volume,
                before_rename,
                &[rename_child_key, rename_key],
                &excluded_config,
            )
            .unwrap(),
            [None, None]
        );
        let mut excluded_position = before_rename;
        let mut excluded_rename_seen = false;
        while excluded_position.last_usn < position.last_usn {
            let batch = tail_usn_batch_with_config(&volume, excluded_position, &excluded_config)
                .expect("excluded directory rename replay must succeed");
            assert!(batch.cursor.last_usn > excluded_position.last_usn);
            excluded_position = batch.cursor;
            excluded_rename_seen |= batch.events.iter().any(|event| {
                matches!(event, FileEvent::DirectoryRenamed { doc, .. } if *doc == rename_key)
            });
        }
        assert!(excluded_rename_seen);

        // Recursive directory deletion must remain incremental. Move one child
        // out first: deleting its former ancestors must not remove that stable
        // file identity or turn routine directory tombstones into volume resets.
        let tree = dir.path().join("removed-tree");
        let nested = tree.join("nested");
        let deep = nested.join("deep");
        let gone = deep.join("removed-child.txt");
        let sibling = tree.join("removed-sibling.txt");
        let moving = nested.join("surviving-child.txt");
        let survived = dir.path().join("surviving-child.txt");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(&gone, b"delete this nested file").unwrap();
        std::fs::write(&sibling, b"delete this sibling").unwrap();
        std::fs::write(&moving, b"survives the ancestor deletion").unwrap();
        let created = read_to_head(&volume, &mut position);
        let [
            tree_key,
            nested_key,
            deep_key,
            gone_key,
            sibling_key,
            moved_key,
        ] = [&tree, &nested, &deep, &gone, &sibling, &moving]
            .map(|path| created_key(&created, path));
        let deleted_keys = std::collections::BTreeSet::from([
            tree_key,
            nested_key,
            deep_key,
            gone_key,
            sibling_key,
        ]);
        assert_eq!(deleted_keys.len(), 5);
        assert!(!deleted_keys.contains(&moved_key));
        let before_deletion = position;
        std::fs::rename(&moving, &survived).unwrap();
        std::fs::remove_dir_all(&tree).unwrap();
        let expected_survivor_path = canonical_path(&survived).unwrap();
        let deleted = read_to_head(&volume, &mut position);
        assert!(position.last_usn > before_deletion.last_usn);
        let mut replay_position = before_deletion;
        let replayed = read_to_head(&volume, &mut replay_position);
        assert!(replay_position.last_usn >= position.last_usn);
        for events in [&deleted, &replayed] {
            let mut observed_deletions = std::collections::BTreeSet::new();
            let mut moved = false;
            for event in events {
                match event {
                    FileEvent::Deleted(doc) => {
                        assert_ne!(
                            *doc, moved_key,
                            "moved child must survive its former ancestors"
                        );
                        if deleted_keys.contains(doc) {
                            observed_deletions.insert(*doc);
                        }
                    }
                    FileEvent::RescanRequired { doc } => {
                        assert!(
                            !deleted_keys.contains(doc),
                            "ordinary directory deletion must not reset the volume"
                        );
                    }
                    FileEvent::Renamed { from, to } if *from == moved_key => {
                        assert_eq!(to.key, moved_key);
                        assert_eq!(to.path.as_deref(), Some(expected_survivor_path.as_str()));
                        moved = true;
                    }
                    _ => {}
                }
            }
            assert_eq!(observed_deletions, deleted_keys);
            assert!(
                moved,
                "the moved child must retain its rename event and identity"
            );
        }
        assert!(!tree.exists());
        assert_eq!(
            std::fs::read(&survived).unwrap(),
            b"survives the ancestor deletion"
        );

        let wrong_identity = JournalCursor {
            journal_id: position.journal_id ^ 1,
            ..position
        };
        assert!(matches!(
            tail_usn(&volume, wrong_identity),
            Err(NtfsError::GapDetected)
        ));
        assert!(matches!(
            resolve_file_ids(
                &volume,
                wrong_identity,
                &[rename_child_key],
                &resolve_config
            ),
            Err(NtfsError::GapDetected)
        ));
        let future = JournalCursor {
            last_usn: i64::MAX as u64,
            ..position
        };
        assert!(matches!(
            tail_usn(&volume, future),
            Err(NtfsError::GapDetected)
        ));
        assert!(matches!(
            resolve_file_ids(&volume, future, &[rename_child_key], &resolve_config),
            Err(NtfsError::GapDetected)
        ));
    }

    #[test]
    fn in_memory_watcher_emits_provided_data() {
        let vols = vec![VolumeInfo {
            id: 1,
            guid_path: r"\\?\Volume{abc}\".to_string(),
            drive_letters: vec!['C'],
        }];
        let mft = vec![FileMeta::new(
            DocKey::from_parts(1, 10),
            1,
            Some(DocKey::from_parts(1, 5)),
            "foo.txt".into(),
            None,
            123,
            0,
            0,
            FileFlags::empty(),
        )];
        let events = vec![FileEvent::Deleted(DocKey::from_parts(1, 10))];

        let watcher = InMemoryWatcher::new(vols.clone(), mft.clone(), events.clone());
        assert_eq!(watcher.discover_volumes().unwrap().len(), vols.len());

        let got_mft = watcher.enumerate_mft(&vols[0]).unwrap();
        assert_eq!(got_mft.len(), mft.len());
        assert_eq!(got_mft[0].key, mft[0].key);

        let (evs, cur) = watcher
            .tail_usn(
                &vols[0],
                JournalCursor {
                    last_usn: 0,
                    journal_id: 1,
                },
            )
            .unwrap();
        assert_eq!(evs.len(), events.len());
        assert_eq!(cur.last_usn, 0);
    }
}
