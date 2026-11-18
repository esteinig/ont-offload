// SPDX-License-Identifier: AGPL-3.0-or-later
//! Live state of a simulation: `<out>/.ont-runsim/status.json` (rewritten
//! atomically) and `<out>/.ont-runsim/events.jsonl` (one JSON object per line).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::{Error, Result, STATE_DIR, io_err, write_atomic};

/// `<out>/.ont-runsim`
#[must_use]
pub fn state_dir(out: &Path) -> PathBuf {
    out.join(STATE_DIR)
}

/// `<out>/.ont-runsim/status.json`
#[must_use]
pub fn status_path(out: &Path) -> PathBuf {
    state_dir(out).join("status.json")
}

/// `<out>/.ont-runsim/events.jsonl`
#[must_use]
pub fn events_path(out: &Path) -> PathBuf {
    state_dir(out).join("events.jsonl")
}

/// `<out>/.ont-runsim/manifests`
#[must_use]
pub fn manifests_dir(out: &Path) -> PathBuf {
    state_dir(out).join("manifests")
}

/// `<out>/.ont-runsim/runs.txt`: every run folder created, relative to `out`.
#[must_use]
pub fn runs_list(out: &Path) -> PathBuf {
    state_dir(out).join("runs.txt")
}

/// RFC 3339 UTC timestamp, seconds precision.
#[must_use]
pub fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

/// Progress of one run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStatus {
    /// Run folder name.
    pub name: String,
    /// Run folder path.
    pub path: String,
    /// `waiting`, `sequencing`, `complete` or `aborted`.
    pub state: String,
    /// Files complete.
    pub files_closed: usize,
    /// Files planned.
    pub files_planned: usize,
    /// Bytes written.
    pub bytes_written: u64,
}

/// Snapshot written to `status.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    /// Always `ont-runsim`.
    pub tool: String,
    /// Tool version.
    pub version: String,
    /// Always true: this is simulated data.
    pub simulated: bool,
    /// Process id of the simulator.
    pub pid: u32,
    /// `running`, `finished`, `stopped` or `failed`.
    pub state: String,
    /// Output folder.
    pub out: String,
    /// Start time.
    pub started: String,
    /// Time of this snapshot.
    pub updated: String,
    /// Simulated seconds per wall-clock second.
    pub speed: f64,
    /// Simulated seconds elapsed.
    pub sim_seconds: f64,
    /// Simulated seconds in total.
    pub total_seconds: f64,
    /// Bytes written.
    pub bytes_written: u64,
    /// Files complete.
    pub files_closed: usize,
    /// Files planned.
    pub files_planned: usize,
    /// Last error or stop reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Per run.
    pub runs: Vec<RunStatus>,
}

impl Status {
    /// Write atomically.
    ///
    /// # Errors
    /// I/O errors.
    pub fn write(&self, out: &Path) -> Result<()> {
        let text = serde_json::to_vec_pretty(self).map_err(|e| Error::Scenario(e.to_string()))?;
        write_atomic(&status_path(out), &text)
    }

    /// Read the snapshot of an output folder.
    ///
    /// # Errors
    /// I/O or format errors.
    pub fn read(out: &Path) -> Result<Self> {
        let p = status_path(out);
        let text = std::fs::read_to_string(&p).map_err(io_err("read", &p))?;
        serde_json::from_str(&text).map_err(|e| Error::Scenario(format!("{}: {e}", p.display())))
    }

    /// True when the snapshot says running and the process still exists.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.state == "running" && pid_alive(self.pid)
    }
}

/// Whether a process exists (Linux: `/proc/<pid>`; elsewhere assumed alive).
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    let proc_root = Path::new("/proc");
    if proc_root.join("self").exists() {
        proc_root.join(pid.to_string()).exists()
    } else {
        true
    }
}

/// Appends events to `events.jsonl`.
#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    /// Event log of an output folder.
    #[must_use]
    pub fn new(out: &Path) -> Self {
        Self {
            path: events_path(out),
        }
    }

    /// Append one event.
    ///
    /// # Errors
    /// I/O errors.
    pub fn emit(&self, sim_s: f64, event: &str, run: Option<&str>, detail: &str) -> Result<()> {
        let line = serde_json::json!({
            "ts": now_rfc3339(),
            "sim_s": (sim_s * 10.0).round() / 10.0,
            "event": event,
            "run": run,
            "detail": detail,
        });
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(io_err("open", &self.path))?;
        writeln!(f, "{line}").map_err(io_err("write", &self.path))
    }
}
