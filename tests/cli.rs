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
    bin().arg("status").assert().code(1).stderr(
        predicate::str::contains("not implemented yet").and(predicate::str::contains("v0.4")),
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

fn write_config(dir: &std::path::Path, dest: &std::path::Path, extra: &str) -> std::path::PathBuf {
    let data = dir.join("data");
    let state = dir.join("state");
    std::fs::create_dir_all(&data).expect("mkdir");
    std::fs::create_dir_all(&state).expect("mkdir");
    let path = dir.join("config.toml");
    let text = format!(
        "dest_root = {:?}\ntarget_id = \"lab-nas-01\"\nstate_dir = {:?}\n{extra}\n[source]\nroot = {:?}\n",
        dest.to_str().expect("utf8"),
        state.to_str().expect("utf8"),
        data.to_str().expect("utf8"),
    );
    std::fs::write(&path, text).expect("write config");
    path
}

/// Verifies: SRS-CFG-002, SRS-CFG-004
#[test]
fn check_config_refuses_destination_on_source_device() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("not-mounted");
    std::fs::create_dir(&dest).expect("mkdir");
    let cfg = write_config(dir.path(), &dest, "");
    bin()
        .args(["check-config", "--config", cfg.to_str().expect("utf8")])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("same device"));
}

/// Verifies: SRS-CFG-003, SRS-CFG-004
#[test]
fn check_config_lists_every_problem() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(
        dir.path(),
        &dir.path().join("missing"),
        "[bandwidth]\nworkers = 0\nrate_limit = \"100MB\"",
    );
    bin()
        .args(["check-config", "--config", cfg.to_str().expect("utf8")])
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("3 problem(s)")
                .and(predicate::str::contains("dest_root"))
                .and(predicate::str::contains("bandwidth.workers"))
                .and(
                    predicate::str::contains("add \\\"/s\\\"")
                        .or(predicate::str::contains("add \"/s\"")),
                ),
        );
}

/// Verifies: SRS-CFG-003
#[test]
fn check_config_rejects_unknown_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = write_config(dir.path(), dir.path(), "bwlimit = \"10MB/s\"");
    bin()
        .args(["check-config", "--config", cfg.to_str().expect("utf8")])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown field `bwlimit`"));
}

/// Verifies: SRS-CFG-004, SRS-GRD-001
///
/// Needs a second filesystem to stand in for the share; uses `/dev/shm`
/// when it is on a different device than the temp dir, otherwise skips.
#[cfg(target_os = "linux")]
#[test]
fn check_config_accepts_valid_config_and_prints_summary() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let Ok(share) = tempfile::tempdir_in("/dev/shm") else {
        return;
    };
    let dev = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.dev()).ok();
    if dev(dir.path()) == dev(share.path()) {
        return;
    }
    std::fs::write(share.path().join(".ont-offload-target"), "lab-nas-01\n").expect("sentinel");
    let cfg = write_config(
        dir.path(),
        share.path(),
        "[bandwidth]\nworkers = 4\nrate_limit = \"100MB/s\"\nnetwork_speed = \"1Gbit/s\"\n\
         [guard]\nallowed_fstypes = [\"tmpfs\"]",
    );
    bin()
        .args(["check-config", "--config", cfg.to_str().expect("utf8")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("configuration OK")
                .and(predicate::str::contains(
                    "4 workers, total 100 MB/s (about 25 MB/s per worker), 80% of the 1 Gbit/s link",
                ))
                .and(predicate::str::contains("runs detected: 0"))
                .and(predicate::str::contains("destination guard OK")),
        );
}
