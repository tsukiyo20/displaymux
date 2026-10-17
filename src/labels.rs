//! User-supplied names for a monitor's inputs.
//!
//! DDC names ports in MCCS terms, and those do not describe the physical
//! connector. A monitor whose USB-C port carries DisplayPort alt-mode normally
//! reports it as one of the DisplayPort values -- on the monitor this was
//! written against, the socket labelled Type-C answers as "DisplayPort 2".
//! Nothing in the DDC transaction distinguishes the two, so which code is which
//! socket is a decision only the person at the desk can make.
//!
//! The mapping therefore lives in a small file rather than being guessed:
//!
//! ```json
//! {
//!   "R32U81": {
//!     "0x0f": "DP - Windows",
//!     "0x10": "Type-C - Mac"
//!   }
//! }
//! ```
//!
//! Keys are the display id shown by `dmux list`, then the VCP 0x60 value in
//! hex. Capitalisation of the code does not matter. Anything not listed keeps
//! its MCCS name, so the file can be partial or absent.

use std::collections::HashMap;
use std::path::PathBuf;

/// Overrides the file location. Used by the tests, and by a portable install.
const ENV: &str = "DMUX_LABELS";

/// Where the label file lives, following each platform's config convention.
pub fn labels_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var("HOME").ok()?).join("Library/Application Support")
    } else if cfg!(target_os = "windows") {
        PathBuf::from(std::env::var("APPDATA").ok()?)
    } else {
        PathBuf::from(std::env::var("HOME").ok()?).join(".config")
    };
    Some(base.join("displaymux").join("labels.json"))
}

fn resolved_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(ENV) {
        return Some(PathBuf::from(p));
    }
    labels_path()
}

/// Display id -> (lower-cased VCP code -> label).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Labels(HashMap<String, HashMap<String, String>>);

impl Labels {
    /// Read the file, or an empty mapping if it is absent or unreadable.
    ///
    /// A malformed file is treated as absent rather than reported: labels are
    /// cosmetic, and refusing to switch inputs because a JSON file has a typo
    /// would be a poor trade.
    pub fn load() -> Labels {
        let Some(path) = resolved_path() else {
            return Labels::default();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Labels::default();
        };
        let Ok(raw) = serde_json::from_str::<HashMap<String, HashMap<String, String>>>(&text)
        else {
            return Labels::default();
        };
        Labels(
            raw.into_iter()
                .map(|(display, entries)| {
                    let entries = entries
                        .into_iter()
                        .map(|(code, label)| (normalize(&code), label))
                        .collect();
                    (display, entries)
                })
                .collect(),
        )
    }

    /// The label for `code` on `display`, if the user supplied one.
    pub fn get(&self, display: &str, code: &str) -> Option<&str> {
        self.0
            .get(display)?
            .get(&normalize(code))
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every display and its mapping, for `dmux labels`.
    pub fn entries(&self) -> impl Iterator<Item = (&String, &HashMap<String, String>)> {
        self.0.iter()
    }

    /// Set one label, creating the display's entry if needed.
    pub fn set(&mut self, display: &str, code: &str, label: &str) {
        self.0
            .entry(display.to_string())
            .or_default()
            .insert(normalize(code), label.to_string());
    }

    /// Write the file, creating its directory. Returns where it was written.
    pub fn save(&self) -> std::io::Result<PathBuf> {
        let path =
            resolved_path().ok_or_else(|| std::io::Error::other("no writable config directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&path, text + "\n")?;
        Ok(path)
    }
}

/// VCP codes are written inconsistently (`0x0F`, `0x0f`, `0xf`); compare them
/// all in one canonical form.
fn normalize(code: &str) -> String {
    let trimmed = code.trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    match u8::from_str_radix(digits, 16) {
        Ok(v) => format!("0x{:02x}", v),
        Err(_) => trimmed.to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_code_spelling() {
        assert_eq!(normalize("0x0F"), "0x0f");
        assert_eq!(normalize("0xf"), "0x0f");
        assert_eq!(normalize(" 0X10 "), "0x10");
        assert_eq!(normalize("0x0f"), "0x0f");
    }

    #[test]
    fn looks_up_by_display_and_code() {
        let mut by_display = HashMap::new();
        let mut r32 = HashMap::new();
        r32.insert("0x10".to_string(), "Type-C".to_string());
        by_display.insert("R32U81".to_string(), r32);
        let labels = Labels(by_display);

        assert_eq!(labels.get("R32U81", "0x10"), Some("Type-C"));
        assert_eq!(labels.get("R32U81", "0X10"), Some("Type-C"));
        // unknown code, unknown display, and a different display's entry
        assert_eq!(labels.get("R32U81", "0x0f"), None);
        assert_eq!(labels.get("other", "0x10"), None);
    }

    #[test]
    fn missing_file_is_an_empty_mapping() {
        // point at a path that cannot exist, so this does not depend on the
        // developer's own labels file
        std::env::set_var(ENV, "/nonexistent/displaymux/labels.json");
        let labels = Labels::load();
        std::env::remove_var(ENV);
        assert!(labels.is_empty());
    }
}
