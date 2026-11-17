// SPDX-License-Identifier: AGPL-3.0-or-later
//! Live state (SWI-STATE): `state.json`, a snapshot rewritten atomically
//! every `state.interval`; `events.jsonl`, one line per state change,
//! rotated to `events.jsonl.1` at `state.events_max_size`, the previous
//! `events.jsonl.1` moving into the never-pruned archive `events/`; and
//! `notify_command`, run for every
//! event that needs a person. The formats are described by the JSON
//! Schemas in `schema/` (`state-v1.json`, `event-v1.json`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::fsio::Fs;
use crate::journal::write_atomic;
use crate::logging::escape_control;

/// Snapshot file name in `state_dir`.
pub const STATE_FILE: &str = "state.json";
/// Event stream file name in `state_dir`.
pub const EVENTS_FILE: &str = "events.jsonl";
/// Archive folder in `state_dir` for rotated event segments, never pruned.
pub const EVENTS_ARCHIVE: &str = "events";
/// `schema` value of a snapshot.
pub const STATE_SCHEMA: &str = "ont-offload/state/v1";
/// `schema` value of an event.
pub const EVENT_SCHEMA: &str = "ont-offload/event/v1";
/// `schema_version` of both formats. Raised only for changes a reader
/// must know about; fields may be added within a version.
pub const SCHEMA_VERSION: u32 = 1;

/// The whole snapshot (`state.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// Always [`STATE_SCHEMA`].
    pub schema: String,
    /// Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// ont-offload version.
    pub tool_version: String,
    /// Host name.
    pub host: String,
    /// Process id.
    pub pid: u32,
    /// When `watch` started (RFC 3339).
    pub started_at: String,
    /// When this snapshot was written (RFC 3339).
    pub updated_at: String,
    /// Snapshot interval in seconds (a reader treats a snapshot older than
    /// three intervals as stale).
    pub interval_s: u64,
    /// `starting`, `running` (copying or waiting for runs), `outage`
    /// (the share is unavailable) or `stopping`.
    pub status: String,
    /// Destination target id.
    pub target_id: String,
    /// Source root.
    pub source_root: String,
    /// Destination state.
    pub destination: Destination,
    /// Free bytes on the source (`null` when unknown).
    pub source_free_bytes: Option<u64>,
    /// Workers.
    pub workers: Workers,
    /// Rate limit and current speed.
    pub rate: Rate,
    /// Counters since start.
    pub totals: Totals,
    /// Runs being offloaded or watched.
    pub runs: Vec<RunState>,
    /// Conditions that need a person, oldest first.
    pub alerts: Vec<Alert>,
    /// Source cleanup (absent when `cleanup.delete` is `off`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<CleanupState>,
}

/// Source cleanup: the mode, the next pass and the last pass's result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanupState {
    /// `dry-run` or `delete`.
    pub mode: String,
    /// `storage-verify`, `readback` or `both`.
    pub gate: String,
    /// A pass is running.
    pub running: bool,
    /// When the last pass ended (`null` before the first).
    pub last_pass_at: Option<String>,
    /// Seconds until the next pass.
    pub next_pass_s: u64,
    /// Runs deleted in the last pass.
    pub deleted_runs: u64,
    /// Bytes deleted in the last pass.
    pub deleted_bytes: u64,
    /// Runs that passed every gate in the last dry-run pass.
    pub would_delete: u64,
    /// Runs that passed every gate but were held by `only_when_free_below`.
    pub held: u64,
    /// Runs waiting for a gate.
    pub waiting: u64,
    /// Runs refused by a gate (see the alerts).
    pub refused: u64,
    /// Why the last pass stopped early, if it did.
    pub stopped: Option<String>,
}

/// Rate limit and current speed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    /// Total limit in bytes per second (`null`: unlimited).
    pub limit_bytes_per_s: Option<u64>,
    /// Bytes per second committed over the last minute.
    pub current_bytes_per_s: u64,
}

/// A condition that needs a person, shown until it clears.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Alert {
    /// The event kind that raised it.
    pub kind: Kind,
    /// `warning` or `error`.
    pub level: String,
    /// When it was raised (RFC 3339).
    pub since: String,
    /// The run, if the alert is about one.
    pub run: Option<String>,
    /// What happened.
    pub message: String,
}

/// Destination state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    /// Destination root.
    pub root: String,
    /// File system type from the mount table (`null` when unknown).
    pub fstype: Option<String>,
    /// `starting`, `ok` or `unavailable`.
    pub status: String,
    /// The last guard check passed.
    pub guard_ok: bool,
    /// Free bytes on the share, from the last check (`null` when unknown).
    pub free_bytes: Option<u64>,
    /// The current outage, if any.
    pub outage: Option<OutageState>,
}

/// An ongoing outage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutageState {
    /// When it started (RFC 3339).
    pub since: String,
    /// Seconds since it started.
    pub duration_s: u64,
    /// Last error.
    pub reason: String,
    /// `notice`, `warning` (after `warn_after`) or `alert` (after `alert_after`).
    pub level: String,
    /// Failed checks so far.
    pub checks: u64,
    /// Seconds until the next check.
    pub next_check_s: u64,
    /// The share passed the last check; waiting for `stable_before_resume`.
    pub recovering: bool,
    /// Files waiting.
    pub backlog_files: u64,
    /// Bytes waiting.
    pub backlog_bytes: u64,
    /// Workers with no progress for `stall_timeout`.
    pub stuck_workers: u64,
}

/// Workers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Workers {
    /// Configured workers.
    pub count: u64,
    /// Workers paused by an outage.
    pub paused: bool,
    /// Busy workers.
    pub busy: Vec<WorkerState>,
}

/// One busy worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerState {
    /// Worker index.
    pub worker: u64,
    /// Run (relative to the source root).
    pub run: String,
    /// File or task.
    pub task: String,
    /// Bytes written so far.
    pub bytes: u64,
    /// Size of the file (`null` for other tasks).
    pub bytes_total: Option<u64>,
    /// Seconds since the last progress.
    pub idle_s: u64,
    /// Reported as stalled.
    pub stalled: bool,
}

/// Counters since start.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Totals {
    /// Files committed.
    pub committed_files: u64,
    /// Bytes committed.
    pub committed_bytes: u64,
    /// Files that failed for good.
    pub failed_files: u64,
    /// Runs completed.
    pub completed_runs: u64,
    /// Runs that cannot be completed.
    pub failed_runs: u64,
    /// Completed runs reopened because the source changed.
    pub reopened_runs: u64,
    /// Outages.
    pub outages: u64,
    /// Stalls.
    pub stalls: u64,
    /// Source runs deleted by cleanup.
    pub deleted_runs: u64,
}

/// One run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunState {
    /// Run id.
    pub run_id: String,
    /// Run path relative to the source root (prefixed by the source name
    /// when several sources are configured).
    pub name: String,
    /// Source name, only when several sources are configured.
    ///
    /// Implements: SRS-SRC-005
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    /// Source folder.
    pub source: String,
    /// Destination folder.
    pub destination: String,
    /// `live`, `settling`, `reconciling`, `finalising`, `complete` (still
    /// watched for changes) or `failed`.
    pub phase: String,
    /// Selected files.
    pub files: u64,
    /// Selected bytes.
    pub bytes: u64,
    /// Files committed as they are now.
    pub committed_files: u64,
    /// Files waiting to be copied.
    pub backlog_files: u64,
    /// Bytes waiting to be copied.
    pub backlog_bytes: u64,
    /// Files being copied.
    pub copying: u64,
    /// Files that failed for good.
    pub failed: Vec<FailedFile>,
    /// Copies whose destination did not match what was read (each retried).
    pub integrity_mismatches: u64,
    /// Entries not copied: symlinks and special files (relative paths).
    pub skipped: Vec<String>,
    /// Estimated seconds until the backlog is copied at the current speed
    /// (`null` when nothing is moving).
    pub eta_s: Option<u64>,
    /// Complete marker, if present.
    pub complete_marker: Option<String>,
    /// Manifest digest once complete.
    pub manifest_sha256: Option<String>,
    /// Why the run cannot be completed, if so.
    pub error: Option<String>,
}

/// A failed file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FailedFile {
    /// Path relative to the run.
    pub file: String,
    /// Reason.
    pub reason: String,
}

/// Event kinds (`kind` in `events.jsonl`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `watch` started.
    Started,
    /// The destination passed its first check.
    DestinationOk,
    /// A run was found.
    RunDiscovered,
    /// A run changed phase.
    RunStateChanged,
    /// A file was committed.
    FileCommitted,
    /// A file failed for good.
    FileFailed,
    /// A copy did not match its source (it is retried).
    IntegrityMismatch,
    /// A run was completed.
    RunComplete,
    /// A completed run changed at the source; `COMPLETE` withdrawn.
    RunReopened,
    /// A run cannot be completed (claim refused, integrity).
    RunFailed,
    /// A completed run is no longer watched and was dropped from memory.
    RunRetired,
    /// Workers paused (the share is unavailable).
    Paused,
    /// Workers resumed.
    Resumed,
    /// The destination failed a guard check.
    GuardFailed,
    /// Free space on the share fell below `min_free`.
    CapacityLow,
    /// Free space on the share is above `min_free` again.
    CapacityOk,
    /// Outage started.
    OutageStarted,
    /// Outage summary.
    OutageSummary,
    /// Outage reached `warn_after`.
    OutageWarning,
    /// Outage reached `alert_after`.
    OutageAlert,
    /// Outage still going, every `remind_every` after the alert.
    OutageReminder,
    /// The share passes checks again; waiting for stability.
    OutageRecovering,
    /// Outage ended.
    OutageEnded,
    /// A worker stalled.
    WorkerStalled,
    /// Free space on the source fell below `source_free_alert`.
    SourceSpaceLow,
    /// Free space on the source is above `source_free_alert` again.
    SourceSpaceOk,
    /// A source run was deleted after passing every gate.
    RunDeleted,
    /// A dry-run cleanup pass: what would have been deleted.
    DeletionDryRun,
    /// A completed run failed a deletion gate and is kept.
    DeletionRefused,
    /// Deleting a source run stopped on an error; it resumes next pass.
    DeletionStopped,
    /// Free space where the records are kept (`state_dir`, the log file)
    /// fell below `state.free_alert`.
    RecordsSpaceLow,
    /// Free space for the records is above `state.free_alert` again.
    RecordsSpaceOk,
}

impl Kind {
    /// Every kind, in the order of the schema.
    pub const ALL: [Self; 32] = [
        Self::Started,
        Self::DestinationOk,
        Self::RunDiscovered,
        Self::RunStateChanged,
        Self::FileCommitted,
        Self::FileFailed,
        Self::IntegrityMismatch,
        Self::RunComplete,
        Self::RunReopened,
        Self::RunFailed,
        Self::RunRetired,
        Self::Paused,
        Self::Resumed,
        Self::GuardFailed,
        Self::CapacityLow,
        Self::CapacityOk,
        Self::OutageStarted,
        Self::OutageSummary,
        Self::OutageWarning,
        Self::OutageAlert,
        Self::OutageReminder,
        Self::OutageRecovering,
        Self::OutageEnded,
        Self::WorkerStalled,
        Self::SourceSpaceLow,
        Self::SourceSpaceOk,
        Self::RunDeleted,
        Self::DeletionDryRun,
        Self::DeletionRefused,
        Self::DeletionStopped,
        Self::RecordsSpaceLow,
        Self::RecordsSpaceOk,
    ];

    /// Events that need a person: `notify_command` runs for these.
    ///
    /// Implements: SRS-STATE-003
    #[must_use]
    pub fn alerts(self) -> bool {
        matches!(
            self,
            Self::OutageStarted
                | Self::OutageWarning
                | Self::OutageAlert
                | Self::OutageReminder
                | Self::OutageEnded
                | Self::GuardFailed
                | Self::CapacityLow
                | Self::IntegrityMismatch
                | Self::WorkerStalled
                | Self::FileFailed
                | Self::RunFailed
                | Self::RunReopened
                | Self::SourceSpaceLow
                | Self::DeletionRefused
                | Self::DeletionStopped
                | Self::RecordsSpaceLow
        )
    }

    /// The snake-case name.
    #[must_use]
    pub fn as_str(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    }
}

/// One line of `events.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// Always [`EVENT_SCHEMA`].
    pub schema: String,
    /// Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Sequence number, from 1 at each start.
    pub seq: u64,
    /// Time (RFC 3339).
    pub ts: String,
    /// Event kind.
    pub kind: Kind,
    /// Severity: `info`, `warning` or `error`.
    pub level: String,
    /// Run path relative to the source root, if the event is about a run.
    pub run: Option<String>,
    /// File relative to the run, if the event is about a file.
    pub file: Option<String>,
    /// Human-readable message (control characters escaped).
    pub message: String,
    /// Kind-specific details.
    pub data: serde_json::Value,
}

/// Rotate the event stream without losing a line: the previous
/// `events.jsonl.1` moves into `events/` under the time of its first event,
/// then `events.jsonl` becomes `events.jsonl.1` (the name the dashboard
/// follows). Nothing in the archive is ever removed.
///
/// Implements: SRS-STATE-002, SRS-LOG-006
///
/// # Errors
/// The archive folder cannot be created or a rename fails; the stream then
/// keeps growing in `events.jsonl` and nothing is lost.
pub fn rotate_events(fs: &dyn Fs, dir: &Path) -> std::io::Result<()> {
    let current = dir.join(EVENTS_FILE);
    let previous = dir.join(format!("{EVENTS_FILE}.1"));
    if fs.metadata(&previous).is_ok() {
        let archive = dir.join(EVENTS_ARCHIVE);
        fs.create_dir_all(&archive)?;
        let stamp = first_event_stamp(fs, &previous).unwrap_or_else(|| {
            crate::journal::now_rfc3339()
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect()
        });
        let mut target = archive.join(format!("events-{stamp}.jsonl"));
        let mut n = 2;
        while fs.metadata(&target).is_ok() {
            // `_` sorts after `.`, so name order stays time order.
            target = archive.join(format!("events-{stamp}_{n:04}.jsonl"));
            n += 1;
        }
        fs.rename(&previous, &target)?;
    }
    fs.rename(&current, &previous)
}

/// `20261117T081500Z` from the `ts` of the first event in a segment.
fn first_event_stamp(fs: &dyn Fs, path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let mut line = String::new();
    BufReader::new(fs.open_read(path).ok()?)
        .read_line(&mut line)
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let stamp: String = v
        .get("ts")?
        .as_str()?
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    (!stamp.is_empty()).then_some(stamp)
}

/// Every event file in `state_dir`, oldest first: the archive in name
/// order, then `events.jsonl.1`, then `events.jsonl`.
///
/// Implements: SRS-LOG-006
#[must_use]
pub fn event_files(fs: &dyn Fs, dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs
        .read_dir(&dir.join(EVENTS_ARCHIVE))
        .map(|entries| {
            entries
                .into_iter()
                .map(|e| e.name.to_string_lossy().into_owned())
                .filter(|n| {
                    n.starts_with("events-")
                        && Path::new(n).extension().is_some_and(|e| e == "jsonl")
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .map(|n| dir.join(EVENTS_ARCHIVE).join(n))
        .collect();
    out.sort();
    for name in [format!("{EVENTS_FILE}.1"), EVENTS_FILE.to_owned()] {
        let p = dir.join(name);
        if fs.metadata(&p).is_ok() {
            out.push(p);
        }
    }
    out
}

/// Writes `state.json` and `events.jsonl` and runs `notify_command`.
#[derive(Debug)]
pub struct StateWriter {
    dir: PathBuf,
    interval: Duration,
    events_max_size: u64,
    seq: u64,
    last_snapshot: Option<Duration>,
    notify: Option<String>,
    events_size: u64,
}

impl StateWriter {
    /// A writer for `state_dir`.
    #[must_use]
    pub fn new(
        fs: &dyn Fs,
        dir: &Path,
        interval: Duration,
        events_max_size: u64,
        notify: Option<String>,
    ) -> Self {
        let events_size = fs.metadata(&dir.join(EVENTS_FILE)).map_or(0, |m| m.len);
        Self {
            dir: dir.to_path_buf(),
            interval,
            events_max_size,
            seq: 0,
            last_snapshot: None,
            notify,
            events_size,
        }
    }

    /// Append an event, rotating the stream when it is full, and run
    /// `notify_command` for events that need a person. Errors writing the
    /// state are logged, never fatal: the offload goes on.
    ///
    /// Implements: SRS-STATE-002, SRS-STATE-003
    #[allow(clippy::too_many_arguments)]
    pub fn event(
        &mut self,
        fs: &dyn Fs,
        kind: Kind,
        level: log::Level,
        run: Option<&str>,
        file: Option<&str>,
        message: &str,
        data: serde_json::Value,
    ) -> Event {
        self.seq += 1;
        let ev = Event {
            schema: EVENT_SCHEMA.into(),
            schema_version: SCHEMA_VERSION,
            seq: self.seq,
            ts: crate::journal::now_rfc3339(),
            kind,
            level: match level {
                log::Level::Error => "error",
                log::Level::Warn => "warning",
                _ => "info",
            }
            .into(),
            run: run.map(escape_control),
            file: file.map(escape_control),
            message: escape_control(message),
            data,
        };
        let mut line = serde_json::to_string(&ev).unwrap_or_default();
        line.push('\n');
        let path = self.dir.join(EVENTS_FILE);
        if self.events_size + line.len() as u64 > self.events_max_size && self.events_size > 0 {
            match rotate_events(fs, &self.dir) {
                Ok(()) => self.events_size = 0,
                Err(e) => log::warn!("cannot rotate {EVENTS_FILE}: {e}"),
            }
        }
        match fs.append_sync(&path, line.as_bytes()) {
            Ok(()) => self.events_size += line.len() as u64,
            Err(e) => log::warn!("cannot write {EVENTS_FILE}: {e}"),
        }
        if kind.alerts() {
            if let Some(cmd) = &self.notify {
                run_notify(cmd, &ev, &line);
            }
        }
        ev
    }

    /// Whether a snapshot is due at clock reading `now`.
    #[must_use]
    pub fn due(&self, now: Duration) -> bool {
        self.last_snapshot
            .is_none_or(|t| now.saturating_sub(t) >= self.interval)
    }

    /// Write `state.json` atomically.
    ///
    /// Implements: SRS-STATE-001
    ///
    /// # Errors
    /// I/O errors (the caller logs them).
    pub fn snapshot(&mut self, fs: &dyn Fs, now: Duration, snap: &Snapshot) -> Result<()> {
        self.last_snapshot = Some(now);
        let text = serde_json::to_string_pretty(snap).unwrap_or_default() + "\n";
        write_atomic(fs, &self.dir, STATE_FILE, text.as_bytes())
    }
}

/// Run `notify_command` (program and arguments, no shell) for one event,
/// without waiting: the event is given as one JSON line on stdin and in
/// `ONT_OFFLOAD_EVENT_JSON`, with `ONT_OFFLOAD_EVENT` (kind),
/// `ONT_OFFLOAD_LEVEL` and `ONT_OFFLOAD_MESSAGE` for simple scripts.
/// A command that runs longer than a minute is killed.
///
/// Implements: SRS-STATE-003
pub fn run_notify(cmd: &str, ev: &Event, json_line: &str) {
    let mut parts = cmd.split_whitespace();
    let Some(program) = parts.next() else { return };
    let spawned = std::process::Command::new(program)
        .args(parts)
        .env("ONT_OFFLOAD_EVENT", ev.kind.as_str())
        .env("ONT_OFFLOAD_LEVEL", &ev.level)
        .env("ONT_OFFLOAD_MESSAGE", &ev.message)
        .env("ONT_OFFLOAD_EVENT_JSON", json_line.trim_end())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            let input = json_line.to_owned();
            let _ = std::thread::Builder::new()
                .name("notify".into())
                .spawn(move || {
                    if let Some(mut stdin) = child.stdin.take() {
                        use std::io::Write as _;
                        // A command that ignores stdin closes it early; that is fine.
                        let _ = stdin.write_all(input.as_bytes());
                    }
                    let t0 = std::time::Instant::now();
                    loop {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                if !status.success() {
                                    log::warn!("notify_command exited with {status}");
                                }
                                return;
                            }
                            Ok(None) if t0.elapsed() > Duration::from_secs(60) => {
                                let _ = child.kill();
                                let _ = child.wait();
                                log::warn!("notify_command ran longer than 60 s; killed");
                                return;
                            }
                            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                            Err(e) => {
                                log::warn!("notify_command: {e}");
                                return;
                            }
                        }
                    }
                });
        }
        Err(e) => log::warn!("cannot run notify_command {}: {e}", escape_control(program)),
    }
}

/// Read and parse `state_dir/state.json`.
///
/// # Errors
/// I/O or format errors.
pub fn read_snapshot(fs: &dyn Fs, state_dir: &Path) -> std::result::Result<Snapshot, String> {
    let path = state_dir.join(STATE_FILE);
    let bytes = fs.read(&path).map_err(|e| {
        format!(
            "cannot read {}: {e}",
            escape_control(&path.to_string_lossy())
        )
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("{}: {e}", escape_control(&path.to_string_lossy())))
}

/// How old a snapshot may be before `status` calls it stale.
#[must_use]
pub fn stale_after(snap: &Snapshot) -> Duration {
    Duration::from_secs((3 * snap.interval_s).max(30))
}

/// The snapshot in plain words, for `status`.
///
/// Implements: SRS-STATE-001
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn describe(snap: &Snapshot, now: std::time::SystemTime) -> String {
    use crate::config::units::Bytes;
    use crate::sched::human;
    use std::fmt::Write as _;
    let mut s = String::new();
    let age = humantime::parse_rfc3339_weak(&snap.updated_at)
        .ok()
        .and_then(|t| now.duration_since(t).ok());
    let _ = writeln!(
        s,
        "ont-offload {} on {} (pid {}), started {}",
        snap.tool_version, snap.host, snap.pid, snap.started_at
    );
    match age {
        Some(a) if a > stale_after(snap) => {
            let _ = writeln!(
                s,
                "STALE: state last written {} ago ({}); the daemon is not running or is stuck",
                human(a),
                snap.updated_at
            );
        }
        Some(a) => {
            let _ = writeln!(s, "updated {} ago", human(a));
        }
        None => {
            let _ = writeln!(s, "updated {}", snap.updated_at);
        }
    }
    let d = &snap.destination;
    let _ = writeln!(
        s,
        "status: {}; destination {} {}{}{}",
        snap.status,
        d.root,
        d.status,
        d.fstype
            .as_deref()
            .map(|f| format!(" ({f})"))
            .unwrap_or_default(),
        d.free_bytes
            .map(|f| format!(", {} free", Bytes(f)))
            .unwrap_or_default()
    );
    if let Some(o) = &d.outage {
        let _ = writeln!(
            s,
            "outage: {} for {} ({}), {} check(s), next in {}{}; {} file(s), {} waiting; {} worker(s) stuck",
            o.level,
            human(Duration::from_secs(o.duration_s)),
            o.reason,
            o.checks,
            human(Duration::from_secs(o.next_check_s)),
            if o.recovering { ", recovering" } else { "" },
            o.backlog_files,
            Bytes(o.backlog_bytes),
            o.stuck_workers
        );
    }
    if let Some(f) = snap.source_free_bytes {
        let _ = writeln!(s, "source: {} free", Bytes(f));
    }
    let _ = writeln!(
        s,
        "speed: {}/s (limit {}); workers {} busy of {}{}",
        Bytes(snap.rate.current_bytes_per_s),
        snap.rate
            .limit_bytes_per_s
            .map_or_else(|| "none".to_owned(), |l| format!("{}/s", Bytes(l))),
        snap.workers.busy.len(),
        snap.workers.count,
        if snap.workers.paused { " (paused)" } else { "" }
    );
    for w in &snap.workers.busy {
        let _ = writeln!(
            s,
            "  worker {}: {} {} ({}{}){}",
            w.worker,
            w.run,
            w.task,
            Bytes(w.bytes),
            w.bytes_total
                .map(|t| format!(" of {}", Bytes(t)))
                .unwrap_or_default(),
            if w.stalled { " STALLED" } else { "" }
        );
    }
    let t = &snap.totals;
    let _ = writeln!(
        s,
        "since start: {} file(s), {} committed; {} run(s) complete, {} reopened, {} failed, {} deleted at the source; {} failed file(s); {} outage(s), {} stall(s)",
        t.committed_files,
        Bytes(t.committed_bytes),
        t.completed_runs,
        t.reopened_runs,
        t.failed_runs,
        t.deleted_runs,
        t.failed_files,
        t.outages,
        t.stalls
    );
    if let Some(c) = &snap.cleanup {
        let _ = writeln!(
            s,
            "cleanup: {} (gate {}){}; last pass {}: {} deleted ({}), {} would be deleted, {} held, {} waiting, {} refused{}; next in {}",
            c.mode,
            c.gate,
            if c.running { ", running" } else { "" },
            c.last_pass_at.as_deref().unwrap_or("none yet"),
            c.deleted_runs,
            Bytes(c.deleted_bytes),
            c.would_delete,
            c.held,
            c.waiting,
            c.refused,
            c.stopped
                .as_deref()
                .map(|w| format!("; STOPPED: {w}"))
                .unwrap_or_default(),
            human(Duration::from_secs(c.next_pass_s))
        );
    }
    let _ = writeln!(s, "runs: {}", snap.runs.len());
    for r in &snap.runs {
        let mut line = format!(
            "  {}  {}  {}/{} file(s), {}",
            r.name,
            r.phase,
            r.committed_files,
            r.files,
            Bytes(r.bytes)
        );
        if r.backlog_files > 0 {
            let _ = write!(
                line,
                "; {} waiting ({})",
                r.backlog_files,
                Bytes(r.backlog_bytes)
            );
        }
        if let Some(e) = r.eta_s {
            let _ = write!(line, ", eta {}", human(Duration::from_secs(e)));
        }
        if !r.failed.is_empty() {
            let _ = write!(line, "; {} FAILED", r.failed.len());
        }
        if r.integrity_mismatches > 0 {
            let _ = write!(line, "; {} mismatch(es) retried", r.integrity_mismatches);
        }
        if let Some(e) = &r.error {
            let _ = write!(line, "; error: {e}");
        }
        let _ = writeln!(s, "{line}");
    }
    if snap.alerts.is_empty() {
        let _ = writeln!(s, "alerts: none");
    } else {
        let _ = writeln!(s, "alerts: {}", snap.alerts.len());
        for a in &snap.alerts {
            let _ = writeln!(s, "  [{}] since {}: {}", a.level, a.since, a.message);
        }
    }
    s
}

#[cfg(test)]
pub(crate) mod schema_check;
#[cfg(test)]
mod tests;
