//! dmux-gui - GPUI front end for the monitor input switcher.
//!
//! This binary is gated behind the `gui` feature so the CLI (and a plain
//! `cargo test`) do not pull in GPUI's ~700-crate dependency tree:
//!
//!     cargo build --release --features gui --bin dmux-gui
//!
//! Without the feature this file compiles to a stub that explains itself.
//!
//! ## What this UI refuses to claim
//!
//! It is a switcher and nothing else. Every row is an action; none of them says
//! which input the display is on now.
//!
//! That is a deliberate refusal rather than an omission. There is no portable
//! way to read a monitor's selected input back: VCP 0x60 is the code that
//! should report it, but implementations vary — some answer with the byte
//! duplicated, some do not answer reads at all, and whether a monitor answers
//! while it is displaying another machine is not defined by the spec. Reporting
//! a value we cannot stand behind would be worse than reporting nothing, and a
//! wrong "you are here" invites a switch that strands the user.
//!
//! There is also no hotkey to fall back on. When the display moves, this host
//! loses the video path, and on a desk where the keyboard and mouse hang off the
//! monitor's USB hub it loses those too, so recovery means the monitor's own
//! buttons.
//!
//! ## Threading model
//!
//! Forced by the behaviour of the DDC channel on macOS:
//!
//! ```text
//! UI thread (GPUI)            worker thread                   fresh process
//! ----------------            -------------                   -------------
//! send(Request)        --->   persistent Reader (reads only)  dmux-ddc-helper serve
//! poll via 60ms timer  <---   response channel
//!                             ddc::set_input (writes)      -> dmux-ddc-helper set-input
//! ```
//!
//! * `ddc::Reader` owns a `std::process::Child`, so it is **not `Send`** and has
//!   to live entirely inside the worker thread. The UI thread only ever touches
//!   channels, and never blocks on DDC (a read costs ~120-200 ms).
//! * Writes go through a throwaway process, because a write wedges the DCP I2C
//!   channel for the rest of that process. Reads are safe to share one
//!   long-lived process, which is why the reader is persistent.

// Windows: this is a window + tray app, so it must not own a console. Without
// this the binary is built for the console subsystem and launching it -- from
// Explorer especially -- opens a black console window next to the UI. The
// subsystem is only overridden when the GPUI front end is actually compiled in,
// so the "built without the GPUI front end" stub below still has a console to
// print its explanation to. (Ignored on non-Windows targets.)
#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

// rust-i18n's `t!` only resolves in the crate that declared the loader, so
// the binary needs its own declaration even though the library has one.
rust_i18n::i18n!("locales", fallback = "en");

#[cfg(not(feature = "gui"))]
fn main() {
    eprintln!(
        "dmux-gui was built without the GPUI front end.\n\n\
         Build it with:\n\n\
         \tcargo build --release --features gui --bin dmux-gui\n"
    );
    std::process::exit(2);
}

#[cfg(feature = "gui")]
fn main() {
    gui::run();
}

#[cfg(feature = "gui")]
mod gui {
    use displaymux::ddc::{self, Status};
    use displaymux::labels::Labels;
    use displaymux::settings::Settings;
    use displaymux::tray::{Tray, TrayEvent};
    use gpui_kit::component::input::{Input, InputState};
    use gpui_kit::component::{Icon, IconName, Sizable, Size, WindowExt as _};
    // `gpui_kit::*` is GPUI: the facade re-exports `gpui-pre` wholesale, so the
    // element and app types come from the same place as before.
    use gpui_kit::{
        div, point, prelude::*, px, rgb, size, App, Bounds, ClickEvent, Context, Entity, Global,
        IntoElement, QuitMode, SharedString, TitlebarOptions, WeakEntity, Window, WindowAppearance,
        WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions,
    };
    use rust_i18n::t;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Seconds between arming a switch and it happening. Long enough to read
    /// the countdown, short enough not to be in the way.
    const CONFIRM_SECONDS: u8 = 3;
    const READ_TIMEOUT: Duration = Duration::from_secs(12);
    const POLL_INTERVAL: Duration = Duration::from_millis(60);
    /// Tray clicks are checked on a timer; the menu bar is a secondary surface
    /// so a leisurely poll is fine and keeps everything on the GPUI thread.

    #[derive(Debug)]
    enum Request {
        Refresh,
        Switch { index: usize, code: String },
    }

    #[derive(Debug)]
    /// What to say after a switch. Most outcomes need no words: the monitor
    /// moving is the feedback, and the verdict strings explain machinery the
    /// user cannot act on.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SwitchNote {
        Silent,
        /// The monitor acknowledged the write but reports a different input,
        /// which almost always means the target port has no signal.
        Reverted,
        Failed,
    }

    enum Response {
        /// Every display that could be listed, in system order.
        Statuses(Box<Result<Vec<Status>, String>>),
        Switched(Box<Result<SwitchNote, String>>),
    }

    /// Owns the DDC reader and performs every DDC call off the UI thread.
    struct Worker {
        tx: Sender<Request>,
        /// Shared so the polling future can own a handle without borrowing
        /// `self`: GPUI's `spawn` needs a `'static` future. This satisfies that
        /// without any `unsafe`.
        rx: Arc<Mutex<Receiver<Response>>>,
    }

    impl Worker {
        fn spawn() -> Worker {
            let (req_tx, req_rx) = channel::<Request>();
            let (res_tx, res_rx) = channel::<Response>();

            std::thread::Builder::new()
                .name("ddc-worker".into())
                .spawn(move || {
                    // Created inside the thread: `Reader` is !Send.
                    let mut reader: Option<ddc::Reader> = None;

                    while let Ok(req) = req_rx.recv() {
                        match req {
                            Request::Refresh => {
                                // Never panic here: a missing helper used to abort
                                // the worker, after which the UI waited on a
                                // channel nobody would ever answer and sat on
                                // "working..." forever.
                                let out = match ddc::list_displays(READ_TIMEOUT) {
                                    Err(e) => Err(e.to_string()),
                                    Ok(list) if list.is_empty() => Ok(Vec::new()),
                                    Ok(list) => {
                                        let mut statuses = Vec::with_capacity(list.len());
                                        for display in list {
                                            if reader.is_none() {
                                                reader = ddc::Reader::spawn().ok();
                                            }
                                            let read = match reader.as_mut() {
                                                Some(r) => r.status(display.index, READ_TIMEOUT),
                                                None => {
                                                    statuses.push(Status::unreadable(
                                                        display.index,
                                                        display.id.clone(),
                                                        "DDC reader unavailable".to_string(),
                                                    ));
                                                    continue;
                                                }
                                            };
                                            match read {
                                                Ok(status) => statuses.push(status),
                                                Err(e) => {
                                                    // The channel may be wedged for this
                                                    // display only; start a fresh
                                                    // connection for the next one.
                                                    reader = None;
                                                    statuses.push(Status::unreadable(
                                                        display.index,
                                                        display.id.clone(),
                                                        e.to_string(),
                                                    ));
                                                }
                                            }
                                        }
                                        Ok(statuses)
                                    }
                                };
                                if res_tx.send(Response::Statuses(Box::new(out))).is_err() {
                                    break;
                                }
                            }
                            Request::Switch { index, code } => {
                                let out = ddc::set_input(index, &code, READ_TIMEOUT)
                                    .map(|o| match (&o.confirmed_current, o.acknowledged) {
                                        (Some(cur), _) if *cur == o.wrote => SwitchNote::Silent,
                                        (Some(_), _) => SwitchNote::Reverted,
                                        (None, true) => SwitchNote::Silent,
                                        (None, false) => SwitchNote::Failed,
                                    })
                                    .map_err(|e| e.to_string());
                                // The switch may have invalidated the reader's
                                // IOAVService, so force a fresh one next time.
                                reader = None;
                                if res_tx.send(Response::Switched(Box::new(out))).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
                .expect("spawn ddc worker");

            Worker {
                tx: req_tx,
                rx: Arc::new(Mutex::new(res_rx)),
            }
        }
    }

    struct SwitchApp {
        worker: Worker,
        /// Every display the last read found, in system order.
        displays: Vec<Status>,
        message: SharedString,
        busy: bool,
        /// Requests sent and not yet answered.
        pending: usize,
        /// Which entry of `displays` the window is showing.
        selected: usize,
        /// The target of an in-flight switch, remembered once it is accepted.
        pending_input: Option<String>,
        settings: Settings,
        /// The one port whose label is being edited, with its field. Built on
        /// demand because `InputState` needs the window that renders it.
        editing: Option<(String, Entity<InputState>)>,
        /// Whether the language list is showing.
        lang_open: bool,
        /// Whether the appearance list is showing.
        theme_open: bool,
        /// A release newer than this build, once a check has found one.
        update_available: Option<displaymux::update::Available>,
        /// Whether an update check is in flight.
        checking_update: bool,
        /// Whether this launch has already looked for a newer release.
        update_checked: bool,
        theme: ThemeChoice,
        /// The appearance actually in force, after resolving "system".
        dark: bool,
    }

    impl SwitchApp {
        fn new(cx: &mut Context<Self>) -> Self {
            let worker = Worker::spawn();
            let settings = Settings::load();
            let settings_theme = settings.theme.clone();
            let app = SwitchApp {
                worker,
                displays: Vec::new(),
                message: t!("reading").into(),
                busy: false,
                pending: 0,
                selected: 0,
                pending_input: None,
                settings,
                editing: None,
                lang_open: false,
                theme_open: false,
                update_available: None,
                checking_update: false,
                update_checked: false,
                theme: ThemeChoice::from_setting(settings_theme.as_deref()),
                dark: true,
            };
            app.poll(cx);
            // The first DDC transaction opens an I2C connection to the display,
            // and starting one while the window is still setting up its display
            // link is the closest this app comes to disturbing the panel -- on a
            // link running HDR that showed up as the screen's colours breaking
            // for a moment at launch. Nothing in the window waits on the read,
            // so it waits for the window: frontmost first, then one more beat
            // for the link. The timer covers a launch that opens behind
            // something else, where active never arrives on its own.
            cx.spawn(async move |this, cx| {
                let started = std::time::Instant::now();
                while started.elapsed() < Duration::from_secs(2) {
                    let frontmost = cx.update(|cx| {
                        cx.global::<Installed>()
                            .window
                            .and_then(|handle| {
                                handle
                                    .update(cx, |_, window, _| window.is_window_active())
                                    .ok()
                            })
                            .unwrap_or(false)
                    });
                    if frontmost {
                        break;
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(60))
                        .await;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let _ = this.update(cx, |app: &mut SwitchApp, cx| {
                    app.send(Request::Refresh, t!("reading"), cx);
                });
            })
            .detach();
            app
        }

        /// Push current state into the menu bar menu.
        ///
        /// The tray itself lives in [`Installed`], not in this view: it has to
        /// outlive the window, which `QuitMode::Explicit` keeps the app running
        /// past, and a tray that dies with the window leaves an icon whose menu
        /// does nothing at all -- not even quit.
        fn sync_tray(&mut self, cx: &mut Context<Self>) {
            let available = self
                .update_available
                .as_ref()
                .map(|a| a.version.to_string());
            let Some(tray) = cx.global_mut::<Installed>().tray.as_mut() else {
                return;
            };
            tray.set_update(available.as_deref());
            let groups: Vec<displaymux::tray::DisplayMenu> = self
                .displays
                .iter()
                .map(|display| displaymux::tray::DisplayMenu {
                    index: display.display_index,
                    name: display.display_id.clone(),
                    ports: display
                        .inputs
                        .iter()
                        .map(|i| (i.code.clone(), i.name.clone()))
                        .collect(),
                })
                .collect();
            tray.update(&groups);
        }

        /// Act on the tray clicks that need this view.
        ///
        /// `Show` and `Quit` never reach here: they are handled where the tray
        /// lives, outside this view's borrow, because raising the window means
        /// borrowing the view again.
        fn on_tray(&mut self, event: TrayEvent, cx: &mut Context<Self>) {
            match event {
                TrayEvent::Refresh => self.send(Request::Refresh, t!("reading"), cx),
                // The menu bar is a second entrance to the same irreversible
                // command, so it gets the same confirmation rather than firing
                // straight from a menu click.
                TrayEvent::CheckUpdates => self.open_update(cx),
                TrayEvent::Switch { index, code } => {
                    let p = self.palette();
                    let target = SwitchTarget {
                        display_index: index,
                        display: self.display_name(index),
                        name: self.port_name(index, &code),
                        code,
                    };
                    open_confirm(cx, p, target);
                }
                TrayEvent::Show | TrayEvent::Quit => {}
            }
        }

        /// Poll the worker's response channel on a timer, so the UI thread never
        /// blocks on DDC. This is the bridge to the worker thread.
        /// Drain the worker's answers, forever.
        ///
        /// One loop for the life of the view, rather than one per request: with
        /// several displays a read can take seconds, and the old
        /// request-per-poller design could only ever have one in flight -- it
        /// refused the next one instead of queueing it, so a switch made while a
        /// read was running was silently dropped.
        fn poll(&self, cx: &mut Context<Self>) {
            let rx = self.worker.rx.clone();
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(POLL_INTERVAL).await;
                    // Take everything that has arrived before waiting again.
                    loop {
                        let got = rx.lock().ok().map(|r| r.try_recv());
                        match got {
                            Some(Ok(resp)) => {
                                let _ = this.update(cx, |app, cx| {
                                    app.apply(resp, cx);
                                    cx.notify();
                                });
                            }
                            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => break,
                            // The worker died without answering. Clear the
                            // pending count so the UI does not sit on
                            // "working..." forever.
                            _ => {
                                let _ = this.update(cx, |app, cx| {
                                    app.pending = 0;
                                    app.busy = false;
                                    app.message = "DDC worker stopped unexpectedly".into();
                                    cx.notify();
                                });
                                return;
                            }
                        }
                    }
                }
            })
            .detach();
        }

        fn apply(&mut self, resp: Response, cx: &mut Context<Self>) {
            self.pending = self.pending.saturating_sub(1);
            self.busy = self.pending > 0;
            match resp {
                Response::Statuses(list) => match *list {
                    Ok(list) => {
                        // Keep the display the window was showing across a
                        // refresh; the indices are the system's, and a display
                        // waking up can renumber them.
                        let showing = self.selected_display().map(|s| s.display_index);
                        self.displays = list;
                        self.selected = showing
                            .and_then(|index| {
                                self.displays.iter().position(|s| s.display_index == index)
                            })
                            .unwrap_or(0);
                        // Report only what we can stand behind. The monitor's
                        // VCP 0x60 value is not shown as "current input" because
                        // on the reference monitor it is a constant that does not
                        // follow the actual input.
                        // Only speak when there is something to say. A list of
                        // ports is its own explanation.
                        self.message = match self.selected_display() {
                            Some(s) if s.inputs.is_empty() => t!("no_inputs").into(),
                            Some(_) => String::new().into(),
                            None => t!("no_monitor").into(),
                        };
                        // Once the window has something to show, look for a
                        // newer release. Once per launch: a check on every
                        // refresh would be noise, and the menu bar can re-run it.
                        if !self.update_checked && !self.displays.is_empty() {
                            self.update_checked = true;
                            self.check_updates(cx);
                        }
                    }
                    Err(e) => self.message = t!("read_failed", error = e.as_str()).into(),
                },
                Response::Switched(r) => {
                    match *r {
                        Ok(note) => {
                            // The write was accepted, which is all that can honestly
                            // be claimed from here: the monitor may act on it, and
                            // once it does this host loses the display entirely.
                            self.pending_input.take();
                            self.message = match note {
                                SwitchNote::Silent => String::new().into(),
                                SwitchNote::Reverted => t!("reverted").to_string().into(),
                                SwitchNote::Failed => t!("switch_failed_short").to_string().into(),
                            };
                        }
                        Err(e) => {
                            self.message =
                                t!("switch_failed", error = e.to_string().as_str()).into()
                        }
                    }
                }
            }
            self.sync_tray(cx);
        }

        fn send(&mut self, req: Request, note: impl Into<SharedString>, cx: &mut Context<Self>) {
            // Queued, never refused: the worker applies requests one at a time,
            // so there is nothing here that a busy moment needs to protect.
            self.pending += 1;
            self.busy = true;
            self.message = note.into();
            let _ = self.worker.tx.send(req);
            cx.notify();
        }
    }

    /// A full colour set. Two of them, chosen by the appearance in force.
    ///
    /// The accent is the amber from the application icon: it is the one colour
    /// that already means "this tool" everywhere else it appears. On a light
    /// background it has to be darker to keep any contrast against the surface,
    /// which is the only place the two sets differ in intent rather than value.
    #[derive(Clone, Copy)]
    struct Palette {
        bg: u32,
        surface: u32,
        surface_hover: u32,
        surface_active: u32,
        hairline: u32,
        text: u32,
        text_dim: u32,
        text_faint: u32,
        accent: u32,
        on_accent: u32,
    }

    // Both sets are the Tailwind **neutral** scale, which is the one
    // gpui-component's own themes use (and the one shadcn/ui is built on). The
    // point is not the scale itself but sharing it: dialogs and text fields come
    // from that library, and a window tinted a different grey family makes them
    // look pasted on. Checks against gpui-component's `default-theme.json`:
    //   dark  background neutral-950, foreground neutral-50, border neutral-800
    //   light background white,       foreground neutral-950, border neutral-200
    const DARK: Palette = Palette {
        bg: 0x0a0a0a,            // neutral-950
        surface: 0x171717,       // neutral-900
        surface_hover: 0x262626, // neutral-800
        surface_active: 0x333333,
        hairline: 0x262626,   // neutral-800
        text: 0xfafafa,       // neutral-50
        text_dim: 0xa3a3a3,   // neutral-400
        text_faint: 0x737373, // neutral-500
        accent: 0xffb627,     // the colour of the application icon
        on_accent: 0x1a1205,
    };

    const LIGHT: Palette = Palette {
        bg: 0xf8f8f8,
        surface: 0xffffff,
        surface_hover: 0xf5f5f5,  // neutral-100
        surface_active: 0xe5e5e5, // neutral-200
        hairline: 0xe5e5e5,       // neutral-200
        text: 0x0a0a0a,           // neutral-950
        text_dim: 0x525252,       // neutral-600
        text_faint: 0x737373,     // neutral-500
        // Amber-700. The icon's amber has almost no contrast on white; this is
        // the same hue carried far enough down the scale to stay legible.
        accent: 0xb45309,
        on_accent: 0xffffff,
    };

    /// What the user asked for, which may be "whatever the system is doing".
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ThemeChoice {
        Dark,
        Light,
        System,
    }

    impl ThemeChoice {
        const ALL: [Self; 3] = [Self::Dark, Self::Light, Self::System];

        fn from_setting(value: Option<&str>) -> Self {
            match value {
                Some("dark") => Self::Dark,
                Some("light") => Self::Light,
                _ => Self::System,
            }
        }

        fn id(self) -> &'static str {
            match self {
                Self::Dark => "dark",
                Self::Light => "light",
                Self::System => "system",
            }
        }

        fn label(self) -> String {
            match self {
                Self::Dark => t!("theme_dark").to_string(),
                Self::Light => t!("theme_light").to_string(),
                Self::System => t!("theme_system").to_string(),
            }
        }
    }

    /// Lucide's `sun-moon`, for the "follow the system" choice. The bundled
    /// component icons have a sun and a moon but nothing that means "either".
    const SUN_MOON_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 8a2.83 2.83 0 0 0 4 4 4 4 0 1 1-4-4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.9 4.9 1.4 1.4"/><path d="m17.7 17.7 1.4 1.4"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.3 17.7-1.4 1.4"/><path d="m19.1 4.9-1.4 1.4"/></svg>"#;

    /// The application mark, redrawn for a 24px box in a single colour.
    ///
    /// The shipped `assets/mark.svg` carries the amber/dormant pair, but an
    /// `Icon` tints the whole SVG with one colour, so a two-tone mark would be
    /// flattened at this size. Same geometry, tinted with the accent.
    const MARK_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M16 5.4V9.5L12 14.1"/><path d="M8 3.2V9.5L12 14.1V20.8"/></svg>"#;

    /// Lucide's pencil. The bundled component icons have no pencil.
    const PENCIL_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/><path d="m15 5 4 4"/></svg>"#;

    /// Lucide's `languages`: the A beside a CJK glyph, which is the icon
    /// i18n is conventionally drawn with. Also missing from the bundled set.
    const LANGUAGES_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m5 8 6 6"/><path d="m4 14 6-6 2-3"/><path d="M2 5h12"/><path d="M7 2h1"/><path d="m22 22-5-10-5 10"/><path d="M14 18h6"/></svg>"#;

    /// One switchable input.
    ///
    /// The row is not a button. Switching is a deliberate act with its own hit
    /// area, because a stray click here takes the display -- and, on a desk
    /// where the peripherals hang off the monitor, the keyboard and mouse --
    /// away from this machine.
    fn input_row(
        p: Palette,
        code: String,
        name: String,
        advertised: bool,
        weak: WeakEntity<SwitchApp>,
    ) -> gpui_kit::AnyElement {
        let trailing = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child({
                let weak = weak.clone();
                let code = code.clone();
                div()
                    .id(SharedString::from(format!("edit-{}", code)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(24.0))
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(p.surface_active)))
                    .child(
                        Icon::default()
                            .data(PENCIL_SVG)
                            .text_color(rgb(p.text_faint))
                            .text_sm(),
                    )
                    .on_click(move |_e: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let code = code.clone();
                        let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                            app.open_edit_dialog(code, window, cx);
                        });
                    })
            })
            .child({
                let weak = weak.clone();
                let code = code.clone();
                div()
                    .id(SharedString::from(format!("switch-{}", code)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .h(px(24.0))
                    .px_2()
                    .rounded_md()
                    .bg(rgb(p.surface_active))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x3c3c4a)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(p.text_dim))
                            .child(t!("switch")),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .text_color(rgb(p.text_faint))
                            .text_xs(),
                    )
                    .on_click(move |_e: &ClickEvent, _window: &mut Window, cx: &mut App| {
                        let code = code.clone();
                        let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                            let Some(display) = app.selected_display() else {
                                return;
                            };
                            let index = display.display_index;
                            let p = app.palette();
                            let target = SwitchTarget {
                                display_index: index,
                                display: app.display_name(index),
                                name: app.port_name(index, &code),
                                code,
                            };
                            open_confirm(cx, p, target);
                        });
                    })
            })
            .into_any_element();

        div()
            .id(SharedString::from(format!("input-{}", code)))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .w_full()
            .px_3()
            .py(px(9.0))
            .rounded_lg()
            .bg(rgb(p.surface))
            .child(
                div()
                    .w(px(34.0))
                    .text_sm()
                    .text_color(rgb(p.text_faint))
                    .child(code),
            )
            .child(div().flex_1().text_sm().text_color(rgb(p.text)).child(name))
            .when(!advertised, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(rgb(p.text_faint))
                        .child(t!("unlisted")),
                )
            })
            .child(trailing)
            .into_any_element()
    }

    /// A standalone confirmation window.
    ///
    /// Not a dialog on the main window: a dialog has to be shown to be seen,
    /// which drags the whole switcher forward. From the menu bar the user asked
    /// for a confirmation, not for the window.
    ///
    /// It is its own view because the countdown changes every second and a
    /// window's content is built once; it also owns the auto-fire, so "clicked
    /// Switch" and "ran out" take exactly the same path.
    /// What a confirmation is about: one port on one display.
    #[derive(Clone)]
    struct SwitchTarget {
        display_index: usize,
        display: String,
        code: String,
        name: String,
    }

    struct ConfirmView {
        p: Palette,
        target: SwitchTarget,
        left: u8,
        /// `None` once the main window is closed: the menu bar can still switch,
        /// it just has nowhere to report to.
        app: Option<WeakEntity<SwitchApp>>,
        window: Option<gpui_kit::AnyWindowHandle>,
    }

    /// Send the switch, whether or not there is a view to route it through.
    ///
    /// A weak reference can be alive when the confirmation opens and dead by the
    /// time it is answered -- the main window may have closed in between. The
    /// update has to be *checked*, not assumed: swallowing its error is how the
    /// menu bar came to confirm a switch and then do nothing.
    fn dispatch_switch(
        app: Option<WeakEntity<SwitchApp>>,
        index: usize,
        code: String,
        cx: &mut App,
    ) {
        if let Some(app) = app {
            if app
                .update(cx, |app: &mut SwitchApp, cx| {
                    app.fire_switch(index, code.clone(), cx)
                })
                .is_ok()
            {
                return;
            }
        }
        // Nothing to drive it, so ask the monitor directly -- one shot, like
        // every other write.
        cx.background_spawn(async move {
            let _ = displaymux::ddc::set_input(index, &code, Duration::from_secs(12));
        })
        .detach();
    }

    impl ConfirmView {
        fn new(
            p: Palette,
            target: SwitchTarget,
            app: Option<WeakEntity<SwitchApp>>,
            window: Option<gpui_kit::AnyWindowHandle>,
            cx: &mut Context<Self>,
        ) -> Self {
            // Ticking stops on its own: closing the window drops this entity and
            // the weak upgrade below then fails.
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let fire = this.update(cx, |view, cx| {
                        if view.left == 0 {
                            return false;
                        }
                        view.left -= 1;
                        cx.notify();
                        view.left == 0
                    });
                    match fire {
                        Ok(true) => break,
                        Ok(false) => continue,
                        Err(_) => return,
                    }
                }
                let _ = this.update(cx, |view, cx| {
                    let code = view.target.code.clone();
                    let index = view.target.display_index;
                    let app = view.app.clone();
                    let window = view.window;
                    dispatch_switch(app, index, code, cx);
                    if let Some(window) = window {
                        let _ = window.update(cx, |_, window, cx| ConfirmView::dismiss(window, cx));
                    }
                });
            })
            .detach();

            Self {
                p,
                target,
                left: CONFIRM_SECONDS,
                app,
                window,
            }
        }
    }

    impl ConfirmView {
        /// Close this window and forget it, so the next request opens a new one.
        fn dismiss(window: &mut Window, cx: &mut App) {
            cx.global_mut::<Installed>().confirm = None;
            window.remove_window();
        }
    }

    impl Render for ConfirmView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let p = self.p;
            let left = self.left;
            let target = self.target.clone();
            let app = self.app.clone();

            div()
                .flex()
                .flex_col()
                .size_full()
                .rounded_lg()
                .bg(rgb(p.bg))
                .text_color(rgb(p.text))
                // A compact title strip: the traffic lights are a system window
                // decoration and sit above whatever is at y=0, so the content
                // has to start below them.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .h(px(38.0))
                        .px(px(16.0))
                        .border_b(px(1.0))
                        .border_color(rgb(p.hairline))
                        .child(
                            Icon::default()
                                .data(MARK_SVG)
                                .text_color(rgb(p.accent))
                                .with_size(Size::Size(px(13.0))),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(p.text_dim))
                                .child(t!("confirm_title").to_string()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .p(px(16.0))
                        .gap_2()
                        .child(
                            div().text_sm().child(
                                t!(
                                    "confirm_body",
                                    display = target.display.as_str(),
                                    name = target.name.as_str()
                                )
                                .to_string(),
                            ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(p.text_faint))
                                .child(t!("confirm_note").to_string()),
                        )
                        .child(
                            div()
                                .mt_auto()
                                .flex()
                                .flex_row()
                                .justify_end()
                                .gap_2()
                                .child({
                                    let app = app.clone();
                                    div()
                                .id("confirm-cancel")
                                .px_3()
                                .py(px(5.0))
                                .rounded_md()
                                .bg(rgb(p.surface_active))
                                .text_color(rgb(p.text_dim))
                                .text_xs()
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.85))
                                .child(t!("cancel"))
                                .on_click(
                                    move |_e: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        if let Some(app) = app.clone() {
                                            let _ = app.update(cx, |app: &mut SwitchApp, cx| {
                                                app.message = String::new().into();
                                                cx.notify();
                                            });
                                        }
                                        ConfirmView::dismiss(window, cx);
                                    },
                                )
                                })
                                .child({
                                    let app = app.clone();
                                    div()
                                .id("confirm-ok")
                                .px_3()
                                .py(px(5.0))
                                .rounded_md()
                                .bg(rgb(p.accent))
                                .text_color(rgb(p.on_accent))
                                .text_xs()
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.9))
                                .child(t!("confirm_switch", count = left))
                                .on_click(
                                    move |_e: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        dispatch_switch(
                                            app.clone(),
                                            target.display_index,
                                            target.code.clone(),
                                            cx,
                                        );
                                        ConfirmView::dismiss(window, cx);
                                    },
                                )
                                }),
                        ),
                )
        }
    }

    /// What the update window is showing.
    #[derive(Clone)]
    enum UpdateState {
        Checking,
        Latest,
        Available(displaymux::update::Available),
        Downloading(u8),
        Restarting,
        Failed(String),
    }

    /// The small window that checks for, downloads, and installs a release.
    ///
    /// Its own window for the same reason the confirmation is: the menu bar can
    /// open it, and a dialog would drag the switcher forward with it.
    struct UpdateView {
        p: Palette,
        state: UpdateState,
        app: Option<WeakEntity<SwitchApp>>,
    }

    impl UpdateView {
        fn new(
            p: Palette,
            initial: Option<displaymux::update::Available>,
            app: Option<WeakEntity<SwitchApp>>,
            _window: Option<gpui_kit::AnyWindowHandle>,
            cx: &mut Context<Self>,
        ) -> Self {
            let state = match initial {
                Some(available) => UpdateState::Available(available),
                None => UpdateState::Checking,
            };
            if matches!(state, UpdateState::Checking) {
                // Opening straight from the menu bar means nothing has checked
                // yet, so this window does it and reports what it finds.
                cx.spawn(async move |this, cx| {
                    let found = cx
                        .background_spawn(async { displaymux::update::check() })
                        .await;
                    let _ = this.update(cx, |view, cx| {
                        view.state = match found {
                            Ok(Some(available)) => UpdateState::Available(available),
                            Ok(None) => UpdateState::Latest,
                            Err(e) => UpdateState::Failed(e),
                        };
                        cx.notify();
                    });
                })
                .detach();
            }
            Self { p, state, app }
        }

        /// Download and install, reporting progress while it happens.
        ///
        /// The download runs on a background thread and publishes its percentage
        /// through an atomic; a small poller in the UI turns that into a
        /// redraw. Progress callbacks fire on the worker, which must not touch
        /// the view.
        fn install(&mut self, cx: &mut Context<Self>) {
            if !matches!(self.state, UpdateState::Available(_)) {
                return;
            }
            self.state = UpdateState::Downloading(0);
            cx.notify();

            let progress = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));

            let watched = progress.clone();
            cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let percent = watched.load(std::sync::atomic::Ordering::Relaxed);
                let running = this.update(cx, |view, cx| match view.state {
                    UpdateState::Downloading(_) => {
                        view.state = UpdateState::Downloading(percent);
                        cx.notify();
                        true
                    }
                    _ => false,
                });
                if !matches!(running, Ok(true)) {
                    break;
                }
            })
            .detach();

            let written = progress;
            cx.spawn(async move |this, cx| {
                let outcome = cx
                    .background_spawn(async move {
                        displaymux::update::install(move |done, total| {
                            let percent = match total {
                                Some(total) if total > 0 => {
                                    (done.saturating_mul(100) / total).min(100) as u8
                                }
                                _ => 0,
                            };
                            written.store(percent, std::sync::atomic::Ordering::Relaxed);
                        })
                    })
                    .await;
                let restarting = match outcome {
                    Ok(()) => true,
                    Err(e) => {
                        let _ = this.update(cx, |view: &mut UpdateView, cx| {
                            view.state = UpdateState::Failed(e);
                            cx.notify();
                        });
                        false
                    }
                };
                if !restarting {
                    return;
                }
                let _ = this.update(cx, |view: &mut UpdateView, cx| {
                    view.state = UpdateState::Restarting;
                    cx.notify();
                    // The new build is already running: this process has
                    // nothing left to do, and staying would leave two.
                    cx.quit();
                });
            })
            .detach();
        }
    }

    impl Render for UpdateView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let p = self.p;
            let app = self.app.clone();
            let weak = cx.entity().downgrade();

            let (line, detail, action): (String, Option<String>, Option<String>) = match &self.state
            {
                UpdateState::Checking => (t!("update_checking").to_string(), None, None),
                UpdateState::Latest => (t!("update_latest").to_string(), None, None),
                UpdateState::Available(available) => (
                    t!("update_available", version = available.version.to_string()).to_string(),
                    Some(t!("update_body").to_string()),
                    Some(t!("update_download").to_string()),
                ),
                UpdateState::Downloading(percent) => (
                    t!("update_downloading", percent = *percent as usize).to_string(),
                    None,
                    None,
                ),
                UpdateState::Restarting => (t!("update_restarting").to_string(), None, None),
                UpdateState::Failed(error) => (
                    t!("update_failed", error = error.as_str()).to_string(),
                    None,
                    None,
                ),
            };

            div()
                .flex()
                .flex_col()
                .size_full()
                .rounded_lg()
                .bg(rgb(p.bg))
                .text_color(rgb(p.text))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .h(px(38.0))
                        .px(px(16.0))
                        .border_b(px(1.0))
                        .border_color(rgb(p.hairline))
                        .child(
                            Icon::default()
                                .data(MARK_SVG)
                                .text_color(rgb(p.accent))
                                .with_size(Size::Size(px(13.0))),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(p.text_dim))
                                .child(t!("update_title").to_string()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .p(px(16.0))
                        .gap_2()
                        .child(div().text_sm().child(line))
                        .when_some(detail, |d, detail| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(p.text_faint))
                                    .child(detail),
                            )
                        })
                        .child(
                            div()
                                .mt_auto()
                                .flex()
                                .flex_row()
                                .justify_end()
                                .gap_2()
                                .child({
                                    let app = app.clone();
                                    div()
                                        .id("update-close")
                                        .px_3()
                                        .py(px(5.0))
                                        .rounded_md()
                                        .bg(rgb(p.surface_active))
                                        .text_color(rgb(p.text_dim))
                                        .text_xs()
                                        .cursor_pointer()
                                        .hover(|s| s.opacity(0.85))
                                        .child(t!("update_later").to_string())
                                        .on_click(
                                            move |_e: &ClickEvent, window: &mut Window, cx: &mut App| {
                                                if let Some(app) = app.clone() {
                                                    let _ = app.update(cx, |_: &mut SwitchApp, cx| {
                                                        cx.notify();
                                                    });
                                                }
                                                window.remove_window();
                                            },
                                        )
                                })
                                .when_some(action, |d, action| {
                                    d.child(
                                        div()
                                            .id("update-install")
                                            .px_3()
                                            .py(px(5.0))
                                            .rounded_md()
                                            .bg(rgb(p.accent))
                                            .text_color(rgb(p.on_accent))
                                            .text_xs()
                                            .cursor_pointer()
                                            .hover(|s| s.opacity(0.9))
                                            .child(action)
                                            .on_click(
                                                move |_e: &ClickEvent, _window: &mut Window, cx: &mut App| {
                                                    let _ = weak.update(cx, |view: &mut UpdateView, cx| {
                                                        view.install(cx);
                                                    });
                                                },
                                            ),
                                    )
                                }),
                        ),
                )
        }
    }

    /// Show the update window, centred and small.
    fn open_update_window(
        cx: &mut App,
        p: Palette,
        available: Option<displaymux::update::Available>,
        app: Option<WeakEntity<SwitchApp>>,
    ) {
        if let Some(open) = cx.global::<Installed>().update_window {
            if open.update(cx, |_, _, _| {}).is_ok() {
                return;
            }
        }
        let bounds = Bounds::centered(None, size(px(380.0), px(196.0)), cx);
        let opened = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_background: WindowBackgroundAppearance::Transparent,
                window_decorations: Some(WindowDecorations::Client),
                titlebar: None,
                is_resizable: false,
                focus: true,
                ..Default::default()
            },
            cx,
            move |_window, cx| {
                let available = available.clone();
                let app = app.clone();
                cx.new(move |cx| {
                    let handle = cx.active_window();
                    UpdateView::new(p, available, app, handle, cx)
                })
            },
        );
        if let Ok((handle, _)) = opened {
            cx.global_mut::<Installed>().update_window = Some(handle);
        }
    }

    /// Show the confirmation in its own small window, centred.
    ///
    /// Shared by both entrances -- the Switch button and the menu bar -- so
    /// there is one confirmation to learn rather than two.
    fn open_confirm(cx: &mut App, p: Palette, target: SwitchTarget) {
        // One at a time: a second tray click should not stack windows. The
        // handle is cleared by `ConfirmView::dismiss`, because a closed window's
        // handle does not reliably report itself as gone.
        if cx.global::<Installed>().confirm.is_some() {
            return;
        }
        let app = cx.global::<Installed>().app.clone();
        let bounds = Bounds::centered(None, size(px(380.0), px(208.0)), cx);
        let opened = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // No system chrome: a confirmation is not a window you close,
                // minimise or resize, and three such buttons would offer three
                // ways to answer a question that has two answers.
                window_decorations: Some(WindowDecorations::Client),
                titlebar: None,
                is_resizable: false,
                focus: true,
                ..Default::default()
            },
            cx,
            move |_window, cx| {
                let target = target.clone();
                let app = app.clone();
                cx.new(move |cx| {
                    let window = cx.active_window();
                    ConfirmView::new(p, target, app, window, cx)
                })
            },
        );
        if let Ok((handle, _)) = opened {
            cx.global_mut::<Installed>().confirm = Some(handle);
        }
    }

    impl SwitchApp {
        /// The display the window is showing.
        fn selected_display(&self) -> Option<&Status> {
            self.displays.get(self.selected)
        }

        /// A display by the index the system gave it.
        fn display_by_index(&self, index: usize) -> Option<&Status> {
            self.displays.iter().find(|d| d.display_index == index)
        }

        /// What to call a display in a sentence.
        fn display_name(&self, index: usize) -> String {
            self.display_by_index(index)
                .map(|d| d.display_id.clone())
                .unwrap_or_else(|| t!("no_monitor").to_string())
        }

        /// How a port is named in a confirmation: its label, then its code.
        fn port_name(&self, index: usize, code: &str) -> String {
            self.display_by_index(index)
                .and_then(|d| d.inputs.iter().find(|i| i.code == code))
                .map(|i| format!("{} ({})", i.name, i.code))
                .unwrap_or_else(|| code.to_string())
        }

        /// Look for a newer release without saying anything about failures.
        ///
        /// A background check that interrupts with "could not reach the
        /// internet" is worse than one that stays quiet: the user did not ask.
        fn check_updates(&mut self, cx: &mut Context<Self>) {
            if self.checking_update {
                return;
            }
            self.checking_update = true;
            cx.notify();
            cx.spawn(async move |this, cx| {
                let found = cx
                    .background_spawn(async { displaymux::update::check() })
                    .await;
                let _ = this.update(cx, |app: &mut SwitchApp, cx| {
                    app.checking_update = false;
                    if let Ok(Some(available)) = found {
                        app.update_available = Some(available);
                        app.sync_tray(cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        }

        /// Open the update window seeded with whatever the check found.
        fn open_update(&mut self, cx: &mut Context<Self>) {
            let p = self.palette();
            let available = self.update_available.clone();
            let app = cx.entity().downgrade();
            open_update_window(cx, p, available, Some(app));
        }

        fn palette(&self) -> Palette {
            if self.dark {
                DARK
            } else {
                LIGHT
            }
        }

        /// Send the switch the dialog was opened for.
        fn fire_switch(&mut self, index: usize, code: String, cx: &mut Context<Self>) {
            self.send(
                Request::Switch {
                    index,
                    code: code.clone(),
                },
                t!("switching_to", code = code.as_str()),
                cx,
            );
        }

        /// Edit one port's label in a dialog.
        ///
        /// The field is created before the dialog opens and captured by its
        /// builder: the builder is a render closure that may run more than
        /// once, and creating the state inside it would reset the text.
        fn open_edit_dialog(&mut self, code: String, window: &mut Window, cx: &mut Context<Self>) {
            let current = self
                .selected_display()
                .and_then(|d| d.inputs.iter().find(|i| i.code == code))
                .map(|i| i.name.clone())
                .unwrap_or_default();
            let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
            self.editing = Some((code.clone(), input.clone()));

            let weak = cx.entity().downgrade();
            let p = self.palette();
            window.open_dialog(cx, move |dialog, _window, _cx| {
                let input = input.clone();
                let weak = weak.clone();
                let code = code.clone();
                // An explicit footer: the default one renders no buttons unless
                // `show_cancel` is asked for, and the styling has to match the
                // rest of the window anyway.
                dialog
                    .title(t!("edit_label").to_string())
                    .content(move |content, _window, _cx| content.child(Input::new(&input)))
                    .footer(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .w_full()
                            .child({
                                let weak = weak.clone();
                                let code = code.clone();
                                div()
                                    .id("dialog-save")
                                    .px_3()
                                    .py(px(5.0))
                                    .rounded_md()
                                    .bg(rgb(p.accent))
                                    .text_color(rgb(p.on_accent))
                                    .text_xs()
                                    .cursor_pointer()
                                    .hover(|s| s.opacity(0.9))
                                    .child(t!("save"))
                                    .on_click(
                                        move |_e: &ClickEvent, window: &mut Window, cx: &mut App| {
                                            let code = code.clone();
                                            let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                                                app.commit_edit(&code, cx);
                                            });
                                            window.close_dialog(cx);
                                        },
                                    )
                            })
                            .child({
                                let weak = weak.clone();
                                div()
                                    .id("dialog-cancel")
                                    .px_3()
                                    .py(px(5.0))
                                    .rounded_md()
                                    .bg(rgb(p.surface_active))
                                    .text_color(rgb(p.text_dim))
                                    .text_xs()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(0x3c3c4a)))
                                    .child(t!("cancel"))
                                    .on_click(
                                        move |_e: &ClickEvent, window: &mut Window, cx: &mut App| {
                                            let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                                                app.editing = None;
                                                cx.notify();
                                            });
                                            window.close_dialog(cx);
                                        },
                                    )
                            }),
                    )
            });
        }

        /// Persist the edited label, then reload so the row shows it.
        ///
        /// An empty field, or one still holding the name the monitor reports,
        /// removes the label rather than freezing that name in place.
        fn commit_edit(&mut self, code: &str, cx: &mut Context<Self>) {
            let Some((_, editor)) = self.editing.take() else {
                return;
            };
            let Some(display) = self.selected_display().map(|d| d.display_id.clone()) else {
                return;
            };
            let text = editor.read(cx).value().trim().to_string();
            let mccs = u16::from_str_radix(code.trim_start_matches("0x"), 16)
                .map(displaymux::vcp::input_name)
                .unwrap_or_default();
            let mut labels = Labels::load();
            if text.is_empty() || text == mccs {
                labels.clear(&display, code);
            } else {
                labels.set(&display, code, &text);
            }
            match labels.save() {
                Ok(_) => self.send(Request::Refresh, t!("reading"), cx),
                Err(e) => {
                    self.message = t!("read_failed", error = e.to_string().as_str()).into();
                    cx.notify();
                }
            }
        }

        fn choose_theme(&mut self, choice: ThemeChoice, cx: &mut Context<Self>) {
            self.theme = choice;
            self.dark = match choice {
                ThemeChoice::Dark => true,
                ThemeChoice::Light => false,
                ThemeChoice::System => matches!(
                    cx.window_appearance(),
                    WindowAppearance::Dark | WindowAppearance::VibrantDark
                ),
            };
            // The component library keeps its own theme for the dialogs and the
            // text fields; it has to follow ours.
            let mode = if self.dark {
                gpui_kit::component::theme::ThemeMode::Dark
            } else {
                gpui_kit::component::theme::ThemeMode::Light
            };
            gpui_kit::component::theme::Theme::change(mode, None, cx);
            self.settings.theme = Some(choice.id().to_string());
            let _ = self.settings.save();
            self.theme_open = false;
            cx.notify();
        }

        fn choose_language(&mut self, tag: &'static str, cx: &mut Context<Self>) {
            // The locale is process-global, so this also changes the component
            // library's own strings.
            rust_i18n::set_locale(tag);
            self.settings.locale = Some(tag.to_string());
            let _ = self.settings.save();
            self.lang_open = false;
            // The menu bar was built with the previous language.
            self.sync_tray(cx);
            cx.notify();
        }

        /// The custom title bar.
        ///
        /// The language picker expands *in the bar* rather than floating over
        /// the content: a hand-positioned overlay is one layout change away
        /// from sitting under its own backdrop, and did.
        fn title_bar(&self, p: Palette, cx: &Context<Self>) -> impl IntoElement {
            let weak = cx.entity().downgrade();
            let current = self.settings.locale.clone().unwrap_or_default();
            let update_available = self
                .update_available
                .as_ref()
                .map(|a| a.version.to_string());

            let theme_icon: gpui_kit::AnyElement = match self.theme {
                ThemeChoice::Dark => Icon::new(IconName::Moon)
                    .text_color(rgb(p.text_dim))
                    .with_size(Size::Size(px(16.0)))
                    .into_any_element(),
                ThemeChoice::Light => Icon::new(IconName::Sun)
                    .text_color(rgb(p.text_dim))
                    .with_size(Size::Size(px(16.0)))
                    .into_any_element(),
                ThemeChoice::System => Icon::default()
                    .data(SUN_MOON_SVG)
                    .text_color(rgb(p.text_dim))
                    .with_size(Size::Size(px(16.0)))
                    .into_any_element(),
            };

            // Only one list can be open at a time, so the header never has to
            // fit both.
            let picker: gpui_kit::AnyElement = if self.theme_open {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .children(ThemeChoice::ALL.iter().map(|choice| {
                        let choice = *choice;
                        let selected = choice == self.theme;
                        let weak = weak.clone();
                        div()
                            .id(SharedString::from(format!("theme-{}", choice.id())))
                            .px_2()
                            .py(px(3.0))
                            .rounded_md()
                            .bg(if selected {
                                rgb(p.surface_active)
                            } else {
                                rgb(p.surface)
                            })
                            .text_xs()
                            .text_color(if selected {
                                rgb(p.text)
                            } else {
                                rgb(p.text_dim)
                            })
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(p.surface_active)))
                            .child(choice.label())
                            .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                                    app.choose_theme(choice, cx);
                                });
                            })
                            .into_any_element()
                    }))
                    .into_any_element()
            } else if self.lang_open {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .children(LOCALES.iter().map(|(tag, name)| {
                        let tag = *tag;
                        let selected = tag == current;
                        let weak = weak.clone();
                        div()
                            .id(SharedString::from(format!("lang-{tag}")))
                            .px_2()
                            .py(px(3.0))
                            .rounded_md()
                            .bg(if selected {
                                rgb(p.surface_active)
                            } else {
                                rgb(p.surface)
                            })
                            .text_xs()
                            .text_color(if selected {
                                rgb(p.text)
                            } else {
                                rgb(p.text_dim)
                            })
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(p.surface_active)))
                            .child(*name)
                            .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                                    app.choose_language(tag, cx);
                                });
                            })
                            .into_any_element()
                    }))
                    .into_any_element()
            } else {
                let theme_weak = weak.clone();
                let language_weak = weak.clone();
                let update_weak = weak.clone();
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .when_some(update_available, |d, version| {
                        d.child(
                            div()
                                .id("update-badge")
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .px_2()
                                .py(px(3.0))
                                .rounded_md()
                                .bg(rgb(p.surface_active))
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.85))
                                .child(
                                    Icon::new(IconName::ArrowDown)
                                        .text_color(rgb(p.accent))
                                        .text_xs(),
                                )
                                .child(div().text_xs().text_color(rgb(p.text_dim)).child(version))
                                .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                    let _ = update_weak.update(cx, |app: &mut SwitchApp, cx| {
                                        app.open_update(cx);
                                    });
                                }),
                        )
                    })
                    .child(
                        div()
                            .id("theme")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.0))
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(p.surface_hover)))
                            .child(theme_icon)
                            .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                let _ = theme_weak.update(cx, |app: &mut SwitchApp, cx| {
                                    app.theme_open = true;
                                    app.lang_open = false;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        div()
                            .id("language")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.0))
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(p.surface_hover)))
                            .child(
                                Icon::default()
                                    .data(LANGUAGES_SVG)
                                    .text_color(rgb(p.text_dim))
                                    .with_size(Size::Size(px(17.0))),
                            )
                            .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                let _ = language_weak.update(cx, |app: &mut SwitchApp, cx| {
                                    app.lang_open = true;
                                    app.theme_open = false;
                                    cx.notify();
                                });
                            }),
                    )
                    .into_any_element()
            };

            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .h(px(56.0))
                .pl(px(88.0))
                .pr(px(12.0))
                .border_b(px(1.0))
                .border_color(rgb(p.hairline))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::default()
                                .data(MARK_SVG)
                                .text_color(rgb(p.accent))
                                .with_size(Size::Size(px(17.0))),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(p.text_dim))
                                .child("displaymux"),
                        ),
                )
                .child(picker)
        }

        fn main_pane(&mut self, p: Palette, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
            let weak = cx.entity().downgrade();
            let busy = self.busy;
            let message = self.message.clone();
            // One entry per display the last read found, so the header can
            // offer them as a selector instead of naming one.
            let choices: Vec<(usize, usize, String)> = self
                .displays
                .iter()
                .enumerate()
                .map(|(position, d)| (position, d.display_index, d.display_id.clone()))
                .collect();
            let showing = self.selected_display().map(|d| d.display_index);
            // A display can still be listed while its DDC channel is silent --
            // typically because it is showing another computer. Saying so is the
            // difference between "this is expected" and "the button is broken".
            let unreachable = self
                .selected_display()
                .and_then(|d| d.read_error.as_ref())
                .map(|e| (e.clone(), t!("display_unreachable").to_string()));
            let rows: Vec<gpui_kit::AnyElement> = self
                .selected_display()
                .map(|s| {
                    s.inputs
                        .iter()
                        .map(|i| {
                            input_row(
                                p,
                                i.code.clone(),
                                i.name.clone(),
                                i.advertised,
                                weak.clone(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();

            div()
                .flex()
                .flex_col()
                .size_full()
                .child(self.title_bar(p, cx))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px(px(14.0))
                        .pt(px(12.0))
                        .pb(px(8.0))
                        .child({
                            let weak = weak.clone();
                            // One display: a label. More than one: a selector,
                            // because the window can only show one at a time.
                            if choices.len() < 2 {
                                let name = choices
                                    .first()
                                    .map(|(_, _, id)| id.clone())
                                    .unwrap_or_else(|| t!("no_monitor").to_string());
                                div()
                                    .text_xs()
                                    .text_color(rgb(p.text_faint))
                                    .child(name)
                                    .into_any_element()
                            } else {
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .children(choices.iter().map(|(position, index, id)| {
                                        let position = *position;
                                        let index = *index;
                                        let selected = showing == Some(index);
                                        let weak = weak.clone();
                                        div()
                                            .id(SharedString::from(format!("pick-{index}")))
                                            .px_2()
                                            .py(px(2.0))
                                            .rounded_md()
                                            .bg(if selected {
                                                rgb(p.surface_active)
                                            } else {
                                                rgb(p.surface)
                                            })
                                            .text_xs()
                                            .text_color(if selected {
                                                rgb(p.text)
                                            } else {
                                                rgb(p.text_faint)
                                            })
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(p.surface_active)))
                                            .child(id.clone())
                                            .on_click(
                                                move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                                    let _ = weak.update(
                                                        cx,
                                                        |app: &mut SwitchApp, cx| {
                                                            app.selected = position;
                                                            cx.notify();
                                                        },
                                                    );
                                                },
                                            )
                                            .into_any_element()
                                    }))
                                    .into_any_element()
                            }
                        })
                        .child(if busy {
                            div()
                                .text_xs()
                                .text_color(rgb(p.accent))
                                .child(t!("working"))
                                .into_any_element()
                        } else {
                            let weak = weak.clone();
                            div()
                                .id("refresh")
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(28.0))
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(p.surface_hover)))
                                .child(
                                    Icon::new(IconName::RefreshCw)
                                        .text_color(rgb(p.text_dim))
                                        .text_sm(),
                                )
                                .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                    let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                                        app.send(Request::Refresh, t!("reading"), cx);
                                    });
                                })
                                .into_any_element()
                        }),
                )
                .when_some(unreachable, |d, (error, explanation)| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .mx(px(14.0))
                            .mb(px(8.0))
                            .p(px(9.0))
                            .rounded_md()
                            .bg(rgb(p.surface))
                            .border_1()
                            .border_color(rgb(p.hairline))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(p.accent))
                                    .child(explanation),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(p.text_faint))
                                    .child(error),
                            ),
                    )
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(7.0))
                        .px(px(14.0))
                        .pt(px(2.0))
                        .children(rows),
                )
                .child(
                    div()
                        .mt_auto()
                        .px(px(16.0))
                        .pb(px(14.0))
                        .text_xs()
                        .text_color(rgb(p.text_faint))
                        .child(message),
                )
                .into_any_element()
        }
    }

    impl Render for SwitchApp {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            // Resolved every frame, so "follow the system" tracks an appearance
            // change without needing a subscription.
            self.dark = match self.theme {
                ThemeChoice::Dark => true,
                ThemeChoice::Light => false,
                ThemeChoice::System => matches!(
                    window.appearance(),
                    WindowAppearance::Dark | WindowAppearance::VibrantDark
                ),
            };
            let p = self.palette();
            div()
                .size_full()
                .bg(rgb(p.bg))
                .text_color(rgb(p.text))
                .child(self.main_pane(p, cx))
        }
    }

    /// The tray, and the view it forwards to, held at app level.
    ///
    /// The tray used to live in `SwitchApp`, which put the poll loop on the
    /// window's view. Closing the window dropped that entity, and with it the
    /// loop: the icon stayed in the tray but its menu went dead, and
    /// `QuitMode::Explicit` leaves "Quit dmux" as the only way out. Holding it
    /// here also lets a left click raise the window from outside the view's
    /// borrow, which is what `WindowHandle::update` requires.
    struct Installed {
        /// `None` when the status item could not be created; the window then
        /// remains the only interface.
        tray: Option<Tray>,
        /// The live view, while there is a window. `None` once the window has
        /// been closed, which the tray now survives.
        app: Option<WeakEntity<SwitchApp>>,
        /// The window itself, so raising it does not depend on the root entity
        /// type (`gpui_base::Root` once gpui-kit wraps the view).
        window: Option<gpui_kit::AnyWindowHandle>,
        /// The open confirmation, if any, so a second request does not stack.
        /// The open confirmation, if any, so a second request does not stack.
        confirm: Option<gpui_kit::AnyWindowHandle>,
        /// The open update window, if any.
        update_window: Option<gpui_kit::AnyWindowHandle>,
    }

    /// The languages this build ships.
    const LOCALES: &[(&str, &str)] = &[("en", "English"), ("zh-CN", "简体中文")];

    impl Global for Installed {}

    /// Serve tray clicks for the life of the app, outside any view's borrow.
    ///
    /// `tray-icon` delivers them through global callbacks on threads we do not
    /// own, so `tray.rs` forwards them into a channel and this awaits it.
    ///
    /// Awaiting beats the 120ms timer this used to run on for two reasons, and
    /// neither is CPU: measured over a minute idle, both versions cost the same
    /// 0.8% of a core. It reacts as soon as a click lands rather than up to
    /// 120ms later, and it holds no timer wakeups open, which is what keeps a
    /// laptop out of its deeper idle states.
    fn poll_tray(cx: &mut App, events: async_channel::Receiver<TrayEvent>) {
        cx.spawn(async move |cx| {
            while let Ok(event) = events.recv().await {
                match event {
                    TrayEvent::Quit => {
                        cx.update(|cx| cx.quit());
                        break;
                    }
                    TrayEvent::Show => cx.update(raise_main_window),
                    // The rest is about the display, so it needs a view to act on.
                    TrayEvent::Refresh => cx.update(|cx| forward_to_view(cx, TrayEvent::Refresh)),
                    TrayEvent::Switch { index, code } => {
                        cx.update(|cx| forward_to_view(cx, TrayEvent::Switch { index, code }))
                    }
                    TrayEvent::CheckUpdates => {
                        cx.update(|cx| forward_to_view(cx, TrayEvent::CheckUpdates))
                    }
                }
            }
        })
        .detach();
    }

    /// Hand a click to the view, if a window is still open.
    fn forward_to_view(cx: &mut App, event: TrayEvent) {
        let app = cx.global::<Installed>().app.clone();
        if let Some(app) = app {
            if app
                .update(cx, |app: &mut SwitchApp, cx| app.on_tray(event.clone(), cx))
                .is_ok()
            {
                return;
            }
        }
        // The window is gone. A switch from the menu bar can still confirm
        // itself: the confirmation is its own window, so nothing has to be
        // brought forward for it. Port labels are unknown here, which is why
        // the code stands in for the name.
        if let TrayEvent::CheckUpdates = event {
            let choice = ThemeChoice::from_setting(Settings::load().theme.as_deref());
            let dark = match choice {
                ThemeChoice::Dark => true,
                ThemeChoice::Light => false,
                ThemeChoice::System => matches!(
                    cx.window_appearance(),
                    WindowAppearance::Dark | WindowAppearance::VibrantDark
                ),
            };
            let app = cx.global::<Installed>().app.clone();
            open_update_window(cx, if dark { DARK } else { LIGHT }, None, app);
        }
        if let TrayEvent::Switch { index, code } = event {
            let choice = ThemeChoice::from_setting(Settings::load().theme.as_deref());
            let dark = match choice {
                ThemeChoice::Dark => true,
                ThemeChoice::Light => false,
                ThemeChoice::System => matches!(
                    cx.window_appearance(),
                    WindowAppearance::Dark | WindowAppearance::VibrantDark
                ),
            };
            // No window, so no labels: the port code stands in for its name and
            // the display is named by index.
            open_confirm(
                cx,
                if dark { DARK } else { LIGHT },
                SwitchTarget {
                    display_index: index,
                    display: format!("display {}", index + 1),
                    name: code.clone(),
                    code,
                },
            );
        }
    }

    /// Open the main window and register its view with the tray.
    fn open_main_window(cx: &mut App) -> Option<gpui_kit::AnyWindowHandle> {
        let bounds = Bounds::centered(None, size(px(420.0), px(360.0)), cx);
        // `gpui_kit::open_window` wraps our view in `gpui_base::Root`, which is
        // what installs the theme and the dialog/notification layers. Opening
        // the window through `App::open_window` instead compiles but leaves the
        // tree without that root.
        let (handle, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // The header is ours: the system titlebar goes transparent and
                // the traffic lights move down into our strip.
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(16.0), px(20.0))),
                }),
                // A fixed-size panel. It also removes the macOS zoom gesture,
                // which a quick second click in the title-bar strip -- where the
                // language chips sit -- would otherwise trigger: the window
                // would jump to the full screen instead of changing language.
                is_resizable: false,
                is_minimizable: true,
                ..Default::default()
            },
            cx,
            |_window, cx| cx.new(SwitchApp::new),
        )
        .ok()?;
        cx.global_mut::<Installed>().app = Some(view.downgrade());
        cx.global_mut::<Installed>().window = Some(handle);
        Some(handle)
    }

    /// Bring the window back, restoring it if it is minimized, or build it again
    /// if it was closed.
    ///
    /// Two calls, because they do different jobs and each platform implements
    /// only one of them:
    ///
    /// * `App::activate` raises the *application*. Without it macOS leaves the
    ///   window behind whatever was already in front, because activating a
    ///   window only reorders windows inside its own app. The Windows backend
    ///   implements this as an empty function.
    /// * `Window::activate_window` raises the window itself, which is what
    ///   actually does the work on Windows.
    fn raise_main_window(cx: &mut App) {
        cx.activate(true);
        // The stored handle is used rather than searching `cx.windows()`: with
        // gpui-kit the window's root entity is `gpui_base::Root`, not
        // `SwitchApp`, so a downcast on the view type would never match.
        let handle = cx.global::<Installed>().window;
        let raised = handle
            .map(|h| {
                h.update(cx, |_, window, _| window.activate_window())
                    .is_ok()
            })
            .unwrap_or(false);
        // Closing the window does not quit (`QuitMode::Explicit`), so the tray
        // has to be able to bring it back rather than leave "Show window" and a
        // left click doing nothing at all.
        if !raised {
            open_main_window(cx);
        }
    }

    pub fn run() {
        // Modern entry point: the platform backends live in `gpui_platform`, and
        // the platform instance is now passed in explicitly rather than created
        // implicitly by `Application::new()`.
        // The component icons are paths into an asset bundle. Without this
        // the paths resolve to nothing and every icon is simply absent.
        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(|cx: &mut App| {
                // Initialises the enabled gpui-kit layers (base, styled
                // components, assets). Must run before any window is opened.
                gpui_kit::init(cx);

                // Everything the user chose last time, before any of it can be
                // seen: the component library keeps its own theme for dialogs and
                // text fields, and the locale is process-global.
                let saved = displaymux::settings::Settings::load();
                let choice = ThemeChoice::from_setting(saved.theme.as_deref());
                let dark = match choice {
                    ThemeChoice::Dark => true,
                    ThemeChoice::Light => false,
                    ThemeChoice::System => matches!(
                        cx.window_appearance(),
                        WindowAppearance::Dark | WindowAppearance::VibrantDark
                    ),
                };
                gpui_kit::component::theme::Theme::change(
                    if dark {
                        gpui_kit::component::theme::ThemeMode::Dark
                    } else {
                        gpui_kit::component::theme::ThemeMode::Light
                    },
                    None,
                    cx,
                );
                if let Some(tag) = saved.locale.clone() {
                    rust_i18n::set_locale(&tag);
                }

                // The status item has to be created here: on macOS it needs the main
                // thread with an event loop already running, which is exactly what
                // the `run` closure provides. It goes in before the window so that
                // the view always finds it.
                let (events, clicks) = async_channel::unbounded();
                let tray = Tray::install(events);
                let installed = tray.is_some();
                cx.set_global(Installed {
                    tray,
                    app: None,
                    window: None,
                    confirm: None,
                    update_window: None,
                });
                match installed {
                    // Without a tray there is nothing to bring the window back, so
                    // closing it has to mean quitting rather than leaving the app
                    // running with no interface at all.
                    true => {
                        cx.set_quit_mode(QuitMode::Explicit);
                        poll_tray(cx, clicks);
                    }
                    false => eprintln!("dmux: running without a menu bar item"),
                }

                open_main_window(cx).expect("open window");

                cx.activate(true);
            });
    }
}
