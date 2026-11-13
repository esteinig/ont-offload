// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the state snapshot, the event stream, the JSON Schemas and
//! `notify_command`.

use std::time::{Duration, SystemTime};

use serde_json::json;

use super::schema_check::{load, validate};
use super::*;
use crate::fsio::RealFs;

fn sample() -> Snapshot {
    Snapshot {
        schema: STATE_SCHEMA.into(),
        schema_version: SCHEMA_VERSION,
        tool_version: "0.4.0".into(),
        host: "gridion-01".into(),
        pid: 42,
        started_at: "2026-11-12T08:00:00Z".into(),
        updated_at: crate::journal::now_rfc3339(),
        interval_s: 2,
        status: "outage".into(),
        target_id: "lab-share-01".into(),
        source_root: "/data".into(),
        destination: Destination {
            root: "/mnt/nas/ont".into(),
            fstype: Some("nfs4".into()),
            status: "unavailable".into(),
            guard_ok: false,
            free_bytes: Some(18_210_000_000_000),
            outage: Some(OutageState {
                since: "2026-11-12T02:10:41Z".into(),
                duration_s: 8100,
                reason: "ENOTCONN".into(),
                level: "alert".into(),
                checks: 41,
                next_check_s: 300,
                recovering: false,
                backlog_files: 1240,
                backlog_bytes: 380_000_000_000,
                stuck_workers: 1,
            }),
        },
        source_free_bytes: Some(2_100_000_000_000),
        workers: Workers {
            count: 4,
            paused: true,
            busy: vec![WorkerState {
                worker: 0,
                run: "exp/s/run".into(),
                task: "pod5/a.pod5".into(),
                bytes: 5,
                bytes_total: Some(10),
                idle_s: 8000,
                stalled: true,
            }],
        },
        rate: Rate {
            limit_bytes_per_s: Some(100_000_000),
            current_bytes_per_s: 0,
        },
        totals: Totals::default(),
        runs: vec![RunState {
            run_id: "abc".into(),
            name: "exp/s/run".into(),
            source: "/data/exp/s/run".into(),
            destination: "/mnt/nas/ont/exp/s/run".into(),
            phase: "live".into(),
            files: 812,
            bytes: 1,
            committed_files: 790,
            backlog_files: 22,
            backlog_bytes: 1,
            copying: 1,
            failed: vec![FailedFile {
                file: "x".into(),
                reason: "EACCES".into(),
            }],
            integrity_mismatches: 1,
            skipped: vec!["link".into()],
            eta_s: None,
            complete_marker: None,
            manifest_sha256: None,
            error: None,
        }],
        alerts: vec![Alert {
            kind: Kind::OutageAlert,
            level: "error".into(),
            since: "2026-11-12T03:10:41Z".into(),
            run: None,
            message: "share unavailable for 1h".into(),
        }],
        cleanup: Some(CleanupState {
            mode: "dry-run".into(),
            gate: "storage-verify".into(),
            running: false,
            last_pass_at: Some("2026-11-12T07:00:00Z".into()),
            next_pass_s: 1200,
            deleted_runs: 0,
            deleted_bytes: 0,
            would_delete: 2,
            held: 0,
            waiting: 3,
            refused: 1,
            stopped: None,
        }),
    }
}

/// Verifies: SRS-STATE-001, SRS-STATE-003
#[test]
fn snapshot_matches_the_schema_and_drift_is_caught() {
    let schema = load("state-v1.json");
    let v = serde_json::to_value(sample()).unwrap();
    assert_eq!(validate(&schema, &v), Vec::<String>::new());
    // A field only in the code, a field only in the schema, a wrong enum.
    let mut extra = v.clone();
    extra["destination"]["mtu"] = json!(1500);
    assert!(
        validate(&schema, &extra)
            .iter()
            .any(|p| p.contains("unexpected mtu"))
    );
    let mut missing = v.clone();
    missing["rate"]
        .as_object_mut()
        .unwrap()
        .remove("current_bytes_per_s");
    assert!(
        validate(&schema, &missing)
            .iter()
            .any(|p| p.contains("missing current_bytes_per_s"))
    );
    let mut bad = v;
    bad["runs"][0]["phase"] = json!("sleeping");
    assert!(!validate(&schema, &bad).is_empty());
    // And it reads back.
    let back: Snapshot = serde_json::from_str(&serde_json::to_string(&sample()).unwrap()).unwrap();
    assert_eq!(back.destination, sample().destination);
}

/// Verifies: SRS-STATE-002
#[test]
fn every_event_kind_is_in_the_schema_and_nothing_else() {
    let schema = load("event-v1.json");
    let listed: Vec<String> = schema["properties"]["kind"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_owned())
        .collect();
    let ours: Vec<String> = Kind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(listed, ours);
    let state = load("state-v1.json");
    assert_eq!(
        state["$defs"]["kind"]["enum"],
        schema["properties"]["kind"]["enum"]
    );
    // The design plan's names.
    for k in [
        "run_discovered",
        "run_state_changed",
        "file_committed",
        "file_failed",
        "integrity_mismatch",
        "paused",
        "resumed",
        "guard_failed",
        "capacity_low",
        "outage_started",
        "outage_alert",
        "outage_reminder",
        "outage_ended",
        "worker_stalled",
        "run_deleted",
        "deletion_dry_run",
        "deletion_refused",
        "deletion_stopped",
    ] {
        assert!(ours.iter().any(|o| o == k), "{k}");
    }
}

/// Verifies: SRS-STATE-002
#[test]
fn events_are_schema_valid_and_rotate_by_size() {
    let dir = tempfile::tempdir().unwrap();
    let schema = load("event-v1.json");
    let mut w = StateWriter::new(&RealFs, dir.path(), Duration::from_secs(2), 4096, None);
    for i in 0..100 {
        let ev = w.event(
            &RealFs,
            Kind::FileCommitted,
            log::Level::Info,
            Some("exp/s/run"),
            Some(&format!("pod5/f{i}\u{7}.pod5")),
            "committed",
            json!({"size": i}),
        );
        assert_eq!(ev.seq, i + 1);
        assert_eq!(
            ev.file.as_deref(),
            Some(format!("pod5/f{i}\\u{{7}}.pod5").as_str())
        );
    }
    let cur = std::fs::read_to_string(dir.path().join(EVENTS_FILE)).unwrap();
    let old = std::fs::read_to_string(dir.path().join(format!("{EVENTS_FILE}.1"))).unwrap();
    assert!(
        cur.len() <= 4096 && old.len() <= 4096,
        "{} {}",
        cur.len(),
        old.len()
    );
    for line in old.lines().chain(cur.lines()) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(validate(&schema, &v), Vec::<String>::new(), "{line}");
    }
    // The newest events are kept, in order.
    let last: Event = serde_json::from_str(cur.lines().last().unwrap()).unwrap();
    assert_eq!(last.seq, 100);
    // A restart continues the size count from the file.
    let w2 = StateWriter::new(&RealFs, dir.path(), Duration::from_secs(2), 4096, None);
    assert_eq!(w2.events_size, cur.len() as u64);
}

/// Verifies: SRS-STATE-001
#[test]
fn snapshot_is_replaced_atomically_and_only_when_due() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = StateWriter::new(&RealFs, dir.path(), Duration::from_secs(2), 1 << 20, None);
    assert!(w.due(Duration::ZERO));
    w.snapshot(&RealFs, Duration::from_secs(10), &sample())
        .unwrap();
    assert!(!w.due(Duration::from_secs(11)));
    assert!(w.due(Duration::from_secs(12)));
    assert_eq!(
        read_snapshot(&RealFs, dir.path()).unwrap(),
        sample_with_time(&dir)
    );
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![STATE_FILE.to_owned()],
        "no temp file left behind"
    );
}

fn sample_with_time(dir: &tempfile::TempDir) -> Snapshot {
    let on_disk = read_snapshot(&RealFs, dir.path()).unwrap();
    Snapshot {
        updated_at: on_disk.updated_at,
        ..sample()
    }
}

/// Verifies: SRS-STATE-001
#[test]
fn status_text_flags_a_stale_snapshot() {
    let snap = sample();
    let now = SystemTime::now();
    let text = describe(&snap, now);
    assert!(!text.contains("STALE"), "{text}");
    assert!(text.contains("outage: alert for 2h 15m"), "{text}");
    assert!(text.contains("pod5/a.pod5"), "{text}");
    assert!(text.contains("1 FAILED"), "{text}");
    assert!(text.contains("share unavailable for 1h"), "{text}");
    assert!(text.contains("cleanup: dry-run"), "{text}");
    assert!(text.contains("2 would be deleted"), "{text}");
    let later = now + Duration::from_secs(31);
    assert!(describe(&snap, later).contains("STALE"));
    assert_eq!(
        stale_after(&Snapshot {
            interval_s: 60,
            ..sample()
        }),
        Duration::from_secs(180)
    );
}

/// Verifies: SRS-STATE-003
#[cfg(unix)]
#[test]
fn notify_command_gets_alerting_events_on_stdin_and_in_the_environment() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("got");
    let script = dir.path().join("notify.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nset -e\nbody=$(cat)\nprintf '%s|%s|%s\\n' \"$ONT_OFFLOAD_EVENT\" \"$ONT_OFFLOAD_LEVEL\" \"$body\" >> {:?}\n",
            out.to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut w = StateWriter::new(
        &RealFs,
        dir.path(),
        Duration::from_secs(2),
        1 << 20,
        Some(format!("{} --flag", script.to_str().unwrap())),
    );
    // Not for a person: no command.
    w.event(
        &RealFs,
        Kind::FileCommitted,
        log::Level::Info,
        None,
        None,
        "x",
        json!({}),
    );
    // For a person.
    w.event(
        &RealFs,
        Kind::OutageAlert,
        log::Level::Error,
        None,
        None,
        "share unavailable for 1h",
        json!({"duration_s": 3600}),
    );
    let t0 = std::time::Instant::now();
    let text = loop {
        if let Ok(t) = std::fs::read_to_string(&out) {
            if t.ends_with('\n') {
                break t;
            }
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "notify_command did not run"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(text.lines().count(), 1, "{text}");
    let (head, body) = text.trim_end().split_once("|error|").unwrap();
    assert_eq!(head, "outage_alert");
    let ev: Event = serde_json::from_str(body).unwrap();
    assert_eq!(ev.kind, Kind::OutageAlert);
    assert_eq!(ev.data["duration_s"], 3600);
    for k in [
        Kind::OutageStarted,
        Kind::OutageAlert,
        Kind::OutageReminder,
        Kind::OutageEnded,
        Kind::WorkerStalled,
        Kind::SourceSpaceLow,
        Kind::CapacityLow,
        Kind::RunFailed,
        Kind::DeletionRefused,
        Kind::DeletionStopped,
    ] {
        assert!(k.alerts(), "{k:?}");
    }
    assert!(!Kind::FileCommitted.alerts());
    assert!(!Kind::RunDeleted.alerts() && !Kind::DeletionDryRun.alerts());
}
