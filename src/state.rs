//! Persisted state that cannot be read back from the hardware.
//!
//! Why this exists: some monitors report a **constant** VCP 0x60 regardless of
//! the selected input. On such a display the readback says `0x10` both while it
//! is showing this host and after it has been switched to another machine, even
//! though a switch commanded to `0x0f` demonstrably moves the display. The write
//! works; the readback is not evidence.
//!
//! So "which input is this host on" cannot be derived from the monitor. We
//! record what the user last asked for and treat that as the truth, with the
//! readback as a hint only. Getting this wrong is not cosmetic: the reclaim
//! action re-asserts the current input, so a wrong idea of "current" would
//! actively switch the display to the *other* machine.

use std::path::PathBuf;

/// Where the state file lives. Kept next to the other per-user config so it
/// survives rebuilds and does not pollute the repository.
fn state_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var("HOME").ok()?).join("Library/Application Support")
    } else if cfg!(target_os = "windows") {
        PathBuf::from(std::env::var("APPDATA").ok()?)
    } else {
        PathBuf::from(std::env::var("HOME").ok()?).join(".config")
    };
    Some(base.join("displaymux").join("state.json"))
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// The input code this host was last switched to, e.g. "0x0f".
    ///
    /// Held per display so a multi-monitor setup records each one separately.
    #[serde(default)]
    pub last_input: std::collections::HashMap<String, String>,
}

/// The display key used in the state file: stable across launches, unlike the
/// enumeration index.
fn key(display_id: &str) -> String {
    display_id.to_string()
}

impl State {
    pub fn load() -> State {
        let Some(path) = state_path() else {
            return State::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = state_path() else { return };
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            // Best effort: failing to persist must never break a switch.
            let _ = std::fs::write(path, text);
        }
    }

    /// The input this host was last switched to on `display_id`, if known.
    pub fn last_input(&self, display_id: &str) -> Option<&str> {
        self.last_input.get(&key(display_id)).map(|s| s.as_str())
    }

    pub fn remember_input(&mut self, display_id: &str, code: &str) {
        self.last_input
            .insert(key(display_id), code.to_ascii_lowercase());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_per_display() {
        let mut s = State::default();
        s.remember_input("monitor-a", "0x0F");
        s.remember_input("Other", "0x11");
        assert_eq!(s.last_input("monitor-a"), Some("0x0f"));
        assert_eq!(s.last_input("Other"), Some("0x11"));
        assert_eq!(s.last_input("unknown"), None);
    }

    #[test]
    fn round_trips_through_json() {
        let mut s = State::default();
        s.remember_input("monitor-a", "0x0f");
        let text = serde_json::to_string(&s).unwrap();
        let back: State = serde_json::from_str(&text).unwrap();
        assert_eq!(back.last_input("monitor-a"), Some("0x0f"));
    }

    #[test]
    fn tolerates_missing_or_foreign_file() {
        // An empty file, or one written by a future version, must not panic.
        let empty: State = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.last_input("monitor-a"), None);
        let extra: State =
            serde_json::from_str(r#"{"last_input":{"d":"0x10"},"future_field":42}"#).unwrap();
        assert_eq!(extra.last_input("d"), Some("0x10"));
    }
}
