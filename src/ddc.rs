//! Supervisor layer: runs the one-shot DDC helper and interprets its results.
//!
//! The UI never touches DDC directly. It asks this module, which owns process
//! spawning, timeouts, and the "did the switch actually happen?" verdict.

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

pub const HELPER_ENV: &str = "DMUX_DDC_HELPER";

#[derive(Debug)]
pub enum DdcError {
    HelperNotFound,
    Timeout { secs: u64 },
    Spawn(String),
    Protocol(String),
}

impl std::fmt::Display for DdcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DdcError::HelperNotFound => write!(
                f,
                "DDC helper binary not found (looked for dmux-ddc-helper next to the app and on PATH; \
                 override with {})",
                HELPER_ENV
            ),
            DdcError::Timeout { secs } => {
                write!(f, "DDC operation timed out after {}s (the monitor is not answering)", secs)
            }
            DdcError::Spawn(e) => write!(f, "could not spawn DDC helper: {}", e),
            DdcError::Protocol(e) => write!(f, "unexpected helper output: {}", e),
        }
    }
}

impl std::error::Error for DdcError {}

/// Locate `dmux-ddc-helper`: explicit env override, then alongside the current
/// executable (works for `cargo run` and for a bundled .app), then PATH.
pub fn helper_path() -> Result<PathBuf, DdcError> {
    if let Ok(p) = std::env::var(HELPER_ENV) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(DdcError::HelperNotFound);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for cand in [dir.join("dmux-ddc-helper"), dir.join("dmux-ddc-helper.exe")] {
                if cand.is_file() {
                    return Ok(cand);
                }
            }
            // target/debug/dmux -> target/debug/dmux-ddc-helper is the same dir,
            // but when installed as Foo.app/Contents/MacOS/dmux the helper sits
            // beside it too, so the same lookup covers both.
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in ["dmux-ddc-helper", "dmux-ddc-helper.exe"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Ok(cand);
                }
            }
        }
    }
    Err(DdcError::HelperNotFound)
}

/// Run one DDC operation in a fresh process and parse its JSON result.
pub fn run(op: &str, args: &[String], timeout: Duration) -> Result<Value, DdcError> {
    let helper = helper_path()?;
    let mut cmd = Command::new(&helper);
    cmd.arg(op)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd.spawn().map_err(|e| DdcError::Spawn(e.to_string()))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut f = stdout;
        let _ = f.read_to_end(&mut buf);
        buf
    });

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(DdcError::Timeout { secs: timeout.as_secs() });
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(DdcError::Spawn(e.to_string())),
        }
    }

    let buf = reader.join().unwrap_or_default();
    let text = String::from_utf8_lossy(&buf);
    let line = text
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .ok_or_else(|| DdcError::Protocol(format!("no JSON in helper output: {:?}", text.trim())))?;
    serde_json::from_str(line)
        .map_err(|e| DdcError::Protocol(format!("{}: {}", e, line.trim())))
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|x| x.to_string())
}

#[derive(Debug, Clone)]
pub struct InputOption {
    pub code: String,
    pub name: String,
    pub current: bool,
    pub advertised: bool,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub display_index: usize,
    pub display_id: String,
    pub current_input: Option<String>,
    pub current_input_name: Option<String>,
    pub inputs: Vec<InputOption>,
    pub caps_raw: Option<String>,
    pub read_error: Option<String>,
    pub caps_error: Option<String>,
    pub elapsed_ms: u64,
}

impl Status {
    fn from_json(v: &Value) -> Status {
        let idx = v
            .get("display")
            .and_then(|d| d.get("index"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as usize;
        let id = v
            .get("display")
            .and_then(|d| d.get("id"))
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string();
        let inputs = v
            .get("inputs")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|i| {
                        Some(InputOption {
                            code: s(i, "code")?,
                            name: s(i, "name").unwrap_or_default(),
                            current: i.get("current").and_then(|c| c.as_bool()).unwrap_or(false),
                            advertised: i
                                .get("advertised")
                                .and_then(|c| c.as_bool())
                                .unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Status {
            display_index: idx,
            display_id: id,
            current_input: s(v, "current_input"),
            current_input_name: s(v, "current_input_name"),
            inputs,
            caps_raw: s(v, "caps_raw"),
            read_error: s(v, "read_error"),
            caps_error: s(v, "caps_error"),
            elapsed_ms: v.get("elapsed_ms").and_then(|x| x.as_u64()).unwrap_or(0),
        }
    }
}

/// Query one display's status. `caps` also fetches the capabilities string.
pub fn status(index: usize, caps: bool, timeout: Duration) -> Result<Status, DdcError> {
    let mut args = vec![format!("--index={}", index)];
    if caps {
        args.push("--caps".into());
    }
    let v = run("status", &args, timeout)?;
    if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
        return Err(DdcError::Protocol(
            s(&v, "error").unwrap_or_else(|| "helper reported failure".into()),
        ));
    }
    Ok(Status::from_json(&v))
}

/// Outcome of asking a monitor to change input.
#[derive(Debug, Clone)]
pub struct SwitchOutcome {
    /// The monitor acknowledged the DDC write. This is what actually matters.
    pub acknowledged: bool,
    /// Some monitors keep answering after the write, letting us confirm. When
    /// the monitor switched away it cannot answer, so this stays `None` and the
    /// switch is still considered a success.
    pub confirmed_current: Option<String>,
    pub wrote: String,
    pub wrote_name: String,
    pub elapsed_ms: u64,
}

impl SwitchOutcome {
    /// Human-facing verdict, aware that "no readback" is the *expected* result of
    /// a successful switch rather than a failure.
    pub fn verdict(&self) -> &'static str {
        match (&self.confirmed_current, self.acknowledged) {
            (Some(cur), _) if *cur == self.wrote => "switched and confirmed",
            (Some(_), true) => "monitor acknowledged the write but reported a different input \
                                (likely auto-reverted because that input has no signal)",
            (None, true) => "switch sent (monitor moved off this host, so no readback expected)",
            (_, false) => "failed",
        }
    }
}

/// Ask the monitor to switch to `value` (hex string such as "0x11").
pub fn set_input(index: usize, value: &str, timeout: Duration) -> Result<SwitchOutcome, DdcError> {
    let v = run(
        "set-input",
        &[format!("--index={}", index), format!("--value={}", value)],
        timeout,
    )?;
    let ok = v.get("ok").and_then(|o| o.as_bool()) == Some(true);
    if !ok {
        return Err(DdcError::Protocol(
            s(&v, "error").unwrap_or_else(|| "helper reported failure".into()),
        ));
    }
    Ok(SwitchOutcome {
        acknowledged: true,
        confirmed_current: None,
        wrote: s(&v, "wrote").unwrap_or_else(|| value.to_string()),
        wrote_name: s(&v, "wrote_name").unwrap_or_default(),
        elapsed_ms: v.get("elapsed_ms").and_then(|x| x.as_u64()).unwrap_or(0),
    })
}

/// Ask the monitor to switch, then try once to confirm. Used by the CLI where a
/// short extra wait is acceptable; the UI uses the non-blocking path.
pub fn set_input_and_confirm(
    index: usize,
    value: &str,
    timeout: Duration,
    confirm_after: Duration,
) -> Result<SwitchOutcome, DdcError> {
    let mut outcome = set_input(index, value, timeout)?;
    std::thread::sleep(confirm_after);
    if let Ok(st) = status(index, false, timeout) {
        outcome.confirmed_current = st.current_input;
    }
    Ok(outcome)
}

pub fn set_brightness(index: usize, value: u16, timeout: Duration) -> Result<Value, DdcError> {
    let v = run(
        "set-brightness",
        &[
            format!("--index={}", index),
            format!("--value={:x}", value),
        ],
        timeout,
    )?;
    if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
        return Err(DdcError::Protocol(
            s(&v, "error").unwrap_or_else(|| "helper reported failure".into()),
        ));
    }
    Ok(v)
}

#[derive(Debug, Clone)]
pub struct DisplayRef {
    pub index: usize,
    pub id: String,
}

pub fn list_displays(timeout: Duration) -> Result<Vec<DisplayRef>, DdcError> {
    let v = run("list", &[], timeout)?;
    let arr = v
        .get("displays")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|d| {
            Some(DisplayRef {
                index: d.get("index")?.as_u64()? as usize,
                id: d.get("id").and_then(|x| x.as_str())?.to_string(),
            })
        })
        .collect())
}

/// Read status, retrying because an Apple Silicon DCP DDC channel that has just
/// carried a write (or that was left mid-switch by the other machine) needs a
/// moment -- and because each retry is a brand-new process, hence a brand-new
/// IOAVService, which is exactly what recovers a wedged channel.
pub fn status_retrying(
    index: usize,
    caps: bool,
    attempts: usize,
    timeout: Duration,
    backoff: Duration,
) -> Result<Status, DdcError> {
    status_retrying_with(index, caps, attempts, timeout, backoff, &|_, _| {})
}

/// Same as [`status_retrying`] but reports each failed attempt, so a UI can show
/// "retrying (2/3)" instead of appearing frozen.
pub fn status_retrying_with(
    index: usize,
    caps: bool,
    attempts: usize,
    timeout: Duration,
    backoff: Duration,
    on_retry: &dyn Fn(usize, &str),
) -> Result<Status, DdcError> {
    let attempts = attempts.max(1);
    let mut last: Option<DdcError> = None;
    for attempt in 1..=attempts {
        match status(index, caps, timeout) {
            // A status reply whose input read failed is still useful (we learn the
            // input list), but if we can retry, prefer a complete answer.
            Ok(st) if st.current_input.is_some() || attempt == attempts => return Ok(st),
            Ok(st) => {
                on_retry(attempt, st.read_error.as_deref().unwrap_or("input unreadable"));
                last = None;
            }
            Err(e) => {
                on_retry(attempt, &e.to_string());
                last = Some(e);
            }
        }
        if attempt < attempts {
            std::thread::sleep(backoff);
        }
    }
    Err(last.unwrap_or(DdcError::Protocol("status failed".into())))
}

// ---------------------------------------------------------------------------
// Persistent read-only session
// ---------------------------------------------------------------------------

use std::io::{BufRead, BufReader, Write};
use std::process::Child;
use std::sync::mpsc;

/// A long-lived `dmux-ddc-helper serve` process.
///
/// Reads never wedge the DCP channel (measured: 8/8 fresh-process and 8/8
/// same-process reads succeeded), so unlike writes they can share one process.
/// This removes process-spawn and I/O Kit enumeration cost from the UI refresh
/// path: ~200 ms per full status instead of a fresh process each time.
///
/// A wedged or dead reader is transparently replaced on the next call, so a
/// monitor input switch (which can invalidate the IOAVService) self-heals.
pub struct Reader {
    child: Child,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<String>,
    /// Cached so we only pay for a capabilities fetch once per session.
    advertised: Option<Vec<u16>>,
}

impl Reader {
    pub fn spawn() -> Result<Reader, DdcError> {
        let helper = helper_path()?;
        let mut child = Command::new(&helper)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| DdcError::Spawn(e.to_string()))?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = child.stdin.take().expect("stdin piped");

        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut reader = Reader {
            child: child,
            stdin: stdin,
            rx: rx,
            advertised: None,
        };

        // Consume the `serve-ready` handshake. It doubles as proof that the
        // helper finished its DDC warm-up, so the very first real request cannot
        // hit the "fresh IOAVService" garbage transaction.
        match reader.rx.recv_timeout(Duration::from_secs(10)) {
            Ok(line) => {
                let v: Value = serde_json::from_str(&line)
                    .map_err(|e| DdcError::Protocol(format!("bad handshake: {}: {}", e, line)))?;
                if v.get("displays").and_then(|d| d.as_array()).map(|a| a.is_empty()) == Some(true)
                {
                    return Err(DdcError::Protocol("no DDC-capable displays found".into()));
                }
                reader
                    .advertised
                    .is_none()
                    .then_some(())
                    .ok_or_else(|| DdcError::Protocol("handshake failed".into()))?;
            }
            Err(_) => return Err(DdcError::Timeout { secs: 10 }),
        }
        Ok(reader)
    }

    /// One request/response round trip. Returns JSON sentinel errors rather than
    /// failing, so callers can decide how loudly to complain.
    fn request(&mut self, index: usize, caps: bool, timeout: Duration) -> Result<Value, DdcError> {
        let msg = serde_json::json!({ "index": index, "caps": caps });
        writeln!(self.stdin, "{}", msg).map_err(|e| DdcError::Spawn(e.to_string()))?;
        self.stdin.flush().map_err(|e| DdcError::Spawn(e.to_string()))?;
        match self.rx.recv_timeout(timeout) {
            Ok(line) => serde_json::from_str(&line)
                .map_err(|e| DdcError::Protocol(format!("{}: {}", e, line))),
            Err(_) => Err(DdcError::Timeout { secs: timeout.as_secs() }),
        }
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Read status, replacing a dead reader. Also fetches + caches the
    /// capabilities list on first use.
    pub fn status(&mut self, index: usize, timeout: Duration) -> Result<Status, DdcError> {
        let need_caps = self.advertised.is_none();
        let mut v = self.request(index, need_caps, timeout)?;
        if v.get("ok").and_then(|o| o.as_bool()) != Some(true) && !self.alive() {
            // reader died (e.g. the display handle was invalidated by a switch)
            *self = Reader::spawn()?;
            v = self.request(index, need_caps, timeout)?;
        }
        if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
            return Err(DdcError::Protocol(
                s(&v, "error").unwrap_or_else(|| "reader reported failure".into()),
            ));
        }
        if let Some(arr) = v.get("advertised_inputs").and_then(|x| x.as_array()) {
            if !arr.is_empty() && self.advertised.is_none() {
                self.advertised = Some(
                    arr.iter()
                        .filter_map(|c| c.as_str())
                        .filter_map(|c| u16::from_str_radix(c.trim_start_matches("0x"), 16).ok())
                        .collect(),
                );
            }
        }
        let mut st = Status::from_json(&v);
        // The reader reports an empty advertised set when it skipped caps; fill
        // from cache so the UI always has the real port list.
        if st.inputs.iter().all(|i| !i.advertised) {
            if let Some(adv) = self.advertised.as_ref() {
                let current = st
                    .current_input
                    .as_ref()
                    .and_then(|c| u16::from_str_radix(c.trim_start_matches("0x"), 16).ok());
                let codes = crate::vcp::candidate_inputs(adv, current);
                st.inputs = codes
                    .iter()
                    .map(|c| InputOption {
                        code: format!("{:#04x}", c),
                        name: crate::vcp::input_name(*c),
                        current: Some(*c) == current,
                        advertised: adv.contains(c),
                    })
                    .collect();
            }
        }
        Ok(st)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
