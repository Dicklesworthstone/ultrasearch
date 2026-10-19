use crate::{
    DocKey, ExtractContext, ExtractError, ExtractedContent, Extractor, enforce_limits_str,
    read_file_snapshot, resolve_ext, with_snapshot_tempfile,
};
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// Capabilities a plugin might require.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginCapability {
    NetworkAccess,
    FileSystemRead,
    FileSystemWrite, // Generally disallowed for extractors
}

/// Metadata about a plugin.
#[derive(Debug, Clone)]
pub struct PluginMeta {
    pub id: String,
    pub version: String,
    pub description: String,
    pub capabilities: Vec<PluginCapability>,
}

/// Trait for an extractor plugin.
///
/// In the future, this could be backed by a WASM runtime.
pub trait ExtractorPlugin: Send + Sync {
    fn meta(&self) -> PluginMeta;

    /// Check if this plugin supports the given file extension/mime.
    fn supports(&self, ext: &str) -> bool;

    /// Perform extraction from a path on disk.
    fn extract(&self, path: &Path) -> Result<String>;

    /// Perform extraction from in-memory bytes when supported directly by the plugin.
    fn extract_bytes(&self, _bytes: &[u8]) -> Option<Result<String>> {
        None
    }
}

/// Registry for managing active plugins.
#[derive(Default, Clone)]
pub struct PluginRegistry {
    plugins: Arc<RwLock<HashMap<String, Arc<dyn ExtractorPlugin>>>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, plugin: Box<dyn ExtractorPlugin>) {
        let mut map = self.plugins.write().unwrap();
        let id = plugin.meta().id;
        map.insert(id, Arc::from(plugin));
    }

    pub fn register_arc(&self, plugin: Arc<dyn ExtractorPlugin>) {
        let mut map = self.plugins.write().unwrap();
        let id = plugin.meta().id;
        map.insert(id, plugin);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn ExtractorPlugin>> {
        let map = self.plugins.read().ok()?;
        map.get(id).cloned()
    }

    /// Find the first plugin that supports the extension.
    pub fn find_for_ext(&self, ext: &str) -> Option<String> {
        let normalized = ext.trim_start_matches('.').to_ascii_lowercase();
        let map = self.plugins.read().ok()?;
        for (id, plugin) in map.iter() {
            if plugin.supports(&normalized) {
                return Some(id.clone());
            }
        }
        None
    }

    /// Execute a named plugin on a path.
    pub fn extract_with(&self, id: &str, path: &Path) -> Result<String> {
        let plugin = self
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("plugin not found: {id}"))?;
        plugin.extract(path)
    }
}

impl Extractor for PluginRegistry {
    fn name(&self) -> &'static str {
        "plugin-registry"
    }

    fn supports(&self, ctx: &ExtractContext) -> bool {
        let Some(ext) = resolve_ext(ctx) else {
            return false;
        };
        self.find_for_ext(&ext).is_some()
    }

    fn extract(&self, ctx: &ExtractContext, key: DocKey) -> Result<ExtractedContent, ExtractError> {
        let file =
            fs::File::open(Path::new(ctx.path)).map_err(|e| ExtractError::Failed(e.to_string()))?;
        self.extract_file(ctx, key, &file)
    }

    fn extract_file(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        file: &fs::File,
    ) -> Result<ExtractedContent, ExtractError> {
        let bytes = read_file_snapshot(file, ctx.max_bytes)?;
        self.extract_bytes(ctx, key, &bytes)
    }

    fn extract_bytes(
        &self,
        ctx: &ExtractContext,
        key: DocKey,
        bytes: &[u8],
    ) -> Result<ExtractedContent, ExtractError> {
        if bytes.len() > ctx.max_bytes {
            return Err(ExtractError::FileTooLarge {
                bytes: bytes.len() as u64,
                max_bytes: ctx.max_bytes as u64,
            });
        }
        let ext = resolve_ext(ctx).ok_or_else(|| ExtractError::Unsupported("unknown".into()))?;
        let plugin_id = self
            .find_for_ext(&ext)
            .ok_or_else(|| ExtractError::Unsupported(ext.clone()))?;
        let plugin = self.get(&plugin_id).ok_or(ExtractError::Unsupported(ext))?;

        let raw_text = if let Some(res) = plugin.extract_bytes(bytes) {
            res.map_err(|e| ExtractError::Failed(e.to_string()))?
        } else {
            with_snapshot_tempfile(ctx, bytes, |temp_path| {
                plugin
                    .extract(temp_path)
                    .map_err(|e| ExtractError::Failed(e.to_string()))
            })?
        };

        let (text, truncated, used_bytes) = enforce_limits_str(&raw_text, ctx);
        Ok(ExtractedContent {
            key,
            text,
            lang: None,
            truncated,
            content_lang: None,
            bytes_processed: used_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExtractorStack;

    struct MockPlugin;
    impl ExtractorPlugin for MockPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta {
                id: "mock".into(),
                version: "0.1".into(),
                description: "A mock".into(),
                capabilities: vec![],
            }
        }
        fn supports(&self, ext: &str) -> bool {
            ext == "mock"
        }
        fn extract(&self, path: &Path) -> Result<String> {
            let data = fs::read_to_string(path)?;
            Ok(format!("plugin:{data}"))
        }
    }

    #[test]
    fn registry_works_and_integrates_with_extractor_stack() {
        let registry = PluginRegistry::new();
        registry.register(Box::new(MockPlugin));

        assert!(registry.get("mock").is_some());
        let found = registry.find_for_ext("MOCK");
        assert_eq!(found.as_deref(), Some("mock"));

        let stack = ExtractorStack::with_plugin_registry(registry);
        let ctx = ExtractContext {
            path: "sample.mock",
            max_bytes: 1024,
            max_chars: 1024,
            ext_hint: Some("mock"),
            mime_hint: None,
        };
        let key = DocKey::from_parts(1, 42);
        let extracted = stack.extract_bytes(key, &ctx, b"hello world").unwrap();
        assert_eq!(extracted.text, "plugin:hello world");
    }
}
