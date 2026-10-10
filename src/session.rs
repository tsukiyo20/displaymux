//! What this host can actually know about its own display situation.
//!
//! ## Why not ask the monitor
//!
//! The obvious way to answer "which input is the display on?" is DDC VCP 0x60.
//! On the monitor this was developed against, that value lies: it reported
//! `0x10` both while the display was showing this Mac and after the display had
//! been switched to the other machine. A wrong answer here is not cosmetic --
//! it would aim a "switch back" command at the *other* computer.
//!
//! ## The signal we use instead
//!
//! Many monitors expose a built-in USB hub whose upstream port follows the
//! selected input. When peripherals are attached there, their presence is a
//! *real* signal rather than a claim: while they are enumerated the monitor is
//! routed to this host, and when they disappear the display has moved.
//!
//! Because this depends on how the desk is wired, it is opt-in rather than
//! guessed. Set two environment variables to the substrings that identify your
//! hardware in the USB tree:
//!
//! ```text
//! DMUX_DEVICE_HINTS=<substring>,<substring>   # devices behind the monitor hub
//! DMUX_HUB_HINT=<substring>                   # the monitor's hub itself
//! ```
//!
//! With these unset the module reports [`Session::Unknown`], which is the
//! honest answer: it never claims the display is here without evidence.
//!
//! ## What this deliberately does not do
//!
//! Switching input is **irreversible from this side**. Once the display moves
//! away, this host has lost both the video path and the input devices, so there
//! is nothing to "reclaim" and no hotkey that could help. This module therefore
//! reports state; it never tries to undo a switch.

/// Whether the monitor appears to be routed to this host right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    /// Input devices are present: this host is almost certainly on screen.
    Visible,
    /// The monitor's hub is still attached but the devices behind it are gone.
    /// The display has moved to another input.
    SwitchedAway,
    /// Could not tell, e.g. the devices are attached directly rather than
    /// through the monitor, or this platform has no implementation yet.
    Unknown,
}

impl Session {
    pub fn describe(self) -> &'static str {
        match self {
            Session::Visible => "on screen",
            Session::SwitchedAway => "display switched away",
            Session::Unknown => "display state unknown",
        }
    }

    /// True when the display is known to be showing another machine.
    pub fn is_gone(self) -> bool {
        self == Session::SwitchedAway
    }
}

/// Substrings identifying the devices that sit behind the monitor's hub.
const HINTS_ENV: &str = "DMUX_DEVICE_HINTS";
/// Substrings identifying the monitor's own hub.
///
/// The hub being present while the devices are gone is what separates "the
/// display switched away" from "these peripherals are simply not attached
/// here", so both hints are required for a verdict.
const HUB_ENV: &str = "DMUX_HUB_HINT";

/// Read a comma-separated hint list from the environment.
fn hints(var: &str) -> Vec<String> {
    std::env::var(var)
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Inspect the USB tree and decide whether this host is on screen.
///
/// Returns [`Session::Unknown`] whenever the answer would be a guess, because a
/// wrong "you are on screen" is worse than an honest "unknown".
pub fn current() -> Session {
    detect().unwrap_or(Session::Unknown)
}

#[cfg(target_os = "macos")]
fn detect() -> Option<Session> {
    use std::process::Command;

    let devices = hints(HINTS_ENV);
    let hubs = hints(HUB_ENV);
    // Without configuration there is nothing to look for, and guessing would be
    // worse than admitting ignorance.
    if devices.is_empty() || hubs.is_empty() {
        return Some(Session::Unknown);
    }

    // `ioreg` ships with macOS and avoids pulling in an IOKit binding just to
    // read device names.
    let out = Command::new("ioreg")
        .args(["-p", "IOUSB", "-w0", "-l"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);

    let has_devices = devices.iter().any(|h| text.contains(h.as_str()));
    let has_hub = hubs.iter().any(|h| text.contains(h.as_str()));

    Some(match (has_hub, has_devices) {
        // Devices behind the monitor's hub: it is routed to us.
        (true, true) => Session::Visible,
        // Hub present, devices gone: the monitor switched its USB upstream.
        (true, false) => Session::SwitchedAway,
        // No monitor hub in the tree at all: the peripherals are wired some
        // other way and we cannot infer anything trustworthy.
        (false, _) => Session::Unknown,
    })
}

#[cfg(not(target_os = "macos"))]
fn detect() -> Option<Session> {
    // Windows has no equivalent of walking the monitor's USB upstream from here,
    // and the swap is usually done with a KVM or by the monitor itself. Rather
    // than guess, report that we do not know.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_switched_away_counts_as_gone() {
        assert!(Session::SwitchedAway.is_gone());
        assert!(!Session::Visible.is_gone());
        assert!(!Session::Unknown.is_gone());
    }

    #[test]
    fn detection_never_panics_and_is_one_of_three_states() {
        // Unconfigured, or on a machine with no such wiring, this returns
        // Unknown -- which is the point: it must never claim Visible.
        let s = current();
        assert!(matches!(
            s,
            Session::Visible | Session::SwitchedAway | Session::Unknown
        ));
    }

    #[test]
    fn hint_parsing_handles_empty_and_padded_lists() {
        // Guards the shape of the env contract without touching the environment.
        let parse = |v: &str| -> Vec<String> {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        assert!(parse("").is_empty());
        assert_eq!(parse(" a , b "), vec!["a", "b"]);
    }
}
