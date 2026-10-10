//! Core identifiers and shared lightweight types for UltraSearch.
//!
//! These types intentionally avoid heavy dependencies and aim to be
//! serialization-friendly for rkyv/bincode and IPC payloads.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub type VolumeId = u16;
pub type FileId = u64;
pub type Timestamp = i64; // Unix timestamp (seconds); i64 for easy serde and fast fields.

/// Lossless identifier combining a volume id and the full NTFS file reference number.
///
/// NTFS uses the high 16 bits of a file reference for its sequence number. They
/// must survive indexing: an MFT slot reused by a different file is a different
/// document. The volume occupies bits 64..80; all 64 FRN bits remain intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocKey(pub u128);

impl DocKey {
    /// Pack a `VolumeId` and the complete `FileId` into a `DocKey`.
    pub const fn from_parts(volume: VolumeId, file: FileId) -> Self {
        let packed = ((volume as u128) << 64) | file as u128;
        DocKey(packed)
    }

    /// Split the packed id back into `(VolumeId, FileId)`.
    pub const fn into_parts(self) -> (VolumeId, FileId) {
        let volume = (self.0 >> 64) as VolumeId;
        let file = self.0 as FileId;
        (volume, file)
    }

    /// Return the volume id component.
    pub const fn volume(self) -> VolumeId {
        (self.0 >> 64) as VolumeId
    }

    /// Return the file id (FRN) component.
    pub const fn file_id(self) -> FileId {
        self.0 as FileId
    }
}

impl core::fmt::Display for DocKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (v, id) = self.into_parts();
        write!(f, "{}:{:#018x}", v, id)
    }
}

impl FromStr for DocKey {
    type Err = &'static str;

    /// Parses the Display form: `<volume>:0x<frn_hex>`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (vol_part, frn_part) = s.split_once(':').ok_or("missing ':'")?;
        let volume: VolumeId = vol_part.parse().map_err(|_| "invalid volume id")?;
        let frn_hex = frn_part.strip_prefix("0x").ok_or("missing 0x prefix")?;
        let file = u64::from_str_radix(frn_hex, 16).map_err(|_| "invalid frn hex")?;
        Ok(DocKey::from_parts(volume, file))
    }
}

impl Serialize for DocKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0 >> 80 != 0 {
            return Err(serde::ser::Error::custom("document key exceeds 80 bits"));
        }
        if serializer.is_human_readable() {
            // JSON numbers do not universally preserve even u64 values. Strings
            // also survive serde's buffered enum/map representations losslessly.
            serializer.collect_str(self)
        } else {
            self.0.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for DocKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let encoded = String::deserialize(deserializer)?;
            encoded.parse().map_err(serde::de::Error::custom)
        } else {
            let value = u128::deserialize(deserializer)?;
            if value >> 80 != 0 {
                return Err(serde::de::Error::custom("document key exceeds 80 bits"));
            }
            Ok(Self(value))
        }
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct FileFlags: u32 {
        const IS_DIR   = 0b0000_0001;
        const HIDDEN   = 0b0000_0010;
        const SYSTEM   = 0b0000_0100;
        const ARCHIVE  = 0b0000_1000;
        const REPARSE  = 0b0001_0000;
        const OFFLINE  = 0b0010_0000;
        const TEMPORARY= 0b0100_0000;
    }
}

/// Minimal metadata carried through indexing pipelines.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMeta {
    pub key: DocKey,
    pub volume: VolumeId,
    pub parent: Option<DocKey>,
    pub name: String,
    pub ext: Option<String>,
    pub path: Option<String>,
    pub size: u64,
    pub created: Timestamp,
    pub modified: Timestamp,
    pub flags: FileFlags,
}

impl FileMeta {
    /// Create a new FileMeta, deriving extension if not provided.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: DocKey,
        volume: VolumeId,
        parent: Option<DocKey>,
        name: String,
        path: Option<String>,
        size: u64,
        created: Timestamp,
        modified: Timestamp,
        flags: FileFlags,
    ) -> Self {
        let ext = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase());
        Self {
            key,
            volume,
            parent,
            name,
            ext,
            path,
            size,
            created,
            modified,
            flags,
        }
    }
}

/// Per-volume configuration snapshot (kept simple for now).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeSettings {
    pub volume: VolumeId,
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub content_indexing: bool,
}

/// Basic descriptor for a discovered NTFS volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeDescriptor {
    pub id: VolumeId,
    /// NT-style volume GUID path, e.g. `\\\\?\\Volume{...}\\`
    pub guid_path: String,
    /// Optional drive letters mapped to this volume, e.g. ["C:", "D:"].
    pub drive_letters: Vec<String>,
}

pub mod config;

impl FileFlags {
    pub fn is_dir(self) -> bool {
        self.contains(Self::IS_DIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_key_round_trips() {
        let dk = DocKey::from_parts(42, 0xfedc_1234_5678_9abc);
        let (v, f) = dk.into_parts();
        assert_eq!(v, 42);
        assert_eq!(f, 0xfedc_1234_5678_9abc);
        assert_eq!(dk.file_id(), f);
        assert_eq!(dk.volume(), v);
    }

    #[test]
    fn reused_mft_slots_and_different_volumes_have_distinct_keys() {
        let original = DocKey::from_parts(1, 0x0001_0000_0000_002a);
        let reused = DocKey::from_parts(1, 0x0002_0000_0000_002a);
        let other_volume = DocKey::from_parts(2, 0x0001_0000_0000_002a);
        assert_ne!(original, reused);
        assert_ne!(original, other_volume);
        let maximum = DocKey::from_parts(u16::MAX, u64::MAX);
        assert_eq!(maximum.into_parts(), (u16::MAX, u64::MAX));
    }

    #[test]
    fn file_meta_ext_derives_lowercase() {
        let key = DocKey::from_parts(1, 2);
        let fm = FileMeta::new(
            key,
            1,
            None,
            "Report.PDF".to_string(),
            None,
            10,
            0,
            0,
            FileFlags::empty(),
        );
        assert_eq!(fm.ext.as_deref(), Some("pdf"));
    }

    #[test]
    fn doc_key_display_is_stable() {
        let dk = DocKey::from_parts(7, 0xabc);
        assert_eq!(dk.to_string(), "7:0x0000000000000abc");
    }

    #[test]
    fn doc_key_parse_round_trip() {
        let original = DocKey::from_parts(9, 0xface_cafe_feed_beef);
        let parsed: DocKey = original.to_string().parse().unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn journal_file_meta_json_round_trip_preserves_full_reference_numbers() {
        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        enum MetadataChange {
            Upsert(FileMeta),
            Delete(DocKey),
        }
        let key = DocKey::from_parts(u16::MAX, 0xfedc_ba98_7654_3210);
        let parent = DocKey::from_parts(u16::MAX, 0xabcd_0000_0000_0005);
        let meta = FileMeta::new(
            key,
            u16::MAX,
            Some(parent),
            "report.txt".into(),
            Some(r"C:\report.txt".into()),
            42,
            1,
            2,
            FileFlags::ARCHIVE,
        );
        let bytes = serde_json::to_vec(&meta).unwrap();
        let decoded: FileMeta = serde_json::from_reader(bytes.as_slice()).unwrap();
        assert_eq!(decoded, meta);
        assert_eq!(decoded.key.file_id(), 0xfedc_ba98_7654_3210);
        assert_eq!(decoded.parent, Some(parent));
        let changes = vec![MetadataChange::Upsert(meta), MetadataChange::Delete(parent)];
        let bytes = serde_json::to_vec(&changes).unwrap();
        let decoded: Vec<MetadataChange> = serde_json::from_reader(bytes.as_slice()).unwrap();
        assert_eq!(decoded, changes);
        let value = serde_json::to_value(&changes).unwrap();
        assert_eq!(
            serde_json::from_value::<Vec<MetadataChange>>(value).unwrap(),
            changes
        );
    }

    #[test]
    fn volume_descriptor_holds_letters() {
        let vd = VolumeDescriptor {
            id: 1,
            guid_path: r"\\?\Volume{abc}\\".to_string(),
            drive_letters: vec!["C:".into(), "D:".into()],
        };
        assert_eq!(vd.id, 1);
        assert_eq!(vd.drive_letters.len(), 2);
    }
}
