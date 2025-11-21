use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use core_serialization::{from_rkyv_bytes, to_rkyv_bytes};
use rkyv::{Archive, Deserialize, Serialize};

/// Persistent state for a single volume.
#[derive(Debug, Clone, Default, Archive, Serialize, Deserialize, PartialEq, Eq)]
#[archive(check_bytes)]
pub struct VolumeState {
    pub last_usn: u64,
    pub journal_id: u64,
    pub last_mft_scan_generation: u64,
    pub settings_hash: u64,
}

impl VolumeState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = fs::read(path).context("read state file")?;
        from_rkyv_bytes::<Self>(&bytes).context("deserialize state")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = to_rkyv_bytes(self).context("serialize state")?;

        // Atomic write: write to tmp, rename.
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, &bytes).context("write tmp state file")?;
        fs::rename(&tmp_path, path).context("rename state file")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.rkyv");

        let state = VolumeState {
            last_usn: 12345,
            journal_id: 999,
            last_mft_scan_generation: 1,
            settings_hash: 0xCAFEBABE,
        };

        state.save(&path).unwrap();
        let loaded = VolumeState::load(&path).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.rkyv");
        let state = VolumeState::load(&path).unwrap();
        assert_eq!(state, VolumeState::default());
    }
}
