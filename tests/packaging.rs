// SPDX-License-Identifier: AGPL-3.0-or-later
//! Checks of the deployment files in `packaging/` and `scripts/`: the
//! service runs as its own user with private state and logs, the log is
//! rotated without copytruncate, and the shell scripts parse.
#![allow(clippy::expect_used, missing_docs)]

use std::path::Path;

fn read(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).expect(rel)
}

/// `key=value` lines of a systemd unit (comments dropped).
fn directives(unit: &str) -> Vec<(String, String)> {
    unit.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

/// Verifies: SRS-DEP-001
#[test]
fn service_runs_as_its_own_user_with_private_state_and_logs() {
    let d = directives(&read("packaging/ont-offload.service"));
    let get = |k: &str| {
        d.iter()
            .find(|(key, _)| key == k)
            .map_or_else(|| panic!("{k} missing"), |(_, v)| v.as_str())
    };
    assert_eq!(get("User"), "ont-offload");
    assert_eq!(get("Group"), "ont-offload");
    assert_eq!(get("StateDirectoryMode"), "0750");
    assert_eq!(get("LogsDirectoryMode"), "0750");
    assert_eq!(get("UMask"), "0027");
    assert_eq!(get("NoNewPrivileges"), "yes");
    // Unchanged duties of the unit.
    assert_eq!(get("Type"), "notify");
    assert_eq!(get("RestartPreventExitStatus"), "2");
    assert_eq!(get("RestartForceExitStatus"), "75");
    assert!(get("ExecStart").contains(" watch "));
    let sysusers = read("packaging/ont-offload.sysusers");
    assert!(
        sysusers
            .lines()
            .any(|l| l.starts_with("u ont-offload ") && l.contains("/var/lib/ont-offload")),
        "{sysusers}"
    );
}

/// Verifies: SRS-LOG-005
#[test]
fn the_log_is_rotated_by_renaming_with_private_mode() {
    let conf = read("packaging/ont-offload.logrotate");
    let lines: Vec<&str> = conf
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .collect();
    assert!(
        !lines.iter().any(|l| l.starts_with("copytruncate")),
        "{conf}"
    );
    assert!(
        lines.contains(&"create 0640 ont-offload ont-offload"),
        "{conf}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("/var/log/ont-offload/")),
        "{conf}"
    );
}

/// Verifies: SRS-DEP-001, SRS-VER-005, SRS-TUI-009
#[test]
fn shell_scripts_parse() {
    for (shell, rel) in [
        ("sh", "packaging/ont-offload-tui"),
        ("bash", "scripts/compare-with-legacy.sh"),
        ("bash", "scripts/replay-soak.sh"),
        ("bash", "scripts/real-share-test.sh"),
        ("bash", "scripts/local-test.sh"),
    ] {
        let Ok(out) = std::process::Command::new(shell)
            .arg("-n")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
            .output()
        else {
            continue; // shell not installed
        };
        assert!(
            out.status.success(),
            "{rel}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The drop-in for an existing account (such as `prom`) sets a user and
/// group and never root.
///
/// Verifies: SRS-DEP-001
#[test]
fn existing_user_drop_in_is_not_root() {
    let d = directives(&read("packaging/run-as-existing-user.conf"));
    let user = d.iter().find(|(k, _)| k == "User").map(|(_, v)| v.as_str());
    let group = d
        .iter()
        .find(|(k, _)| k == "Group")
        .map(|(_, v)| v.as_str());
    assert_eq!(user, Some("prom"));
    assert_eq!(group, Some("prom"));
    assert!(d.iter().all(|(_, v)| v != "root" && v != "0"));
}
