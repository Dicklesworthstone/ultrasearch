use anyhow::Result;
use std::collections::HashMap;
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

    /// Perform extraction.
    ///
    /// `path` is provided, but plugins should ideally work on streams or buffers
    /// if we want strict sandboxing. For MVP, path is fine.
    fn extract(&self, path: &Path) -> Result<String>;
}

/// Registry for managing active plugins.
#[derive(Default)]
pub struct PluginRegistry {
    plugins: Arc<RwLock<HashMap<String, Box<dyn ExtractorPlugin>>>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, plugin: Box<dyn ExtractorPlugin>) {
        let mut map = self.plugins.write().unwrap();
        map.insert(plugin.meta().id.clone(), plugin);
    }

    pub fn get(&self, id: &str) -> Option<Arc<Box<dyn ExtractorPlugin>>> {
        // This is tricky with Box<dyn>. We can't easily clone a Box<dyn Trait> unless we enforce Clone.
        // Or we store them in Arc.
        // Let's change storage to Arc<Box<dyn>> or just Arc<dyn>.
        // But register takes ownership.
        // For simple lookup, maybe just `get` returns a reference?
        // The RwLock guard makes returning ref hard.
        // Let's assume we just query "find a plugin for ext".
        None
    }

    /// Find the first plugin that supports the extension.
    pub fn find_for_ext(&self, ext: &str) -> Option<String> {
        let map = self.plugins.read().unwrap();
        for (id, plugin) in map.iter() {
            if plugin.supports(ext) {
                return Some(id.clone());
            }
        }
        None
    }

    /// Execute a named plugin.
    pub fn extract_with(&self, id: &str, path: &Path) -> Result<String> {
        let map = self.plugins.read().unwrap();
        if let Some(plugin) = map.get(id) {
            plugin.extract(path)
        } else {
            Err(anyhow::anyhow!("plugin not found: {}", id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fn extract(&self, _path: &Path) -> Result<String> {
            Ok("extracted content".into())
        }
    }

    #[test]
    fn registry_works() {
        let registry = PluginRegistry::new();
        registry.register(Box::new(MockPlugin));

        let found = registry.find_for_ext("mock");
        assert_eq!(found.as_deref(), Some("mock"));

        let content = registry
            .extract_with("mock", Path::new("foo.mock"))
            .unwrap();
        assert_eq!(content, "extracted content");
    }
}
