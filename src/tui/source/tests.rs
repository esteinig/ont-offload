// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the file tails: torn lines, rotation, truncation, sequence gaps.

use std::io::Write;

use super::*;
use crate::fsio::RealFs;
use crate::state::Kind;
use crate::tui::fixtures::{append_events, at, event};

fn append(p: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// Verifies: SRS-TUI-005
#[test]
fn a_torn_last_line_is_held_back_until_complete() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.log");
    let mut t = LineTail::new(p.clone(), None);
    assert!(t.poll(&RealFs).error.is_some(), "missing file is reported");
    append(&p, "one\ntw");
    assert_eq!(t.poll(&RealFs).lines, ["one"]);
    assert!(t.poll(&RealFs).lines.is_empty());
    append(&p, "o\n\nthree\n");
    assert_eq!(t.poll(&RealFs).lines, ["two", "three"]);
}

/// Verifies: SRS-TUI-005
#[test]
fn a_large_file_is_shown_from_near_its_end_at_a_line_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.log");
    let line = format!("{}\n", "x".repeat(99));
    append(&p, &line.repeat(2000)); // 200 kB
    let mut t = LineTail::new(p.clone(), None);
    let lines = t.poll(&RealFs).lines;
    assert!(!lines.is_empty());
    assert!(lines.len() <= usize::try_from(INITIAL_TAIL / 100).unwrap() + 1);
    assert!(lines.iter().all(|l| l.len() == 99), "no partial first line");
}

/// Verifies: SRS-TUI-005
#[test]
fn rotation_reads_the_rest_of_the_old_file_then_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("events.jsonl");
    let old = dir.path().join("events.jsonl.1");
    let mut t = LineTail::new(p.clone(), Some(old.clone()));
    append(&p, "a\nb\n");
    assert_eq!(t.poll(&RealFs).lines, ["a", "b"]);
    append(&p, "c\n");
    std::fs::rename(&p, &old).unwrap();
    append(&p, "d\n");
    let r = t.poll(&RealFs);
    assert!(r.rotated);
    assert_eq!(r.lines, ["c", "d"], "nothing lost across the rotation");
    // Truncation in place: read again from the start.
    std::fs::write(&p, "").unwrap();
    assert!(t.poll(&RealFs).rotated);
    append(&p, "e\n");
    assert_eq!(t.poll(&RealFs).lines, ["e"]);
}

/// Verifies: SRS-TUI-005
#[test]
fn event_gaps_restarts_and_unreadable_lines_are_counted_not_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = EventTail::new(dir.path());
    let e = |s| event(s, &at(0), Kind::FileCommitted, "info", "c");
    append_events(dir.path(), &[e(1), e(2), e(5)]);
    let r = t.poll(&RealFs);
    assert_eq!(r.events.len(), 3);
    assert_eq!(r.missed, 2);
    append(&dir.path().join(EVENTS_FILE), "not json\n");
    append_events(dir.path(), &[e(1), e(2)]);
    let r = t.poll(&RealFs);
    assert_eq!(r.missed, 1);
    assert!(r.restarted);
    assert_eq!(r.events.len(), 2);
}

/// Verifies: SRS-TUI-005
#[test]
fn events_read_twice_across_a_rotation_are_shown_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = EventTail::new(dir.path());
    let e = |s| event(s, &at(0), Kind::FileCommitted, "info", "c");
    append_events(dir.path(), &[e(1), e(2)]);
    assert_eq!(t.poll(&RealFs).events.len(), 2);
    // Simulate a copy-then-truncate rotation that re-presents seq 2.
    std::fs::write(dir.path().join(EVENTS_FILE), "").unwrap();
    assert!(t.poll(&RealFs).events.is_empty());
    append_events(dir.path(), &[e(2), e(3)]);
    let r = t.poll(&RealFs);
    assert_eq!(r.events.iter().map(|e| e.seq).collect::<Vec<_>>(), [3]);
    assert_eq!(r.missed, 0);
}

/// Verifies: SRS-TUI-007
#[test]
fn snapshot_fields_of_another_version_are_never_read() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(STATE_FILE);
    std::fs::write(
        &p,
        r#"{"schema":"ont-offload/state/v1","schema_version":9,"runs":"??"}"#,
    )
    .unwrap();
    assert!(
        matches!(read_snapshot(&RealFs, dir.path()), SnapshotRead::Unsupported(w) if w.contains("version 9"))
    );
    std::fs::write(&p, r#"{"schema":"other","schema_version":1}"#).unwrap();
    assert!(matches!(
        read_snapshot(&RealFs, dir.path()),
        SnapshotRead::Unsupported(_)
    ));
    std::fs::write(
        &p,
        r#"{"schema":"ont-offload/state/v1","schema_version":1}"#,
    )
    .unwrap();
    assert!(matches!(
        read_snapshot(&RealFs, dir.path()),
        SnapshotRead::Unreadable(_)
    ));
}
