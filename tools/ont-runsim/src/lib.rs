// SPDX-License-Identifier: AGPL-3.0-or-later
//! ont-runsim: writes MinKNOW-shaped runs to disk over time for testing ont-offload.
//!
//! Test tool, not part of the medical software. v0.1 scaffold: generation,
//! profiles, presets, `check` and `clean` are added in v0.1 development.

/// Errors from the simulator.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The command exists but is not implemented yet.
    #[error("`{0}` is not implemented yet (planned for v0.1)")]
    NotImplemented(&'static str),
}

/// Built-in scenario presets.
pub const PRESETS: &[&str] = &["quick-2h", "promethion-24h", "outage-drill"];
