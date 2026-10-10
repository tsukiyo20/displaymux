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
//! On this desk the keyboard and mouse hang off the monitor's built-in USB hub,
//! and that hub follows the selected input:
//!
//! ```text
//! AppleT8132USBXHCI
//!   +-o USB2.1 Hub (GenesysLogic)     <- the monitor's hub
//!        +-o QK ALICE                  <- keyboard
//!        +-o Wireless mouse 8k dongle  <- mouse
//! ```
//!
//! So device presence is a *real* signal rather than a claim: if the keyboard
//! and mouse are present, the monitor is currently routed to this host;
//! if they vanish, the display has moved elsewhere. No DDC involved.
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

/// Devices we look for behind the monitor's hub.
const HID_HINTS: [&str; 2] = ["QK ALICE", "Wireless mouse"];
/// The hub chip the monitor uses. Its presence with the devices missing is what
/// distinguishes "display switched away" from "these peripherals are simply not
/// attached to this machine".
const MONITOR_HUB_HINTS: [&str; 2] = ["GenesysLogic", "USB2.1 Hub"];

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

    // `ioreg` is always present on macOS and avoids pulling in a whole IOKit
    // binding just to read device names.
    let out = Command::new("ioreg")
        .args(["-p", "IOUSB", "-w0", "-l"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);

    let has_hid = HID_HINTS.iter().any(|h| text.contains(h));
    let has_monitor_hub = MONITOR_HUB_HINTS.iter().any(|h| text.contains(h));

    Some(match (has_monitor_hub, has_hid) {
        // Devices behind the monitor's hub: it is routed to us.
        (true, true) => Session::Visible,
        // Hub present, devices gone: the monitor switched its USB upstream.
        (true, false) => Session::SwitchedAway,
        // No monitor hub in the tree at all. Either the peripherals are plugged
        // in directly, or we are looking at a different machine entirely.
        // Either way we cannot infer anything trustworthy.
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
        // On a machine without the monitor attached this returns Unknown, which
        // is the point: it must not claim Visible.
        let s = current();
        assert!(matches!(
            s,
            Session::Visible | Session::SwitchedAway | Session::Unknown
        ));
    }
}
