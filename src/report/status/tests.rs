// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the status record on hand-written event histories.

use std::path::Path;

use serde_json::json;

use super::*;
use crate::fsio::RealFs;
use crate::report::time::{parse, period};

/// Writes event lines with chosen times, as the daemon would.
struct History {
    dir: tempfile::TempDir,
    seq: u64,
    lines: Vec<String>,
}

impl History {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            seq: 0,
            lines: Vec::new(),
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn ev(&mut self, ts: &str, kind: Kind, run: Option<&str>, data: Value) -> &mut Self {
        if kind == Kind::Started {
            self.seq = 0;
        }
        self.seq += 1;
        let level = if kind.alerts() { "error" } else { "info" };
        let ev = json!({
            "schema": "ont-offload/event/v1", "schema_version": 1, "seq": self.seq,
            "ts": ts, "kind": kind.as_str(), "level": level, "run": run,
            "file": null, "message": format!("{} {}", kind.as_str(), run.unwrap_or("")),
            "data": data,
        });
        self.lines.push(ev.to_string());
        self
    }

    fn skip(&mut self, n: u64) -> &mut Self {
        self.seq += n;
        self
    }

    fn state(&self) -> &Path {
        self.dir.path()
    }

    /// Write the history: all but the last 3 lines into the archive, the
    /// rest into events.jsonl, so reading crosses files.
    fn write(&self) {
        let split = self.lines.len().saturating_sub(3);
        let arch = self.state().join("events");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::write(
            arch.join("events-20260901T000000Z.jsonl"),
            self.lines[..split].iter().fold(String::new(), |mut a, l| {
                a.push_str(l);
                a.push('\n');
                a
            }),
        )
        .unwrap();
        std::fs::write(
            self.state().join("events.jsonl"),
            self.lines[split..].iter().fold(String::new(), |mut a, l| {
                a.push_str(l);
                a.push('\n');
                a
            }),
        )
        .unwrap();
    }

    fn build(&self, check_destination: bool, appendix: bool) -> StatusRecord {
        self.write();
        let inputs = StatusInputs {
            state_dir: self.state().to_path_buf(),
            log: None,
            period: period(None, None, Some("2026-10"), None, 0).unwrap(),
            check_destination,
            appendix,
            generated_at: parse("2026-11-01T08:00:00Z").unwrap(),
            host: "p2i-seq01".into(),
            site: Some("Lab".into()),
            prefix: "OOF".into(),
            target_id: Some("lab-nas".into()),
            destination: Some("/mnt/nas".into()),
            sources: Vec::new(),
        };
        build(&RealFs, &inputs)
    }
}

const RUN_A: &str = "exp/s1/20261001_0100_1A_PAW11111_aaaaaaaa";
const RUN_B: &str = "exp/s2/20261003_0100_2B_PAW22222_bbbbbbbb";

fn good_month(h: &mut History, dest: &Path) {
    h.ev(
        "2026-09-30T10:00:00Z",
        Kind::Started,
        None,
        json!({"pid": 10, "version": "0.8.0"}),
    )
    .ev(
        "2026-09-30T11:00:00Z",
        Kind::RunDiscovered,
        Some(RUN_A),
        json!({}),
    )
    .ev(
        "2026-09-30T23:00:00Z",
        Kind::FileCommitted,
        Some(RUN_A),
        json!({"size": 7}),
    )
    .ev(
        "2026-10-01T01:00:00Z",
        Kind::FileCommitted,
        Some(RUN_A),
        json!({"size": 1000}),
    )
    .ev(
        "2026-10-01T02:00:00Z",
        Kind::FileCommitted,
        Some(RUN_A),
        json!({"size": 2000}),
    )
    .ev(
        "2026-10-01T03:00:00Z",
        Kind::RunComplete,
        Some(RUN_A),
        json!({"files": 3, "bytes": 3007, "manifest_sha256": "ab".repeat(32),
                   "destination": dest.to_string_lossy()}),
    )
    .ev(
        "2026-10-02T00:00:00Z",
        Kind::OutageStarted,
        None,
        json!({"reason": "ENOTCONN", "backlog_files": 4}),
    )
    .ev(
        "2026-10-02T00:30:00Z",
        Kind::OutageEnded,
        None,
        json!({"duration_s": 1800, "backlog_files": 4}),
    )
    .ev(
        "2026-10-20T08:00:00Z",
        Kind::Started,
        None,
        json!({"pid": 20, "version": "0.9.0"}),
    )
    .ev(
        "2026-10-20T09:00:00Z",
        Kind::RunDeleted,
        Some(RUN_A),
        json!({"files": 3, "bytes": 3007}),
    )
    .ev(
        "2026-11-02T00:00:00Z",
        Kind::FileCommitted,
        Some(RUN_A),
        json!({"size": 99}),
    );
}

/// Verifies: SRS-RPT-001
#[test]
fn a_month_with_an_outage_needs_attention_and_counts_only_the_period() {
    let mut h = History::new();
    let dest = h.state().join("dest");
    good_month(&mut h, &dest);
    let r = h.build(false, false);
    assert_eq!(r.schema, STATUS_SCHEMA);
    assert!(
        r.report_id.starts_with("OOF-SR-20261031-"),
        "{}",
        r.report_id
    );
    assert_eq!(r.period.last_day, "2026-10-31");
    assert_eq!(r.verdict.status, verdict::ATTENTION, "{:?}", r.verdict);
    assert!(
        r.verdict
            .reasons
            .iter()
            .any(|x| x.contains("1 outage with 30 min"))
    );
    // Commits before and after the period are not counted.
    assert_eq!(r.totals.files_committed, 2);
    assert_eq!(r.totals.bytes_committed, 3000);
    assert_eq!(r.totals.runs_completed, 1);
    assert_eq!(r.totals.runs_deleted, 1);
    assert_eq!(r.totals.outages, 1);
    assert_eq!(r.totals.outage_s, 1800);
    assert!(
        (r.totals.availability_pct - 99.93).abs() < 0.001,
        "{}",
        r.totals.availability_pct
    );
    assert_eq!(r.daily.len(), 31);
    assert_eq!(r.daily[0].bytes, 3000);
    assert_eq!(r.daily[1].outage_s, 1800);
    let run = &r.runs[0];
    assert_eq!(run.status, "deleted");
    assert_eq!(run.flow_cell.as_deref(), Some("PAW11111"));
    assert_eq!(run.position.as_deref(), Some("1A"));
    assert_eq!(run.duration_s, Some(16 * 3600));
    assert_eq!(run.files, Some(3));
    let outage = &r.incidents[0];
    assert_eq!(
        (outage.kind.as_str(), outage.outcome.as_str()),
        ("outage_started", "resolved")
    );
    assert_eq!(outage.duration_s, Some(1800));
    assert_eq!(r.cleanup.len(), 1);
    // Both starts overlap the period; the first is followed by a restart.
    assert_eq!(r.sessions.len(), 2);
    assert_eq!(r.sessions[0].version.as_deref(), Some("0.8.0"));
    assert_eq!(r.sessions[0].last_record, "2026-10-02T00:30:00Z");
    assert_eq!(r.sessions[1].restart_after_s, None);
    assert_eq!(r.versions, ["0.8.0", "0.9.0"]);
    assert!(r.inputs[0].gaps.is_empty(), "{:?}", r.inputs[0].gaps);
    assert_eq!(r.inputs[0].files, 2);
    assert!(r.events.is_empty());
    // The record matches its schema and round-trips through JSON.
    let schema = crate::state::schema_check::load("status-report-v1.json");
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(
        crate::state::schema_check::validate(&schema, &v),
        Vec::<String>::new()
    );
    let back: StatusRecord = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
    assert_eq!(back, r);
}

/// Verifies: SRS-RPT-001, SRS-RPT-002
#[test]
fn failures_open_conditions_and_record_gaps_require_action() {
    let mut h = History::new();
    let dest = h.state().join("dest");
    good_month(&mut h, &dest);
    h.ev(
        "2026-10-21T00:00:00Z",
        Kind::RunDiscovered,
        Some(RUN_B),
        json!({}),
    )
    .ev(
        "2026-10-21T01:00:00Z",
        Kind::IntegrityMismatch,
        Some(RUN_B),
        json!({"reason": "x"}),
    )
    .ev(
        "2026-10-21T02:00:00Z",
        Kind::FileFailed,
        Some(RUN_B),
        json!({"reason": "EIO"}),
    )
    .ev(
        "2026-10-21T03:00:00Z",
        Kind::RunFailed,
        Some(RUN_B),
        json!({"reason": "claim refused"}),
    )
    .skip(2)
    .ev(
        "2026-10-30T00:00:00Z",
        Kind::OutageStarted,
        None,
        json!({"reason": "ESTALE"}),
    );
    let r = h.build(false, true);
    assert_eq!(r.verdict.status, verdict::ACTION, "{:?}", r.verdict);
    let reasons = r.verdict.reasons.join("\n");
    assert!(
        reasons.contains("1 run failed and not offloaded: ") && reasons.contains(RUN_B),
        "{reasons}"
    );
    assert!(reasons.contains("1 file failed for good"), "{reasons}");
    assert!(
        reasons.contains("outage started since 2026-10-30T00:00:00Z is still open"),
        "{reasons}"
    );
    assert!(reasons.contains("events 8 to 9 missing"), "{reasons}");
    assert!(
        r.verdict.headline.starts_with("4 issues need action, "),
        "{}",
        r.verdict.headline
    );
    // The open outage runs to the end of the period.
    assert_eq!(r.totals.outages, 2);
    assert_eq!(r.totals.outage_s, 1800 + 2 * 86_400);
    assert_eq!(r.daily[30].outage_s, 86_400);
    let failed = r.runs.iter().find(|x| x.name == RUN_B).unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.error.as_deref(), Some("claim refused"));
    assert_eq!(r.totals.integrity_mismatches, 1);
    let mm = r
        .incidents
        .iter()
        .find(|i| i.kind == "integrity_mismatch")
        .unwrap();
    assert_eq!(mm.outcome, "retried");
    // The appendix lists every event of the period but per-file commits.
    assert!(!r.events.is_empty());
    assert!(r.events.iter().all(|e| e.kind != "file_committed"));
    assert!(r.events.iter().all(|e| e.ts.as_str() >= "2026-10-01"));
    let schema = crate::state::schema_check::load("status-report-v1.json");
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(
        crate::state::schema_check::validate(&schema, &v),
        Vec::<String>::new()
    );
}

/// Verifies: SRS-RPT-001
#[test]
fn a_quiet_month_is_ok_and_receipts_on_the_share_mark_runs_verified() {
    let mut h = History::new();
    let dest = h.state().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    h.ev(
        "2026-09-01T00:00:00Z",
        Kind::Started,
        None,
        json!({"pid": 1, "version": "0.9.0"}),
    )
    .ev(
        "2026-10-05T00:00:00Z",
        Kind::RunDiscovered,
        Some(RUN_A),
        json!({}),
    )
    .ev(
        "2026-10-05T01:00:00Z",
        Kind::FileCommitted,
        Some(RUN_A),
        json!({"size": 10}),
    )
    .ev(
        "2026-10-05T02:00:00Z",
        Kind::RunComplete,
        Some(RUN_A),
        json!({"files": 1, "bytes": 10, "manifest_sha256": "cd".repeat(32),
                   "destination": dest.to_string_lossy()}),
    );
    // Without a receipt the run is complete but not verified.
    let r = h.build(true, false);
    assert_eq!(r.totals.unverified_runs, Some(1));
    assert_eq!(r.verdict.status, verdict::ATTENTION);
    std::fs::write(
        dest.join(VERIFIED),
        json!({"result": "ok", "manifest_sha256": "cd".repeat(32)}).to_string(),
    )
    .unwrap();
    let r = h.build(true, false);
    assert_eq!(r.runs[0].status, "verified");
    assert_eq!(r.totals.unverified_runs, Some(0));
    assert_eq!(r.verdict.status, verdict::OK, "{:?}", r.verdict);
    assert!(
        r.verdict
            .headline
            .starts_with("Every run offloaded without incident.")
    );
    assert!((r.totals.availability_pct - 100.0).abs() < f64::EPSILON);
}

/// Verifies: SRS-RPT-002
#[test]
fn missing_records_are_never_ok() {
    let h = History::new();
    let inputs = StatusInputs {
        state_dir: h.state().join("nothing"),
        log: Some(h.state().join("no.log")),
        period: period(None, None, Some("2026-10"), None, 0).unwrap(),
        check_destination: false,
        appendix: false,
        generated_at: 0,
        host: "h".into(),
        site: None,
        prefix: "OOF".into(),
        target_id: None,
        destination: None,
        sources: Vec::new(),
    };
    let r = build(&RealFs, &inputs);
    assert_eq!(r.verdict.status, verdict::ACTION);
    let reasons = r.verdict.reasons.join("\n");
    assert!(reasons.contains("no event files found"), "{reasons}");
    assert!(reasons.contains("no log files found"), "{reasons}");
    assert!(
        reasons.contains("no events in the records at all"),
        "{reasons}"
    );
}

/// Verifies: SRS-RPT-001
#[test]
fn minknow_folder_names_give_flow_cell_and_position() {
    assert_eq!(
        flow_cell_and_position("minknow/exp/s/20261017_1158_1A_PAW12345_abcdef12"),
        (Some("PAW12345".into()), Some("1A".into()))
    );
    assert_eq!(flow_cell_and_position("transfer/dataset-7"), (None, None));
}
