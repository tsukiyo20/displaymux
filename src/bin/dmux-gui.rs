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
    use gpui_kit::component::{Icon, IconName, Sizable, Size};
    // `gpui_kit::*` is GPUI: the facade re-exports `gpui-pre` wholesale, so the
    // element and app types come from the same place as before.
    use gpui_kit::{
        div, point, prelude::*, px, rgb, size, App, Bounds, ClickEvent, Context, Entity, Global,
        IntoElement, QuitMode, SharedString, TitlebarOptions, WeakEntity, Window, WindowBounds,
        WindowOptions,
    };
    use rust_i18n::t;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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
        Status(Box<Result<Status, String>>),
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
                                let out = match ddc::Reader::spawn() {
                                    Err(e) if reader.is_none() => Err(e.to_string()),
                                    _ => {
                                        if reader.is_none() {
                                            reader = ddc::Reader::spawn().ok();
                                        }
                                        match reader.as_mut() {
                                            None => Err("DDC reader unavailable".to_string()),
                                            Some(r) => match r.status(0, READ_TIMEOUT) {
                                                Ok(st) => Ok(st),
                                                Err(e) => {
                                                    // Drop it: the next request builds
                                                    // a fresh IOAVService, the only
                                                    // way to recover a wedged channel.
                                                    reader = None;
                                                    Err(e.to_string())
                                                }
                                            },
                                        }
                                    }
                                };
                                if res_tx.send(Response::Status(Box::new(out))).is_err() {
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
        status: Option<Status>,
        message: SharedString,
        busy: bool,
        index: usize,
        /// The target of an in-flight switch, remembered once it is accepted.
        pending_input: Option<String>,
        view: View,
        settings: Settings,
        /// One text field per port while the settings pane is open. Built on
        /// demand because `InputState` needs the window that renders it.
        editors: Vec<(String, Entity<InputState>)>,
    }

    impl SwitchApp {
        fn new(cx: &mut Context<Self>) -> Self {
            let worker = Worker::spawn();
            let _ = worker.tx.send(Request::Refresh);
            let app = SwitchApp {
                worker,
                status: None,
                message: t!("reading").into(),
                busy: true,
                index: 0,
                pending_input: None,
                view: View::Main,
                settings: Settings::load(),
                editors: Vec::new(),
            };
            app.poll(cx);
            app
        }

        /// Push current state into the menu bar menu.
        ///
        /// The tray itself lives in [`Installed`], not in this view: it has to
        /// outlive the window, which `QuitMode::Explicit` keeps the app running
        /// past, and a tray that dies with the window leaves an icon whose menu
        /// does nothing at all -- not even quit.
        fn sync_tray(&mut self, cx: &mut Context<Self>) {
            let Some(tray) = cx.global_mut::<Installed>().tray.as_mut() else {
                return;
            };
            let display = self
                .status
                .as_ref()
                .map(|s| s.display_id.clone())
                .unwrap_or_else(|| t!("no_monitor").to_string());
            let inputs: Vec<(String, String)> = self
                .status
                .as_ref()
                .map(|s| {
                    s.inputs
                        .iter()
                        .map(|i| (i.code.clone(), i.name.clone()))
                        .collect()
                })
                .unwrap_or_default();
            tray.update(&display, &inputs);
        }

        /// Act on the tray clicks that need this view.
        ///
        /// `Show` and `Quit` never reach here: they are handled where the tray
        /// lives, outside this view's borrow, because raising the window means
        /// borrowing the view again.
        fn on_tray(&mut self, event: TrayEvent, cx: &mut Context<Self>) {
            match event {
                TrayEvent::Refresh => self.send(Request::Refresh, t!("reading"), cx),
                TrayEvent::Switch(code) => {
                    self.pending_input = Some(code.clone());
                    self.send(
                        Request::Switch {
                            index: self.index,
                            code: code.clone(),
                        },
                        t!("switching_to", code = code.as_str()),
                        cx,
                    )
                }
                TrayEvent::Show | TrayEvent::Quit => {}
            }
        }

        /// Poll the worker's response channel on a timer, so the UI thread never
        /// blocks on DDC. This is the bridge to the worker thread.
        fn poll(&self, cx: &mut Context<Self>) {
            let rx = self.worker.rx.clone();
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(POLL_INTERVAL).await;
                    let got = rx.lock().ok().map(|r| r.try_recv());
                    match got {
                        Some(Ok(resp)) => {
                            let _ = this.update(cx, |app, cx| {
                                app.apply(resp, cx);
                                cx.notify();
                            });
                            break;
                        }
                        Some(Err(std::sync::mpsc::TryRecvError::Empty)) => continue,
                        // The worker died without answering. Clear `busy` so the
                        // UI does not sit on "working..." forever.
                        _ => {
                            let _ = this.update(cx, |app, cx| {
                                app.busy = false;
                                app.message = "DDC worker stopped unexpectedly".into();
                                cx.notify();
                            });
                            break;
                        }
                    }
                }
            })
            .detach();
        }

        fn apply(&mut self, resp: Response, cx: &mut Context<Self>) {
            self.busy = false;
            match resp {
                Response::Status(st) => match *st {
                    Ok(s) => {
                        // Report only what we can stand behind. The monitor's
                        // VCP 0x60 value is not shown as "current input" because
                        // on the reference monitor it is a constant that does not
                        // follow the actual input; the honest state comes from
                        // the USB-based banner above.
                        // Only speak when there is something to say. A list of
                        // ports is its own explanation.
                        self.message = if s.inputs.is_empty() {
                            t!("no_inputs").into()
                        } else {
                            String::new().into()
                        };
                        if !s.inputs.is_empty() {
                            self.index = s.display_index;
                        }
                        self.status = Some(s);
                    }
                    Err(e) => {
                        self.message = t!("read_failed", error = e.to_string().as_str()).into()
                    }
                },
                Response::Switched(r) => match *r {
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
                        self.message = t!("switch_failed", error = e.to_string().as_str()).into()
                    }
                },
            }
            self.sync_tray(cx);
        }

        fn send(&mut self, req: Request, note: impl Into<SharedString>, cx: &mut Context<Self>) {
            if self.busy {
                return;
            }
            self.busy = true;
            self.message = note.into();
            let _ = self.worker.tx.send(req);
            self.poll(cx);
            cx.notify();
        }
    }

    /// One switchable input. Deliberately carries no state: the row is an
    /// action, not a reading.
    /// One place for the palette, so the two panes cannot drift apart.
    ///
    /// The accent is the amber from the application icon: it is the one colour
    /// that already means "this tool" everywhere else it appears.
    mod look {
        pub const BG: u32 = 0x141419;
        pub const SURFACE: u32 = 0x1d1d24;
        pub const SURFACE_HOVER: u32 = 0x282832;
        pub const HAIRLINE: u32 = 0x26262f;
        pub const TEXT: u32 = 0xececf2;
        pub const TEXT_DIM: u32 = 0x9a9aa8;
        pub const TEXT_FAINT: u32 = 0x666673;
        pub const ACCENT: u32 = 0xffb627;
    }

    /// The application mark, redrawn for a 24px box and in a single colour.
    ///
    /// The shipped `assets/mark.svg` carries the amber/dormant pair, but an
    /// `Icon` tints the whole SVG with one colour, so a two-tone mark would be
    /// flattened into a grey smear at this size. Same geometry, one colour,
    /// tinted with the accent instead.
    const MARK_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M16 5.4V9.5L12 14.1"/><path d="M8 3.2V9.5L12 14.1V20.8"/></svg>"#;

    /// A square, borderless icon button. Used for every action in the chrome.
    fn icon_button(
        id: &'static str,
        icon: IconName,
        weak: WeakEntity<SwitchApp>,
        on_click: fn(&mut SwitchApp, &mut Context<SwitchApp>),
    ) -> impl IntoElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .size(px(28.0))
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(rgb(look::SURFACE_HOVER)))
            .child(Icon::new(icon).text_color(rgb(look::TEXT_DIM)).text_sm())
            .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                let _ = weak.update(cx, |app: &mut SwitchApp, cx| on_click(app, cx));
            })
    }

    /// One switchable input.
    ///
    /// A row is an action, never a reading: it carries the port, its name and a
    /// chevron, and nothing that claims to know where the display is.
    fn input_row(
        code: String,
        name: String,
        advertised: bool,
        weak: WeakEntity<SwitchApp>,
    ) -> impl IntoElement {
        div()
            .id(SharedString::from(format!("input-{}", code)))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .w_full()
            .px_3()
            .py(px(11.0))
            .rounded_lg()
            .bg(rgb(look::SURFACE))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(look::SURFACE_HOVER)))
            .child(
                div()
                    .w(px(34.0))
                    .text_sm()
                    .text_color(rgb(look::TEXT_FAINT))
                    .child(code.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(rgb(look::TEXT))
                    .child(name),
            )
            .when(!advertised, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(rgb(look::TEXT_FAINT))
                        .child(t!("unlisted")),
                )
            })
            .child(
                Icon::new(IconName::ChevronRight)
                    .text_color(rgb(look::TEXT_FAINT))
                    .text_sm(),
            )
            .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                let code = code.clone();
                let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                    app.send(
                        Request::Switch {
                            index: app.index,
                            code: code.clone(),
                        },
                        t!("switching_to", code = code.as_str()),
                        cx,
                    );
                });
            })
    }

    impl SwitchApp {
        /// Enter the settings pane. The text fields are built during render,
        /// because `InputState` needs the window that will host it.
        fn open_settings(&mut self, cx: &mut Context<Self>) {
            self.editors.clear();
            self.view = View::Settings;
            cx.notify();
        }

        fn cancel_settings(&mut self, cx: &mut Context<Self>) {
            self.editors.clear();
            self.view = View::Main;
            cx.notify();
        }

        /// One text field per advertised port, seeded with the current label.
        fn ensure_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            if !self.editors.is_empty() {
                return;
            }
            let inputs: Vec<(String, String)> = self
                .status
                .as_ref()
                .map(|s| {
                    s.inputs
                        .iter()
                        .map(|i| (i.code.clone(), i.name.clone()))
                        .collect()
                })
                .unwrap_or_default();
            for (code, name) in inputs {
                let editor = cx.new(|cx| InputState::new(window, cx).default_value(name));
                self.editors.push((code, editor));
            }
        }

        /// Write every field to labels.json and reload.
        ///
        /// An empty field removes the label rather than storing an empty one, so
        /// the port falls back to the name the monitor reports.
        fn save_labels(&mut self, cx: &mut Context<Self>) {
            let Some(display) = self.status.as_ref().map(|s| s.display_id.clone()) else {
                return;
            };
            let mut labels = Labels::load();
            for (code, editor) in &self.editors {
                let text = editor.read(cx).value().trim().to_string();
                // A field that still holds the MCCS name is not a label: writing
                // it would pin the name the monitor reports and stop it following
                // the hardware. Only a real difference is worth persisting.
                let mccs = u16::from_str_radix(code.trim_start_matches("0x"), 16)
                    .map(displaymux::vcp::input_name)
                    .unwrap_or_default();
                if text.is_empty() || text == mccs {
                    labels.clear(&display, code);
                } else {
                    labels.set(&display, code, &text);
                }
            }
            match labels.save() {
                Ok(_) => {
                    self.editors.clear();
                    self.view = View::Main;
                    self.send(Request::Refresh, t!("reading"), cx);
                    self.message = String::new().into();
                }
                Err(e) => {
                    self.message = t!("read_failed", error = e.to_string().as_str()).into();
                    cx.notify();
                }
            }
        }

        fn choose_language(&mut self, tag: &'static str, cx: &mut Context<Self>) {
            // The locale is process-global, so this also changes the component
            // library's own strings.
            rust_i18n::set_locale(tag);
            self.settings.locale = Some(tag.to_string());
            let _ = self.settings.save();
            // The menu bar was built with the previous language.
            self.sync_tray(cx);
            cx.notify();
        }

        /// The custom title bar: the mark, the name, and the one global action.
        ///
        /// The window is created with a transparent system titlebar and the
        /// traffic lights pushed right, so this strip is where they sit and the
        /// content starts after them.
        fn title_bar(&self, cx: &Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .h(px(56.0))
                .pl(px(88.0))
                .pr(px(12.0))
                .border_b(px(1.0))
                .border_color(rgb(look::HAIRLINE))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::default()
                                .data(MARK_SVG)
                                .text_color(rgb(look::ACCENT))
                                .with_size(Size::Size(px(17.0))),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(look::TEXT_DIM))
                                .child("displaymux"),
                        ),
                )
                .child(icon_button(
                    "open-settings",
                    IconName::Settings,
                    cx.entity().downgrade(),
                    |app, cx| app.open_settings(cx),
                ))
        }

        fn main_pane(&mut self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
            let weak = cx.entity().downgrade();
            let busy = self.busy;
            let message = self.message.clone();
            let display = self
                .status
                .as_ref()
                .map(|s| s.display_id.clone())
                .unwrap_or_else(|| t!("no_monitor").to_string());

            let rows: Vec<gpui_kit::AnyElement> = self
                .status
                .as_ref()
                .map(|s| {
                    s.inputs
                        .iter()
                        .map(|i| {
                            input_row(i.code.clone(), i.name.clone(), i.advertised, weak.clone())
                                .into_any_element()
                        })
                        .collect()
                })
                .unwrap_or_default();

            div()
                .flex()
                .flex_col()
                .size_full()
                .child(self.title_bar(cx))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .px(px(14.0))
                        .pt(px(12.0))
                        .pb(px(8.0))
                        .child(
                            div().flex().flex_row().items_center().gap_1().child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(look::TEXT_FAINT))
                                    .child(display),
                            ),
                        )
                        .child(if busy {
                            div()
                                .text_xs()
                                .text_color(rgb(look::ACCENT))
                                .child(t!("working"))
                                .into_any_element()
                        } else {
                            icon_button("refresh", IconName::RefreshCw, weak.clone(), |app, cx| {
                                app.send(Request::Refresh, t!("reading"), cx);
                            })
                            .into_any_element()
                        }),
                )
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
                        .text_color(rgb(look::TEXT_FAINT))
                        .child(message),
                )
                .into_any_element()
        }

        fn settings_pane(
            &mut self,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> gpui_kit::AnyElement {
            self.ensure_editors(window, cx);
            let weak = cx.entity().downgrade();
            let current = self.settings.locale.clone().unwrap_or_default();

            let languages: Vec<gpui_kit::AnyElement> = LOCALES
                .iter()
                .map(|(tag, name)| {
                    let tag = *tag;
                    let selected = tag == current;
                    let weak = weak.clone();
                    div()
                        .id(SharedString::from(format!("lang-{tag}")))
                        .px_3()
                        .py(px(6.0))
                        .rounded_md()
                        .bg(if selected {
                            rgb(look::SURFACE_HOVER)
                        } else {
                            rgb(look::SURFACE)
                        })
                        .border_1()
                        .border_color(if selected {
                            rgb(look::ACCENT)
                        } else {
                            rgb(look::HAIRLINE)
                        })
                        .text_sm()
                        .text_color(if selected {
                            rgb(look::TEXT)
                        } else {
                            rgb(look::TEXT_DIM)
                        })
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(look::SURFACE_HOVER)))
                        .child(*name)
                        .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                            let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                                app.choose_language(tag, cx);
                            });
                        })
                        .into_any_element()
                })
                .collect();

            let fields: Vec<gpui_kit::AnyElement> = self
                .editors
                .iter()
                .map(|(code, editor)| {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .w(px(34.0))
                                .text_sm()
                                .text_color(rgb(look::TEXT_FAINT))
                                .child(code.clone()),
                        )
                        .child(div().flex_1().child(Input::new(editor)))
                        .into_any_element()
                })
                .collect();

            div()
                .flex()
                .flex_col()
                .size_full()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .h(px(56.0))
                        .pl(px(88.0))
                        .pr(px(12.0))
                        .border_b(px(1.0))
                        .border_color(rgb(look::HAIRLINE))
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(look::TEXT_DIM))
                                .child(t!("settings")),
                        )
                        .child(icon_button(
                            "close-settings",
                            IconName::ArrowLeft,
                            weak.clone(),
                            |app, cx| app.cancel_settings(cx),
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .px(px(14.0))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(64.0))
                                        .text_xs()
                                        .text_color(rgb(look::TEXT_FAINT))
                                        .child(t!("language")),
                                )
                                .children(languages),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(look::TEXT_FAINT))
                                        .child(t!("port_labels")),
                                )
                                .children(fields)
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(look::TEXT_FAINT))
                                        .child(t!("port_labels_hint")),
                                ),
                        ),
                )
                .child(
                    div()
                        .mt_auto()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .px(px(14.0))
                        .pb(px(14.0))
                        .child({
                            let weak = weak.clone();
                            div()
                                .id("save")
                                .px_3()
                                .py(px(6.0))
                                .rounded_md()
                                .bg(rgb(look::ACCENT))
                                .text_color(rgb(0x1a1205))
                                .text_sm()
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.9))
                                .child(t!("save"))
                                .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                    let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                                        app.save_labels(cx);
                                    });
                                })
                        })
                        .child({
                            let weak = weak.clone();
                            div()
                                .id("cancel")
                                .px_3()
                                .py(px(6.0))
                                .rounded_md()
                                .bg(rgb(look::SURFACE))
                                .text_color(rgb(look::TEXT_DIM))
                                .text_sm()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(look::SURFACE_HOVER)))
                                .child(t!("cancel"))
                                .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                    let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                                        app.cancel_settings(cx);
                                    });
                                })
                        }),
                )
                .into_any_element()
        }
    }

    impl Render for SwitchApp {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let pane = if self.view == View::Settings {
                self.settings_pane(window, cx)
            } else {
                self.main_pane(cx)
            };
            div()
                .size_full()
                .bg(rgb(look::BG))
                .text_color(rgb(look::TEXT))
                .child(pane)
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
    }

    /// Which pane the window is showing.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum View {
        Main,
        Settings,
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
                    TrayEvent::Switch(code) => {
                        cx.update(|cx| forward_to_view(cx, TrayEvent::Switch(code)))
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
            let _ = app.update(cx, |app, cx| app.on_tray(event, cx));
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

                // The component library ships a light theme by default; this window is
                // dark throughout, so its inputs would otherwise render as white boxes.
                gpui_kit::component::theme::Theme::change(
                    gpui_kit::component::theme::ThemeMode::Dark,
                    None,
                    cx,
                );

                // A saved language wins over the compile-time default. The locale is
                // process-global, so this also switches the component library's own
                // strings (dialog buttons, date pickers) in one call.
                if let Some(tag) = displaymux::settings::Settings::load().locale {
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
