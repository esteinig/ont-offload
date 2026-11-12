// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration tests of the ont-offload binary.
#![allow(clippy::expect_used, missing_docs)]

use assert_cmd::Command;
use predicates::prelude::*;

fn bin() -> Command {
    Command::cargo_bin("ont-offload").expect("binary builds")
}

/// Verifies: SRS-CLI-002
#[test]
fn help_lists_commands() {
    bin().arg("--help").assert().success().stdout(
        predicate::str::contains("watch")
            .and(predicate::str::contains("check-config"))
            .and(predicate::str::contains("cleanup")),
    );
}

/// Verifies: SRS-CLI-002
#[test]
fn version_prints_package_version() {
    bin()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

/// Verifies: SRS-CLI-003
#[test]
fn unknown_subcommand_is_a_usage_error() {
    bin()
        .arg("frobnicate")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("frobnicate"));
}

/// Verifies: SRS-CLI-004
#[test]
fn unimplemented_command_exits_1_with_plan() {
    bin().arg("watch").assert().code(1).stderr(
        predicate::str::contains("not implemented yet").and(predicate::str::contains("v0.3")),
    );
}

/// Verifies: SRS-LOG-002
#[test]
fn log_file_receives_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("ont-offload.log");
    bin()
        .args(["--log-file", log.to_str().expect("utf8"), "bench"])
        .assert()
        .code(1);
    let text = std::fs::read_to_string(&log).expect("log written");
    assert!(text.contains("starting: bench"), "{text}");
}

/// Verifies: SRS-LOG-001
#[test]
fn json_log_format_on_stderr() {
    bin()
        .args(["--log-format", "json", "bench"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("\"level\":\"INFO\""));
}
