// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end tests of `run`, `verify` and `check-config` through the
//! binary, on runs made by ont-runsim and a "share" on `/dev/shm` (tmpfs, a
//! different device from the temp dir). Each test skips when `/dev/shm` is
//! missing or on the same device.
#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, missing_docs)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use assert_cmd::Command;
use predicates::prelude::*;

const TARGET: &str = "lab-share-01";

struct Env {
    dir: tempfile::TempDir,
    share: tempfile::TempDir,
    config: PathBuf,
}

impl Env {
    fn new(with_sentinel: bool) -> Option<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let share = tempfile::tempdir_in("/dev/shm").ok()?;
        let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
        if dev(dir.path()) == dev(share.path()) {
            return None;
        }
        if with_sentinel {
            std::fs::write(
                share.path().join(".ont-offload-target"),
                format!("{TARGET}\n"),
            )
            .expect("sentinel");
        }
        for d in ["data", "state"] {
            std::fs::create_dir_all(dir.path().join(d)).expect("mkdir");
        }
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "dest_root = {:?}\ntarget_id = {TARGET:?}\nstate_dir = {:?}\n\
                 [guard]\nallowed_fstypes = [\"tmpfs\"]\n[source]\nroot = {:?}\n",
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

    /// One finished run, made by ont-runsim on a fake clock.
    fn simulate(&self) -> PathBuf {
        let toml = "seed = 5\nhours = 0.5\n[fake]\nsize_scale = 0.0002\n\
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

    fn dest_of(&self, run: &Path) -> PathBuf {
        self.share
            .path()
            .join(run.strip_prefix(self.data()).expect("below data"))
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::cargo_bin("ont-offload").expect("binary builds");
        c.args(["--config", self.config.to_str().expect("utf8")])
            .args(args);
        c
    }
}

fn sim_manifest(data: &Path, run: &Path) -> PathBuf {
    ont_runsim::status::manifests_dir(data).join(format!(
        "{}.sha256",
        run.file_name().expect("name").to_string_lossy()
    ))
}

/// Verifies: SRS-SEL-001, SRS-SEL-006, SRS-SEL-007, SRS-VER-002, SRS-CLI-004
#[test]
fn run_copies_a_simulated_run_that_sha256sum_accepts() {
    let Some(env) = Env::new(true) else { return };
    let run = env.simulate();
    env.cmd(&["run", run.to_str().expect("utf8")])
        .assert()
        .success()
        .stdout(predicate::str::contains("COMPLETE"));
    let dest = env.dest_of(&run);
    for f in ["manifest.sha256", "run.json", "COMPLETE"] {
        assert!(dest.join(f).is_file(), "{f} missing");
    }
    // The simulator's own manifest agrees file by file.
    let rep =
        ont_runsim::check::check(&dest, &sim_manifest(&env.data(), &run), &[]).expect("check");
    assert!(rep.ok(), "{rep:?}");
    // And so does GNU sha256sum, when it is installed.
    if let Ok(out) = std::process::Command::new("sha256sum")
        .args(["--quiet", "-c", "manifest.sha256"])
        .current_dir(&dest)
        .output()
    {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// Verifies: SRS-RES-004
#[test]
fn second_run_reports_already_complete() {
    let Some(env) = Env::new(true) else { return };
    let run = env.simulate();
    let r = run.to_str().expect("utf8");
    env.cmd(&["run", r]).assert().success();
    env.cmd(&["run", r])
        .assert()
        .success()
        .stdout(predicate::str::contains("already complete"));
}

/// Verifies: SRS-VER-003
#[test]
fn verify_writes_the_receipt_and_detects_a_changed_file() {
    let Some(env) = Env::new(true) else { return };
    let run = env.simulate();
    env.cmd(&["run", run.to_str().expect("utf8")])
        .assert()
        .success();
    let dest = env.dest_of(&run);
    let d = dest.to_str().expect("utf8");
    env.cmd(&["verify", d, "--source"]).assert().success();
    assert!(dest.join("VERIFIED.json").is_file());

    std::fs::remove_file(dest.join("VERIFIED.json")).expect("rm");
    let victim = dest.join("final_summary_FAQ00001_aaaaaaaa_00000000.txt");
    let victim = if victim.is_file() {
        victim
    } else {
        std::fs::read_to_string(dest.join("manifest.sha256"))
            .expect("manifest")
            .lines()
            .next()
            .and_then(|l| l.split_once("  "))
            .map(|(_, f)| dest.join(f))
            .expect("a file")
    };
    std::fs::write(&victim, b"tampered").expect("tamper");
    env.cmd(&["verify", d])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("1 changed"));
    assert!(!dest.join("VERIFIED.json").exists());
}

/// Verifies: SRS-GRD-001, SRS-CFG-004
#[test]
fn missing_sentinel_exits_2_and_writes_nothing() {
    let Some(env) = Env::new(false) else { return };
    let run = env.simulate();
    env.cmd(&["run", run.to_str().expect("utf8")])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("sentinel"));
    let left: Vec<_> = std::fs::read_dir(env.share.path()).expect("ls").collect();
    assert!(left.is_empty(), "share was written to: {left:?}");
}

/// Verifies: SRS-SEL-006, SRS-SEL-007, SRS-CFG-004
#[test]
fn check_config_lists_runs_and_their_destinations() {
    let Some(env) = Env::new(true) else { return };
    let run = env.simulate();
    let dest = env.dest_of(&run);
    env.cmd(&["check-config"]).assert().success().stdout(
        predicate::str::contains("runs detected: 1")
            .and(predicate::str::contains(dest.to_str().expect("utf8")))
            .and(predicate::str::contains("complete: final_summary"))
            .and(predicate::str::contains(
                "destination guard OK: sentinel matches, tmpfs",
            )),
    );
}
