// SPDX-License-Identifier: AGPL-3.0-or-later
//! ont-runsim: writes MinKNOW-shaped runs to disk over time for testing ont-offload.
//!
//! Test tool, not part of the medical software, and independent of the
//! `ont-offload` crate so that it can judge it.
//!
//! - [`scenario`]: what to simulate (TOML file or built-in preset).
//! - [`profile`]: timelines, recorded from a real run or synthetic.
//! - [`plan`]: a timeline fitted to the requested hours, per run, with faults.
//! - [`engine`]: writes the planned files as simulated time passes and keeps
//!   SHA-256 manifests of what it wrote.
//! - [`check`]: compares an offloaded copy with a manifest.
//! - [`clean`]: removes what the simulator created, and nothing else.
//! - [`status`]: live `status.json` and `events.jsonl`.
//! - [`resume`]: `resume.json`, to continue a stopped simulation.

pub mod check;
pub mod clean;
pub mod engine;
pub mod plan;
pub mod profile;
pub mod resume;
pub mod rng;
pub mod scenario;
pub mod status;
pub mod units;

use std::path::{Path, PathBuf};

/// Folder inside the output folder where the simulator keeps its own state.
/// Its presence marks the output folder as created by ont-runsim.
pub const STATE_DIR: &str = ".ont-runsim";

/// Label file written into every simulated run folder.
pub const LABEL_FILE: &str = "SIMULATED_RUN.txt";

/// Built-in scenario presets.
pub const PRESETS: &[&str] = &[
    "quick-2h",
    "outage-drill",
    "promethion-24h",
    "promethion-72h",
    "promethion-72h-barcoded",
    "promethion-72h-rehearsal",
    "promethion-p24-48h",
];

/// Errors from the simulator.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem operation failed.
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// What was being done.
        op: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Cause.
        #[source]
        source: std::io::Error,
    },
    /// The scenario is invalid.
    #[error("scenario: {0}")]
    Scenario(String),
    /// A profile could not be read, recorded or used.
    #[error("profile: {0}")]
    Profile(String),
    /// Free space fell below `keep_free`.
    #[error("stopped: only {free} bytes free on the output disk, keep_free is {keep}")]
    DiskFull {
        /// Free bytes.
        free: u64,
        /// Configured floor.
        keep: u64,
    },
    /// The request would touch something the simulator did not create.
    #[error("refused: {0}")]
    Refused(String),
    /// A copy did not match its manifest.
    #[error("{0}")]
    Mismatch(String),
}

/// Result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Attach an operation and path to an I/O error.
pub(crate) fn io_err(op: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.to_owned();
    move |source| Error::Io { op, path, source }
}

/// Write a file atomically: temp file in the same folder, then rename.
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(io_err("write", &tmp))?;
    std::fs::rename(&tmp, path).map_err(io_err("rename", path))
}
