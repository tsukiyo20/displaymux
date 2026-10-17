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

/// Write a file so that a crash mid-write cannot leave a truncated one.
///
/// `fs::write` truncates first, so being killed between the truncate and the
/// last byte leaves invalid JSON on disk -- and every reader in this crate
/// treats unparseable config as "no config", which would silently discard the
/// user's labels. Writing a sibling temp file and renaming over the target is
/// atomic on every platform we support.
pub fn write_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
