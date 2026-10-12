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
//! Switching a monitor's input is **not reversible from this side**. When the
//! display moves to another machine, this host loses the video path — and on a
//! desk where the keyboard and mouse hang off the monitor's USB hub, it loses
//! its input devices too. There is no hotkey that could bring the display back.
//!
//! For the same reason the UI never states which input the display is on. Some
//! monitors report a constant VCP 0x60 regardless of the selected input, so the
//! readback is not evidence; the only trustworthy "is it showing us?" signal is
//! [`displaymux::session`], and rows are labelled "last sent here" because that
//! is a record of our own command rather than a reading of the hardware.
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
    use displaymux::session::{self, Session};
    use displaymux::state::State;
    use displaymux::tray::{Tray, TrayEvent};
    use gpui::{
        div, prelude::*, px, rgb, size, App, Bounds, ClickEvent, Context, Global, IntoElement,
        QuitMode, SharedString, WeakEntity, Window, WindowBounds, WindowHandle, WindowOptions,
    };
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const READ_TIMEOUT: Duration = Duration::from_secs(12);
    const POLL_INTERVAL: Duration = Duration::from_millis(60);
    /// Tray clicks are checked on a timer; the menu bar is a secondary surface
    /// so a leisurely poll is fine and keeps everything on the GPUI thread.
    const TRAY_POLL_INTERVAL: Duration = Duration::from_millis(120);
    /// How often the USB tree is inspected for the "on screen?" signal. Cheap
    /// enough at this rate, and a switch takes about this long to settle anyway.
    const SESSION_POLL_INTERVAL: Duration = Duration::from_millis(1500);

    #[derive(Debug)]
    enum Request {
        Refresh,
        Switch { index: usize, code: String },
    }

    #[derive(Debug)]
    enum Response {
        Status(Box<Result<Status, String>>),
        Switched(Box<Result<String, String>>),
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
                                    .map(|o| o.verdict().to_string())
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
        /// Persisted "where is this host plugged in" knowledge. Authoritative
        /// over the monitor's readback, which is unreliable on some monitors
        /// (see `displaymux::state`).
        state: State,
        /// The input we last switched to. Used only to mark a row as "you sent
        /// the display here"; it is *not* a claim about the monitor's state.
        current: Option<String>,
        /// Whether the monitor is routed to this host, measured from USB device
        /// presence rather than asked of the monitor. See `displaymux::session`.
        session: Session,
        /// The target of an in-flight switch, remembered once it is accepted.
        pending_input: Option<String>,
    }

    impl SwitchApp {
        fn new(cx: &mut Context<Self>) -> Self {
            let worker = Worker::spawn();
            let _ = worker.tx.send(Request::Refresh);
            let app = SwitchApp {
                worker,
                status: None,
                message: "reading monitor state...".into(),
                busy: true,
                index: 0,
                state: State::load(),
                current: None,
                session: session::current(),
                pending_input: None,
            };
            app.poll(cx);
            app.poll_session(cx);
            app
        }

        /// Re-read the USB-based "is this host on screen?" signal on a timer.
        ///
        /// This is the honest replacement for asking the monitor which input it
        /// is on: the keyboard and mouse sit behind the monitor's hub, so their
        /// presence is evidence, not a claim.
        fn poll_session(&self, cx: &mut Context<Self>) {
            cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(SESSION_POLL_INTERVAL).await;
                let now = session::current();
                let changed = this
                    .update(cx, |app, _| {
                        let changed = app.session != now;
                        app.session = now;
                        changed
                    })
                    .unwrap_or(false);
                if changed {
                    let _ = this.update(cx, |app, cx| {
                        app.sync_tray(cx);
                        cx.notify();
                    });
                }
            })
            .detach();
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
                .unwrap_or_else(|| "no DDC monitor".to_string());
            let current = self.current.clone().map(|code| {
                let name = self
                    .status
                    .as_ref()
                    .and_then(|s| {
                        s.inputs
                            .iter()
                            .find(|i| i.code.eq_ignore_ascii_case(&code))
                            .map(|i| i.name.clone())
                    })
                    .unwrap_or_else(|| code.clone());
                (code, name)
            });
            let inputs: Vec<(String, String, bool)> = self
                .status
                .as_ref()
                .map(|s| {
                    s.inputs
                        .iter()
                        .map(|i| {
                            let is_current = self
                                .current
                                .as_deref()
                                .map(|c| c.eq_ignore_ascii_case(&i.code))
                                .unwrap_or(i.current);
                            (i.code.clone(), i.name.clone(), is_current)
                        })
                        .collect()
                })
                .unwrap_or_default();
            tray.update(
                &display,
                current.as_ref().map(|(c, n)| (c.as_str(), n.as_str())),
                &inputs,
            );
        }

        /// Act on the tray clicks that need this view.
        ///
        /// `Show` and `Quit` never reach here: they are handled where the tray
        /// lives, outside this view's borrow, because raising the window means
        /// borrowing the view again.
        fn on_tray(&mut self, event: TrayEvent, cx: &mut Context<Self>) {
            match event {
                TrayEvent::Refresh => self.send(Request::Refresh, "reading monitor state...", cx),
                TrayEvent::Switch(code) => {
                    self.pending_input = Some(code.clone());
                    self.send(
                        Request::Switch {
                            index: self.index,
                            code: code.clone(),
                        },
                        format!("switching to {}...", code),
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
                        self.message = if s.inputs.is_empty() {
                            "monitor did not report any switchable inputs".into()
                        } else {
                            format!("{} inputs available", s.inputs.len()).into()
                        };
                        if !s.inputs.is_empty() {
                            self.index = s.display_index;
                        }
                        // Deliberately *not* taking the monitor's VCP 0x60 as
                        // "where we are": on the reference monitor it reports a
                        // constant value regardless of the actual input.
                        let remembered =
                            self.state.last_input(&s.display_id).map(|c| c.to_string());
                        self.current = remembered;
                        self.status = Some(s);
                    }
                    Err(e) => self.message = format!("read failed: {}", e).into(),
                },
                Response::Switched(r) => match *r {
                    Ok(verdict) => {
                        // The write was accepted. That is all we can honestly
                        // claim: whether the monitor acted on it is not knowable
                        // from here, and once it does act, this host loses the
                        // display entirely.
                        if let (Some(code), Some(id)) = (
                            self.pending_input.take(),
                            self.status.as_ref().map(|s| s.display_id.clone()),
                        ) {
                            let mut text = verdict;
                            if let Some(back) =
                                self.status.as_ref().and_then(|s| s.current_input.clone())
                            {
                                if !back.eq_ignore_ascii_case(&code) {
                                    // A write that does not move the reported
                                    // value is the signature of a monitor whose
                                    // VCP 0x60 does not follow the input, so say
                                    // so instead of implying it lied about ours.
                                    text = format!(
                                        "command sent for {} (monitor still reports {} for VCP 0x60)",
                                        code, back
                                    );
                                }
                            }
                            self.state.remember_input(&id, &code);
                            self.state.save();
                            self.current = Some(code);
                            self.message = text.into();
                        } else {
                            self.message = verdict.into();
                        }
                    }
                    Err(e) => self.message = format!("switch failed: {}", e).into(),
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

    fn input_row(
        code: String,
        name: String,
        current: bool,
        advertised: bool,
        app: gpui::WeakEntity<SwitchApp>,
    ) -> impl IntoElement {
        let row_bg = if current {
            rgb(0x2d4f7c)
        } else {
            rgb(0x26262e)
        };
        let code_for_click = code.clone();
        div()
            .id(SharedString::from(format!("input-{}", code)))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(row_bg)
            .cursor_pointer()
            .hover(|s| s.bg(rgb(0x3a3a46)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(div().text_color(rgb(0x8ab4f8)).child(code.clone()))
                    .child(div().text_color(rgb(0xe6e6e6)).child(name))
                    .when(!advertised, |d| {
                        d.child(div().text_color(rgb(0x8a8a90)).text_sm().child("unlisted"))
                    }),
            )
            .child(if current {
                div()
                    .text_color(rgb(0xa8a8b0))
                    .child("last sent here")
                    .into_any_element()
            } else {
                div()
                    .text_color(rgb(0x8ab4f8))
                    .child("switch")
                    .into_any_element()
            })
            .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                let code = code_for_click.clone();
                let _ = app.update(cx, |app: &mut SwitchApp, cx| {
                    app.send(
                        Request::Switch {
                            index: app.index,
                            code: code.clone(),
                        },
                        format!("switching to {}...", code),
                        cx,
                    );
                });
            })
    }

    impl Render for SwitchApp {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let title = self
                .status
                .as_ref()
                .map(|s| s.display_id.clone())
                .unwrap_or_else(|| "no DDC monitor".to_string());
            let busy = self.busy;
            let message = self.message.clone();
            let weak = cx.entity().downgrade();
            let session = self.session;

            let current = self.current.clone();
            let rows: Vec<gpui::AnyElement> = self
                .status
                .as_ref()
                .map(|s| {
                    s.inputs
                        .iter()
                        .map(|i| {
                            let is_current = current
                                .as_deref()
                                .map(|c| c.eq_ignore_ascii_case(&i.code))
                                .unwrap_or(i.current);
                            input_row(
                                i.code.clone(),
                                i.name.clone(),
                                is_current,
                                i.advertised,
                                weak.clone(),
                            )
                            .into_any_element()
                        })
                        .collect()
                })
                .unwrap_or_default();

            div()
                .flex()
                .flex_col()
                .gap_2()
                .bg(rgb(0x1b1b20))
                .size_full()
                .p_3()
                .text_color(rgb(0xe6e6e6))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .items_center()
                        .child(div().text_lg().child(title))
                        .child(
                            div()
                                .id("refresh")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(rgb(0x2f2f38))
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(0x3d3d48)))
                                .child(if busy { "working..." } else { "refresh" })
                                .on_click({
                                    let weak = weak.clone();
                                    move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                        let _ = weak.update(cx, |app: &mut SwitchApp, cx| {
                                            app.send(
                                                Request::Refresh,
                                                "reading monitor state...",
                                                cx,
                                            );
                                        });
                                    }
                                }),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(match session {
                            Session::Visible => rgb(0x8fdf8f),
                            Session::SwitchedAway => rgb(0xe0a060),
                            Session::Unknown => rgb(0x8a8a90),
                        })
                        .child(match session {
                            Session::Visible => {
                                "This display is showing this computer (keyboard and mouse present)"
                            }
                            Session::SwitchedAway => {
                                "This display has switched away - the only way back is the monitor's own input menu"
                            }
                            Session::Unknown => {
                                "Display state unknown (peripherals are not behind the monitor)"
                            }
                        }),
                )
                .child(div().flex().flex_col().gap_1().children(rows))
                .child(
                    div()
                        .mt_auto()
                        .text_sm()
                        .text_color(rgb(0x9a9aa2))
                        .child(message),
                )
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
    }

    impl Global for Installed {}

    /// Poll the tray for the life of the app, outside any view's borrow.
    ///
    /// `tray-icon` delivers clicks through global callbacks on threads we do not
    /// own, so they are forwarded into a channel and drained here, on the GPUI
    /// thread.
    fn poll_tray(cx: &mut App) {
        cx.spawn(async move |cx| loop {
            cx.background_executor().timer(TRAY_POLL_INTERVAL).await;
            let event = cx.update(|cx| {
                cx.global_mut::<Installed>()
                    .tray
                    .as_mut()
                    .and_then(|tray| tray.try_recv())
            });
            let Some(event) = event else {
                continue;
            };
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
    fn open_main_window(cx: &mut App) -> Option<WindowHandle<SwitchApp>> {
        let bounds = Bounds::centered(None, size(px(420.0), px(420.0)), cx);
        let handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_window, cx| cx.new(SwitchApp::new),
            )
            .ok()?;
        // Give the tray the view it forwards to.
        if let Ok(app) = handle.update(cx, |_, _, cx| cx.entity().downgrade()) {
            cx.global_mut::<Installed>().app = Some(app);
        }
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
        let handle = cx
            .windows()
            .into_iter()
            .find_map(|any| any.downcast::<SwitchApp>());
        match handle {
            Some(handle) => {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            }
            // Closing the window does not quit (`QuitMode::Explicit`), so the
            // tray has to be able to bring it back rather than leave "Show
            // window" and a left click doing nothing at all.
            None => {
                open_main_window(cx);
            }
        }
    }

    pub fn run() {
        // Modern entry point: the platform backends live in `gpui_platform`, and
        // the platform instance is now passed in explicitly rather than created
        // implicitly by `Application::new()`.
        gpui_platform::application().run(|cx: &mut App| {
            // A menu bar app must outlive its window; closing the window is not
            // a request to exit. `TrayEvent::Quit` is the only way out.
            cx.set_quit_mode(QuitMode::Explicit);

            // The status item has to be created here: on macOS it needs the main
            // thread with an event loop already running, which is exactly what
            // the `run` closure provides. It goes in before the window so that
            // the view always finds it.
            let tray = Tray::install();
            let installed = tray.is_some();
            cx.set_global(Installed { tray, app: None });
            if installed {
                poll_tray(cx);
            } else {
                eprintln!("dmux: running without a menu bar item");
            }

            open_main_window(cx).expect("open window");

            cx.activate(true);
        });
    }
}
