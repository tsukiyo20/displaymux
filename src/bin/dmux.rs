//! dmux - command line front end for the monitor input switcher.
//!
//! This exercises exactly the same core the GPUI app will use (`ddc` module),
//! so the switching behaviour is validated before any UI exists.

use displaymux::ddc::{self, DdcError};
use displaymux::vcp;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(12);

fn usage() {
    println!(
        r#"dmux - control a monitor's input source over DDC/CI

USAGE:
    dmux list                              list DDC-capable displays
    dmux status [--index=N] [--caps]       show current input, brightness, available inputs
    dmux inputs [--index=N]                list switchable inputs
    dmux set <input> [--index=N]           switch input (hdmi1, dp2, 0x11, 17 ...)
    dmux toggle [--index=N] [--to=<input>] switch, or switch back to <input> if already there
    dmux hold <input> [--index=N] [--every=SECS] [--for=SECS]
                                            keep re-asserting <input> until this host is
                                            selected -- the tug-of-war that lets a Mac
                                            reclaim the monitor from the other machine
    dmux bright <0-100> [--index=N]        set brightness
    dmux vcp <code> [<code>...] [--index=N]
                                            read arbitrary VCP codes, e.g.
                                            `dmux vcp 0x60 0xae`. Useful for
                                            checking what a monitor actually
                                            answers: 0xae is the vertical
                                            frequency of the signal it is
                                            displaying right now, which tells
                                            you which input it is showing when
                                            the two machines differ in refresh
                                            rate.

ENVIRONMENT:
    DMUX_DDC_HELPER   path to dmux-ddc-helper
"#
    );
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let p = format!("--{}=", name);
    args.iter()
        .find_map(|a| a.strip_prefix(&p).map(|s| s.to_string()))
}

fn positional(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect()
}

fn index_of(args: &[String]) -> usize {
    flag(args, "index")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn print_inputs(st: &ddc::Status) {
    for i in &st.inputs {
        let marker = if i.current { "*" } else { " " };
        let adv = if i.advertised {
            ""
        } else {
            "  (not advertised)"
        };
        println!("  {} {:<8} {:<16}{}", marker, i.code, i.name, adv);
    }
}

fn cmd_status(args: &[String]) -> Result<(), DdcError> {
    let idx = index_of(args);
    let persistent = args.iter().any(|a| a == "--persistent");
    // Always read capabilities: without them the input list would be the MCCS
    // fallback, and every port would be mislabelled as "(not advertised)".
    let want_caps = true;
    let st = if persistent {
        let mut r = ddc::Reader::spawn()?;
        r.status(idx, DEFAULT_TIMEOUT)?
    } else {
        ddc::status_retrying_with(
            idx,
            want_caps,
            3,
            DEFAULT_TIMEOUT,
            Duration::from_millis(1200),
            &|attempt, why| eprintln!("  (retry {}/3: {})", attempt, why),
        )?
    };
    println!("display   : [{}] {}", st.display_index, st.display_id);
    // What the monitor answers, not what it is doing: VCP 0x60 is the code that
    // should report the selected input, and its reliability varies by monitor.
    // Naming the source of the value keeps `status` a diagnostic.
    match (&st.current_input, &st.current_input_name) {
        (Some(c), Some(n)) => println!("reported  : {} ({})  [VCP 0x60]", c, n),
        _ => println!(
            "reported  : unknown  [VCP 0x60]{}",
            st.read_error
                .as_ref()
                .map(|e| format!("  [{}]", e))
                .unwrap_or_default()
        ),
    }
    if let Some(b) = st.caps_error.as_ref() {
        println!("caps      : unreadable [{}]", b);
    }
    println!("switchable:");
    print_inputs(&st);
    if let Some(c) = st.caps_raw.as_ref() {
        println!("caps raw  : {}", c.trim());
    }
    println!("latency   : {} ms", st.elapsed_ms);
    Ok(())
}

fn cmd_vcp(args: &[String]) -> Result<(), DdcError> {
    let index = index_of(args);
    let codes: Vec<u8> = args
        .iter()
        .filter(|a| !a.starts_with("--") && !a.starts_with('-'))
        .filter_map(|a| u8::from_str_radix(a.trim().trim_start_matches("0x"), 16).ok())
        .collect();
    if codes.is_empty() {
        eprintln!("usage: dmux vcp <code> [<code>...] [--index=N]   (codes in hex)");
        std::process::exit(2);
    }
    let reads = ddc::read_vcps(index, &codes, DEFAULT_TIMEOUT)?;
    for r in &reads {
        match r.value {
            Some(v) => {
                // Only 0x60's values are input identifiers; naming the values
                // of any other code off that table would be a fabrication.
                let note = match r.code {
                    // The raw reply is shown as read. When a monitor duplicates
                    // the byte, that is worth seeing rather than silently
                    // correcting, because it explains a wrong-looking value.
                    0x60 if vcp::normalize_input_value(v) != v => format!(
                        "  ({}; the byte is duplicated, as some monitors do)",
                        vcp::input_name(vcp::normalize_input_value(v))
                    ),
                    0x60 => format!("  ({})", vcp::input_name(v)),
                    // MCCS gives 0xae vertical frequency in hundredths of a
                    // hertz, which is what makes it usable as "what is on
                    // screen right now".
                    0xae => format!("  ({:.2} Hz)", v as f64 / 100.0),
                    0xac => format!("  ({} Hz)", v),
                    _ => String::new(),
                };
                println!("0x{:02x}  = 0x{:04x}{}", r.code, v, note);
            }
            None => println!(
                "0x{:02x}  = --      [{}]",
                r.code,
                r.error.as_deref().unwrap_or("no answer")
            ),
        }
    }
    Ok(())
}

fn cmd_inputs(args: &[String]) -> Result<(), DdcError> {
    let st = ddc::status_retrying(
        index_of(args),
        false,
        3,
        DEFAULT_TIMEOUT,
        Duration::from_millis(1200),
    )?;
    print_inputs(&st);
    Ok(())
}

fn cmd_set(args: &[String]) -> Result<(), DdcError> {
    let pos = positional(args);
    let Some(spec) = pos.first() else {
        eprintln!("error: which input? e.g. `dmux set hdmi1`");
        std::process::exit(2);
    };
    let Some(code) = vcp::resolve_input(spec) else {
        eprintln!(
            "error: cannot interpret input {:?}. Try hdmi1/dp2/usbc1 or a hex code like 0x11.",
            spec
        );
        std::process::exit(2);
    };
    let idx = index_of(args);
    let hex = format!("{:#04x}", code);
    println!(
        "switching [{}] to {} ({}) ...",
        idx,
        hex,
        vcp::input_name(code)
    );
    let outcome =
        ddc::set_input_and_confirm(idx, &hex, DEFAULT_TIMEOUT, Duration::from_millis(2500))?;
    println!("result    : {}", outcome.verdict());
    if let Some(cur) = &outcome.confirmed_current {
        println!("readback  : {}", cur);
    }
    Ok(())
}

fn cmd_toggle(args: &[String]) -> Result<(), DdcError> {
    let idx = index_of(args);
    let st = ddc::status(idx, false, DEFAULT_TIMEOUT)?;
    let target = if let Some(to) = flag(args, "to") {
        vcp::resolve_input(&to)
    } else {
        // default: the first advertised input that is not the current one
        st.inputs
            .iter()
            .find(|i| !i.current && i.advertised)
            .and_then(|i| u16::from_str_radix(i.code.trim_start_matches("0x"), 16).ok())
    };
    let Some(code) = target else {
        eprintln!("error: nothing to toggle to; pass --to=<input>");
        std::process::exit(2);
    };
    let hex = format!("{:#04x}", code);
    println!(
        "current {} -> toggling to {} ({})",
        st.current_input.clone().unwrap_or_else(|| "?".into()),
        hex,
        vcp::input_name(code)
    );
    let outcome = ddc::set_input(idx, &hex, DEFAULT_TIMEOUT)?;
    println!("result    : {}", outcome.verdict());
    Ok(())
}

/// Keep re-asserting an input every `every` seconds for up to `for` seconds.
///
/// This is the macOS-specific trick that makes a Mac able to *reclaim* a
/// monitor: while the monitor displays the other machine, DDC over the
/// still-connected link keeps working, so a persistent request eventually wins
/// as soon as the other machine stops driving its input (or the user powers it
/// down). It also covers monitors that auto-revert while the target is idle.
fn cmd_hold(args: &[String]) -> Result<(), DdcError> {
    let pos = positional(args);
    let Some(spec) = pos.first() else {
        eprintln!("error: which input should be held? e.g. `dmux hold dp2`");
        std::process::exit(2);
    };
    let Some(code) = vcp::resolve_input(spec) else {
        eprintln!("error: cannot interpret {:?}", spec);
        std::process::exit(2);
    };
    let idx = index_of(args);
    let every = flag(args, "every")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3u64);
    let for_secs = flag(args, "for")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30u64);
    let hex = format!("{:#04x}", code);

    println!(
        "holding {} ({}) on display [{}], re-asserting every {}s for up to {}s",
        hex,
        vcp::input_name(code),
        idx,
        every,
        for_secs
    );

    let start = std::time::Instant::now();
    let mut attempt = 0u32;
    // Reads are safe to share one process, so the reclaim loop polls through a
    // persistent reader while every write still gets its own process.
    let mut reader = ddc::Reader::spawn().ok();
    loop {
        attempt += 1;
        let elapsed = start.elapsed().as_secs();
        // Only a successful *readback* proves this host is selected again.
        let already = reader
            .as_mut()
            .and_then(|r| r.status(idx, DEFAULT_TIMEOUT).ok())
            .and_then(|s| s.current_input)
            .map(|c| c.eq_ignore_ascii_case(&hex))
            .unwrap_or(false);
        if already {
            println!(
                "attempt {:>2}: already on {} -- done after {}s",
                attempt, hex, elapsed
            );
            return Ok(());
        }
        match ddc::set_input(idx, &hex, DEFAULT_TIMEOUT) {
            Ok(o) => println!("attempt {:>2}: sent ({})", attempt, o.verdict()),
            Err(e) => println!("attempt {:>2}: {}", attempt, e),
        }
        if elapsed >= for_secs {
            println!("gave up after {}s / {} attempts", elapsed, attempt);
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(every));
    }
}

fn cmd_bright(args: &[String]) -> Result<(), DdcError> {
    let pos = positional(args);
    let Some(spec) = pos.first() else {
        eprintln!("error: brightness 0-100 required");
        std::process::exit(2);
    };
    let value: u16 = spec.parse().unwrap_or_else(|_| {
        eprintln!("error: brightness must be a number, got {:?}", spec);
        std::process::exit(2);
    });
    let v = ddc::set_brightness(index_of(args), value, DEFAULT_TIMEOUT)?;
    println!(
        "set brightness -> {}",
        v.get("readback")
            .map(|r| r.to_string())
            .unwrap_or("?".into())
    );
    Ok(())
}

fn cmd_list() -> Result<(), DdcError> {
    let ds = ddc::list_displays(DEFAULT_TIMEOUT)?;
    if ds.is_empty() {
        println!("no DDC-capable displays found");
    }
    for d in ds {
        println!("[{}] {}", d.index, d.id);
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(|s| s.as_str()) else {
        usage();
        return;
    };
    let rest = &args[1..];
    let result = match cmd {
        "list" => cmd_list(),
        "status" => cmd_status(rest),
        "inputs" => cmd_inputs(rest),
        "set" => cmd_set(rest),
        "toggle" => cmd_toggle(rest),
        "hold" => cmd_hold(rest),
        "bright" | "brightness" => cmd_bright(rest),
        "vcp" => cmd_vcp(rest),
        "-h" | "--help" | "help" => {
            usage();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {}\n", other);
            usage();
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}
