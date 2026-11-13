// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end tests of `bench` through the binary, against a "share" on
//! `/dev/shm` (tmpfs). Skipped when `/dev/shm` is missing or on the same
//! device as the temp dir.
#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, missing_docs)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

struct Env {
    dir: tempfile::TempDir,
    share: tempfile::TempDir,
    config: PathBuf,
}

impl Env {
    fn new() -> Option<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let share = tempfile::tempdir_in("/dev/shm").ok()?;
        let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
        if dev(dir.path()) == dev(share.path()) {
            return None;
        }
        std::fs::write(share.path().join(".ont-offload-target"), "bench-share\n")
            .expect("sentinel");
        for d in ["data", "state"] {
            std::fs::create_dir_all(dir.path().join(d)).expect("mkdir");
        }
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "dest_root = {:?}\ntarget_id = \"bench-share\"\nstate_dir = {:?}\n\
                 [guard]\nallowed_fstypes = [\"tmpfs\"]\n[transfer]\nmin_free = \"1MB\"\n\
                 [source]\nroot = {:?}\n",
                share.path().to_str().expect("utf8"),
                dir.path().join("state").to_str().expect("utf8"),
                dir.path().join("data").to_str().expect("utf8"),
            ),
        )
        .expect("config");
        Some(Self { dir, share, config })
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::cargo_bin("ont-offload").expect("binary builds");
        c.args(["--config", self.config.to_str().expect("utf8")])
            .args(args);
        c
    }
}

/// Verifies: SRS-PAR-002
#[test]
fn bench_measures_suggests_records_and_cleans_up() {
    let Some(env) = Env::new() else { return };
    env.cmd(&["bench", "--size", "8MB", "--max-streams", "4"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("1 stream(s)")
                .and(predicate::str::contains(" 4 stream(s)"))
                .and(predicate::str::contains("chunk"))
                .and(predicate::str::contains("workers = "))
                .and(predicate::str::contains("rate_limit = \""))
                .and(predicate::str::contains("chunk_size = \"")),
        );
    let report: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.dir.path().join("state/bench.json")).expect("bench.json"),
    )
    .expect("json");
    assert_eq!(report["streams"].as_array().expect("steps").len(), 3);
    assert_eq!(report["chunks"].as_array().expect("steps").len(), 3);
    assert!(
        report["mount"]
            .as_str()
            .expect("mount")
            .starts_with("tmpfs")
    );
    let workers = report["suggestion"]["workers"].as_u64().expect("workers");
    assert!([1, 2, 4].contains(&workers));
    // Nothing is left on the share but the sentinel.
    let left: Vec<_> = std::fs::read_dir(env.share.path())
        .expect("ls")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(left, vec![std::ffi::OsString::from(".ont-offload-target")]);
}

/// Verifies: SRS-PAR-002, SRS-GRD-001
#[test]
fn bench_writes_nothing_to_a_share_that_fails_the_guard() {
    let Some(env) = Env::new() else { return };
    std::fs::write(env.share.path().join(".ont-offload-target"), "other\n").expect("sentinel");
    env.cmd(&["bench", "--size", "1MB", "--no-chunks"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("sentinel"));
    assert_eq!(std::fs::read_dir(env.share.path()).expect("ls").count(), 1);
    assert!(!env.dir.path().join("state/bench.json").exists());
}
