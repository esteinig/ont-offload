// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end tests of `init` through the binary.
#![allow(clippy::expect_used, missing_docs)]

use assert_cmd::Command;
use predicates::prelude::*;

fn bin(home: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("ont-offload").expect("binary builds");
    c.env("HOME", home).env_remove("ONT_OFFLOAD_CONFIG");
    c
}

/// Verifies: SRS-CFG-005, SRS-CLI-001
#[test]
fn init_print_writes_a_configuration_to_stdout_only() {
    let home = tempfile::tempdir().expect("tempdir");
    bin(home.path())
        .args(["init", "--print", "--set", "transfer.settle=7s"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("[[source]]")
                .and(predicate::str::contains("name = \"simulation\""))
                .and(predicate::str::contains("settle = \"7s\"")),
        );
    assert_eq!(std::fs::read_dir(home.path()).expect("read").count(), 0);
}

/// Verifies: SRS-CFG-005
#[test]
fn init_without_a_share_for_production_is_a_configuration_error() {
    let home = tempfile::tempdir().expect("tempdir");
    bin(home.path())
        .args(["init", "--preset", "production"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("needs --share"));
}

/// Verifies: SRS-CFG-005, SRS-CFG-004
#[cfg(target_os = "linux")]
#[test]
fn local_init_then_check_config_passes() {
    use std::os::unix::fs::MetadataExt;
    let home = tempfile::tempdir().expect("tempdir");
    let Ok(shm) = tempfile::tempdir_in("/dev/shm") else {
        return;
    };
    let dev = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.dev()).ok();
    if dev(home.path()) == dev(shm.path()) {
        return;
    }
    let share = shm.path().join("share");
    bin(home.path())
        .args(["init", "--share", share.to_str().expect("utf8")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Next steps")
                .and(predicate::str::contains("configuration valid")),
        );
    let cfg = home.path().join("ont-local/config.toml");
    bin(home.path())
        .args(["--config", cfg.to_str().expect("utf8"), "check-config"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("destination guard OK")
                .and(predicate::str::contains("configuration OK")),
        );
}
