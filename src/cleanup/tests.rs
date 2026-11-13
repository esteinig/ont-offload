// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the deletion gates, the deletion mechanics and the pass
//! report, on simulated MinKNOW runs (ont-runsim) offloaded to a fake share,
//! with fault injection.
#![allow(
    clippy::case_sensitive_file_extension_comparisons,
    clippy::cast_possible_truncation
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::*;
use crate::config::{self, Config};
use crate::fsio::faulty::{Fault, FaultyFs, Op, Rule};
use crate::fsio::{RealFs, errno};
use crate::guard::{Guard, SENTINEL};
use crate::journal::{COMPLETE, Journal, VERIFIED};
use crate::run;

const TARGET: &str = "lab-share-01";
const DAY: Duration = Duration::from_secs(86_400);

struct Env {
    dir: tempfile::TempDir,
    fs: FaultyFs,
    mountinfo: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["data", "share", "state"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        let share = std::fs::canonicalize(dir.path().join("share")).unwrap();
        std::fs::write(share.join(SENTINEL), format!("{TARGET}\n")).unwrap();
        let mountinfo = dir.path().join("mountinfo");
        std::fs::write(
            &mountinfo,
            format!(
                "22 1 8:1 / / rw - ext4 /dev/sda1 rw\n40 22 0:50 / {} rw - nfs4 nas01:/ont rw\n",
                share.display()
            ),
        )
        .unwrap();
        let env = Self {
            dir,
            fs: FaultyFs::new(Arc::new(RealFs)),
            mountinfo,
        };
        env.reset_faults();
        env
    }

    /// Only the rule that puts the share on its own device.
    fn reset_faults(&self) {
        self.fs.clear();
        self.fs.add(Rule::always(
            Op::Metadata,
            "/share",
            Fault::Device(9_999_999),
        ));
    }

    fn p(&self, rel: &str) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap().join(rel)
    }

    fn cfg(&self, cleanup: &str, transfer: &str) -> Config {
        let path = self.dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "dest_root = {:?}\ntarget_id = {TARGET:?}\nstate_dir = {:?}\n\
                 [source]\nroot = {:?}\n\
                 [transfer]\nmin_free = \"1MB\"\n{transfer}\n\
                 [cleanup]\n{cleanup}\n",
                self.p("share").to_str().unwrap(),
                self.p("state").to_str().unwrap(),
                self.p("data").to_str().unwrap(),
            ),
        )
        .unwrap();
        config::load(&path, &self.fs).unwrap().config
    }

    fn guard(&self, cfg: &Config) -> Guard {
        Guard::new(cfg).with_mountinfo(&self.mountinfo)
    }

    /// Simulate finished runs into data/ with ont-runsim.
    fn simulate(&self, runs: &[(&str, &str)]) -> Vec<PathBuf> {
        let mut toml = "seed = 5\nhours = 0.5\n[fake]\nsize_scale = 0.0002\n".to_owned();
        for (e, s) in runs {
            toml.push_str(&format!("[[runs]]\nexperiment = {e:?}\nsample = {s:?}\n"));
        }
        let sc = ont_runsim::scenario::Scenario::from_toml(&toml, None).unwrap();
        let out = self.p("data");
        ont_runsim::engine::prepare_out(&out).unwrap();
        let plans = ont_runsim::plan::build(&sc, SystemTime::UNIX_EPOCH).unwrap();
        let mut e = ont_runsim::engine::Engine::new(&out, plans).unwrap();
        let mut t = 0.0;
        while !e.done() {
            e.advance_to(t).unwrap();
            t += 300.0;
        }
        e.run_dirs()
            .into_iter()
            .map(|r| std::fs::canonicalize(r).unwrap())
            .collect()
    }

    /// Offload a run and return its destination folder.
    fn offload(&self, cfg: &Config, run_dir: &Path) -> PathBuf {
        let det = run::detect(cfg, &self.fs)
            .unwrap()
            .runs
            .into_iter()
            .find(|r| r.path == run_dir)
            .expect("run detected");
        let rep = run::offload(cfg, &self.fs, &self.guard(cfg), &det, false).unwrap();
        assert!(rep.failed.is_empty());
        rep.dest
    }

    /// Storage-side verification, as a pipeline on the receiving side does.
    fn verify(&self, dest: &Path) {
        journal::verify(&self.fs, dest, false).unwrap();
    }

    fn pass<'a>(
        &'a self,
        cfg: &'a Config,
        guard: &'a Guard,
        delete: bool,
        now: SystemTime,
    ) -> Pass<'a> {
        Pass {
            cfg,
            fs: &self.fs,
            guard,
            now,
            delete,
            only: None,
            skip: BTreeSet::new(),
            rate: None,
            stop: None,
            record: true,
        }
    }
}

fn files_below(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.strip_prefix(dir).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

fn later(days: u64) -> SystemTime {
    SystemTime::now() + DAY * u32::try_from(days).unwrap()
}

/// One offloaded and storage-verified run, and its destination.
fn ready_run(env: &Env, cfg: &Config) -> (PathBuf, PathBuf) {
    let run = env.simulate(&[("exp1", "s1")]).remove(0);
    let dest = env.offload(cfg, &run);
    env.verify(&dest);
    (run, dest)
}

/// Verifies: SRS-DEL-001
#[test]
fn deletion_is_off_by_default_and_dry_run_deletes_nothing() {
    let env = Env::new();
    let cfg = env.cfg("", "");
    assert_eq!(cfg.cleanup.delete, DeleteMode::Off);
    assert_eq!(mode(&cfg, false), None, "off: no pass at all");
    assert_eq!(mode(&cfg, true), Some(false), "--dry-run reports");
    let on = env.cfg("delete = \"on\"", "");
    assert_eq!(mode(&on, false), Some(true));
    assert_eq!(mode(&on, true), Some(false), "--dry-run wins over on");

    let dry = env.cfg("delete = \"dry-run\"", "");
    let (run, _) = ready_run(&env, &dry);
    let before = files_below(&run);
    let guard = env.guard(&dry);
    let rep = env.pass(&dry, &guard, false, later(8)).run().unwrap();
    assert_eq!(rep.mode, "dry-run");
    assert_eq!(rep.count(Outcome::WouldDelete), 1, "{rep:?}");
    assert_eq!(rep.runs[0].files as usize, before.len());
    assert!(rep.runs[0].evidence.as_ref().unwrap().verified_at.is_some());
    assert_eq!(files_below(&run), before, "nothing deleted");
    assert!(!env.p("state").join(DELETIONS).exists());
    // The report is kept for review.
    let text = std::fs::read_to_string(env.p("state").join(REPORTS)).unwrap();
    let back: Report = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(back, rep);
    assert_eq!(back.schema, REPORT_SCHEMA);
}

/// Verifies: SRS-DEL-002
#[test]
fn finished_gate_waits_for_delete_after_and_complete() {
    let env = Env::new();
    let cfg = env.cfg("delete = \"on\"\ndelete_after = \"7d\"", "");
    let guard = env.guard(&cfg);
    let (run, dest) = ready_run(&env, &cfg);
    let before = files_below(&run);

    let rep = env.pass(&cfg, &guard, true, later(6)).run().unwrap();
    assert_eq!(rep.runs[0].outcome, Outcome::Waiting);
    assert!(rep.runs[0].reason.contains("delete_after"), "{rep:?}");

    // COMPLETE withdrawn (re-offload in progress): wait.
    let complete = std::fs::read(dest.join(COMPLETE)).unwrap();
    std::fs::remove_file(dest.join(COMPLETE)).unwrap();
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.runs[0].outcome, Outcome::Waiting, "{rep:?}");
    // COMPLETE for another manifest: refuse.
    std::fs::write(
        dest.join(COMPLETE),
        format!("manifest_sha256={}\n", "0".repeat(64)),
    )
    .unwrap();
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.runs[0].outcome, Outcome::Refused, "{rep:?}");
    std::fs::write(dest.join(COMPLETE), complete).unwrap();
    assert_eq!(files_below(&run), before, "nothing deleted");
}

/// Verifies: SRS-DEL-002
#[test]
fn storage_verify_gate_needs_a_matching_newer_receipt() {
    let env = Env::new();
    let cfg = env.cfg("delete = \"on\"", "");
    let guard = env.guard(&cfg);
    let (run, dest) = ready_run(&env, &cfg);
    let before = files_below(&run);
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dest.join(VERIFIED)).unwrap()).unwrap();
    let outcome = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut r = receipt.clone();
        edit(&mut r);
        std::fs::write(dest.join(VERIFIED), r.to_string()).unwrap();
        let rep = env.pass(&cfg, &guard, false, later(8)).run().unwrap();
        (rep.runs[0].outcome, rep.runs[0].reason.clone())
    };
    assert_eq!(outcome(&|_| {}).0, Outcome::WouldDelete);
    assert_eq!(
        outcome(&|r| r["verified_at"] = "2001-01-01T00:00:00Z".into()).0,
        Outcome::Waiting,
        "a receipt from before the completion is stale"
    );
    assert_eq!(
        outcome(&|r| r["manifest_sha256"] = "1".repeat(64).into()).0,
        Outcome::Refused
    );
    assert_eq!(outcome(&|r| r["files"] = 1.into()).0, Outcome::Refused);
    assert_eq!(outcome(&|r| r["bytes"] = 7.into()).0, Outcome::Refused);
    assert_eq!(
        outcome(&|r| r["result"] = "failed".into()).0,
        Outcome::Refused
    );
    std::fs::remove_file(dest.join(VERIFIED)).unwrap();
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.runs[0].outcome, Outcome::Waiting);
    assert!(rep.runs[0].reason.contains("storage-side"), "{rep:?}");
    assert_eq!(files_below(&run), before, "nothing deleted");
}

/// Verifies: SRS-DEL-002
#[test]
fn readback_gate_rereads_the_share_and_catches_a_silent_change() {
    let env = Env::new();
    let cfg = env.cfg("delete = \"on\"\ngate = \"both\"", "verify = \"readback\"");
    let guard = env.guard(&cfg);
    let (run, dest) = ready_run(&env, &cfg);
    let rep = env.pass(&cfg, &guard, false, later(8)).run().unwrap();
    assert_eq!(rep.runs[0].outcome, Outcome::WouldDelete, "{rep:?}");
    assert!(rep.runs[0].evidence.as_ref().unwrap().readback);
    // The share changes a byte after the storage-side receipt: same size,
    // so only reading it back finds it.
    let victim = files_below(&dest)
        .into_iter()
        .find(|f| f.ends_with(".pod5"))
        .unwrap();
    let path = dest.join(&victim);
    let mut data = std::fs::read(&path).unwrap();
    data[0] ^= 1;
    std::fs::write(&path, data).unwrap();
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.runs[0].outcome, Outcome::Refused, "{rep:?}");
    assert!(rep.runs[0].reason.contains("does not match"), "{rep:?}");
    assert!(run.join(&victim).exists(), "nothing deleted");
}

/// Verifies: SRS-DEL-002
#[test]
fn source_and_share_gates_refuse_changes() {
    let env = Env::new();
    let cfg = env.cfg("delete = \"on\"", "");
    let guard = env.guard(&cfg);
    let (run, dest) = ready_run(&env, &cfg);
    let before = files_below(&run);
    let pod5 = before
        .iter()
        .find(|f| f.ends_with(".pod5"))
        .unwrap()
        .clone();

    // Source changed in place with the same size and time: only the
    // re-hash finds it.
    let src = run.join(&pod5);
    let mtime = std::fs::metadata(&src).unwrap().modified().unwrap();
    let orig = std::fs::read(&src).unwrap();
    let mut data = orig.clone();
    data[0] ^= 1;
    std::fs::write(&src, &data).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.runs[0].outcome, Outcome::Refused, "{rep:?}");
    assert!(rep.runs[0].reason.contains("digest"), "{rep:?}");
    let no_rehash = env.cfg("delete = \"on\"\nrehash_source = false", "");
    let rep = env.pass(&no_rehash, &guard, false, later(8)).run().unwrap();
    assert_eq!(
        rep.runs[0].outcome,
        Outcome::WouldDelete,
        "without rehash_source only size and time are compared"
    );
    std::fs::write(&src, &orig).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_modified(mtime)
        .unwrap();

    // Source grew.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&src)
        .unwrap()
        .write_all(b"more")
        .unwrap();
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.runs[0].outcome, Outcome::Refused, "{rep:?}");
    std::fs::write(&src, &orig).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_modified(mtime)
        .unwrap();

    // Destination file truncated.
    let target = dest.join(&pod5);
    let full = std::fs::read(&target).unwrap();
    std::fs::write(&target, &full[..full.len() / 2]).unwrap();
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.runs[0].outcome, Outcome::Refused, "{rep:?}");
    assert!(rep.runs[0].reason.contains("on the share"), "{rep:?}");
    std::fs::write(&target, &full).unwrap();

    // Share not mounted (sentinel unreadable): the pass stops at once.
    env.fs.add(Rule::always(
        Op::Read,
        SENTINEL,
        Fault::Errno(errno::ENOTCONN),
    ));
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert!(rep.runs.is_empty(), "{rep:?}");
    assert!(
        rep.stopped.as_deref().unwrap().contains("destination"),
        "{rep:?}"
    );
    env.reset_faults();
    assert_eq!(files_below(&run), before, "nothing deleted");
}

use std::io::Write as _;

/// Verifies: SRS-DEL-003
#[test]
fn deletes_only_listed_files_with_a_record_of_each() {
    let env = Env::new();
    let cfg = env.cfg("delete = \"on\"", "");
    let guard = env.guard(&cfg);
    let runs = env.simulate(&[("exp1", "s1"), ("exp1", "s2")]);
    let (a, b) = (runs[0].clone(), runs[1].clone());
    let dest_a = env.offload(&cfg, &a);
    let dest_b = env.offload(&cfg, &b);
    env.verify(&dest_a);
    env.verify(&dest_b);
    let listed_a = files_below(&a);
    // A file written after completion is not in the manifest: it stays,
    // and so does the run folder.
    std::fs::write(a.join("notes-after-run.txt"), b"keep me").unwrap();

    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.count(Outcome::Deleted), 2, "{rep:?}");
    assert_eq!(files_below(&a), vec!["notes-after-run.txt".to_owned()]);
    assert!(!b.exists(), "an emptied run folder is removed");
    assert!(b.parent().unwrap().exists(), "folders above the run stay");
    let ra = rep
        .runs
        .iter()
        .find(|r| r.run_id == journal::run_id(&a))
        .unwrap();
    assert_eq!(ra.kept, vec!["notes-after-run.txt".to_owned()]);
    assert_eq!(ra.deleted_files as usize, listed_a.len());

    // One deletion record per file, written before it was removed.
    let log = std::fs::read_to_string(env.p("state").join(DELETIONS)).unwrap();
    let recs: Vec<DeletionRecord> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let for_a: BTreeSet<String> = recs
        .iter()
        .filter(|r| r.run_id == journal::run_id(&a))
        .map(|r| r.rel.clone())
        .collect();
    assert_eq!(for_a, listed_a.iter().cloned().collect());
    assert!(
        recs.iter()
            .all(|r| r.evidence.verified_at.is_some() && r.evidence.source_rehashed)
    );
    // Run markers (top-level files) go last.
    let order: Vec<&DeletionRecord> = recs
        .iter()
        .filter(|r| r.run_id == journal::run_id(&a))
        .collect();
    let first_top = order.iter().position(|r| !r.rel.contains('/')).unwrap();
    assert!(order[first_top..].iter().all(|r| !r.rel.contains('/')));

    // DELETED.json on the share; the destination still verifies.
    let deleted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dest_a.join(DELETED)).unwrap()).unwrap();
    assert_eq!(deleted["files"], listed_a.len());
    assert_eq!(deleted["source_folder_removed"], false);
    journal::verify(&RealFs, &dest_a, false).unwrap();
    let ja = Journal::open(&env.fs, &env.p("state"), &a).unwrap();
    assert!(ja.deleting.is_some() && ja.deleted_at.is_some());

    // A deleted run is never considered again, nor offloaded again.
    let rep = env.pass(&cfg, &guard, true, later(9)).run().unwrap();
    assert!(rep.runs.is_empty(), "{rep:?}");
    std::fs::write(a.join("sample_sheet_x.csv"), b"marker").unwrap();
    let det = run::detect(&cfg, &env.fs)
        .unwrap()
        .runs
        .into_iter()
        .find(|r| r.path == a)
        .unwrap();
    let err = run::offload(&cfg, &env.fs, &guard, &det, true).unwrap_err();
    assert!(err.to_string().contains("deleted by cleanup"), "{err}");
}

/// Verifies: SRS-DEL-004
#[test]
fn a_failure_mid_deletion_stops_the_pass_and_the_next_pass_resumes() {
    let env = Env::new();
    let cfg = env.cfg("delete = \"on\"", "");
    let guard = env.guard(&cfg);
    let runs = env.simulate(&[("exp1", "s1"), ("exp1", "s2")]);
    for r in &runs {
        let d = env.offload(&cfg, r);
        env.verify(&d);
    }
    let total: usize = runs.iter().map(|r| files_below(r).len()).sum();

    // The share goes away after a few deletions: the guard check before
    // the next file fails and the pass stops.
    env.fs.add(Rule::after(
        Op::Read,
        SENTINEL,
        Fault::Errno(errno::ENOTCONN),
        8,
    ));
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert!(rep.stopped.as_deref().unwrap().contains("guard"), "{rep:?}");
    assert_eq!(rep.count(Outcome::Stopped), 1, "{rep:?}");
    assert_eq!(
        rep.count(Outcome::Held),
        1,
        "the other run is not attempted: {rep:?}"
    );
    let left: usize = runs.iter().map(|r| files_below(r).len()).sum();
    assert!(left < total && left > 0, "{left} of {total}");
    env.reset_faults();

    // A removal error stops too, after the record was written: the file
    // is listed but still there.
    env.fs.add(Rule::times(
        Op::RemoveFile,
        "/data/",
        Fault::Errno(errno::EACCES),
        1,
    ));
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert!(
        rep.stopped.as_deref().unwrap().contains("removing"),
        "{rep:?}"
    );
    env.reset_faults();

    // The next pass resumes both runs: files already gone are accepted
    // only because the deletion log lists them.
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.count(Outcome::Deleted), 2, "{rep:?}");
    assert!(rep.stopped.is_none());
    assert!(runs.iter().all(|r| !r.exists()));
}

/// Verifies: SRS-DEL-002, SRS-DEL-004
#[test]
fn a_missing_source_file_not_in_the_deletion_log_is_refused() {
    let env = Env::new();
    let cfg = env.cfg("delete = \"on\"", "");
    let guard = env.guard(&cfg);
    let (run, _) = ready_run(&env, &cfg);
    let victim = files_below(&run)
        .into_iter()
        .find(|f| f.ends_with(".pod5"))
        .unwrap();
    std::fs::remove_file(run.join(&victim)).unwrap();
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.runs[0].outcome, Outcome::Refused, "{rep:?}");
    assert!(run.exists());
}

/// Verifies: SRS-DEL-005
#[test]
fn free_space_condition_deletes_oldest_first_until_enough_is_free() {
    let env = Env::new();
    let cfg = env.cfg("delete = \"on\"\nonly_when_free_below = \"1TB\"", "");
    let guard = env.guard(&cfg);
    let runs = env.simulate(&[("exp1", "s1"), ("exp1", "s2")]);
    // Complete s2 first: it is the older one.
    let d2 = env.offload(&cfg, &runs[1]);
    std::thread::sleep(Duration::from_millis(1100));
    let d1 = env.offload(&cfg, &runs[0]);
    env.verify(&d1);
    env.verify(&d2);

    // Plenty of space: nothing deleted.
    env.fs.add(Rule::always(
        Op::FreeSpace,
        "/data",
        Fault::Free(2_000_000_000_000),
    ));
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(rep.count(Outcome::Held), 2, "{rep:?}");
    assert!(runs.iter().all(|r| r.exists()));
    env.reset_faults();

    // One byte short: the oldest run alone frees enough.
    env.fs.add(Rule::always(
        Op::FreeSpace,
        "/data",
        Fault::Free(1_000_000_000_000 - 1),
    ));
    let rep = env.pass(&cfg, &guard, true, later(8)).run().unwrap();
    assert_eq!(
        rep.runs[0].run_id,
        journal::run_id(&runs[1]),
        "oldest first"
    );
    assert_eq!(rep.runs[0].outcome, Outcome::Deleted, "{rep:?}");
    assert_eq!(rep.runs[1].outcome, Outcome::Held, "{rep:?}");
    assert!(!runs[1].exists() && runs[0].exists());
    env.reset_faults();
}

/// Verifies: SRS-DEL-006
#[test]
fn skipped_and_other_runs_are_left_alone() {
    let env = Env::new();
    let cfg = env.cfg("delete = \"on\"", "");
    let guard = env.guard(&cfg);
    let runs = env.simulate(&[("exp1", "s1"), ("exp1", "s2")]);
    for r in &runs {
        let d = env.offload(&cfg, r);
        env.verify(&d);
    }
    let mut pass = env.pass(&cfg, &guard, true, later(8));
    pass.skip.insert(journal::run_id(&runs[0]));
    pass.only = None;
    let rep = pass.run().unwrap();
    assert_eq!(rep.runs.len(), 1);
    assert!(runs[0].exists() && !runs[1].exists());
    let mut pass = env.pass(&cfg, &guard, false, later(8));
    pass.only = Some(runs[0].clone());
    let rep = pass.run().unwrap();
    assert_eq!(rep.runs.len(), 1);
    assert_eq!(rep.runs[0].outcome, Outcome::WouldDelete);
}
