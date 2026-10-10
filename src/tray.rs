//! Menu-bar (tray) presence for the resident switcher.
//!
//! GPUI ships no tray API, so this is built on `tray-icon`, the same choice
//! Sonora makes. Two constraints drive the shape of this module:
//!
//! * On macOS the status item must be created **on the main thread while an
//!   event loop is running**. GPUI's `run` closure is exactly that point, so
//!   [`install`] is called from inside it.
//! * `tray-icon` delivers menu clicks through a global callback that fires on
//!   whatever thread AppKit runs it on. We forward them into a channel and let
//!   the GUI poll, which keeps all UI mutation on the GPUI thread.
//!
//! The tray is also why the app must not exit when its window closes
//! (`QuitMode::Explicit`): the menu bar item is the primary interface once the
//! window is gone.

use std::sync::mpsc::{channel, Receiver, Sender};

use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// A click on one of our menu rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEvent {
    /// Switch this display to the given VCP 0x60 value, e.g. "0x11".
    Switch(String),
    /// Re-read the monitor and rebuild the menu.
    Refresh,
    /// Re-assert the currently selected input until it sticks.
    Reclaim,
    /// Show the main window.
    Show,
    Quit,
}

const ID_SWITCH_PREFIX: &str = "input:";
const ID_REFRESH: &str = "refresh";
const ID_RECLAIM: &str = "reclaim";
const ID_SHOW: &str = "show";
const ID_QUIT: &str = "quit";
const ID_HEADER: &str = "header";

/// Build a monochrome monitor glyph as a macOS template image.
///
/// macOS tints template images from their alpha channel, so drawing in black
/// with full alpha is correct and gives a light/dark-adaptive menu bar icon
/// without shipping any binary asset.
fn monitor_icon() -> tray_icon::Icon {
    const S: u32 = 36;
    let mut rgba = vec![0u8; (S * S * 4) as usize];
    let mut plot = |x: u32, y: u32| {
        if x < S && y < S {
            let i = ((y * S + x) * 4) as usize;
            rgba[i] = 0;
            rgba[i + 1] = 0;
            rgba[i + 2] = 0;
            rgba[i + 3] = 255;
        }
    };
    // screen: 3px border, with a gap so it reads as a monitor rather than a box
    let (x0, y0, x1, y1) = (5u32, 8u32, S - 5, 24u32);
    for x in x0..x1 {
        for t in 0..3 {
            plot(x, y0 + t);
            plot(x, y1 - 1 - t);
        }
    }
    for y in y0..y1 {
        for t in 0..3 {
            plot(x0 + t, y);
            plot(x1 - 1 - t, y);
        }
    }
    // stand
    for y in y1..y1 + 4 {
        for x in S / 2 - 2..S / 2 + 2 {
            plot(x, y);
        }
    }
    // base
    for x in S / 2 - 7..S / 2 + 7 {
        for y in y1 + 4..y1 + 7 {
            plot(x, y);
        }
    }
    tray_icon::Icon::from_rgba(rgba, S, S).expect("icon buffer is well formed")
}

/// The live menu bar item plus the rows that get rewritten on refresh.
pub struct Tray {
    /// Held so the status item stays alive; dropping it removes the icon.
    _icon: TrayIcon,
    header: MenuItem,
    reclaim: MenuItem,
    show: MenuItem,
    /// Input rows, rebuilt whenever the advertised list changes.
    rows: Vec<(String, CheckMenuItem)>,
    menu: Menu,
    events: Receiver<TrayEvent>,
}

impl Tray {
    /// Create the status item and its menu. Must run on the main thread.
    pub fn install() -> Option<Tray> {
        let (tx, rx) = channel::<TrayEvent>();
        forward_events(tx);

        let header = MenuItem::with_id(ID_HEADER, "dmux: reading monitor...", false, None);
        let refresh = MenuItem::with_id(ID_REFRESH, "Refresh", true, None);
        let reclaim = MenuItem::with_id(ID_RECLAIM, "Reclaim this input", false, None);
        let show = MenuItem::with_id(ID_SHOW, "Show window", true, None);
        let quit = MenuItem::with_id(ID_QUIT, "Quit dmux", true, None);

        let menu = Menu::new();
        if let Err(e) = menu.append_items(&[
            &header,
            &PredefinedMenuItem::separator(),
            &refresh,
            &reclaim,
            &PredefinedMenuItem::separator(),
            &show,
            &quit,
        ]) {
            eprintln!("tray: cannot build the menu: {e}");
            return None;
        }

        // macOS wants a template image so AppKit can tint it to match the menu
        // bar; other platforms take the pixels as they are.
        #[cfg(target_os = "macos")]
        let builder = TrayIconBuilder::new().with_icon_templated(monitor_icon());
        #[cfg(not(target_os = "macos"))]
        let builder = TrayIconBuilder::new().with_icon(monitor_icon());

        let icon = match builder
            .with_tooltip("dmux - monitor input switcher")
            .with_menu(Box::new(menu.clone()))
            .build()
        {
            Ok(icon) => icon,
            Err(e) => {
                eprintln!("tray: cannot create the status item: {e}");
                return None;
            }
        };

        Some(Tray {
            _icon: icon,
            header,
            reclaim,
            show,
            rows: Vec::new(),
            menu,
            events: rx,
        })
    }

    /// Non-blocking poll, called from a GPUI timer on the main thread.
    pub fn try_recv(&self) -> Option<TrayEvent> {
        self.events.try_recv().ok()
    }

    /// Rewrite the menu to match the current monitor state.
    ///
    /// `inputs` is `(code, name, is_current)`; `current` is the raw current
    /// input code, which is what "reclaim" re-asserts.
    pub fn update(
        &mut self,
        display: &str,
        current: Option<(&str, &str)>,
        inputs: &[(String, String, bool)],
    ) {
        match current {
            Some((code, name)) => {
                self.header.set_text(format!("{display} - {code} {name}"));
                self.reclaim.set_enabled(true);
                self.reclaim.set_text(format!("Reclaim {name}"));
            }
            None => {
                self.header.set_text(format!("{display} - input unknown"));
                self.reclaim.set_enabled(false);
                self.reclaim.set_text("Reclaim this input");
            }
        }
        self.show.set_text("Show window");

        // Rebuild the input rows only when the set of ports changed; keeping the
        // rows stable avoids flicker in the open menu.
        let same = self.rows.len() == inputs.len()
            && self
                .rows
                .iter()
                .zip(inputs)
                .all(|((code, _), (want, _, _))| code == want);
        if !same {
            for (_, row) in self.rows.drain(..) {
                let _ = self.menu.remove(&row);
            }
            for (code, name, _) in inputs {
                let row = CheckMenuItem::with_id(
                    format!("{ID_SWITCH_PREFIX}{code}"),
                    name.clone(),
                    true,
                    false,
                    None,
                );
                // The trailing block is [separator, show, quit]; put the port
                // rows above it.
                let at = self.menu.items().len().saturating_sub(3);
                if let Err(e) = self.menu.insert(&row, at) {
                    eprintln!("tray: cannot insert {code}: {e}");
                    continue;
                }
                self.rows.push((code.clone(), row));
            }
        }

        for (code, row) in &self.rows {
            let is_current = inputs.iter().any(|(c, _, cur)| c == code && *cur);
            row.set_checked(is_current);
        }
    }
}

/// Install the global menu handler and hand events to `tx`.
///
/// `tray-icon`'s callback is global and set-once, so this bridges it into a
/// plain channel rather than exposing the crate's types to the UI layer.
fn forward_events(tx: Sender<TrayEvent>) {
    tray_icon::menu::MenuEvent::set_event_handler(Some(move |event: tray_icon::menu::MenuEvent| {
        let id = event.id.0.as_str();
        let translated = if let Some(code) = id.strip_prefix(ID_SWITCH_PREFIX) {
            Some(TrayEvent::Switch(code.to_string()))
        } else {
            match id {
                ID_REFRESH => Some(TrayEvent::Refresh),
                ID_RECLAIM => Some(TrayEvent::Reclaim),
                ID_SHOW => Some(TrayEvent::Show),
                ID_QUIT => Some(TrayEvent::Quit),
                _ => None,
            }
        };
        if let Some(ev) = translated {
            // A dead receiver just means the app is shutting down.
            let _ = tx.send(ev);
        }
    }));
}
