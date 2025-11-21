use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use memmap2::Mmap;

/// A safe wrapper around a memory-mapped file that provides shared access.
#[derive(Clone, Debug)]
pub struct MmapArea {
    inner: Arc<Mmap>,
}

impl MmapArea {
    /// Open a file and map it into memory as read-only.
    pub fn open(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let mmap = unsafe {
            Mmap::map(&file).with_context(|| format!("failed to mmap {}", path.display()))?
        };
        Ok(Self {
            inner: Arc::new(mmap),
        })
    }

    /// Return the entire mapped area as a byte slice.
    pub fn as_slice(&self) -> &[u8] {
        &self.inner[..]
    }
}

impl AsRef<[u8]> for MmapArea {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}
