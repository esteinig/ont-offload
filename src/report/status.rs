// SPDX-License-Identifier: AGPL-3.0-or-later
//! The status record of a period: what ont-offload did, what went wrong
//! and whether the records cover the whole period. Built from the event
//! archive and the log, optionally checked against the receipts on the
//! share. Every count and verdict is decided here, not in the template.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fsio::Fs;
use crate::journal::VERIFIED;
use crate::state::{Event, Kind};

use super::history::{self, InputSummary, LogLine};
use super::time::{self, Period};

/// `schema` of a status record.
pub const STATUS_SCHEMA: &str = "ont-offload/status-report/v1";

/// Verdict values.
pub mod verdict {
    /// Nothing needs a person.
    pub const OK: &str = "ok";
    /// Something happened that a reviewer should read, but nothing is open.
    pub const ATTENTION: &str = "attention";
    /// Something is open, or the records cannot vouch for the period.
    pub const ACTION: &str = "action_required";
}

/// The whole status record (`*.json` next to the PDF).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusRecord {
    /// Always [`STATUS_SCHEMA`].
    pub schema: String,
    /// 1.
    pub schema_version: u32,
    /// Report identifier, e.g. `OOF-SR-20261031-3f9a1c`.
    pub report_id: String,
    /// When the report was made (RFC 3339).
    pub generated_at: String,
    /// ont-offload version that made it.
    pub tool_version: String,
    /// Host it was made on.
    pub host: String,
    /// Site name from `[report]`, if set.
    pub site: Option<String>,
    /// The period.
    pub period: PeriodOut,
    /// Destination target id, from the configuration.
    pub target_id: Option<String>,
    /// Destination root, from the configuration.
    pub destination: Option<String>,
    /// Sources, from the configuration.
    pub sources: Vec<SourceOut>,
    /// ont-offload versions that ran in the period.
    pub versions: Vec<String>,
    /// The verdict.
    pub verdict: Verdict,
    /// Key figures.
    pub totals: Totals,
    /// One entry per calendar day (UTC) of the period.
    pub daily: Vec<Day>,
    /// Runs with any activity in the period.
    pub runs: Vec<RunRow>,
    /// Incidents that started in, or were open during, the period.
    pub incidents: Vec<Incident>,
    /// Starts of the daemon that overlap the period.
    pub sessions: Vec<Session>,
    /// Cleanup of source runs in the period.
    pub cleanup: Vec<CleanupRow>,
    /// The records read, with their coverage and gaps.
    pub inputs: Vec<InputSummary>,
    /// Warning and error lines of the log in the period.
    pub log_issues: Vec<LogLine>,
    /// Every event of the period except per-file commits (only with
    /// `--appendix`; empty otherwise).
    pub events: Vec<EventRow>,
}

/// The period, as printed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodOut {
    /// First second (RFC 3339).
    pub from: String,
    /// First second after the period (RFC 3339).
    pub to: String,
    /// Last calendar day in the period (`YYYY-MM-DD`).
    pub last_day: String,
    /// Calendar days touched.
    pub days: u64,
}

/// A configured source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceOut {
    /// Name.
    pub name: String,
    /// Root folder.
    pub root: String,
}

/// The verdict and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// `ok`, `attention` or `action_required`.
    pub status: String,
    /// One sentence.
    pub headline: String,
    /// Every reason, most serious first.
    pub reasons: Vec<String>,
}

/// Key figures of the period.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    /// Runs with any activity.
    pub runs_active: u64,
    /// Runs completed in the period.
    pub runs_completed: u64,
    /// Runs failed at the end of the period.
    pub runs_failed: u64,
    /// Runs reopened in the period.
    pub runs_reopened: u64,
    /// Source runs deleted in the period.
    pub runs_deleted: u64,
    /// Files committed.
    pub files_committed: u64,
    /// Bytes committed.
    pub bytes_committed: u64,
    /// Files that failed for good.
    pub files_failed: u64,
    /// Copies that did not match their source (each retried).
    pub integrity_mismatches: u64,
    /// Outages that started in or overlapped the period.
    pub outages: u64,
    /// Seconds of outage inside the period.
    pub outage_s: u64,
    /// Share of the period without an outage, in percent.
    pub availability_pct: f64,
    /// Worker stalls.
    pub stalls: u64,
    /// Deletions refused by a gate.
    pub deletion_refused: u64,
    /// Runs complete on the share without a matching receipt (only with
    /// `--check-destination`).
    pub unverified_runs: Option<u64>,
}

/// One calendar day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Day {
    /// `YYYY-MM-DD`.
    pub day: String,
    /// Files committed.
    pub files: u64,
    /// Bytes committed.
    pub bytes: u64,
    /// Seconds of outage.
    pub outage_s: u64,
}

/// One run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRow {
    /// Run (relative to its source root, prefixed by the source name when
    /// several are configured).
    pub name: String,
    /// Flow cell id, when the folder name follows MinKNOW's pattern.
    pub flow_cell: Option<String>,
    /// Position, when the folder name follows MinKNOW's pattern.
    pub position: Option<String>,
    /// First seen (RFC 3339).
    pub discovered_at: Option<String>,
    /// Completed (RFC 3339), last completion.
    pub completed_at: Option<String>,
    /// Seconds from discovery to completion.
    pub duration_s: Option<u64>,
    /// Files committed in the period.
    pub files_in_period: u64,
    /// Bytes committed in the period.
    pub bytes_in_period: u64,
    /// Files in the completed run.
    pub files: Option<u64>,
    /// Bytes in the completed run.
    pub bytes: Option<u64>,
    /// Manifest digest of the completed run.
    pub manifest_sha256: Option<String>,
    /// Destination folder.
    pub destination: Option<String>,
    /// At the end of the period: `in_progress`, `complete`, `verified`,
    /// `reopened`, `failed` or `deleted`.
    pub status: String,
    /// Why it failed, if it did.
    pub error: Option<String>,
}

/// An incident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incident {
    /// Start (RFC 3339).
    pub start: String,
    /// End (RFC 3339), if it ended.
    pub end: Option<String>,
    /// Duration in seconds, for incidents with an end.
    pub duration_s: Option<u64>,
    /// Event kind that started it.
    pub kind: String,
    /// `warning` or `error`.
    pub level: String,
    /// Run, if about one.
    pub run: Option<String>,
    /// File, if about one.
    pub file: Option<String>,
    /// What happened.
    pub message: String,
    /// `resolved`, `retried`, `open` or `failed`.
    pub outcome: String,
}

/// One start of the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Start (RFC 3339).
    pub started: String,
    /// Last record of this start (RFC 3339).
    pub last_record: String,
    /// Version, from the event or the log.
    pub version: Option<String>,
    /// Process id.
    pub pid: Option<u64>,
    /// Seconds from the last record to the next start, if there was one.
    pub restart_after_s: Option<u64>,
}

/// One cleanup action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupRow {
    /// Time (RFC 3339).
    pub ts: String,
    /// Run.
    pub run: Option<String>,
    /// `deleted`, `refused` or `stopped`.
    pub outcome: String,
    /// Files deleted.
    pub files: Option<u64>,
    /// Bytes deleted.
    pub bytes: Option<u64>,
    /// What happened.
    pub message: String,
}

/// An event in the appendix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRow {
    /// Time.
    pub ts: String,
    /// Kind.
    pub kind: String,
    /// Level.
    pub level: String,
    /// Run.
    pub run: Option<String>,
    /// Message.
    pub message: String,
}

/// Everything a status record needs besides the records.
#[derive(Debug, Clone)]
pub struct StatusInputs {
    /// The state folder.
    pub state_dir: PathBuf,
    /// The log file, if any.
    pub log: Option<PathBuf>,
    /// The period.
    pub period: Period,
    /// Read `VERIFIED.json` of each completed run on the share.
    pub check_destination: bool,
    /// Fill [`StatusRecord::events`].
    pub appendix: bool,
    /// Report time (seconds since the epoch).
    pub generated_at: u64,
    /// Host name.
    pub host: String,
    /// Site name.
    pub site: Option<String>,
    /// Report id prefix, e.g. `OOF`.
    pub prefix: String,
    /// Target id.
    pub target_id: Option<String>,
    /// Destination root.
    pub destination: Option<String>,
    /// Sources.
    pub sources: Vec<SourceOut>,
}

#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
struct RunAcc {
    discovered: Option<u64>,
    completed: Option<u64>,
    completed_in_period: bool,
    reopened_after_complete: bool,
    reopened_in_period: bool,
    failed: Option<String>,
    deleted: Option<u64>,
    files_in_period: u64,
    bytes_in_period: u64,
    files: Option<u64>,
    bytes: Option<u64>,
    manifest: Option<String>,
    destination: Option<String>,
    active: bool,
}

#[derive(Debug)]
struct Open {
    start: u64,
    ev: Event,
}

fn u(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(Value::as_u64)
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_owned)
}

/// Flow cell and position from a MinKNOW run folder name
/// (`20261017_1158_1A_PAW12345_abcdef12`).
#[must_use]
pub fn flow_cell_and_position(run: &str) -> (Option<String>, Option<String>) {
    let leaf = run.rsplit('/').next().unwrap_or(run);
    let parts: Vec<&str> = leaf.split('_').collect();
    if parts.len() >= 5 && parts[0].len() == 8 && parts[0].bytes().all(|b| b.is_ascii_digit()) {
        (Some(parts[3].to_owned()), Some(parts[2].to_owned()))
    } else {
        (None, None)
    }
}

/// The pairs of events that open and close a condition.
fn closes(kind: Kind) -> Option<Kind> {
    Some(match kind {
        Kind::OutageEnded => Kind::OutageStarted,
        Kind::CapacityOk => Kind::CapacityLow,
        Kind::SourceSpaceOk => Kind::SourceSpaceLow,
        Kind::RecordsSpaceOk => Kind::RecordsSpaceLow,
        _ => return None,
    })
}

fn opens(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::OutageStarted | Kind::CapacityLow | Kind::SourceSpaceLow | Kind::RecordsSpaceLow
    )
}

fn single(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::GuardFailed
            | Kind::WorkerStalled
            | Kind::IntegrityMismatch
            | Kind::FileFailed
            | Kind::RunFailed
            | Kind::RunReopened
            | Kind::DeletionRefused
            | Kind::DeletionStopped
    )
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Build the status record of a period.
///
/// Implements: SRS-RPT-001, SRS-RPT-002, SRS-RPT-005
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build(fs: &dyn Fs, inp: &StatusInputs) -> StatusRecord {
    let p = inp.period;
    let mut runs: BTreeMap<String, RunAcc> = BTreeMap::new();
    let mut open: BTreeMap<(String, String), Open> = BTreeMap::new();
    let mut incidents: Vec<(u64, Incident)> = Vec::new();
    let mut sessions: Vec<(u64, u64, Option<String>, Option<u64>)> = Vec::new();
    let mut cleanup: Vec<CleanupRow> = Vec::new();
    let mut events = Vec::new();
    let mut daily: BTreeMap<u64, Day> = BTreeMap::new();
    let mut t = Totals::default();
    let first_day = p.from / 86_400;
    for d in first_day..=(p.to - 1) / 86_400 {
        daily.insert(
            d,
            Day {
                day: time::day(d * 86_400),
                files: 0,
                bytes: 0,
                outage_s: 0,
            },
        );
    }
    let mut outages: Vec<(u64, u64)> = Vec::new();

    let ev_sum = history::read_events(fs, &inp.state_dir, |ev, ts| {
        // Sessions span the whole history so the one running at the start
        // of the period is known.
        if ev.kind == Kind::Started {
            sessions.push((ts, ts, s(&ev.data, "version"), u(&ev.data, "pid")));
        } else if let Some(last) = sessions.last_mut() {
            last.1 = ts;
        }
        if ts >= p.to {
            return;
        }
        let inside = p.contains(ts);
        if inside && inp.appendix && ev.kind != Kind::FileCommitted {
            events.push(EventRow {
                ts: ev.ts.clone(),
                kind: ev.kind.as_str(),
                level: ev.level.clone(),
                run: ev.run.clone(),
                message: ev.message.clone(),
            });
        }
        if let Some(run) = &ev.run {
            if !matches!(
                ev.kind,
                Kind::DeletionDryRun | Kind::RunRetired | Kind::GuardFailed
            ) {
                let r = runs.entry(run.clone()).or_default();
                r.active |= inside;
                match ev.kind {
                    Kind::RunDiscovered => {
                        r.discovered.get_or_insert(ts);
                    }
                    Kind::FileCommitted if inside => {
                        r.files_in_period += 1;
                        let size = u(&ev.data, "size").unwrap_or(0);
                        r.bytes_in_period += size;
                        t.files_committed += 1;
                        t.bytes_committed += size;
                        if let Some(d) = daily.get_mut(&(ts / 86_400)) {
                            d.files += 1;
                            d.bytes += size;
                        }
                    }
                    Kind::RunComplete => {
                        r.completed = Some(ts);
                        r.completed_in_period |= inside;
                        r.reopened_after_complete = false;
                        r.failed = None;
                        r.files = u(&ev.data, "files");
                        r.bytes = u(&ev.data, "bytes");
                        r.manifest = s(&ev.data, "manifest_sha256");
                        r.destination = s(&ev.data, "destination");
                    }
                    Kind::RunReopened => {
                        r.reopened_after_complete = true;
                        r.reopened_in_period |= inside;
                    }
                    Kind::RunFailed => {
                        r.failed =
                            Some(s(&ev.data, "reason").unwrap_or_else(|| ev.message.clone()));
                    }
                    Kind::RunDeleted => r.deleted = Some(ts),
                    _ => {}
                }
            }
        }
        match ev.kind {
            Kind::RunDeleted | Kind::DeletionRefused | Kind::DeletionStopped if inside => {
                cleanup.push(CleanupRow {
                    ts: ev.ts.clone(),
                    run: ev.run.clone(),
                    outcome: match ev.kind {
                        Kind::RunDeleted => "deleted",
                        Kind::DeletionRefused => "refused",
                        _ => "stopped",
                    }
                    .into(),
                    files: u(&ev.data, "files"),
                    bytes: u(&ev.data, "bytes"),
                    message: ev.message.clone(),
                });
            }
            _ => {}
        }
        let key = |k: Kind| (ev.run.clone().unwrap_or_default(), k.as_str());
        if opens(ev.kind) {
            open.entry(key(ev.kind)).or_insert(Open {
                start: ts,
                ev: ev.clone(),
            });
        } else if let Some(k) = closes(ev.kind) {
            if let Some(o) = open.remove(&key(k)) {
                let dur = if k == Kind::OutageStarted {
                    u(&ev.data, "duration_s").unwrap_or(ts - o.start)
                } else {
                    ts - o.start
                };
                let start = ts.saturating_sub(dur).min(o.start);
                if k == Kind::OutageStarted {
                    outages.push((start, ts));
                }
                if ts >= p.from {
                    incidents.push((
                        start,
                        Incident {
                            start: time::rfc3339(start),
                            end: Some(ev.ts.clone()),
                            duration_s: Some(ts - start),
                            kind: o.ev.kind.as_str(),
                            level: o.ev.level.clone(),
                            run: o.ev.run.clone(),
                            file: o.ev.file.clone(),
                            message: format!("{} {}", o.ev.message, ev.message),
                            outcome: "resolved".into(),
                        },
                    ));
                }
            }
        } else if single(ev.kind) && inside {
            incidents.push((
                ts,
                Incident {
                    start: ev.ts.clone(),
                    end: None,
                    duration_s: None,
                    kind: ev.kind.as_str(),
                    level: ev.level.clone(),
                    run: ev.run.clone(),
                    file: ev.file.clone(),
                    message: ev.message.clone(),
                    outcome: match ev.kind {
                        Kind::IntegrityMismatch => "retried",
                        Kind::FileFailed | Kind::RunFailed => "failed",
                        Kind::DeletionRefused | Kind::DeletionStopped => "open",
                        _ => "resolved",
                    }
                    .into(),
                },
            ));
            match ev.kind {
                Kind::IntegrityMismatch => t.integrity_mismatches += 1,
                Kind::FileFailed => t.files_failed += 1,
                Kind::WorkerStalled => t.stalls += 1,
                Kind::DeletionRefused => t.deletion_refused += 1,
                _ => {}
            }
        }
    });
    // Conditions still open at the end of the period.
    for o in open.into_values() {
        if o.ev.kind == Kind::OutageStarted {
            outages.push((o.start, p.to));
        }
        incidents.push((
            o.start,
            Incident {
                start: time::rfc3339(o.start),
                end: None,
                duration_s: Some(p.to.min(inp.generated_at.max(o.start)) - o.start),
                kind: o.ev.kind.as_str(),
                level: o.ev.level.clone(),
                run: o.ev.run.clone(),
                file: o.ev.file.clone(),
                message: o.ev.message.clone(),
                outcome: "open".into(),
            },
        ));
    }
    for &(a, b) in &outages {
        if p.overlap(a, b) == 0 && !(a == b && p.contains(a)) {
            continue;
        }
        t.outages += 1;
        t.outage_s += p.overlap(a, b);
        for (d, day) in &mut daily {
            let ds = d * 86_400;
            day.outage_s += p.overlap(a.max(ds), b.min(ds + 86_400).max(a.max(ds)));
        }
    }
    #[allow(clippy::cast_precision_loss)]
    {
        let pct = 100.0 * (1.0 - t.outage_s as f64 / p.secs() as f64);
        t.availability_pct = (pct * 100.0).round() / 100.0;
    }
    incidents.sort_by_key(|(start, _)| *start);
    // A failed run that later completed in the period is resolved.
    for (_, i) in &mut incidents {
        if i.kind == "run_failed" {
            if let Some(r) = i.run.as_ref().and_then(|r| runs.get(r)) {
                if r.failed.is_none() && r.completed.is_some() {
                    i.outcome = "resolved".into();
                }
            }
        }
        if i.kind == "run_reopened" {
            if let Some(r) = i.run.as_ref().and_then(|r| runs.get(r)) {
                i.outcome = if r.reopened_after_complete {
                    "open".into()
                } else {
                    "resolved".into()
                };
            }
        }
    }

    // Runs.
    let mut unverified = 0;
    let mut rows = Vec::new();
    for (name, r) in &runs {
        if !r.active {
            continue;
        }
        t.runs_active += 1;
        let (flow_cell, position) = flow_cell_and_position(name);
        let mut status = if r.deleted.is_some() {
            "deleted"
        } else if r.failed.is_some() {
            "failed"
        } else if r.reopened_after_complete {
            "reopened"
        } else if r.completed.is_some() {
            "complete"
        } else {
            "in_progress"
        };
        if status == "complete" && inp.check_destination {
            if verified(fs, r) {
                status = "verified";
            } else {
                unverified += 1;
            }
        }
        if r.completed_in_period {
            t.runs_completed += 1;
        }
        if r.reopened_in_period {
            t.runs_reopened += 1;
        }
        if status == "failed" {
            t.runs_failed += 1;
        }
        if r.deleted.is_some_and(|d| p.contains(d)) {
            t.runs_deleted += 1;
        }
        rows.push(RunRow {
            name: name.clone(),
            flow_cell,
            position,
            discovered_at: r.discovered.map(time::rfc3339),
            completed_at: r.completed.map(time::rfc3339),
            duration_s: r
                .discovered
                .zip(r.completed)
                .map(|(a, b)| b.saturating_sub(a)),
            files_in_period: r.files_in_period,
            bytes_in_period: r.bytes_in_period,
            files: r.files,
            bytes: r.bytes,
            manifest_sha256: r.manifest.clone(),
            destination: r.destination.clone(),
            status: status.into(),
            error: r.failed.clone(),
        });
    }
    rows.sort_by(|a, b| {
        (a.completed_at.is_none(), &a.completed_at, &a.name).cmp(&(
            b.completed_at.is_none(),
            &b.completed_at,
            &b.name,
        ))
    });
    if inp.check_destination {
        t.unverified_runs = Some(unverified);
    }

    // The log: versions, problems in the period.
    let mut log_issues = Vec::new();
    let mut log_starts: Vec<(u64, String)> = Vec::new();
    let mut inputs = vec![ev_sum];
    if let Some(log) = &inp.log {
        let sum = history::read_log(fs, log, |ts, l| {
            if let Some(v) = l
                .message
                .strip_prefix("ont-offload ")
                .and_then(|m| m.strip_suffix(" starting: watch"))
            {
                log_starts.push((ts, v.to_owned()));
            }
            if p.contains(ts) && matches!(l.level.as_str(), "ERROR" | "WARN") {
                log_issues.push((ts, l.clone()));
            }
        });
        inputs.push(sum);
    }
    log_issues.sort_by_key(|(ts, _)| *ts);
    log_starts.sort();

    // Sessions overlapping the period.
    let mut out_sessions = Vec::new();
    for (i, &(start, last, ref version, pid)) in sessions.iter().enumerate() {
        let next = sessions.get(i + 1).map(|n| n.0);
        if start >= p.to || last < p.from {
            continue;
        }
        let version = version.clone().or_else(|| {
            log_starts
                .iter()
                .rev()
                .find(|(t, _)| *t <= start + 5 && start.saturating_sub(*t) <= 120)
                .map(|(_, v)| v.clone())
        });
        out_sessions.push(Session {
            started: time::rfc3339(start),
            last_record: time::rfc3339(last),
            version,
            pid,
            restart_after_s: next.map(|n| n.saturating_sub(last)),
        });
    }
    let mut versions: Vec<String> = out_sessions
        .iter()
        .filter_map(|s| s.version.clone())
        .collect();
    versions.dedup();
    versions.sort();
    versions.dedup();

    let verdict = judge(&t, &rows, &incidents, &inputs, p);
    let id_seed = format!("{}|{}|{}|{}", inp.host, p.from, p.to, inp.generated_at);
    StatusRecord {
        schema: STATUS_SCHEMA.into(),
        schema_version: 1,
        report_id: format!(
            "{}-SR-{}-{}",
            inp.prefix,
            time::compact_day(p.to - 1),
            &crate::journal::sha256_hex(id_seed.as_bytes())[..6]
        ),
        generated_at: time::rfc3339(inp.generated_at),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        host: inp.host.clone(),
        site: inp.site.clone(),
        period: PeriodOut {
            from: time::rfc3339(p.from),
            to: time::rfc3339(p.to),
            last_day: time::day(p.to - 1),
            days: p.days(),
        },
        target_id: inp.target_id.clone(),
        destination: inp.destination.clone(),
        sources: inp.sources.clone(),
        versions,
        verdict,
        totals: t,
        daily: daily.into_values().collect(),
        runs: rows,
        incidents: incidents.into_iter().map(|(_, i)| i).collect(),
        sessions: out_sessions,
        cleanup,
        inputs,
        log_issues: log_issues.into_iter().map(|(_, l)| l).collect(),
        events,
    }
}

/// The run's `VERIFIED.json` on the share names its manifest digest.
fn verified(fs: &dyn Fs, r: &RunAcc) -> bool {
    let (Some(dest), Some(manifest)) = (&r.destination, &r.manifest) else {
        return false;
    };
    fs.read(&Path::new(dest).join(VERIFIED))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| {
            v.get("result").and_then(Value::as_str) == Some("ok")
                && v.get("manifest_sha256").and_then(Value::as_str) == Some(manifest)
        })
}

/// The verdict rules.
///
/// *Action required*: a run failed and is still failed, a file failed for
/// good, a condition (outage, low space) is still open at the end of the
/// period, or the records have gaps or do not reach back to the start of
/// the period. *Attention*: outages, integrity mismatches (retried),
/// reopened runs, stalls, guard failures, refused or stopped deletions,
/// runs on the share without a receipt, failed runs that recovered.
/// Otherwise *ok*.
///
/// Implements: SRS-RPT-001, SRS-RPT-002
#[allow(clippy::too_many_lines)]
fn judge(
    t: &Totals,
    rows: &[RunRow],
    incidents: &[(u64, Incident)],
    inputs: &[InputSummary],
    p: Period,
) -> Verdict {
    let mut action = Vec::new();
    let mut attention = Vec::new();
    if t.runs_failed > 0 {
        let names: Vec<&str> = rows
            .iter()
            .filter(|r| r.status == "failed")
            .map(|r| r.name.as_str())
            .collect();
        action.push(format!(
            "{} failed and not offloaded: {}",
            plural(t.runs_failed, "run", "runs"),
            names.join(", ")
        ));
    }
    if t.files_failed > 0 {
        action.push(format!(
            "{} failed for good",
            plural(t.files_failed, "file", "files")
        ));
    }
    let open: Vec<&Incident> = incidents
        .iter()
        .map(|(_, i)| i)
        .filter(|i| {
            i.outcome == "open"
                && matches!(
                    i.kind.as_str(),
                    "outage_started" | "capacity_low" | "source_space_low" | "records_space_low"
                )
        })
        .collect();
    for i in &open {
        action.push(format!(
            "{} since {} is still open",
            i.kind.replace('_', " "),
            i.start
        ));
    }
    for inp in inputs {
        for g in &inp.gaps {
            action.push(format!("{} record: {g}", inp.record));
        }
    }
    if let Some(first) = inputs
        .first()
        .and_then(|e| e.first.as_deref())
        .and_then(time::parse)
    {
        if first > p.from && first < p.to {
            attention.push(format!(
                "the event records start at {}, after the start of the period",
                time::rfc3339(first)
            ));
        }
    } else {
        action.push("no events in the records at all".into());
    }
    if t.outages > 0 {
        attention.push(format!(
            "{} with {} min down in total",
            plural(t.outages, "outage", "outages"),
            t.outage_s.div_ceil(60)
        ));
    }
    if t.integrity_mismatches > 0 {
        attention.push(format!(
            "{} (each retried)",
            plural(
                t.integrity_mismatches,
                "integrity mismatch",
                "integrity mismatches"
            )
        ));
    }
    if t.runs_reopened > 0 {
        attention.push(format!(
            "{} after a change at the source",
            plural(t.runs_reopened, "run reopened", "runs reopened")
        ));
    }
    if t.stalls > 0 {
        attention.push(plural(t.stalls, "worker stall", "worker stalls"));
    }
    let guard = incidents
        .iter()
        .filter(|(_, i)| i.kind == "guard_failed")
        .count() as u64;
    if guard > 0 {
        attention.push(plural(
            guard,
            "destination guard failure",
            "destination guard failures",
        ));
    }
    if t.deletion_refused > 0 {
        attention.push(format!(
            "{} refused by a gate",
            plural(t.deletion_refused, "deletion", "deletions")
        ));
    }
    let recovered = incidents
        .iter()
        .filter(|(_, i)| i.kind == "run_failed" && i.outcome == "resolved")
        .count() as u64;
    if recovered > 0 {
        attention.push(format!(
            "{} failed and then completed",
            plural(recovered, "run", "runs")
        ));
    }
    if let Some(n) = t.unverified_runs.filter(|n| *n > 0) {
        attention.push(format!(
            "{} complete on the share without a matching VERIFIED.json",
            plural(n, "run", "runs")
        ));
    }
    let summary = format!(
        "{} completed, {} of data committed in {} files.",
        plural(t.runs_completed, "run", "runs"),
        crate::config::units::Bytes(t.bytes_committed),
        t.files_committed
    );
    let review = if attention.is_empty() {
        String::new()
    } else {
        format!(
            ", {} to review",
            plural(attention.len() as u64, "point", "points")
        )
    };
    let (status, headline) = if !action.is_empty() {
        (
            verdict::ACTION,
            format!(
                "{} action{review}. {summary}",
                plural(action.len() as u64, "issue needs", "issues need")
            ),
        )
    } else if !attention.is_empty() {
        (
            verdict::ATTENTION,
            format!("Nothing is open{review}. {summary}"),
        )
    } else {
        (
            verdict::OK,
            format!("Every run offloaded without incident. {summary}"),
        )
    };
    Verdict {
        status: status.into(),
        headline,
        reasons: action.into_iter().chain(attention).collect(),
    }
}

#[cfg(test)]
mod tests;
