// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end tests of `cleanup` through the binary, on a run made by
//! ont-runsim and a "share" on `/dev/shm` (tmpfs, a different device from
//! the temp dir). Skipped when `/dev/shm` is missing or on the same device.
#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, missing_docs)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use assert_cmd::Command;
use predicates::prelude::*;

const TARGET: &str = "lab-share-01";

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
        std::fs::write(
            share.path().join(".ont-offload-target"),
            format!("{TARGET}\n"),
        )
        .expect("sentinel");
        for d in ["data", "state"] {
            std::fs::create_dir_all(dir.path().join(d)).expect("mkdir");
        }
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "dest_root = {:?}\ntarget_id = {TARGET:?}\nstate_dir = {:?}\n\
                 [guard]\nallowed_fstypes = [\"tmpfs\"]\n\
                 [transfer]\nmin_free = \"1MB\"\nsettle = \"1s\"\nsettle_after_complete = \"2s\"\n\
                 [bandwidth]\nworkers = 3\nrate_limit = \"200MB/s\"\n\
                 [outage]\nsource_free_alert = \"0B\"\n\
                 [source]\nroot = {:?}\n",
                share.path().to_str().expect("utf8"),
                dir.path().join("state").to_str().expect("utf8"),
                dir.path().join("data").to_str().expect("utf8"),
            ),
        )
        .expect("config");
        Some(Self { dir, share, config })
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn simulate(&self) -> PathBuf {
        let toml = "seed = 7\nhours = 0.5\n[fake]\nsize_scale = 0.0002\n\
                    [[runs]]\nexperiment = \"exp1\"\nsample = \"s1\"\n";
        let sc = ont_runsim::scenario::Scenario::from_toml(toml, None).expect("scenario");
        ont_runsim::engine::prepare_out(&self.data()).expect("out");
        let plans = ont_runsim::plan::build(&sc, SystemTime::UNIX_EPOCH).expect("plan");
        let mut e = ont_runsim::engine::Engine::new(&self.data(), plans).expect("engine");
        let mut t = 0.0;
        while !e.done() {
            e.advance_to(t).expect("advance");
            t += 300.0;
        }
        e.run_dirs().into_iter().next().expect("one run")
    }

    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("ont-offload").expect("binary builds");
        c.args(["--config", self.config.to_str().expect("utf8")])
            .timeout(Duration::from_secs(120));
        c
    }
}

impl Env {
    fn dest(&self, run: &Path) -> PathBuf {
        self.share
            .path()
            .join(run.strip_prefix(self.data()).expect("below data"))
    }

    fn set_cleanup(&self, section: &str) {
        let text = std::fs::read_to_string(&self.config).expect("config");
        let base = text.split("\n[cleanup]\n").next().expect("text").to_owned();
        std::fs::write(&self.config, format!("{base}\n[cleanup]\n{section}\n")).expect("config");
    }
}

/// The whole cleanup path by hand: off refuses, dry runs report, the
/// storage-side receipt opens the gate, deletion removes only listed files
/// and the destination still verifies.
///
/// Verifies: SRS-DEL-001, SRS-DEL-002, SRS-DEL-003, SRS-DEL-006, SRS-CLI-004
#[test]
fn cleanup_by_hand_from_dry_run_to_deletion() {
    let Some(env) = Env::new() else { return };
    let run = env.simulate();
    let run_str = run.to_str().expect("utf8").to_owned();
    env.cmd().args(["run", &run_str]).assert().success();
    let dest = env.dest(&run);

    // Off by default: refused with a hint.
    env.cmd()
        .arg("cleanup")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cleanup --dry-run"));

    // A dry run before the storage side verified: waiting.
    env.set_cleanup("delete_after = \"1s\"");
    std::thread::sleep(Duration::from_millis(1100));
    env.cmd()
        .args(["cleanup", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("waiting").and(predicate::str::contains("storage-side")));

    // The receiving side verifies; now the dry run would delete.
    env.cmd()
        .args(["verify", dest.to_str().expect("utf8")])
        .assert()
        .success();
    env.cmd()
        .args(["cleanup", "--dry-run", &run_str])
        .assert()
        .success()
        .stdout(predicate::str::contains("would_delete"));
    assert!(run.join("pod5").exists(), "a dry run deletes nothing");

    // Deletion on. A file added after completion stays.
    std::fs::write(run.join("added-later.txt"), b"keep").expect("write");
    env.set_cleanup("delete = \"on\"\ndelete_after = \"1s\"");
    env.cmd()
        .arg("cleanup")
        .assert()
        .success()
        .stdout(predicate::str::contains("deleted"));
    let left: Vec<String> = std::fs::read_dir(&run)
        .expect("run folder kept")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, vec!["added-later.txt".to_owned()]);
    assert!(dest.join("DELETED.json").exists());
    let state = env.dir.path().join("state");
    assert!(state.join("deletions.jsonl").exists());
    assert_eq!(
        std::fs::read_to_string(state.join("cleanup-reports.jsonl"))
            .expect("reports")
            .lines()
            .count(),
        3,
        "dry runs and the deletion are all reported"
    );
    // The copy is intact and still verifies.
    env.cmd()
        .args(["verify", dest.to_str().expect("utf8")])
        .assert()
        .success();
    // Nothing left to do.
    env.cmd()
        .arg("cleanup")
        .assert()
        .success()
        .stdout(predicate::str::contains("0 deleted"));
}
