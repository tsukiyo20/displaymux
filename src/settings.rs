//! Preferences that survive a restart.
//!
//! Deliberately separate from the port labels: labels describe hardware, this
//! describes the person using it. Both live in [`crate::config::dir`].

use std::path::PathBuf;

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// BCP 47 tag, e.g. `"en"` or `"zh-CN"`. `None` means "not chosen yet", in
    /// which case the UI keeps the compile-time default.
    #[serde(default)]
    pub locale: Option<String>,
}

fn path() -> Option<PathBuf> {
    Some(crate::config::dir()?.join("settings.json"))
}

impl Settings {
    /// Read the file, or defaults if it is absent or unreadable.
    pub fn load() -> Settings {
        let Some(path) = path() else {
            return Settings::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<PathBuf> {
        let path = path().ok_or_else(|| std::io::Error::other("no config directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&path, text + "\n")?;
        Ok(path)
    }
}
