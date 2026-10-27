use crate::categories::Category;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Engine-level settings, persisted as `settings.json` in the data dir.
/// `extra` carries app/UI preferences (theme, tray, notifications…) without the core knowing them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub download_dir: PathBuf,
    /// Put files in per-category sub-folders (Video, Music, …).
    pub category_folders: bool,
    /// Per-category overrides of the folder.
    pub category_dirs: HashMap<Category, PathBuf>,
    /// Connections per download (1..=32).
    pub max_connections: u32,
    /// Downloads running at the same time (1..=16).
    pub max_concurrent: u32,
    /// Global speed limit in bytes/s, 0 = unlimited.
    pub speed_limit: u64,
    pub proxy: Option<String>,
    /// Resume unfinished downloads automatically when the app starts.
    pub auto_resume: bool,
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for Settings {
    fn default() -> Self {
        let base = dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            download_dir: base,
            category_folders: true,
            category_dirs: HashMap::new(),
            max_connections: 8,
            max_concurrent: 3,
            speed_limit: 0,
            proxy: None,
            auto_resume: false,
            extra: Default::default(),
        }
    }
}

impl Settings {
    pub fn sanitized(mut self) -> Self {
        self.max_connections = self.max_connections.clamp(1, 32);
        self.max_concurrent = self.max_concurrent.clamp(1, 16);
        self
    }

    /// Folder a file of `category` should be saved in.
    pub fn dir_for(&self, category: Category) -> PathBuf {
        if let Some(d) = self.category_dirs.get(&category) {
            return d.clone();
        }
        if self.category_folders {
            self.download_dir.join(category.folder())
        } else {
            self.download_dir.clone()
        }
    }
}
