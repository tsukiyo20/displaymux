//! dmux-gui - GPUI front end for the monitor input switcher.
//!
//! This binary is gated behind the `gui` feature so the CLI (and a plain
//! `cargo test`) do not pull in GPUI's ~700-crate dependency tree:
//!
//!     cargo build --release --features gui --bin dmux-gui
//!
//! Without the feature this file compiles to a stub that explains itself.
//!
//! ## Threading model
//!
//! Forced by the hardware behaviour measured in `docs/hardware-notes.md`:
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
    use displaymux::tray::{Tray, TrayEvent};
    use gpui::{
        div, prelude::*, px, rgb, size, App, Bounds, ClickEvent, Context, IntoElement, QuitMode,
        SharedString, Window, WindowBounds, WindowOptions,
    };
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const READ_TIMEOUT: Duration = Duration::from_secs(12);
    const POLL_INTERVAL: Duration = Duration::from_millis(60);
    /// Tray clicks are checked on a timer; the menu bar is a secondary surface
    /// so a leisurely poll is fine and keeps everything on the GPUI thread.
    const TRAY_POLL_INTERVAL: Duration = Duration::from_millis(120);

    #[derive(Debug)]
    enum Request {
        Refresh,
        Switch { index: usize, code: String },
        Reclaim { index: usize, code: String, every: Duration, up_to: Duration },
    }

    #[derive(Debug)]
    enum Response {
        Status(Box<Result<Status, String>>),
        Switched(Box<Result<String, String>>),
        Reclaim(Box<Result<String, String>>),
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
                            Request::Reclaim { index, code, every, up_to } => {
                                let out = reclaim(index, &code, every, up_to, &mut reader)
                                    .map_err(|e| e.to_string());
                                reader = None;
                                if res_tx.send(Response::Reclaim(Box::new(out))).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
                .expect("spawn ddc worker");

            Worker { tx: req_tx, rx: Arc::new(Mutex::new(res_rx)) }
        }
    }

    /// Re-assert `code` every `every` until this host is selected again, or
    /// `up_to` elapses.
    ///
    /// This is what lets the Mac take the monitor back from the other machine:
    /// DDC stays reachable while the monitor displays someone else, so repeating
    /// the request wins as soon as the other input stops being driven.
    fn reclaim(
        index: usize,
        code: &str,
        every: Duration,
        up_to: Duration,
        reader: &mut Option<ddc::Reader>,
    ) -> Result<String, ddc::DdcError> {
        let start = Instant::now();
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            if reader.is_none() {
                *reader = ddc::Reader::spawn().ok();
            }
            let already = reader
                .as_mut()
                .and_then(|r| r.status(index, READ_TIMEOUT).ok())
                .and_then(|s| s.current_input)
                .map(|c| c.eq_ignore_ascii_case(code))
                .unwrap_or(false);
            if already {
                return Ok(format!("already on {} after {} probe(s)", code, attempts));
            }
            ddc::set_input(index, code, READ_TIMEOUT)?;
            if start.elapsed() >= up_to {
                return Ok(format!(
                    "gave up after {} attempt(s) / {}s",
                    attempts,
                    start.elapsed().as_secs()
                ));
            }
            std::thread::sleep(every);
        }
    }

    struct SwitchApp {
        worker: Worker,
        status: Option<Status>,
        message: SharedString,
        busy: bool,
        index: usize,
        /// `None` when the status item could not be created; the window then
        /// remains the only interface.
        tray: Option<Tray>,
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
                tray: None,
            };
            app.poll(cx);
            app
        }

        /// Attach the menu bar item and start listening to it.
        fn attach_tray(&mut self, tray: Tray, cx: &mut Context<Self>) {
            self.tray = Some(tray);
            self.sync_tray();
            self.poll_tray(cx);
        }

        /// Push current state into the menu bar menu.
        fn sync_tray(&mut self) {
            let Some(tray) = self.tray.as_mut() else { return };
            let display = self
                .status
                .as_ref()
                .map(|s| s.display_id.clone())
                .unwrap_or_else(|| "no DDC monitor".to_string());
            let current = self
                .status
                .as_ref()
                .and_then(|s| s.current_input.clone())
                .map(|code| {
                    let name = self
                        .status
                        .as_ref()
                        .and_then(|s| s.current_input_name.clone())
                        .unwrap_or_default();
                    (code, name)
                });
            let inputs: Vec<(String, String, bool)> = self
                .status
                .as_ref()
                .map(|s| {
                    s.inputs
                        .iter()
                        .map(|i| (i.code.clone(), i.name.clone(), i.current))
                        .collect()
                })
                .unwrap_or_default();
            tray.update(
                &display,
                current.as_ref().map(|(c, n)| (c.as_str(), n.as_str())),
                &inputs,
            );
        }

        /// Poll tray clicks on a timer, on the GPUI thread.
        fn poll_tray(&self, cx: &mut Context<Self>) {
            cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(TRAY_POLL_INTERVAL).await;
                let event = this.update(cx, |app, _| {
                    app.tray.as_ref().and_then(|t| t.try_recv())
                });
                match event {
                    Ok(Some(event)) => {
                        if !this
                            .update(cx, |app, cx| app.handle_tray(event, cx))
                            .unwrap_or(false)
                        {
                            break;
                        }
                    }
                    Ok(None) => continue,
                    Err(_) => break,
                }
            })
            .detach();
        }

        /// Returns `false` when the app should stop polling (quit).
        fn handle_tray(&mut self, event: TrayEvent, cx: &mut Context<Self>) -> bool {
            match event {
                TrayEvent::Quit => {
                    cx.quit();
                    return false;
                }
                TrayEvent::Show => {
                    cx.activate(true);
                    return true;
                }
                TrayEvent::Refresh => self.send(Request::Refresh, "reading monitor state...", cx),
                TrayEvent::Switch(code) => self.send(
                    Request::Switch { index: self.index, code: code.clone() },
                    format!("switching to {}...", code),
                    cx,
                ),
                TrayEvent::Reclaim => {
                    let Some(code) = self.status.as_ref().and_then(|s| s.current_input.clone())
                    else {
                        return true;
                    };
                    self.send(
                        Request::Reclaim {
                            index: self.index,
                            code: code.clone(),
                            every: Duration::from_secs(2),
                            up_to: Duration::from_secs(20),
                        },
                        "reclaiming...",
                        cx,
                    );
                }
            }
            true
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
                                app.apply(resp);
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

        fn apply(&mut self, resp: Response) {
            self.busy = false;
            match resp {
                Response::Status(st) => match *st {
                    Ok(s) => {
                        self.message = match (&s.current_input, &s.current_input_name) {
                            (Some(c), Some(n)) => format!("current input {} ({})", c, n).into(),
                            _ => s
                                .read_error
                                .clone()
                                .map(|e| format!("monitor not answering: {}", e).into())
                                .unwrap_or_else(|| "monitor not answering".into()),
                        };
                        if !s.inputs.is_empty() {
                            self.index = s.display_index;
                        }
                        self.status = Some(s);
                    }
                    Err(e) => self.message = format!("read failed: {}", e).into(),
                },
                Response::Switched(r) => {
                    self.message = match *r {
                        Ok(v) => v.into(),
                        Err(e) => format!("switch failed: {}", e).into(),
                    }
                }
                Response::Reclaim(r) => {
                    self.message = match *r {
                        Ok(v) => v.into(),
                        Err(e) => format!("reclaim failed: {}", e).into(),
                    }
                }
            }
            self.sync_tray();
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
        let row_bg = if current { rgb(0x2d4f7c) } else { rgb(0x26262e) };
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
                div().text_color(rgb(0x8fdf8f)).child("current").into_any_element()
            } else {
                div().text_color(rgb(0x6a6a72)).child("switch").into_any_element()
            })
            .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                let code = code_for_click.clone();
                let _ = app.update(cx, |app: &mut SwitchApp, cx| {
                    app.send(
                        Request::Switch { index: app.index, code: code.clone() },
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
            let weak_reclaim = weak.clone();

            let rows: Vec<gpui::AnyElement> = self
                .status
                .as_ref()
                .map(|s| {
                    s.inputs
                        .iter()
                        .map(|i| {
                            input_row(
                                i.code.clone(),
                                i.name.clone(),
                                i.current,
                                i.advertised,
                                weak.clone(),
                            )
                            .into_any_element()
                        })
                        .collect()
                })
                .unwrap_or_default();

            let current_code = self
                .status
                .as_ref()
                .and_then(|s| s.current_input.clone())
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
                .child(div().flex().flex_col().gap_1().children(rows))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            div()
                                .id("reclaim")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(rgb(0x2f2f38))
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(0x3d3d48)))
                                .child("reclaim this input")
                                .on_click(move |_e: &ClickEvent, _w: &mut Window, cx: &mut App| {
                                    let code = current_code.clone();
                                    let _ = weak_reclaim.update(cx, |app: &mut SwitchApp, cx| {
                                        if code.is_empty() {
                                            app.message = "no current input known yet".into();
                                            cx.notify();
                                            return;
                                        }
                                        app.send(
                                            Request::Reclaim {
                                                index: app.index,
                                                code: code.clone(),
                                                every: Duration::from_secs(2),
                                                up_to: Duration::from_secs(20),
                                            },
                                            "reclaiming...",
                                            cx,
                                        );
                                    });
                                }),
                        ),
                )
                .child(
                    div()
                        .mt_auto()
                        .text_sm()
                        .text_color(rgb(0x9a9aa2))
                        .child(message),
                )
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

            let bounds = Bounds::centered(None, size(px(420.0), px(420.0)), cx);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        ..Default::default()
                    },
                    |_window, cx| cx.new(SwitchApp::new),
                )
                .expect("open window");

            // The status item has to be created here: on macOS it needs the main
            // thread with an event loop already running, which is exactly what
            // the `run` closure provides.
            match Tray::install() {
                Some(tray) => {
                    let _ = window.update(cx, |app, _window, cx| app.attach_tray(tray, cx));
                }
                None => eprintln!("dmux: running without a menu bar item"),
            }

            cx.activate(true);
        });
    }
}
