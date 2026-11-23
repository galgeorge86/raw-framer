use crate::{Error, Result, SharedSource};
use std::path::Path;

pub type SourceOpener = Box<dyn Fn(&Path) -> Result<SharedSource> + Send + Sync>;

/// Maps file extensions to backend openers.
#[derive(Default)]
pub struct SourceRegistry {
    entries: Vec<(Vec<String>, String, SourceOpener)>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a backend for the given lowercase extensions (without dot).
    pub fn register(&mut self, name: &str, extensions: &[&str], opener: SourceOpener) {
        self.entries.push((
            extensions.iter().map(|e| e.to_ascii_lowercase()).collect(),
            name.to_string(),
            opener,
        ));
    }

    pub fn extensions(&self) -> Vec<String> {
        self.entries.iter().flat_map(|(e, _, _)| e.clone()).collect()
    }

    pub fn can_open(&self, path: &Path) -> bool {
        let ext = ext_of(path);
        self.entries.iter().any(|(e, _, _)| e.contains(&ext))
    }

    pub fn open(&self, path: &Path) -> Result<SharedSource> {
        let ext = ext_of(path);
        for (exts, _, opener) in &self.entries {
            if exts.contains(&ext) {
                return opener(path);
            }
        }
        Err(Error::Unsupported(path.display().to_string()))
    }
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}
