// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration tests of `ont-offload report` through the binary.
#![allow(clippy::expect_used, clippy::unwrap_used, missing_docs)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use sha2::{Digest, Sha256};

fn bin() -> Command {
    Command::cargo_bin("ont-offload").expect("binary builds")
}

fn event(seq: u64, ts: &str, kind: &str, run: Option<&str>, data: &str) -> String {
    let run = run.map_or("null".to_owned(), |r| format!("{r:?}"));
    format!(
        "{{\"schema\":\"ont-offload/event/v1\",\"schema_version\":1,\"seq\":{seq},\"ts\":\"{ts}\",\
         \"kind\":\"{kind}\",\"level\":\"info\",\"run\":{run},\"file\":null,\"message\":\"{kind}\",\"data\":{data}}}\n"
    )
}

/// A state folder with one run offloaded in October 2026.
fn state(dir: &Path) -> PathBuf {
    let state = dir.join("state");
    std::fs::create_dir_all(state.join("events")).unwrap();
    let run = "exp/s/20261002_0100_1A_PAW00001_0a0a0a0a";
    std::fs::write(
        state.join("events/events-20261001T000000Z.jsonl"),
        event(
            1,
            "2026-10-01T00:00:00Z",
            "started",
            None,
            r#"{"pid":1,"version":"0.9.0"}"#,
        ) + &event(2, "2026-10-02T01:00:00Z", "run_discovered", Some(run), "{}"),
    )
    .unwrap();
    std::fs::write(
        state.join("events.jsonl"),
        event(
            3,
            "2026-10-02T02:00:00Z",
            "file_committed",
            Some(run),
            r#"{"size":2000000}"#,
        ) + &event(
            4,
            "2026-10-02T03:00:00Z",
            "run_complete",
            Some(run),
            r#"{"files":1,"bytes":2000000,"manifest_sha256":"00","destination":"/nowhere"}"#,
        ),
    )
    .unwrap();
    state
}

fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.clone(), std::fs::read(&p).unwrap());
            }
        }
    }
    out
}

/// Verifies: SRS-RPT-001, SRS-RPT-005, SRS-RPT-004
#[test]
fn status_report_writes_its_files_and_changes_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path());
    let before = tree(&state);
    let out = dir.path().join("reports");
    let formats = if cfg!(feature = "report") {
        "pdf,json,typ"
    } else {
        "json,typ"
    };
    bin()
        .args(["--config", "/nonexistent/config.toml", "report", "status"])
        .args(["--month", "2026-10", "--format", formats])
        .arg("--state-dir")
        .arg(&state)
        .arg("--out")
        .arg(&out)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("status report OOF-SR-20261031-")
                .and(predicate::str::contains(": OK (")),
        );
    assert_eq!(tree(&state), before, "the state folder changed");
    let stem = out.join(format!(
        "OOF-SR-{}-20261001-20261031",
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h
                .trim()
                .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_"))
            .unwrap_or_default()
    ));
    let json = std::fs::read(stem.with_extension("json")).unwrap();
    let rec: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(rec["schema"], "ont-offload/status-report/v1");
    assert_eq!(rec["totals"]["bytes_committed"], 2_000_000);
    assert_eq!(rec["runs"][0]["flow_cell"], "PAW00001");
    // The sha256sum file names every output with its digest.
    let sums = std::fs::read_to_string(stem.with_extension("sha256")).unwrap();
    let json_sum = format!("{:x}", Sha256::digest(&json));
    assert!(sums.contains(&json_sum), "{sums}");
    assert!(sums.contains(".typst/status.typ"), "{sums}");
    assert!(stem.with_extension("typst").join("data.json").is_file());
    if cfg!(feature = "report") {
        let pdf = std::fs::read(stem.with_extension("pdf")).unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("<pdfaid:part>2</pdfaid:part>"), "not PDF/A-2");
        let pdf_sum = format!("{:x}", Sha256::digest(&pdf));
        assert!(sums.contains(&pdf_sum), "{sums}");
    }
}

/// Verifies: SRS-RPT-002
#[test]
fn a_report_without_records_requires_action() {
    let dir = tempfile::tempdir().unwrap();
    bin()
        .args(["--config", "/nonexistent/config.toml", "report", "status"])
        .args([
            "--from",
            "2026-10-01",
            "--to",
            "2026-10-07",
            "--format",
            "json",
        ])
        .arg("--state-dir")
        .arg(dir.path().join("missing"))
        .arg("--out")
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("ACTION REQUIRED"));
}

/// Verifies: SRS-RPT-001
#[test]
fn bad_periods_and_formats_are_usage_errors() {
    for args in [
        vec!["--month", "2026-13"],
        vec!["--from", "2026-10-05", "--to", "2026-10-01"],
        vec!["--month", "2026-10", "--format", "docx"],
    ] {
        bin()
            .args([
                "--config",
                "/nonexistent/config.toml",
                "report",
                "status",
                "--state-dir",
                "/tmp",
            ])
            .args(&args)
            .assert()
            .code(2);
    }
    // --month and --from exclude each other.
    bin()
        .args([
            "report",
            "status",
            "--month",
            "2026-10",
            "--from",
            "2026-10-01",
        ])
        .assert()
        .code(2);
}
