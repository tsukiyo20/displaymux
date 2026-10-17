//! Where this tool keeps its own files.
//!
//! One directory, following each platform's convention, so the settings file
//! and the port labels sit together and neither pollutes the repository.

use std::path::PathBuf;

/// `displaymux`'s per-user config directory, e.g.
/// `~/Library/Application Support/displaymux` on macOS.
pub fn dir() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var("HOME").ok()?).join("Library/Application Support")
    } else if cfg!(target_os = "windows") {
        PathBuf::from(std::env::var("APPDATA").ok()?)
    } else {
        PathBuf::from(std::env::var("HOME").ok()?).join(".config")
    };
    Some(base.join("displaymux"))
}
