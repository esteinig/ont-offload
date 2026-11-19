// SPDX-License-Identifier: AGPL-3.0-or-later
//! Resuming a stopped simulation: on a fake clock, and the CLI end to end.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use assert_cmd::Command;
use ont_runsim::engine::{Engine, Phase, prepare_out};
use ont_runsim::scenario::Scenario;
use ont_runsim::{Error, check, plan, status};
use predicates::prelude::*;

const SCENARIO: &str = "hours = 2.0\n[fake]\nsize_scale = 0.0001\n[faults]\nrewrite_closed = 2\nlate_files = 1\n[[runs]]\nexperiment = \"e\"\nsample = \"a\"\n[[runs]]\nexperiment = \"e\"\nsample = \"b\"\nstart_after = \"30m\"\n";

fn scenario(text: &str) -> Scenario {
    Scenario::from_toml(text, None).unwrap()
}

fn plans(sc: &Scenario) -> Vec<plan::RunPlan> {
    plan::build(sc, SystemTime::UNIX_EPOCH).unwrap()
}

/// Advance on a fake clock from `t` in `step` until done; returns the end time.
fn finish(e: &mut Engine, mut t: f64, step: f64) -> f64 {
    while !e.done() {
        e.advance_to(t).unwrap();
        t += step;
        assert!(t < 400.0 * 3600.0, "simulation does not finish");
    }
    t
}

fn manifests(out: &Path) -> Vec<(String, String)> {
    let mut v: Vec<_> = std::fs::read_dir(status::manifests_dir(out))
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read_to_string(&p).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

/// The same scenario run without a stop.
fn uninterrupted(sc: &Scenario, out: &Path) -> Vec<(String, String)> {
    prepare_out(out).unwrap();
    let mut e = Engine::new(out, plans(sc)).unwrap();
    finish(&mut e, 0.0, 97.0);
    manifests(out)
}

/// Files of the plan that are on disk but not yet at their planned size.
fn growing(sc: &Scenario, out: &Path) -> Vec<PathBuf> {
    plans(sc)
        .iter()
        .flat_map(|p| {
            p.files.iter().filter_map(|f| {
                let path = out.join(p.id.rel_dir()).join(&f.rel);
                let len = std::fs::metadata(&path).ok()?.len();
                (len < f.size).then_some(path)
            })
        })
        .collect()
}

fn events(out: &Path, name: &str) -> usize {
    status::read_events(out)
        .unwrap()
        .iter()
        .filter(|e| e.event == name)
        .count()
}

#[test]
fn resumed_run_writes_the_same_bytes_as_an_uninterrupted_one() {
    let dir = tempfile::tempdir().unwrap();
    let sc = scenario(SCENARIO);
    let expected = uninterrupted(&sc, &dir.path().join("whole"));

    let out = dir.path().join("sim");
    prepare_out(&out).unwrap();
    let mut e = Engine::new(&out, plans(&sc)).unwrap();
    // Stop in the middle of a batch: files are half written, run b waits.
    e.advance_to(1500.0).unwrap();
    assert_eq!(e.phases(), vec![Phase::Sequencing, Phase::Waiting]);
    drop(e);
    assert!(
        !growing(&sc, &out).is_empty(),
        "the stop must leave growing files"
    );

    let (mut e, notes) = Engine::resume(&out, plans(&sc)).unwrap();
    assert!(notes.is_empty(), "{notes:?}");
    assert!(e.open_files() > 0);
    assert_eq!(e.phases(), vec![Phase::Sequencing, Phase::Waiting]);
    // Simulated time continues where it stopped.
    finish(&mut e, 1500.0, 61.0);
    assert_eq!(e.phases(), vec![Phase::Complete, Phase::Complete]);
    assert_eq!(manifests(&out), expected);
    for run in e.run_dirs() {
        let m = status::manifests_dir(&out).join(format!(
            "{}.sha256",
            run.file_name().unwrap().to_string_lossy()
        ));
        assert!(check::check(&run, &m, &[]).unwrap().ok());
    }
    assert_eq!(events(&out, "fault_rewrite"), 4);
    assert_eq!(events(&out, "run_complete"), 2);
}

#[test]
fn stops_after_rewrites_and_twice_in_a_row_still_match() {
    let dir = tempfile::tempdir().unwrap();
    let sc = scenario(SCENARIO);
    let expected = uninterrupted(&sc, &dir.path().join("whole"));
    let out = dir.path().join("sim");
    prepare_out(&out).unwrap();
    let mut e = Engine::new(&out, plans(&sc)).unwrap();
    let last_rewrite = plans(&sc)[0]
        .rewrites
        .iter()
        .map(|r| r.at_s)
        .fold(0.0, f64::max);
    e.advance_to(last_rewrite + 30.0).unwrap();
    drop(e);
    let (e, _) = Engine::resume(&out, plans(&sc)).unwrap();
    drop(e); // stopped again before anything happened
    let (mut e, _) = Engine::resume(&out, plans(&sc)).unwrap();
    finish(&mut e, last_rewrite + 30.0, 45.0);
    assert_eq!(manifests(&out), expected);
    // Run a's rewrites were not redone.
    assert_eq!(events(&out, "fault_rewrite"), 4);
}

#[test]
fn torn_tail_and_lost_manifest_are_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let sc = scenario(SCENARIO);
    let expected = uninterrupted(&sc, &dir.path().join("whole"));
    let out = dir.path().join("sim");
    prepare_out(&out).unwrap();
    let mut e = Engine::new(&out, plans(&sc)).unwrap();
    e.advance_to(2700.0).unwrap();
    let runs = e.run_dirs();
    drop(e);
    // Junk after the last good byte of a growing file (as after a power cut),
    // and run a's manifest lost: its closed files must be checked again.
    let torn = growing(&sc, &out).remove(0);
    let good = std::fs::metadata(&torn).unwrap().len();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&torn)
        .unwrap()
        .write_all(&[0xA5; 300])
        .unwrap();
    std::fs::remove_file(status::manifests_dir(&out).join(format!(
        "{}.sha256",
        runs[0].file_name().unwrap().to_string_lossy()
    )))
    .unwrap();

    let (mut e, notes) = Engine::resume(&out, plans(&sc)).unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0]
            .1
            .contains(&format!("kept {good} of {}", good + 300))
    );
    assert!(std::fs::metadata(&torn).unwrap().len() <= good);
    finish(&mut e, 2700.0, 60.0);
    assert_eq!(manifests(&out), expected);
}

#[test]
fn aborted_and_finished_runs_stay_as_they_are() {
    let dir = tempfile::tempdir().unwrap();
    let text = format!("{SCENARIO}[faults]\nabort_after = \"20m\"\n").replace(
        "[faults]\nrewrite_closed = 2\nlate_files = 1\n[[runs]]",
        "[[runs]]",
    );
    let sc = scenario(&text);
    let out = dir.path().join("sim");
    prepare_out(&out).unwrap();
    let mut e = Engine::new(&out, plans(&sc)).unwrap();
    e.advance_to(1800.0).unwrap();
    assert_eq!(e.phases()[0], Phase::Aborted);
    drop(e);
    let before = manifests(&out);
    let (mut e, _) = Engine::resume(&out, plans(&sc)).unwrap();
    assert_eq!(e.phases()[0], Phase::Aborted);
    e.advance_to(1800.0).unwrap();
    assert_eq!(manifests(&out)[0], before[0]);
    finish(&mut e, 1800.0, 60.0);
    assert_eq!(events(&out, "run_aborted"), 2, "one per run, none repeated");
}

#[test]
fn a_plan_from_elsewhere_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    prepare_out(&out).unwrap();
    let mut e = Engine::new(&out, plans(&scenario(SCENARIO))).unwrap();
    e.advance_to(600.0).unwrap();
    drop(e);
    let other = scenario(&SCENARIO.replace("experiment = \"e\"", "experiment = \"x\""));
    assert!(matches!(
        Engine::resume(&out, plans(&other)),
        Err(Error::Refused(_))
    ));
}

fn bin() -> Command {
    Command::cargo_bin("ont-runsim").unwrap()
}

#[test]
fn cli_resume_after_a_kill() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    let out_s = out.to_str().unwrap();
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("ont-runsim"))
        .args([
            "run",
            "--preset",
            "quick-2h",
            "--out",
            out_s,
            "--speed",
            "200",
            "--size-scale",
            "0.0001",
            "--keep-free",
            "0B",
            "-q",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if status::Status::read(&out).is_ok_and(|s| s.files_closed > 2) {
            break;
        }
        assert!(Instant::now() < deadline, "simulation did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A live simulation is not resumed twice.
    bin()
        .args(["run", "--resume", "--out", out_s])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already running"));
    child.kill().unwrap();
    child.wait().unwrap();
    let stopped = status::Status::read(&out).unwrap();
    bin()
        .args(["status", "--out", out_s])
        .assert()
        .success()
        .stdout(predicate::str::contains("stopped (process gone)"))
        .stdout(predicate::str::contains("run --resume"));

    bin()
        .args([
            "run",
            "--resume",
            "--out",
            out_s,
            "--speed",
            "100000",
            "--keep-free",
            "0B",
            "-q",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("complete"));
    let st = status::Status::read(&out).unwrap();
    assert_eq!(st.state, "finished");
    assert_eq!(st.started, stopped.started);
    assert!((st.speed - 100_000.0).abs() < f64::EPSILON);
    let ev = status::read_events(&out).unwrap();
    let resumed = ev
        .iter()
        .find(|e| e.event == "simulation_resumed")
        .expect("resume event");
    assert!(resumed.sim_s >= (stopped.sim_seconds * 10.0).round() / 10.0 - 0.1);
    let run = PathBuf::from(&st.runs[0].path);
    bin()
        .args(["check", "--dest", run.to_str().unwrap(), "--out", out_s])
        .assert()
        .success();
    // Finished: nothing to resume.
    bin()
        .args(["run", "--resume", "--out", out_s])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("finished"));
}

#[test]
fn cli_resume_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    let out_s = out.to_str().unwrap();
    bin()
        .args(["run", "--resume", "--out", out_s])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("nothing to resume"));
    assert!(!out.exists(), "a refused resume creates nothing");
    for extra in [
        &["--hours", "1"][..],
        &["--preset", "quick-2h"],
        &["--dry-run"],
    ] {
        bin()
            .args(["run", "--resume", "--out", out_s])
            .args(extra)
            .assert()
            .code(2);
    }
}
