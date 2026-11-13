// SPDX-License-Identifier: AGPL-3.0-or-later
//! Simulator tests on a fake clock, and the CLI end to end.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use assert_cmd::Command;
use ont_runsim::engine::{Engine, Phase, prepare_out};
use ont_runsim::scenario::Scenario;
use ont_runsim::{LABEL_FILE, check, clean, plan, profile, status};
use predicates::prelude::*;

fn tiny(text: &str) -> Scenario {
    let mut sc = Scenario::from_toml(text, None).unwrap();
    sc.fake.size_scale = 0.00001;
    sc
}

/// Drive a scenario to the end on a fake clock in `step` simulated seconds.
fn drive(sc: &Scenario, out: &Path, step: f64) -> Engine {
    prepare_out(out).unwrap();
    let mut e = Engine::new(out, plan::build(sc, SystemTime::UNIX_EPOCH).unwrap()).unwrap();
    let mut t = 0.0;
    while !e.done() {
        e.advance_to(t).unwrap();
        t += step;
        assert!(t < 400.0 * 3600.0, "simulation does not finish");
    }
    e
}

fn manifest_for(out: &Path, run: &Path) -> PathBuf {
    status::manifests_dir(out).join(format!(
        "{}.sha256",
        run.file_name().unwrap().to_string_lossy()
    ))
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let p = entry.unwrap().path();
        let dest = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dest);
        } else {
            std::fs::copy(&p, &dest).unwrap();
        }
    }
}

#[test]
fn seventy_two_hour_fake_run_matches_its_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    let sc = tiny("hours = 72.0\n[synthetic]\nbatch_every = \"10m\"\nbarcodes = 2\n");
    let e = drive(&sc, &out, 600.0);
    let run = &e.run_dirs()[0];
    assert_eq!(e.phases(), vec![Phase::Complete]);
    let report = check::check(run, &manifest_for(&out, run), &[]).unwrap();
    assert!(report.ok(), "{report:?}");
    let pod5 = std::fs::read_dir(run.join("pod5")).unwrap().count();
    assert_eq!(pod5, 432);
    assert!(run.join(LABEL_FILE).is_file());
    assert!(run.join("fastq_pass/barcode02").is_dir());
    let names: Vec<String> = std::fs::read_dir(run)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().any(|n| n.starts_with("final_summary_")));
    assert!(names.iter().any(|n| n.starts_with("sample_sheet_")));
    let events = std::fs::read_to_string(status::events_path(&out)).unwrap();
    assert!(events.contains("\"run_complete\""), "{events}");
}

#[test]
fn files_grow_while_the_run_is_live() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    let sc = Scenario::from_toml("hours = 1.0\n[fake]\nsize_scale = 0.001\n", None).unwrap();
    prepare_out(&out).unwrap();
    let mut e = Engine::new(&out, plan::build(&sc, SystemTime::UNIX_EPOCH).unwrap()).unwrap();
    e.advance_to(300.0).unwrap(); // half way through the first 10 minute batch
    let run = &e.run_dirs()[0];
    let pod5: Vec<_> = std::fs::read_dir(run.join("pod5"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(pod5.len(), 1);
    let half = std::fs::metadata(&pod5[0]).unwrap().len();
    assert!(half > 200_000 && half < 300_000, "{half}");
    assert!(!std::fs::read_dir(run).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("final_summary_")
    }));
    // The manifest only lists closed files.
    let m = check::read_manifest(&manifest_for(&out, run)).unwrap();
    assert!(!m.keys().any(|k| k.starts_with("pod5/")));
    e.advance_to(700.0).unwrap();
    let full = std::fs::metadata(&pod5[0]).unwrap().len();
    assert!(full > half);
    let m = check::read_manifest(&manifest_for(&out, run)).unwrap();
    assert!(m.keys().any(|k| k.starts_with("pod5/")));
}

#[test]
fn check_reports_missing_changed_and_extra() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    let e = drive(&tiny("hours = 1.0"), &out, 300.0);
    let run = &e.run_dirs()[0];
    let copy = dir.path().join("share").join(run.file_name().unwrap());
    copy_dir(run, &copy);
    let manifest = manifest_for(&out, run);
    assert!(check::check(&copy, &manifest, &[]).unwrap().ok());

    let pod5: Vec<_> = std::fs::read_dir(copy.join("pod5"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    std::fs::remove_file(&pod5[0]).unwrap();
    let mut bytes = std::fs::read(&pod5[1]).unwrap();
    bytes[0] ^= 1;
    std::fs::write(&pod5[1], bytes).unwrap();
    std::fs::write(copy.join("stray.txt"), b"x").unwrap();
    std::fs::write(copy.join("COMPLETE"), b"").unwrap();
    let r = check::check(&copy, &manifest, &[]).unwrap();
    assert_eq!(
        (r.missing.len(), r.changed.len(), r.extra),
        (1, 1, vec!["stray.txt".to_owned()])
    );
    let r = check::check(&copy, &manifest, &["stray.txt".to_owned()]).unwrap();
    assert!(r.extra.is_empty());
}

#[test]
fn abort_fault_leaves_no_final_summary() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    let e = drive(
        &tiny("hours = 2.0\n[faults]\nabort_after = \"45m\"\n"),
        &out,
        120.0,
    );
    assert_eq!(e.phases(), vec![Phase::Aborted]);
    let run = &e.run_dirs()[0];
    assert!(!std::fs::read_dir(run).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("final_summary_")
    }));
    assert!(
        check::check(run, &manifest_for(&out, run), &[])
            .unwrap()
            .ok()
    );
}

#[test]
fn rewrite_fault_changes_a_closed_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    let sc = Scenario::from_toml(
        "hours = 1.0\n[fake]\nsize_scale = 0.0001\n[faults]\nrewrite_closed = 2\nlate_files = 1\n",
        None,
    )
    .unwrap();
    let plans = plan::build(&sc, SystemTime::UNIX_EPOCH).unwrap();
    let first = plans[0]
        .rewrites
        .iter()
        .map(|r| r.at_s)
        .fold(f64::INFINITY, f64::min);
    prepare_out(&out).unwrap();
    let mut e = Engine::new(&out, plans).unwrap();
    e.advance_to(first - 1.0).unwrap();
    let run = e.run_dirs()[0].clone();
    let early = dir.path().join("early").join(run.file_name().unwrap());
    copy_dir(&run, &early);
    let mut t = first;
    while !e.done() {
        e.advance_to(t).unwrap();
        t += 60.0;
    }
    let manifest = manifest_for(&out, &run);
    assert!(check::check(&run, &manifest, &[]).unwrap().ok());
    let r = check::check(&early, &manifest, &[]).unwrap();
    assert!(
        !r.changed.is_empty(),
        "copy taken before the rewrite must differ: {r:?}"
    );
    assert!(run.join("other_reports/late_file_0.txt").is_file());
}

#[test]
fn profile_record_and_replay_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    let e = drive(&tiny("hours = 1.0"), &out, 300.0);
    let original = e.run_dirs()[0].clone();
    let p = profile::record(&original).unwrap();
    assert!(p.files.iter().any(|f| f.path.contains("{flow_cell}")));
    let json = dir.path().join("profile.json");
    p.save(&json).unwrap();
    let loaded = profile::Profile::load(&json).unwrap();
    assert_eq!(loaded.files.len(), p.files.len());

    let scenario = dir.path().join("replay.toml");
    std::fs::write(
        &scenario,
        format!(
            "seed = 9\nhours = 0.25\ncontent = \"replay\"\ntimeline = \"profile\"\nprofile = \"profile.json\"\n[replay]\nrun = {:?}\n[[runs]]\nexperiment = \"replayed\"\nsample = \"s1\"\n",
            original.to_str().unwrap()
        ),
    )
    .unwrap();
    let sc = Scenario::load(&scenario).unwrap();
    let out2 = dir.path().join("sim2");
    let e2 = drive(&sc, &out2, 30.0);
    let run = &e2.run_dirs()[0];
    assert!(
        check::check(run, &manifest_for(&out2, run), &[])
            .unwrap()
            .ok()
    );
    // A pod5 file's bytes come from the original run.
    let a = std::fs::read_dir(run.join("pod5"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let bytes = std::fs::read(a).unwrap();
    let originals: Vec<Vec<u8>> = std::fs::read_dir(original.join("pod5"))
        .unwrap()
        .map(|e| std::fs::read(e.unwrap().path()).unwrap())
        .collect();
    assert!(
        originals
            .iter()
            .any(|o| o.len() >= 16 && bytes.starts_with(&o[..16]))
    );
}

#[test]
fn clean_removes_only_simulated_runs() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    let e = drive(&tiny("hours = 1.0"), &out, 600.0);
    let keep = out.join("someone-else.txt");
    std::fs::write(&keep, b"keep me").unwrap();
    let c = clean::clean(&out).unwrap();
    assert_eq!(c.removed, e.run_dirs());
    assert!(keep.is_file());
    assert!(!out.join(ont_runsim::STATE_DIR).exists());
    assert!(!c.removed_out);

    let foreign = dir.path().join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(foreign.join("data.pod5"), b"real").unwrap();
    assert!(clean::clean(&foreign).is_err());
    assert!(
        prepare_out(&foreign).is_err(),
        "non-empty folder without marker"
    );
    assert!(foreign.join("data.pod5").is_file());
}

fn bin() -> Command {
    Command::cargo_bin("ont-runsim").unwrap()
}

#[test]
fn cli_run_check_status_clean() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    let out_s = out.to_str().unwrap();
    bin()
        .args([
            "run",
            "--preset",
            "quick-2h",
            "--out",
            out_s,
            "--speed",
            "100000",
            "--size-scale",
            "0.0001",
            "--keep-free",
            "0B",
            "-q",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("complete"));
    let st = status::Status::read(&out).unwrap();
    assert_eq!(st.state, "finished");
    let run = PathBuf::from(&st.runs[0].path);
    bin()
        .args(["check", "--dest", run.to_str().unwrap(), "--out", out_s])
        .assert()
        .success()
        .stdout(predicate::str::contains("files match"));
    std::fs::write(run.join("extra.bin"), b"x").unwrap();
    bin()
        .args(["check", "--dest", run.to_str().unwrap(), "--out", out_s])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("extra: extra.bin"));
    bin()
        .args(["status", "--out", out_s])
        .assert()
        .success()
        .stdout(predicate::str::contains("finished"));
    bin().args(["clean", "--out", out_s]).assert().success();
    assert!(!out.exists());
}

#[test]
fn cli_dry_run_and_presets() {
    bin()
        .arg("presets")
        .assert()
        .success()
        .stdout(predicate::str::contains("outage-drill"));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    bin()
        .args([
            "run",
            "--preset",
            "promethion-24h",
            "--out",
            out.to_str().unwrap(),
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 run").or(predicate::str::contains("files,")));
    assert!(!out.exists(), "dry run writes nothing");
    bin()
        .args(["run", "--out", out.to_str().unwrap()])
        .assert()
        .code(2);
    bin()
        .args([
            "run",
            "--preset",
            "quick-2h",
            "--hours",
            "0",
            "--out",
            out.to_str().unwrap(),
        ])
        .assert()
        .code(2);
}

#[test]
fn keep_free_stops_the_simulation() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("sim");
    bin()
        .args([
            "run",
            "--preset",
            "quick-2h",
            "--out",
            out.to_str().unwrap(),
            "--keep-free",
            "1000TB",
            "-q",
        ])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("keep_free"));
    assert_eq!(status::Status::read(&out).unwrap().state, "stopped");
}
