// SPDX-License-Identifier: AGPL-3.0-or-later
//! End to end: ont-runsim writes runs, `watch --until-idle` offloads them to
//! a tmpfs "share", `verify` writes the receipts, and `report verify`
//! checks the execution against the simulator's manifests. Skipped when
//! `/dev/shm` is missing or on the same device as the temp dir.
#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, clippy::unwrap_used, missing_docs)]

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
                 [state]\nfree_alert = \"0B\"\n\
                 [report]\nsite = \"Test lab\"\n\
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

    fn simulate(&self) -> Vec<PathBuf> {
        let toml = "seed = 11\nhours = 0.5\n[fake]\nsize_scale = 0.0002\n\
                    [[runs]]\nexperiment = \"exp1\"\nsample = \"s1\"\n\
                    [[runs]]\nexperiment = \"exp1\"\nsample = \"s2\"\n";
        let sc = ont_runsim::scenario::Scenario::from_toml(toml, None).expect("scenario");
        ont_runsim::engine::prepare_out(&self.data()).expect("out");
        let plans = ont_runsim::plan::build(&sc, SystemTime::now()).expect("plan");
        let mut e = ont_runsim::engine::Engine::new(&self.data(), plans).expect("engine");
        let mut t = 0.0;
        while !e.done() {
            e.advance_to(t).expect("advance");
            t += 300.0;
        }
        e.write_manifests().expect("manifests");
        e.run_dirs()
    }

    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("ont-offload").expect("binary builds");
        c.args(["--config", self.config.to_str().expect("utf8")])
            .timeout(Duration::from_secs(180));
        c
    }

    fn report(&self, protocol: &str, out: &str) -> assert_cmd::assert::Assert {
        self.cmd()
            .args(["report", "verify", "--sim"])
            .arg(self.data())
            .args([
                "--protocol",
                protocol,
                "--operator",
                "Test operator",
                "--format",
            ])
            .arg(if cfg!(feature = "report") {
                "pdf,json"
            } else {
                "json"
            })
            .arg("--out")
            .arg(self.dir.path().join(out))
            .assert()
    }

    fn record(&self, out: &str) -> serde_json::Value {
        let dir = self.dir.path().join(out);
        let json = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "json"))
            .expect("json record");
        serde_json::from_slice(&std::fs::read(json).unwrap()).unwrap()
    }
}

fn result<'a>(rec: &'a serde_json::Value, id: &str) -> &'a str {
    rec["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .and_then(|c| c["result"].as_str())
        .unwrap_or("absent")
}

/// Verifies: SRS-RPT-003, SRS-RPT-004, SRS-RPT-005
#[test]
fn a_clean_simulation_passes_and_a_changed_copy_fails() {
    let Some(env) = Env::new() else { return };
    let runs = env.simulate();
    env.cmd().args(["watch", "--until-idle"]).assert().success();
    let dests: Vec<PathBuf> = runs
        .iter()
        .map(|r| env.share.path().join(r.strip_prefix(env.data()).unwrap()))
        .collect();
    for d in &dests {
        env.cmd().arg("verify").arg(d).assert().success();
    }

    env.report("simulation", "pass")
        .success()
        .stdout(predicate::str::contains(": PASS ("));
    let rec = env.record("pass");
    assert_eq!(rec["verdict"]["status"], "pass", "{:#}", rec["checks"]);
    assert_eq!(rec["simulated"], true);
    assert_eq!(rec["runs"].as_array().unwrap().len(), 2);
    for id in [
        "VC-01", "VC-02", "VC-03", "VC-04", "VC-10", "VC-11", "VC-13", "VC-14",
    ] {
        assert_eq!(result(&rec, id), "pass", "{id}: {:#}", rec["checks"]);
    }
    assert_eq!(result(&rec, "VC-08"), "n/a");
    assert_eq!(rec["site"], "Test lab");
    if cfg!(feature = "report") {
        let pdf = std::fs::read_dir(env.dir.path().join("pass"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "pdf"))
            .expect("pdf");
        assert!(std::fs::read(pdf).unwrap().starts_with(b"%PDF-"));
    }

    // Site acceptance also needs an outage drill and a restart: not decided.
    env.report("p2i-acceptance", "drill").code(5);
    let rec = env.record("drill");
    assert_eq!(rec["verdict"]["status"], "inconclusive");
    assert_eq!(result(&rec, "VC-08"), "inconclusive");
    assert_eq!(result(&rec, "VC-09"), "inconclusive");

    // A copy that no longer matches the reference fails, even though
    // ont-offload's own manifest still lists the old digest.
    let victim = std::fs::read_dir(dests[0].join("pod5"))
        .ok()
        .and_then(|mut d| d.next())
        .map_or_else(
            || dests[0].join(ont_runsim::LABEL_FILE),
            |e| e.unwrap().path(),
        );
    std::fs::write(&victim, b"changed on the share").unwrap();
    std::fs::write(dests[1].join("stray.txt"), b"not from the run").unwrap();
    env.report("simulation", "fail").code(3);
    let rec = env.record("fail");
    assert_eq!(rec["verdict"]["status"], "fail");
    assert_eq!(result(&rec, "VC-03"), "fail");
    let runs = rec["runs"].as_array().unwrap();
    assert_eq!(
        runs.iter()
            .map(|r| r["changed"].as_array().unwrap().len())
            .sum::<usize>(),
        1
    );
    assert_eq!(
        runs.iter()
            .map(|r| r["extra"].as_array().unwrap().len())
            .sum::<usize>(),
        1
    );
}

/// Verifies: SRS-RPT-003
#[test]
fn a_missing_reference_is_inconclusive() {
    let Some(env) = Env::new() else { return };
    env.cmd()
        .args(["report", "verify", "--format", "json", "--sim"])
        .arg(env.dir.path().join("no-such-sim"))
        .arg("--out")
        .arg(env.dir.path().join("none"))
        .assert()
        .code(5)
        .stdout(predicate::str::contains("INCONCLUSIVE"));
}

/// Verifies: SRS-RPT-003
#[test]
fn a_minknow_run_that_is_not_there_is_refused() {
    let Some(env) = Env::new() else { return };
    env.cmd()
        .args(["report", "verify", "--format", "json", "--minknow"])
        .arg(env.data().join("exp1/s1/no-such-run"))
        .arg("--out")
        .arg(env.dir.path().join("none"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not a run folder"));
    assert!(!env.dir.path().join("none").exists());
}
