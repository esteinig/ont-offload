// SPDX-License-Identifier: AGPL-3.0-or-later
//! Integration tests of `ont-offload tui`: launch settings from flags and
//! environment variables, and that the dashboard leaves the state folder
//! untouched.
#![allow(clippy::expect_used, missing_docs)]

use assert_cmd::Command;
use predicates::prelude::*;

fn bin() -> Command {
    let mut c = Command::cargo_bin("ont-offload").expect("binary builds");
    for v in [
        "ONT_OFFLOAD_CONFIG",
        "ONT_OFFLOAD_STATE_DIR",
        "ONT_OFFLOAD_TUI_LOG",
        "ONT_OFFLOAD_TUI_THEME",
        "ONT_OFFLOAD_TUI_REFRESH",
        "ONT_OFFLOAD_TUI_BELL",
        "NO_COLOR",
    ] {
        c.env_remove(v);
    }
    c
}

/// Verifies: SRS-TUI-009
#[test]
fn check_shows_settings_from_environment_variables_and_their_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("state.json"), "{}").expect("write");
    let log = dir.path().join("daemon.log");
    std::fs::write(&log, "x\n").expect("write");
    bin()
        .args(["tui", "--check", "--theme", "matrix"])
        .env("ONT_OFFLOAD_CONFIG", dir.path().join("absent.toml"))
        .env("ONT_OFFLOAD_STATE_DIR", dir.path())
        .env("ONT_OFFLOAD_TUI_LOG", &log)
        .env("ONT_OFFLOAD_TUI_THEME", "phosphor")
        .env("ONT_OFFLOAD_TUI_REFRESH", "3s")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("absent.toml: not found")
                .and(predicate::str::contains("($ONT_OFFLOAD_STATE_DIR)"))
                .and(predicate::str::contains("state.json            readable"))
                .and(predicate::str::contains("($ONT_OFFLOAD_TUI_LOG): readable"))
                .and(predicate::str::contains("theme       matrix  (flag)"))
                .and(predicate::str::contains(
                    "refresh     3s  ($ONT_OFFLOAD_TUI_REFRESH)",
                ))
                .and(predicate::str::contains("bell        on  (default)")),
        );
}

/// Verifies: SRS-TUI-009
#[test]
fn a_bad_setting_is_a_configuration_error() {
    bin()
        .args(["tui", "--check"])
        .env("ONT_OFFLOAD_CONFIG", "/nonexistent/c.toml")
        .env("ONT_OFFLOAD_TUI_THEME", "vaporwave")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("ONT_OFFLOAD_TUI_THEME"));
}

/// Without a terminal the dashboard does not start, says how to check its
/// settings, and creates nothing.
///
/// Verifies: SRS-TUI-001, SRS-TUI-009
#[test]
fn without_a_terminal_it_refuses_and_creates_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("state");
    bin()
        .args(["tui", "--state-dir"])
        .arg(&state)
        .env("ONT_OFFLOAD_CONFIG", "/nonexistent/c.toml")
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("tui --check").or(predicate::str::contains("--features tui")),
        );
    assert!(!state.exists(), "the state folder is not created");
}
