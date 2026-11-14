// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the dashboard model: staleness on an injected clock, alerts and
//! their clearing, and that reading changes nothing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::fsio::faulty::{Fault, FaultyFs, Op, Rule};
use crate::fsio::{RealFs, errno};
use crate::state::schema_check::{load, validate};
use crate::tui::fixtures::{
    append_events, append_report, at, event, failed_run, normal, outage, refused, refused_report,
    stalled, t0, write_state,
};

fn secs(n: u64) -> SystemTime {
    t0() + Duration::from_secs(n)
}

/// Verifies: SRS-TUI-007
#[test]
fn fixtures_follow_the_state_and_event_schemas() {
    let schema = load("state-v1.json");
    for s in [
        normal(&at(0)),
        outage(&at(0)),
        stalled(&at(0)),
        failed_run(&at(0)),
        refused(&at(0)),
    ] {
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(validate(&schema, &v), Vec::<String>::new(), "{}", s.status);
    }
    let ev = load("event-v1.json");
    let e = serde_json::to_value(event(1, &at(0), Kind::OutageStarted, "warning", "x")).unwrap();
    assert_eq!(validate(&ev, &e), Vec::<String>::new());
}

/// Verifies: SRS-TUI-002
#[test]
fn a_snapshot_older_than_three_intervals_is_stale_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &normal(&at(0)));
    let mut m = Model::new(dir.path(), None);
    m.refresh(&RealFs, secs(1));
    assert_eq!(m.health, Some(Health::Live));
    // interval 2 s: stale after max(3 x 2 s, 30 s) = 30 s.
    m.refresh(&RealFs, secs(30));
    assert!(m.health.as_ref().unwrap().live());
    m.refresh(&RealFs, secs(31));
    match m.health.as_ref().unwrap() {
        Health::Stale { age, updated_at } => {
            assert_eq!(*age, Duration::from_secs(31));
            assert_eq!(updated_at, &at(0));
        }
        other => panic!("{other:?}"),
    }
    // The last values stay on screen (drawn dimmed), never blanked.
    assert!(m.snapshot.is_some());
    assert_eq!(m.snapshot_age(secs(31)), Some(Duration::from_secs(31)));
    // A new snapshot makes it live again.
    write_state(dir.path(), &normal(&at(40)));
    m.refresh(&RealFs, secs(41));
    assert_eq!(m.health, Some(Health::Live));
}

/// Verifies: SRS-TUI-002, SRS-TUI-007
#[test]
fn missing_unreadable_and_other_versions_are_not_shown_as_data() {
    let dir = tempfile::tempdir().unwrap();
    let mut m = Model::new(dir.path(), None);
    m.refresh(&RealFs, secs(0));
    assert!(matches!(m.health, Some(Health::NoData { ref why }) if why.contains("does not exist")));

    std::fs::write(dir.path().join("state.json"), b"{ torn").unwrap();
    m.refresh(&RealFs, secs(1));
    assert!(matches!(m.health, Some(Health::NoData { .. })));

    let mut v = serde_json::to_value(normal(&at(0))).unwrap();
    v["schema_version"] = serde_json::json!(2);
    v["new_field"] = serde_json::json!(true);
    std::fs::write(dir.path().join("state.json"), v.to_string()).unwrap();
    write_state(dir.path(), &normal(&at(0)));
    m.refresh(&RealFs, secs(1));
    assert!(m.snapshot.is_some());
    std::fs::write(dir.path().join("state.json"), v.to_string()).unwrap();
    m.refresh(&RealFs, secs(2));
    match m.health.as_ref().unwrap() {
        Health::Unsupported { why } => {
            assert!(why.contains("version 2"), "{why}");
            assert!(why.contains("same ont-offload version"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    // No field of an unknown version is shown.
    assert!(m.snapshot.is_none());
}

/// Verifies: SRS-TUI-003
#[test]
fn snapshot_alerts_show_until_acknowledged_even_after_they_clear() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &outage(&at(0)));
    let mut m = Model::new(dir.path(), None);
    m.refresh(&RealFs, secs(1));
    assert_eq!(m.alerts.len(), 1);
    assert_eq!(m.alerts[0].kind, Kind::OutageAlert);
    assert_eq!(m.new_alerts, 1);
    assert_eq!(m.unacknowledged().len(), 1);
    // The same alert again is not new (the bell rings once).
    m.refresh(&RealFs, secs(2));
    assert_eq!(m.new_alerts, 0);
    assert_eq!(m.alerts.len(), 1);

    // The share recovers: the alert clears but the banner keeps it.
    write_state(dir.path(), &normal(&at(10)));
    m.refresh(&RealFs, secs(11));
    assert!(m.alerts.is_empty());
    assert_eq!(m.cleared.len(), 1);
    assert_eq!(m.unacknowledged().len(), 1, "a short failure is not missed");

    m.acknowledge();
    assert!(m.unacknowledged().is_empty());
    assert_eq!(m.cleared.len(), 1, "acknowledging hides only the banner");

    // A new alert after acknowledging shows again.
    write_state(dir.path(), &failed_run(&at(20)));
    m.refresh(&RealFs, secs(21));
    assert_eq!(m.new_alerts, 1);
    assert_eq!(m.unacknowledged().len(), 1);
    assert_eq!(m.unacknowledged()[0].kind, Kind::RunFailed);
}

/// Verifies: SRS-TUI-003, SRS-TUI-005
#[test]
fn alerting_events_raise_alerts_and_other_events_do_not() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &normal(&at(0)));
    append_events(
        dir.path(),
        &[
            event(1, &at(-5), Kind::Started, "info", "started"),
            event(2, &at(-4), Kind::FileCommitted, "info", "committed"),
            event(
                3,
                &at(5),
                Kind::WorkerStalled,
                "warning",
                "worker 1 stalled",
            ),
        ],
    );
    let mut m = Model::new(dir.path(), None);
    m.refresh(&RealFs, secs(1));
    assert_eq!(m.events.len(), 3);
    assert_eq!(m.alerts.len(), 1);
    assert_eq!(m.alerts[0].origin, Origin::Event);
    assert_eq!(m.new_alerts, 1);
    // A recovery event (info) is listed as an alert but does not ring or
    // show in the banner.
    append_events(
        dir.path(),
        &[event(4, &at(6), Kind::OutageEnded, "info", "share back")],
    );
    m.refresh(&RealFs, secs(2));
    assert_eq!(m.new_alerts, 0);
    assert_eq!(m.unacknowledged().len(), 1);
}

/// Verifies: SRS-TUI-002, SRS-TUI-003
#[test]
fn a_stale_snapshot_neither_raises_nor_clears_alerts() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &outage(&at(0)));
    let mut m = Model::new(dir.path(), None);
    m.refresh(&RealFs, secs(1));
    assert_eq!(m.alerts.len(), 1);
    // The daemon stops; its last snapshot (still saying outage) goes stale
    // and nothing it says is taken as current.
    m.refresh(&RealFs, secs(100));
    assert!(!m.health.as_ref().unwrap().live());
    assert_eq!(m.alerts.len(), 1);
    assert!(m.cleared.is_empty());

    let other = tempfile::tempdir().unwrap();
    write_state(other.path(), &outage(&at(0)));
    let mut fresh = Model::new(other.path(), None);
    fresh.refresh(&RealFs, secs(100));
    assert!(fresh.alerts.is_empty());
}

/// Verifies: SRS-TUI-005
#[test]
fn speed_samples_are_taken_once_per_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    write_state(dir.path(), &normal(&at(0)));
    let mut m = Model::new(dir.path(), None);
    m.refresh(&RealFs, secs(1));
    m.refresh(&RealFs, secs(2));
    assert_eq!(m.rates.len(), 1);
    let mut s = normal(&at(2));
    s.rate.current_bytes_per_s = 7;
    write_state(dir.path(), &s);
    m.refresh(&RealFs, secs(3));
    assert_eq!(m.rates.iter().copied().collect::<Vec<_>>(), [82_000_000, 7]);
    for i in 0..(i64::try_from(RATE_KEPT).unwrap() + 10) {
        write_state(dir.path(), &normal(&at(10 + i)));
        m.refresh(&RealFs, secs(10));
    }
    assert_eq!(m.rates.len(), RATE_KEPT);
}

/// Verifies: SRS-TUI-005
#[test]
fn log_and_cleanup_report_are_followed() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    std::fs::write(&log, "line 1\nline 2\n").unwrap();
    write_state(dir.path(), &refused(&at(0)));
    append_report(dir.path(), &refused_report());
    let mut m = Model::new(dir.path(), Some(&log));
    m.refresh(&RealFs, secs(1));
    assert_eq!(m.log, ["line 1", "line 2"]);
    assert_eq!(m.last_report.as_ref().unwrap().runs.len(), 1);
    std::fs::remove_file(&log).unwrap();
    m.refresh(&RealFs, secs(2));
    assert!(m.log_error.is_some());
    assert_eq!(m.log.len(), 2, "lines already read stay");
}

fn listing(dir: &std::path::Path) -> BTreeMap<String, (u64, SystemTime)> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let md = e.metadata().unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                (md.len(), md.modified().unwrap()),
            )
        })
        .collect()
}

/// Every write-side operation fails; the dashboard still reads everything
/// and the state folder is byte-for-byte unchanged.
///
/// Verifies: SRS-TUI-001
#[test]
fn reading_needs_no_write_access_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    std::fs::write(&log, "a\n").unwrap();
    write_state(dir.path(), &refused(&at(0)));
    append_events(
        dir.path(),
        &[event(1, &at(0), Kind::DeletionRefused, "error", "refused")],
    );
    append_report(dir.path(), &refused_report());
    let before = listing(dir.path());

    let fs = FaultyFs::new(Arc::new(RealFs));
    for op in [
        Op::CreateNew,
        Op::Write,
        Op::SyncData,
        Op::Rename,
        Op::RemoveFile,
        Op::RemoveDir,
        Op::CreateDirAll,
        Op::SyncDir,
        Op::SetMode,
        Op::AppendSync,
    ] {
        fs.add(Rule::always(op, "", Fault::Errno(errno::EACCES)));
    }
    let mut m = Model::new(dir.path(), Some(&log));
    for i in 0..3 {
        m.refresh(&fs, secs(i));
    }
    assert_eq!(m.health, Some(Health::Live));
    assert_eq!(m.events.len(), 1);
    assert_eq!(m.alerts.len(), 2);
    assert!(m.last_report.is_some());
    assert_eq!(m.log, ["a"]);
    assert_eq!(
        listing(dir.path()),
        before,
        "nothing created, changed or removed"
    );
}
