//! Named groups of input switches: one command to put several displays where
//! you want them.
//!
//! A desk with two computers and two monitors has four ports to keep straight,
//! and the interesting states are whole arrangements, not individual ports. A
//! layout names one of those arrangements:
//!
//! ```json
//! {
//!   "work":  { "R32U81": "0x10", "MAG 274URFW": "0x0f" },
//!   "play":  { "R32U81": "0x0f", "MAG 274URFW": "0x12" }
//! }
//! ```
//!
//! Keys of the inner map are display ids as `dmux list` prints them, then the
//! VCP 0x60 value. Keying on the id rather than the index matters: indices are
//! assigned by the system in enumeration order and change when a display
//! sleeps, wakes, or is unplugged, while the id is the monitor's own name.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Overrides the file location, for the tests and for a portable install.
const ENV: &str = "DMUX_LAYOUTS";

/// Layout name -> (display id -> VCP 0x60 code).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Layouts(BTreeMap<String, BTreeMap<String, String>>);

/// Resolve where the file lives.
fn resolved_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var(ENV) {
        return Some(PathBuf::from(path));
    }
    Some(crate::config::dir()?.join("layouts.json"))
}

impl Layouts {
    /// Read the file, or an empty set if it is absent or unreadable.
    ///
    /// A malformed file is treated as absent, like the label file: refusing to
    /// switch a display because a JSON file has a typo would be a poor trade,
    /// and `dmux layouts` is where the problem is reported.
    pub fn load() -> Layouts {
        let Some(path) = resolved_path() else {
            return Layouts::default();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Layouts::default();
        };
        let Ok(raw) = serde_json::from_str::<BTreeMap<String, BTreeMap<String, String>>>(&text)
        else {
            return Layouts::default();
        };
        Layouts(
            raw.into_iter()
                .map(|(name, targets)| {
                    let targets = targets
                        .into_iter()
                        .map(|(display, code)| (display, normalize(&code)))
                        .collect();
                    (name, targets)
                })
                .collect(),
        )
    }

    /// Write the file, atomically.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = resolved_path() else {
            return Err(std::io::Error::other("no configuration directory"));
        };
        let text = serde_json::to_string_pretty(&self.0)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        crate::config::write_atomic(&path, &(text + "\n"))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every layout, in name order.
    pub fn entries(&self) -> impl Iterator<Item = (&String, &BTreeMap<String, String>)> {
        self.0.iter()
    }

    /// One layout's targets.
    pub fn get(&self, name: &str) -> Option<&BTreeMap<String, String>> {
        self.0.get(name)
    }

    /// Define or replace a layout.
    pub fn set(&mut self, name: &str, targets: BTreeMap<String, String>) {
        self.0.insert(name.to_string(), targets);
    }

    /// Forget a layout. Returns whether it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        self.0.remove(name).is_some()
    }
}

/// One switch a layout asks for, resolved against the displays that exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub display_index: usize,
    pub display: String,
    pub code: String,
}

/// Turn a layout into concrete switches for the displays actually present.
///
/// The second return value names displays the layout mentions that are not
/// connected. A layout that silently half-applies is worse than one that says
/// which part it could not do: the user would be left wondering which monitor
/// is showing what.
pub fn plan(
    targets: &BTreeMap<String, String>,
    displays: &[crate::ddc::DisplayRef],
) -> (Vec<Step>, Vec<String>) {
    let mut steps = Vec::new();
    let mut missing = Vec::new();
    for (display, code) in targets {
        match displays.iter().find(|d| &d.id == display) {
            Some(found) => steps.push(Step {
                display_index: found.index,
                display: display.clone(),
                code: normalize(code),
            }),
            None => missing.push(display.clone()),
        }
    }
    (steps, missing)
}

/// Canonical spelling of a VCP code: `0x0f`, never `0xF` or `0xf`.
fn normalize(code: &str) -> String {
    let trimmed = code.trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    match u16::from_str_radix(digits, 16) {
        Ok(value) => format!("0x{value:02x}"),
        Err(_) => trimmed.to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddc::DisplayRef;

    fn displays() -> Vec<DisplayRef> {
        vec![
            DisplayRef {
                index: 0,
                id: "MAG 274URFW".to_string(),
            },
            DisplayRef {
                index: 1,
                id: "R32U81".to_string(),
            },
        ]
    }

    #[test]
    fn a_layout_resolves_by_display_id_not_index() {
        let mut targets = BTreeMap::new();
        targets.insert("R32U81".to_string(), "0x10".to_string());
        let (steps, missing) = plan(&targets, &displays());
        assert!(missing.is_empty());
        assert_eq!(
            steps,
            vec![Step {
                display_index: 1,
                display: "R32U81".to_string(),
                code: "0x10".to_string()
            }]
        );
    }

    #[test]
    fn displays_that_are_not_connected_are_named() {
        let mut targets = BTreeMap::new();
        targets.insert("R32U81".to_string(), "0x10".to_string());
        targets.insert("gone".to_string(), "0x11".to_string());
        let (steps, missing) = plan(&targets, &displays());
        assert_eq!(steps.len(), 1);
        assert_eq!(missing, vec!["gone".to_string()]);
    }

    #[test]
    fn codes_are_canonicalised() {
        assert_eq!(normalize("0xF"), "0x0f");
        assert_eq!(normalize(" 0X10 "), "0x10");
        assert_eq!(normalize("11"), "0x11");
    }

    #[test]
    fn a_loaded_file_normalises_its_codes() {
        let dir = std::env::temp_dir().join("dmux-layouts-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layouts.json");
        std::fs::write(&path, r#"{"work": {"R32U81": "0xF"}}"#).unwrap();
        // SAFETY: the test binary is single-threaded for this variable, and the
        // value is only read by `resolved_path`.
        unsafe { std::env::set_var(ENV, &path) };
        let layouts = Layouts::load();
        unsafe { std::env::remove_var(ENV) };
        assert_eq!(layouts.get("work").unwrap().get("R32U81").unwrap(), "0x0f");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
