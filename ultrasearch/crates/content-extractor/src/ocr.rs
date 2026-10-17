use crate::component_manager::{Component, ComponentManager};
use crate::{ExtractContext, ExtractError, ExtractedContent, Extractor, enforce_limits_str};
use anyhow::Result;
use core_types::DocKey;
use std::path::Path;
use std::process::Command;
use tracing::warn;

pub struct OcrExtractor {
    manager: ComponentManager,
    tesseract_component: Component,
}

impl OcrExtractor {
    pub fn new(manager: ComponentManager) -> Self {
        // Define the standard Tesseract component we expect
        // In a real app, this might come from config or a remote manifest
        let tesseract_component = Component {
            id: "tesseract".to_string(),
            version: "5.3.3".to_string(),
            // Placeholder URL - in production this would be a real release asset
            url: "https://github.com/UB-Mannheim/tesseract/releases/download/v5.3.3/tesseract-ocr-w64-setup-v5.3.3.20231005.exe".to_string(), 
            // Placeholder hash
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
            executable_name: if cfg!(windows) { "tesseract.exe" } else { "tesseract" }.to_string(),
        };

        Self {
            manager,
            tesseract_component,
        }
    }

    fn get_tesseract_path(&self) -> Option<std::path::PathBuf> {
        // Check component manager first
        if let Some(path) = self.manager.get_executable_path(&self.tesseract_component) {
            return Some(path);
        }

        // Fallback to system path
        // This simple check assumes 'tesseract' is in PATH
        if which::which("tesseract").is_ok() {
            return Some("tesseract".into());
        }

        None
    }

    fn extract_from_path(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        input_path: &Path,
    ) -> Result<ExtractedContent, ExtractError> {
        let meta =
            std::fs::metadata(input_path).map_err(|e| ExtractError::Failed(e.to_string()))?;
        let max_bytes = ctx.max_bytes as u64;
        if meta.len() > max_bytes {
            return Err(ExtractError::FileTooLarge {
                bytes: meta.len(),
                max_bytes,
            });
        }

        let tesseract_bin = self
            .get_tesseract_path()
            .ok_or_else(|| ExtractError::Failed("tesseract binary not found".into()))?;

        // Run tesseract: tesseract <image> stdout
        let output = Command::new(tesseract_bin)
            .arg(input_path)
            .arg("stdout")
            .arg("-l")
            .arg("eng")
            .output()
            .map_err(|e| ExtractError::Failed(format!("failed to spawn tesseract: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("tesseract failed for {:?}: {}", input_path, stderr);
            return Err(ExtractError::Failed("tesseract exited with error".into()));
        }

        let text_raw = String::from_utf8_lossy(&output.stdout);
        let (text, truncated, used_bytes) = enforce_limits_str(&text_raw, ctx);

        Ok(ExtractedContent {
            key,
            text,
            lang: Some("eng".into()),
            truncated,
            content_lang: None,
            bytes_processed: used_bytes,
        })
    }
}

impl Extractor for OcrExtractor {
    fn name(&self) -> &'static str {
        "ocr-tesseract"
    }

    fn supports(&self, ctx: &ExtractContext) -> bool {
        if let Some(ext) = super::resolve_ext(ctx) {
            matches!(
                ext.as_str(),
                "png" | "jpg" | "jpeg" | "tiff" | "tif" | "bmp" | "webp"
            )
        } else {
            false
        }
    }

    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        self.extract_from_path(ctx, key, Path::new(ctx.path))
    }

    fn extract_file(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        file: &std::fs::File,
    ) -> Result<ExtractedContent, ExtractError> {
        let bytes = crate::read_file_snapshot(file, ctx.max_bytes)?;
        self.extract_bytes(ctx, key, &bytes)
    }

    fn extract_bytes(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        bytes: &[u8],
    ) -> Result<ExtractedContent, ExtractError> {
        crate::with_snapshot_tempfile(ctx, bytes, |temp_path| {
            self.extract_from_path(ctx, key, temp_path)
        })
    }
}
