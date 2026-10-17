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
//! Two properties of real DDC/CI hardware shape this design, and both are
//! documented where they are implemented rather than assumed:
//!
//! * a write can leave the DDC channel unusable for the rest of that process,
//!   which is why every write gets a throwaway process while reads share one
//!   long-lived process ([`ddc`]);
//! * a switch cannot be verified from the host that issued it, and some
//!   monitors report a constant "current input" regardless of the truth, so the
//!   UI reports intent rather than state ([`session`], [`state`]).

pub mod ddc;
pub mod labels;
pub mod tray;
pub mod vcp;
