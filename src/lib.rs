//! displaymux - switch a monitor's input source over DDC/CI.
//!
//! The crate is split so the switching logic is testable and reusable without
//! any UI:
//!
//! * [`vcp`] - MCCS/VCP value handling: input names, capability parsing, and
//!   turning `"hdmi1"` into `0x11`.
//! * [`ddc`] - the supervisor: spawns the DDC helper, enforces timeouts, and
//!   decides whether a switch actually happened.
//!
//! Everything the supervisor knows about the *hardware* (why a subprocess per
//! write, why a persistent process for reads, why a switch cannot be verified on
//! this host) is documented in `docs/hardware-notes.md`.

pub mod ddc;
pub mod tray;
pub mod vcp;
