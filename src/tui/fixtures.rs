// SPDX-License-Identifier: AGPL-3.0-or-later
//! Test fixtures: schema-valid snapshots and events for the scenarios the
//! dashboard must show (normal, outage, stalled worker, failed run, refused
//! deletion), written to a state folder as the daemon would.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::cleanup::{Evidence, Outcome, REPORTS, Report, RunReport};
use crate::state::{
    Alert, CleanupState, Destination, EVENT_SCHEMA, EVENTS_FILE, Event, FailedFile, Kind,
    OutageState, Rate, RunState, SCHEMA_VERSION, STATE_FILE, STATE_SCHEMA, Snapshot, Totals,
    WorkerState, Workers,
};

/// 2026-11-14T10:00:00Z, the fixed "now" of the fixtures.
pub fn t0() -> SystemTime {
    humantime::parse_rfc3339("2026-11-14T10:00:00Z").expect("time")
}

/// RFC 3339 of `t0() + secs` (negative: before).
pub fn at(secs: i64) -> String {
    let t = if secs >= 0 {
        t0() + Duration::from_secs(secs.unsigned_abs())
    } else {
        t0() - Duration::from_secs(secs.unsigned_abs())
    };
    humantime::format_rfc3339_seconds(t).to_string()
}

fn run(name: &str, phase: &str) -> RunState {
    RunState {
        run_id: format!("id-{name}"),
        name: name.into(),
        source_name: None,
        source: format!("/data/{name}"),
        destination: format!("/mnt/nas/ont/{name}"),
        phase: phase.into(),
        files: 812,
        bytes: 420_000_000_000,
        committed_files: 790,
        backlog_files: 0,
        backlog_bytes: 0,
        copying: 2,
        failed: vec![],
        integrity_mismatches: 0,
        skipped: vec![],
        eta_s: Some(900),
        complete_marker: None,
        manifest_sha256: None,
        error: None,
    }
}

/// A healthy daemon written at `updated` with one live and one complete run.
pub fn normal(updated: &str) -> Snapshot {
    let mut done = run("exp2/s2/20261113_PAW00002", "complete");
    done.committed_files = done.files;
    done.copying = 0;
    done.eta_s = None;
    done.complete_marker = Some("final_summary_PAW00002.txt".into());
    done.manifest_sha256 = Some("ab".repeat(32));
    Snapshot {
        schema: STATE_SCHEMA.into(),
        schema_version: SCHEMA_VERSION,
        tool_version: "0.6.0".into(),
        host: "gridion-01".into(),
        pid: 4242,
        started_at: at(-86_400),
        updated_at: updated.into(),
        interval_s: 2,
        status: "running".into(),
        target_id: "lab-share-01".into(),
        source_root: "/data".into(),
        destination: Destination {
            root: "/mnt/nas/ont".into(),
            fstype: Some("nfs4".into()),
            status: "ok".into(),
            guard_ok: true,
            free_bytes: Some(18_210_000_000_000),
            outage: None,
        },
        source_free_bytes: Some(2_100_000_000_000),
        workers: Workers {
            count: 4,
            paused: false,
            busy: vec![WorkerState {
                worker: 0,
                run: "exp1/s1/20261114_PAW00001".into(),
                task: "pod5/batch_0791.pod5".into(),
                bytes: 120_000_000,
                bytes_total: Some(400_000_000),
                idle_s: 0,
                stalled: false,
            }],
        },
        rate: Rate {
            limit_bytes_per_s: Some(100_000_000),
            current_bytes_per_s: 82_000_000,
        },
        totals: Totals::default(),
        runs: vec![run("exp1/s1/20261114_PAW00001", "live"), done],
        alerts: vec![],
        cleanup: None,
    }
}

/// The share is down for over an hour; an outage alert is open.
pub fn outage(updated: &str) -> Snapshot {
    let mut s = normal(updated);
    s.status = "outage".into();
    s.destination.status = "unavailable".into();
    s.destination.guard_ok = false;
    s.destination.outage = Some(OutageState {
        since: at(-4000),
        duration_s: 4000,
        reason: "ENOTCONN".into(),
        level: "alert".into(),
        checks: 12,
        next_check_s: 300,
        recovering: false,
        backlog_files: 1240,
        backlog_bytes: 380_000_000_000,
        stuck_workers: 1,
    });
    s.workers.paused = true;
    s.rate.current_bytes_per_s = 0;
    s.alerts = vec![Alert {
        kind: Kind::OutageAlert,
        level: "error".into(),
        since: at(-400),
        run: None,
        message: "share unavailable for 1h 6m".into(),
    }];
    s
}

/// A worker has made no progress for longer than `stall_after`.
pub fn stalled(updated: &str) -> Snapshot {
    let mut s = normal(updated);
    s.workers.busy[0].idle_s = 900;
    s.workers.busy[0].stalled = true;
    s.alerts = vec![Alert {
        kind: Kind::WorkerStalled,
        level: "warning".into(),
        since: at(-300),
        run: Some("exp1/s1/20261114_PAW00001".into()),
        message: "worker 0 stalled on pod5/batch_0791.pod5 for 15m".into(),
    }];
    s
}

/// A run failed with files that could not be copied.
pub fn failed_run(updated: &str) -> Snapshot {
    let mut s = normal(updated);
    s.runs[0].phase = "failed".into();
    s.runs[0].failed = vec![FailedFile {
        file: "pod5/batch_0001.pod5".into(),
        reason: "EACCES".into(),
    }];
    s.runs[0].error = Some("1 file could not be copied".into());
    s.alerts = vec![Alert {
        kind: Kind::RunFailed,
        level: "error".into(),
        since: at(-60),
        run: Some(s.runs[0].name.clone()),
        message: "run failed: 1 file could not be copied".into(),
    }];
    s
}

/// The last cleanup pass refused a deletion.
pub fn refused(updated: &str) -> Snapshot {
    let mut s = normal(updated);
    s.cleanup = Some(CleanupState {
        mode: "delete".into(),
        gate: "storage-verify".into(),
        running: false,
        last_pass_at: Some(at(-600)),
        next_pass_s: 3000,
        deleted_runs: 1,
        deleted_bytes: 300_000_000_000,
        would_delete: 0,
        held: 0,
        waiting: 1,
        refused: 1,
        stopped: None,
    });
    s.alerts = vec![Alert {
        kind: Kind::DeletionRefused,
        level: "error".into(),
        since: at(-600),
        run: Some("exp0/s0/20261101_PAW00000".into()),
        message: "deletion refused: manifest does not match the source".into(),
    }];
    s
}

/// The cleanup report that goes with [`refused`].
pub fn refused_report() -> Report {
    Report {
        schema: "ont-offload/cleanup-report/v1".into(),
        tool_version: "0.6.0".into(),
        host: "gridion-01".into(),
        started_at: at(-610),
        finished_at: at(-600),
        mode: "delete".into(),
        gate: "storage-verify".into(),
        source_free_bytes: Some(2_100_000_000_000),
        only_when_free_below: None,
        runs: vec![RunReport {
            run_id: "id-0".into(),
            source: "/data/exp0/s0/20261101_PAW00000".into(),
            destination: "/mnt/nas/ont/exp0/s0/20261101_PAW00000".into(),
            outcome: Outcome::Refused,
            reason: "manifest does not match the source".into(),
            files: 10,
            bytes: 10,
            deleted_files: 0,
            kept: vec![],
            evidence: Some(Evidence::default()),
        }],
        stopped: None,
    }
}

/// An event.
pub fn event(seq: u64, ts: &str, kind: Kind, level: &str, message: &str) -> Event {
    Event {
        schema: EVENT_SCHEMA.into(),
        schema_version: SCHEMA_VERSION,
        seq,
        ts: ts.into(),
        kind,
        level: level.into(),
        run: None,
        file: None,
        message: message.into(),
        data: serde_json::json!({}),
    }
}

/// Write `state.json` as the daemon does.
pub fn write_state(dir: &Path, s: &Snapshot) {
    std::fs::write(dir.join(STATE_FILE), serde_json::to_vec(s).expect("json")).expect("write");
}

/// Append events to `events.jsonl`.
pub fn append_events(dir: &Path, events: &[Event]) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(EVENTS_FILE))
        .expect("open");
    for e in events {
        writeln!(f, "{}", serde_json::to_string(e).expect("json")).expect("write");
    }
}

/// Append a cleanup report.
pub fn append_report(dir: &Path, r: &Report) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(REPORTS))
        .expect("open");
    writeln!(f, "{}", serde_json::to_string(r).expect("json")).expect("write");
}
