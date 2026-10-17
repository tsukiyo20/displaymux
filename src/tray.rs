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

use async_channel::Sender;

use rust_i18n::t;
use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// A click on one of our menu rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEvent {
    /// Switch this display to the given VCP 0x60 value, e.g. "0x11".
    Switch(String),
    /// Re-read the monitor and rebuild the menu.
    Refresh,
    /// Show the main window.
    Show,
    Quit,
}

const ID_SWITCH_PREFIX: &str = "input:";
const ID_REFRESH: &str = "refresh";
const ID_SHOW: &str = "show";
const ID_QUIT: &str = "quit";
const ID_HEADER: &str = "header";

/// Whether a left click opens the menu, or does the obvious thing instead.
///
/// macOS opens a status item's menu on any click, so it stays on there. Windows
/// anchors a pop-up menu by its **bottom** edge at the pointer, which puts the
/// menu's last row under the pointer the moment it opens -- and the last row
/// here is "Quit dmux", so a double click on the icon quits the app outright.
/// Elsewhere a left click therefore brings the window back, and the menu is
/// reached deliberately, with the right button.
const MENU_ON_LEFT_CLICK: bool = cfg!(target_os = "macos");

/// The displaymux mark: two inputs converging into one live output.
///
/// The geometry is the one `tools/make-icon.py` emits, expressed in that
/// script's 1024pt app-icon space and scaled down here -- so the menu bar, the
/// app icon and the README all show literally the same mark.
///
/// A macOS template image is tinted from its alpha channel alone, so the
/// live/dormant distinction that colour carries in the app icon becomes
/// opacity: the dormant branch is drawn at `DORMANT_ALPHA` and AppKit renders
/// it correspondingly lighter.
///
/// Strokes are rasterised from a signed distance field rather than plotted
/// pixel by pixel, which is what keeps the diagonals and the round caps smooth
/// without shipping a binary asset.
fn mux_icon() -> tray_icon::Icon {
    /// `tray-icon` sizes this bitmap into a 22pt-tall `NSImage` (its
    /// `MAX_ICON_HEIGHT`). Supplying fewer pixels than 22pt needs on a Retina
    /// display resamples twice and looks soft, so render at 4x.
    const BITMAP_PX: u32 = 88;
    const NSIMAGE_PT: f32 = 22.0;
    /// How tall the mark should read inside the menu bar.
    const MARK_PT: f32 = 16.0;
    const DORMANT_ALPHA: f32 = 0.45;

    /// Glyph vertices in app-icon space, mirroring `tools/make-icon.py`.
    /// Live branch: full length, ending in the stem that leaves for the display.
    const LIVE: [(f32, f32); 4] = [(-150.0, -330.0), (-150.0, -96.0), (0.0, 80.0), (0.0, 330.0)];
    /// Dormant branch: a shorter stub, which is what stops the mark reading as "Y".
    const DORMANT: [(f32, f32); 3] = [(150.0, -250.0), (150.0, -96.0), (0.0, 80.0)];
    const STROKE: f32 = 92.0;
    /// Top-to-bottom extent of the finished mark, stroke included.
    const EXTENT: f32 = 752.0;

    let scale = (MARK_PT / NSIMAGE_PT) * BITMAP_PX as f32 / EXTENT;
    let half = STROKE * scale / 2.0;
    let centre = BITMAP_PX as f32 / 2.0;
    let place = |points: &[(f32, f32)]| -> Vec<(f32, f32)> {
        points
            .iter()
            .map(|&(x, y)| (x * scale, y * scale))
            .collect()
    };
    let live = place(&LIVE);
    let dormant = place(&DORMANT);

    fn distance_to_segment(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
        let (ax, ay) = a;
        let (vx, vy) = (b.0 - ax, b.1 - ay);
        let len2 = vx * vx + vy * vy;
        let t = if len2 > 0.0 {
            (((px - ax) * vx + (py - ay) * vy) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (dx, dy) = (px - (ax + t * vx), py - (ay + t * vy));
        (dx * dx + dy * dy).sqrt()
    }

    /// Fraction of the pixel covered by a round-capped polyline.
    fn coverage(px: f32, py: f32, points: &[(f32, f32)], half: f32) -> f32 {
        let mut nearest = f32::MAX;
        for pair in points.windows(2) {
            nearest = nearest.min(distance_to_segment(px, py, pair[0], pair[1]));
        }
        (half + 0.5 - nearest).clamp(0.0, 1.0)
    }

    let mut rgba = vec![0u8; (BITMAP_PX * BITMAP_PX * 4) as usize];
    for y in 0..BITMAP_PX {
        for x in 0..BITMAP_PX {
            let px = x as f32 + 0.5 - centre;
            let py = y as f32 + 0.5 - centre;
            let dormant = coverage(px, py, &dormant, half) * DORMANT_ALPHA;
            let live = coverage(px, py, &live, half);
            // Source-over: the live branch wins where the two overlap.
            let alpha = live + dormant * (1.0 - live);
            let i = ((y * BITMAP_PX + x) * 4) as usize;
            rgba[i + 3] = (alpha * 255.0).round() as u8;
        }
    }
    tray_icon::Icon::from_rgba(rgba, BITMAP_PX, BITMAP_PX).expect("icon buffer is well formed")
}

/// The live menu bar item plus the rows that get rewritten on refresh.
pub struct Tray {
    /// Held so the status item stays alive; dropping it removes the icon.
    _icon: TrayIcon,
    header: MenuItem,
    show: MenuItem,
    /// Input rows, rebuilt whenever the advertised list changes.
    rows: Vec<(String, MenuItem)>,
    menu: Menu,
}

impl Tray {
    /// Create the status item and its menu. Must run on the main thread.
    ///
    /// Clicks are delivered on `events`, which the caller keeps: `tray-icon`
    /// hands them over on threads we do not own, and awaiting the channel is
    /// how the UI thread picks them up without polling for them.
    pub fn install(events: Sender<TrayEvent>) -> Option<Tray> {
        forward_events(events);

        let header = MenuItem::with_id(ID_HEADER, "dmux: reading monitor...", false, None);
        let refresh = MenuItem::with_id(ID_REFRESH, t!("tray_refresh"), true, None);
        let show = MenuItem::with_id(ID_SHOW, "Show window", true, None);
        let quit = MenuItem::with_id(ID_QUIT, t!("tray_quit"), true, None);

        let menu = Menu::new();
        if let Err(e) = menu.append_items(&[
            &header,
            &PredefinedMenuItem::separator(),
            &refresh,
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
        let builder = TrayIconBuilder::new().with_icon_templated(mux_icon());
        #[cfg(not(target_os = "macos"))]
        let builder = TrayIconBuilder::new().with_icon(mux_icon());

        let icon = match builder
            .with_tooltip("dmux - monitor input switcher")
            .with_menu(Box::new(menu.clone()))
            .with_menu_on_left_click(MENU_ON_LEFT_CLICK)
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
            show,
            rows: Vec::new(),
            menu,
        })
    }

    /// Rewrite the menu to match the current monitor state.
    ///
    /// `inputs` is `(code, name, is_current)`; `current` is the raw current
    /// input code, which is what "reclaim" re-asserts.
    /// Rewrite the menu to match the current monitor.
    ///
    /// `inputs` is `(code, name)`; `current` is `(code, name)` of the input
    /// that was just asked for, if any.
    ///
    /// Rows are actions, not readings: nothing here says which input the
    /// monitor is actually on, because that cannot be known reliably (see the
    /// README's "How it works"). The header names the display and nothing else.
    pub fn update(&mut self, display: &str, inputs: &[(String, String)]) {
        self.header.set_text(display);
        self.show.set_text(t!("tray_show"));

        // Rebuild the input rows only when the set of ports changed; keeping the
        // rows stable avoids flicker in the open menu.
        let same = self.rows.len() == inputs.len()
            && self
                .rows
                .iter()
                .zip(inputs)
                .all(|((code, _), (want, _))| code == want);
        if !same {
            for (_, row) in self.rows.drain(..) {
                let _ = self.menu.remove(&row);
            }
            for (code, name) in inputs {
                let row = MenuItem::with_id(format!("{ID_SWITCH_PREFIX}{code}"), name, true, None);
                // The trailing block is [separator, show, quit]; put the port
                // rows above it.
                let at = self.menu.items().len().saturating_sub(3);
                if let Err(e) = self.menu.insert(&row, at) {
                    eprintln!("tray: cannot insert {code}: {e}");
                    continue;
                }
                self.rows.push((code.clone(), row));
            }
            return;
        }

        // Same ports, so only the labels can have moved.
        for ((_, row), (_, name)) in self.rows.iter().zip(inputs) {
            row.set_text(name);
        }
    }
}

/// Install the global menu and icon handlers and hand events to `tx`.
///
/// `tray-icon`'s callbacks are global and set-once, so this bridges them into a
/// plain channel rather than exposing the crate's types to the UI layer. They
/// fire on threads we do not own, which is why `try_send` is all that is needed
/// here: the channel is unbounded, so it only ever fails once the app is gone.
fn forward_events(tx: Sender<TrayEvent>) {
    let clicks = tx.clone();
    tray_icon::menu::MenuEvent::set_event_handler(Some(
        move |event: tray_icon::menu::MenuEvent| {
            let id = event.id.0.as_str();
            let translated = if let Some(code) = id.strip_prefix(ID_SWITCH_PREFIX) {
                Some(TrayEvent::Switch(code.to_string()))
            } else {
                match id {
                    ID_REFRESH => Some(TrayEvent::Refresh),
                    ID_SHOW => Some(TrayEvent::Show),
                    ID_QUIT => Some(TrayEvent::Quit),
                    _ => None,
                }
            };
            if let Some(ev) = translated {
                // A dead receiver just means the app is shutting down.
                let _ = tx.try_send(ev);
            }
        },
    ));

    // A left click stands in for "Show window" only where it does not open the
    // menu; on macOS the click itself is what opens it.
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        else {
            return;
        };
        if !MENU_ON_LEFT_CLICK {
            // A dead receiver just means the app is shutting down.
            let _ = clicks.try_send(TrayEvent::Show);
        }
    }));
}
