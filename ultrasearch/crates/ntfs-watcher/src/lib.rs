//! NTFS integration layer: volume discovery, MFT enumeration, and USN tailing.
//!
//! This crate intentionally keeps a small surface: pure data types and trait
//! contracts that the service can implement with platform-specific code under
//! `windows`/`windows-sys`. The goal for c00.3 is to have a compilable,
//! testable scaffold that mirrors the implementation plan without yet wiring
//! Win32 calls.

use core_types::{DocKey, VolumeId};
use thiserror::Error;

pub type Usn = u64;

/// Static information about a mounted NTFS volume.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// Small runtime identifier assigned by the service.
    pub id: VolumeId,
    /// Volume GUID path such as `\\?\Volume{...}\`.
    pub guid_path: String,
    /// Optional drive letters currently mapped to the volume.
    pub drive_letters: Vec<char>,
}

/// Lightweight metadata for a single file or directory.
#[derive(Debug, Clone)]
pub struct FileMeta {
    pub doc_key: DocKey,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Stream of logical file-system events derived from the USN journal.
#[derive(Debug, Clone)]
pub enum FileEvent {
    Created(FileMeta),
    Deleted(DocKey),
    Modified { doc: DocKey },
    Renamed { from: DocKey, to: FileMeta },
    AttributesChanged { doc: DocKey },
}

/// Cursor for resuming USN processing.
#[derive(Debug, Clone, Copy)]
pub struct JournalCursor {
    pub last_usn: Usn,
    pub journal_id: u64,
}

/// Errors that can surface while interacting with NTFS / USN APIs.
#[derive(Debug, Error)]
pub enum NtfsError {
    #[error("volume discovery failed: {0}")]
    Discovery(String),
    #[error("usn journal error: {0}")]
    Journal(String),
    #[error("mft enumeration failed: {0}")]
    Mft(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Trait abstraction to make the platform-specific implementation swap-able in tests.
pub trait NtfsWatcher {
    /// Discover NTFS volumes.
    fn discover_volumes(&self) -> Result<Vec<VolumeInfo>, NtfsError>;

    /// Enumerate the MFT and stream file metadata snapshots.
    fn enumerate_mft(&self, volume: &VolumeInfo) -> Result<Vec<FileMeta>, NtfsError>;

    /// Tail the USN journal starting at the given cursor.
    fn tail_usn(
        &self,
        volume: &VolumeInfo,
        cursor: JournalCursor,
    ) -> Result<(Vec<FileEvent>, JournalCursor), NtfsError>;
}

/// Discover NTFS volumes available on the machine.
///
/// In the scaffold this returns an empty list; the concrete implementation will
/// call Win32 APIs (GetLogicalDrives, GetVolumeInformationW, etc.).
pub fn discover_volumes() -> Result<Vec<VolumeInfo>, NtfsError> {
    // TODO: implement volume discovery via Win32 APIs (windows / windows-sys).
    Ok(Vec::new())
}

/// Enumerate the MFT for a given volume and emit file metadata snapshots.
///
/// A production implementation will stream records to avoid large memory
/// spikes; here we surface the contract only.
pub fn enumerate_mft(_volume: &VolumeInfo) -> Result<Vec<FileMeta>, NtfsError> {
    // TODO: plumb usn-journal-rs MFT iterator and resolve parent/name into paths.
    Ok(Vec::new())
}

/// Tail the USN journal for a volume and emit file events from the given cursor.
pub fn tail_usn(
    _volume: &VolumeInfo,
    _cursor: JournalCursor,
) -> Result<(Vec<FileEvent>, JournalCursor), NtfsError> {
    // TODO: connect to USN journal, read deltas, and return next cursor.
    Ok((Vec::new(), _cursor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_key_round_trip() {
        let doc = DocKey::from_parts(42, 1_234_567_890);
        let (vol, frn) = doc.into_parts();
        assert_eq!(vol, 42);
        assert_eq!(frn, 1_234_567_890);
    }
}
