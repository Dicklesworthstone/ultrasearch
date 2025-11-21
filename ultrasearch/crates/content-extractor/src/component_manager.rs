use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Component {
    pub id: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub executable_name: String,
}

pub struct ComponentManager {
    base_dir: PathBuf,
}

impl ComponentManager {
    pub fn new(base_dir: &Path) -> Self {
        Self {
            base_dir: base_dir.to_path_buf(),
        }
    }

    pub fn with_default_path() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("com", "ultrasearch", "bin")
            .ok_or_else(|| anyhow::anyhow!("failed to resolve local data dir"))?;
        Ok(Self::new(dirs.data_dir()))
    }

    pub fn component_path(&self, component: &Component) -> PathBuf {
        self.base_dir.join(&component.id).join(&component.version)
    }

    pub fn get_executable_path(&self, component: &Component) -> Option<PathBuf> {
        let path = self
            .component_path(component)
            .join(&component.executable_name);
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    pub fn is_installed(&self, component: &Component) -> bool {
        self.get_executable_path(component).is_some()
    }

    pub async fn install(&self, component: &Component) -> Result<PathBuf> {
        if let Some(path) = self.get_executable_path(component) {
            return Ok(path);
        }

        let install_dir = self.component_path(component);
        if install_dir.exists() {
            fs::remove_dir_all(&install_dir)?;
        }
        fs::create_dir_all(&install_dir)?;

        tracing::info!("Downloading component {} from {}", component.id, component.url);

        let response = reqwest::get(&component.url).await?;
        if !response.status().is_success() {
            anyhow::bail!("failed to download: {}", response.status());
        }

        let content = response.bytes().await?;
        
        // Verify SHA256
        let mut hasher = Sha256::new();
        hasher.update(&content);
        let hash = format!("{:x}", hasher.finalize());
        
        if hash != component.sha256 {
            anyhow::bail!("hash mismatch for {}: expected {}, got {}", component.id, component.sha256, hash);
        }

        // Extract or write
        // Assume zip for now if url ends with zip, else single binary
        if component.url.ends_with(".zip") {
            let reader = std::io::Cursor::new(content);
            let mut archive = zip::ZipArchive::new(reader)?;
            archive.extract(&install_dir)?;
        } else {
            let target = install_dir.join(&component.executable_name);
            let mut file = File::create(&target)?;
            file.write_all(&content)?;
            
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&target)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&target, perms)?;
            }
        }

        self.get_executable_path(component)
            .ok_or_else(|| anyhow::anyhow!("executable not found after install"))
    }
}
