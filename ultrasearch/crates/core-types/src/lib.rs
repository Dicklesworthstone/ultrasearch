//! Core identifiers and shared lightweight types for UltraSearch.
//!
//! These types intentionally avoid heavy dependencies and aim to be
//! serialization-friendly for rkyv/bincode and IPC payloads.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

pub type VolumeId = u16;
pub type FileId = u64;
pub type Timestamp = i64; // Unix timestamp (seconds); i64 for easy serde and fast fields.

/// Packed identifier combining a volume id and NTFS file reference number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DocKey(pub u64);

impl DocKey {
    /// Pack a `VolumeId` (high bits) and `FileId` (low bits) into a `DocKey`.
    pub const fn from_parts(volume: VolumeId, file: FileId) -> Self {
        // Use the upper 16 bits for the volume and the remaining 48 bits for the FRN.
        let packed = ((volume as u64) << 48) | (file & 0x0000_FFFF_FFFF_FFFF);
        DocKey(packed)
    }

    /// Split the packed id back into `(VolumeId, FileId)`.
    pub const fn into_parts(self) -> (VolumeId, FileId) {
        let volume = (self.0 >> 48) as VolumeId;
        let file = self.0 & 0x0000_FFFF_FFFF_FFFF;
        (volume, file)
    }
}

bitflags! {
    #[derive(Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub key: DocKey,
    pub parent: Option<DocKey>,
    pub name: String,
    pub size: u64,
    pub created: Timestamp,
    pub modified: Timestamp,
    pub flags: FileFlags,
}

/// Per-volume configuration snapshot (kept simple for now).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeSettings {
    pub volume: VolumeId,
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub content_indexing: bool,
}

pub mod config;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_key_round_trips() {
        let dk = DocKey::from_parts(42, 0x1234_5678_9abc);
        let (v, f) = dk.into_parts();
        assert_eq!(v, 42);
        assert_eq!(f, 0x1234_5678_9abc);
    }
}

pub mod config;
