// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end tests of `watch` through the binary, on a run made by
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

/// Verifies: SRS-PAR-001, SRS-LIVE-003, SRS-VER-002, SRS-RES-007, SRS-RES-003
#[test]
fn watch_until_idle_offloads_and_notifies_systemd() {
    let Some(env) = Env::new() else { return };
    let run = env.simulate();
    let sock_path = env.dir.path().join("notify.sock");
    let sock = std::os::unix::net::UnixDatagram::bind(&sock_path).expect("bind");
    sock.set_read_timeout(Some(Duration::from_secs(1)))
        .expect("timeout");

    env.cmd()
        .env("NOTIFY_SOCKET", &sock_path)
        .args(["watch", "--until-idle"])
        .assert()
        .success()
        .stderr(predicate::str::contains("COMPLETE"));

    let dest = env
        .share
        .path()
        .join(run.strip_prefix(env.data()).expect("below data"));
    let manifest = ont_runsim::status::manifests_dir(&env.data()).join(format!(
        "{}.sha256",
        run.file_name().expect("name").to_string_lossy()
    ));
    let rep = ont_runsim::check::check(&dest, &manifest, &[]).expect("check");
    assert!(rep.ok(), "{rep:?}");
    if let Ok(out) = std::process::Command::new("sha256sum")
        .args(["--quiet", "-c", "manifest.sha256"])
        .current_dir(&dest)
        .output()
    {
        assert!(out.status.success());
    }
    let run_json = std::fs::read_to_string(dest.join("run.json")).expect("run.json");
    assert!(run_json.contains("\"mode\": \"watch\""));

    let mut buf = [0u8; 1024];
    let mut got = Vec::new();
    while let Ok(n) = sock.recv(&mut buf) {
        got.push(String::from_utf8_lossy(&buf[..n]).into_owned());
    }
    assert!(got.iter().any(|m| m.starts_with("READY=1")), "{got:?}");
    assert!(got.iter().any(|m| m == "STOPPING=1"), "{got:?}");
    // The state folder lock is released on exit.
    assert!(!env.dir.path().join("state/ont-offload.lock").exists());
}

/// Verifies: SRS-RES-006
#[test]
fn watch_refuses_a_share_of_another_target() {
    let Some(env) = Env::new() else { return };
    std::fs::write(env.share.path().join(".ont-offload-target"), "other\n").expect("sentinel");
    env.cmd()
        .args(["watch"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("refusing to start"));
}

/// Verifies: SRS-RES-007
#[test]
fn second_instance_on_the_same_state_folder_is_refused() {
    let Some(env) = Env::new() else { return };
    std::fs::write(env.dir.path().join("state/ont-offload.lock"), "1\n").expect("lock");
    env.cmd()
        .args(["watch", "--until-idle"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("another ont-offload"));
}

/// Verifies: SRS-STATE-001, SRS-STATE-002, SRS-CLI-001
#[test]
fn watch_publishes_state_that_status_prints() {
    let Some(env) = Env::new() else { return };
    let run = env.simulate();
    let name = run
        .strip_prefix(env.data())
        .expect("below data")
        .to_string_lossy()
        .into_owned();
    // Before the daemon ever ran: a clear error.
    env.cmd()
        .args(["status"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("is `ont-offload watch` running?"));
    env.cmd().args(["watch", "--until-idle"]).assert().success();

    let state = env.dir.path().join("state");
    let snap: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(state.join("state.json")).expect("state.json"),
    )
    .expect("json");
    assert_eq!(snap["schema"], "ont-offload/state/v1");
    assert_eq!(snap["schema_version"], 1);
    assert_eq!(snap["status"], "running");
    assert_eq!(snap["destination"]["fstype"], "tmpfs");
    assert_eq!(snap["destination"]["guard_ok"], true);
    assert_eq!(snap["totals"]["completed_runs"], 1);
    assert_eq!(snap["runs"][0]["phase"], "complete");
    assert_eq!(snap["runs"][0]["name"], name.as_str());
    assert_eq!(snap["rate"]["limit_bytes_per_s"], 200_000_000);
    assert_eq!(snap["alerts"], serde_json::json!([]));

    let events = std::fs::read_to_string(state.join("events.jsonl")).expect("events");
    let kinds: Vec<String> = events
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("event json");
            v["kind"].as_str().expect("kind").to_owned()
        })
        .collect();
    for k in [
        "started",
        "destination_ok",
        "run_discovered",
        "file_committed",
        "run_state_changed",
        "run_complete",
    ] {
        assert!(kinds.iter().any(|x| x == k), "{k} missing: {kinds:?}");
    }

    env.cmd().args(["status"]).assert().success().stdout(
        predicate::str::contains("status: running")
            .and(predicate::str::contains(name.as_str()))
            .and(predicate::str::contains("complete"))
            .and(predicate::str::contains("alerts: none")),
    );
    let out = env.cmd().args(["status", "--json"]).output().expect("run");
    assert!(out.status.success());
    let raw: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(raw["runs"][0]["run_id"], snap["runs"][0]["run_id"]);
}
