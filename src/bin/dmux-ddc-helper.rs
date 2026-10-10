//! One-shot DDC/CI helper.
//!
//! Every invocation performs exactly ONE DDC operation and then exits.
//!
//! Why a subprocess per operation: on Apple Silicon, `IOAVServiceWriteI2C`
//! leaves the DCP I2C channel unusable for further transactions from the same
//! process (`MacOS kernel I/O error 0x10000003`) after a monitor input switch.
//! A fresh process gets a fresh IOAVService and always talks to a clean channel.
//!
//! Emits a single JSON object on stdout. Exit code 0 = operation completed as
//! requested, 1 = DDC failure (details still in the JSON).
//!
//! HARD RULE derived from measurement on Apple Silicon (M4 + macOS 27):
//!   * reading VCPs never breaks the DCP I2C channel -- 8/8 fresh-process reads
//!     and 8/8 same-process reads succeeded, 45-96 ms each;
//!   * a *write* followed by any further DDC transaction in the SAME process
//!     wedges the channel (`MacOS kernel I/O error 0x10000003`), and the only
//!     recovery is a fresh process / fresh IOAVService.
//! Therefore: one operation per process (`dmux-ddc-helper <op>`), or, for
//! read-only polling, `dmux-ddc-helper serve`, which never writes.

use ddc_hi::{Ddc, Display};
use displaymux::vcp;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const VCP_INPUT: u8 = 0x60;
const VCP_BRIGHTNESS: u8 = 0x10;

struct Args {
    op: String,
    index: usize,
    value: Option<u16>,
    caps: bool,
}

fn parse_args() -> Args {
    let mut a = Args { op: "list".into(), index: 0, value: None, caps: false };
    for arg in std::env::args().skip(1) {
        if let Some(rest) = arg.strip_prefix("--index=") {
            a.index = rest.parse().unwrap_or(0);
        } else if let Some(rest) = arg.strip_prefix("--value=") {
            a.value = u16::from_str_radix(rest.trim_start_matches("0x"), 16).ok();
        } else if arg == "--caps" {
            a.caps = true;
        } else {
            a.op = arg;
        }
    }
    a
}

fn hexstr(v: u16) -> String {
    format!("{:#04x}", v)
}

fn input_list(advertised: &[u16], current: Option<u16>) -> Vec<Value> {
    vcp::candidate_inputs(advertised, current)
        .into_iter()
        .map(|code| {
            json!({
                "code": hexstr(code),
                "name": vcp::input_name(code),
                "current": Some(code) == current,
                "advertised": advertised.contains(&code),
            })
        })
        .collect()
}

/// Long-lived read-only mode. Safe because reads never wedge the channel, and
/// it avoids paying process spawn + I/O Kit enumeration on every UI refresh.
fn serve() {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // Keep the enumerated handles alive for the life of the process; only reads
    // are ever issued against them.
    let mut displays = Display::enumerate();

    // Warm-up: measured on Apple Silicon, the FIRST DDC transaction issued
    // through a freshly created IOAVService fails with "invalid DDC/CI length"
    // (the channel answers with garbage), while every later transaction on the
    // same handle succeeds. Burn that first transaction here so callers never
    // see it. Retry because a handle can also be created mid-switch.
    let mut warm = String::from("no display");
    for attempt in 1..=4 {
        match displays.first_mut() {
            None => {
                warm = "no DDC-capable display".into();
                break;
            }
            Some(d) => match d.handle.get_vcp_feature(VCP_INPUT) {
                Ok(v) => {
                    warm = format!("ready after {} warm-up transaction(s) ({:#06x})", attempt, v.value());
                    break;
                }
                Err(e) => {
                    warm = format!("warm-up attempt {}: {}", attempt, e);
                    std::thread::sleep(Duration::from_millis(250));
                }
            },
        }
    }

    let _ = writeln!(
        out,
        "{}",
        json!({
            "ok": true, "op": "serve-ready",
            "warmup": warm,
            "displays": displays.iter().enumerate()
                .map(|(i, d)| json!({"index": i, "id": d.info.id}))
                .collect::<Vec<Value>>(),
        })
    );
    let _ = out.flush();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(out, "{}", json!({"ok": false, "error": format!("bad request: {}", e)}));
                let _ = out.flush();
                continue;
            }
        };
        let index = req.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let want_caps = req.get("caps").and_then(|v| v.as_bool()).unwrap_or(false);
        let resp = match displays.get_mut(index) {
            None => json!({"ok": false, "error": format!("display index {} out of range", index)}),
            Some(d) => {
                let (current, read_err) = match d.handle.get_vcp_feature(VCP_INPUT) {
                    Ok(v) => (Some(v.value()), None),
                    Err(e) => (None, Some(e.to_string())),
                };
                let (caps_text, caps_err) = if want_caps {
                    match d.handle.capabilities_string() {
                        Ok(raw) => (Some(String::from_utf8_lossy(&raw).into_owned()), None),
                        Err(e) => (None, Some(e.to_string())),
                    }
                } else {
                    (None, None)
                };
                let advertised = caps_text
                    .as_deref()
                    .map(|c| vcp::parse_vcp_values(c, VCP_INPUT))
                    .unwrap_or_default();
                json!({
                    "ok": true, "op": "status",
                    "display": { "index": index, "id": d.info.id },
                    "current_input": current.map(hexstr),
                    "current_input_name": current.map(vcp::input_name),
                    "brightness": d.handle.get_vcp_feature(VCP_BRIGHTNESS).ok().map(|v| v.value()),
                    "advertised_inputs": advertised.iter().map(|c| hexstr(*c)).collect::<Vec<_>>(),
                    "inputs": input_list(&advertised, current),
                    "caps_raw": caps_text,
                    "read_error": read_err,
                    "caps_error": caps_err,
                    "elapsed_ms": 0,
                })
            }
        };
        let _ = writeln!(out, "{}", resp);
        let _ = out.flush();
    }
}

fn main() {
    let args = parse_args();
    if args.op == "serve" {
        serve();
        return;
    }
    let started = Instant::now();
    let mut displays = Display::enumerate();

    if args.op == "list" {
        let list: Vec<Value> = displays
            .iter()
            .enumerate()
            .map(|(i, d)| {
                json!({
                    "index": i,
                    "id": d.info.id,
                    "backend": format!("{:?}", d.info.backend),
                })
            })
            .collect();
        let out = json!({ "ok": true, "op": "list", "displays": list });
        println!("{}", out);
        return;
    }

    if displays.is_empty() {
        println!(
            "{}",
            json!({ "ok": false, "op": args.op, "error": "no DDC-capable displays found" })
        );
        std::process::exit(1);
    }
    if args.index >= displays.len() {
        println!(
            "{}",
            json!({
                "ok": false, "op": args.op,
                "error": format!("display index {} out of range ({} found)", args.index, displays.len())
            })
        );
        std::process::exit(1);
    }

    let display = &mut displays[args.index];
    let id = display.info.id.clone();

    // Read current input. A monitor that has switched away from this host, or a
    // DCP channel still settling, will fail here; that is reported as `null`
    // rather than treated as a fatal error.
    let read = |d: &mut Display| -> (Option<u16>, Option<String>) {
        match d.handle.get_vcp_feature(VCP_INPUT) {
            Ok(v) => (Some(v.value()), None),
            Err(e) => (None, Some(e.to_string())),
        }
    };

    match args.op.as_str() {
        "status" => {
            let (current, read_err) = read(display);
            let (caps_text, caps_err) = if args.caps {
                match display.handle.capabilities_string() {
                    Ok(raw) => (Some(String::from_utf8_lossy(&raw).into_owned()), None),
                    Err(e) => (None, Some(e.to_string())),
                }
            } else {
                (None, None)
            };
            let advertised = caps_text
                .as_deref()
                .map(|c| vcp::parse_vcp_values(c, VCP_INPUT))
                .unwrap_or_default();
            let brightness = display.handle.get_vcp_feature(VCP_BRIGHTNESS).ok().map(|v| v.value());
            let out = json!({
                "ok": true,
                "op": "status",
                "display": { "index": args.index, "id": id },
                "current_input": current.map(hexstr),
                "current_input_name": current.map(vcp::input_name),
                "brightness": brightness,
                "advertised_inputs": advertised.iter().map(|c| hexstr(*c)).collect::<Vec<_>>(),
                "inputs": input_list(&advertised, current),
                "caps_raw": caps_text,
                "read_error": read_err,
                "caps_error": caps_err,
                "elapsed_ms": started.elapsed().as_millis() as u64,
            });
            println!("{}", out);
        }
        "set-input" => {
            let Some(value) = args.value else {
                println!("{}", json!({ "ok": false, "op": "set-input", "error": "--value= required" }));
                std::process::exit(1);
            };
            match display.handle.set_vcp_feature(VCP_INPUT, value) {
                // The DDC write itself succeeded. The monitor now switches away,
                // which breaks the channel for this process -- deliberately not
                // read back here, so this is reported as an unverified success.
                Ok(()) => {
                    let out = json!({
                        "ok": true,
                        "op": "set-input",
                        "display": { "index": args.index, "id": id },
                        "wrote": hexstr(value),
                        "wrote_name": vcp::input_name(value),
                        "verified": false,
                        "note": "DDC write acknowledged by the monitor; input readback intentionally skipped because the monitor may have switched away",
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                    });
                    println!("{}", out);
                }
                Err(e) => {
                    let out = json!({
                        "ok": false,
                        "op": "set-input",
                        "display": { "index": args.index, "id": id },
                        "wrote": hexstr(value),
                        "error": e.to_string(),
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                    });
                    println!("{}", out);
                    std::process::exit(1);
                }
            }
        }
        "set-brightness" => {
            let Some(value) = args.value else {
                println!("{}", json!({ "ok": false, "op": "set-brightness", "error": "--value= required" }));
                std::process::exit(1);
            };
            match display.handle.set_vcp_feature(VCP_BRIGHTNESS, value) {
                Ok(()) => {
                    std::thread::sleep(Duration::from_millis(120));
                    let now = display.handle.get_vcp_feature(VCP_BRIGHTNESS).ok().map(|v| v.value());
                    println!(
                        "{}",
                        json!({
                            "ok": true, "op": "set-brightness",
                            "display": { "index": args.index, "id": id },
                            "wrote": hexstr(value), "readback": now,
                            "elapsed_ms": started.elapsed().as_millis() as u64,
                        })
                    );
                }
                Err(e) => {
                    println!(
                        "{}",
                        json!({ "ok": false, "op": "set-brightness", "error": e.to_string() })
                    );
                    std::process::exit(1);
                }
            }
        }
        other => {
            println!("{}", json!({ "ok": false, "error": format!("unknown op: {}", other) }));
            std::process::exit(2);
        }
    }
}
