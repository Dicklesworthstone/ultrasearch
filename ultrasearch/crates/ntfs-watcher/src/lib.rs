//! NTFS integration layer: volume discovery, MFT enumeration, and USN tailing.
//!
//! This crate intentionally keeps a small surface: pure data types and trait
//! contracts that the service can implement with platform-specific code under
//! `windows`/`windows-sys`. The goal for c00.3 is to have a compilable,
//! testable scaffold that mirrors the implementation plan without yet wiring
//! Win32 calls.

use core_types::{DocKey, FileMeta, VolumeId};
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    Created(FileMeta),
    Deleted(DocKey),
    Modified { doc: DocKey },
    Renamed { from: DocKey, to: FileMeta },
    AttributesChanged { doc: DocKey },
}

/// Configuration knobs for NTFS/USN access.
#[derive(Debug, Clone)]
#[allow(dead_code)] // will be wired when Win32 integrations land
pub struct ReaderConfig {
    pub chunk_size: usize,
    pub max_records_per_tick: usize,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            chunk_size: 1 << 20,          // 1 MiB read buffer
            max_records_per_tick: 10_000, // reasonable default for service loop
        }
    }
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
    #[error("usn gap detected")]
    GapDetected,
    #[error("mft enumeration failed: {0}")]
    Mft(String),
    #[error("operation not supported on this platform")]
    NotSupported,
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
    Err(NtfsError::Discovery(
        "volume discovery only implemented on Windows".into(),
    ))
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

    let mut path_w: Vec<u16> = OsString::from(&volume.guid_path).encode_wide().collect();
    if !volume.guid_path.ends_with('\\') {
        path_w.push('\\' as u16);
    }
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

/// Enumerate the MFT for a given volume and emit file metadata snapshots.
///
/// On Windows this uses usn-journal-rs to iterate the MFT and resolve paths.
#[cfg(windows)]
pub fn enumerate_mft(volume: &VolumeInfo) -> Result<Vec<FileMeta>, NtfsError> {
    use core_types::FileFlags;
    use std::path::Path;
    use usn_journal_rs::mft::Mft;
    use usn_journal_rs::path::PathResolver;
    use usn_journal_rs::volume::Volume;

    let drive = volume
        .drive_letters
        .first()
        .copied()
        .ok_or_else(|| NtfsError::Mft("volume has no drive letter to open".into()))?;

    let vol = Volume::from_drive_letter(drive)
        .map_err(|e| NtfsError::Mft(format!("open volume {drive}: {e}")))?;
    let mut resolver = PathResolver::new(&vol);
    let mft = Mft::new(&vol).iter();

    let mut out = Vec::new();
    for entry in mft {
        let entry = entry.map_err(|e| NtfsError::Mft(format!("mft read: {e}")))?;
        let frn = entry.fid;
        let parent_frn = entry.parent_fid;
        let is_dir = entry.is_dir();

        let path = resolver
            .resolve_path(&entry)
            .and_then(|p| p.to_str().map(|s| s.to_string()));
        let name = path
            .as_deref()
            .and_then(|p| Path::new(p).file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let size = path
            .as_deref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0);

        let key = DocKey::from_parts(volume.id, frn);
        let parent = Some(DocKey::from_parts(volume.id, parent_frn));
        let flags = if is_dir {
            FileFlags::IS_DIR
        } else {
            FileFlags::empty()
        };

        out.push(FileMeta::new(
            key, volume.id, parent, name, path, size, 0, 0, flags,
        ));
    }

    Ok(out)
}

#[cfg(not(windows))]
pub fn enumerate_mft(_volume: &VolumeInfo) -> Result<Vec<FileMeta>, NtfsError> {
    Err(NtfsError::NotSupported)
}

/// Tail the USN journal for a volume and emit file events from the given cursor.
pub fn tail_usn(
    _volume: &VolumeInfo,
    _cursor: JournalCursor,
) -> Result<(Vec<FileEvent>, JournalCursor), NtfsError> {
    // TODO: connect to USN journal, read deltas, and return next cursor.
    Ok((Vec::new(), _cursor))
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
