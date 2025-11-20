//! Common serialization helpers shared across the workspace.

use core_types::{DocKey, FileId, VolumeId};

/// Minimal wire-safe representation of a document key.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub struct DocKeyWire {
    pub volume: VolumeId,
    pub file: FileId,
}

impl From<DocKey> for DocKeyWire {
    fn from(value: DocKey) -> Self {
        let (volume, file) = value.into_parts();
        Self { volume, file }
    }
}

impl From<DocKeyWire> for DocKey {
    fn from(value: DocKeyWire) -> Self {
        DocKey::from_parts(value.volume, value.file)
    }
}
