// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of run detection, mapping, selection and offload, on simulated
//! MinKNOW runs (ont-runsim) and with fault injection.
#![allow(
    clippy::type_complexity,
    clippy::case_sensitive_file_extension_comparisons
)]

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use super::*;
use crate::config::{self, Config, DestLevel};
use crate::copy::{self, CopyFail, CopyOptions};
use crate::fsio::faulty::{Fault, FaultyFs, Op, Rule};
use crate::fsio::{DirEntry, FileStat, FsFile, RealFs, errno};
use crate::guard::{Guard, SENTINEL};
use crate::journal::{self, COMPLETE, MANIFEST, RUN_JSON, VERIFIED};

const TARGET: &str = "lab-share-01";

/// Source, a "share" on its own device and mount, and a state folder.
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
        let fs = FaultyFs::new(Arc::new(RealFs));
        fs.add(Rule::always(
            Op::Metadata,
            "/share",
            Fault::Device(9_999_999),
        ));
        Self { dir, fs, mountinfo }
    }

    fn p(&self, rel: &str) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap().join(rel)
    }

    fn cfg(&self, extra_top: &str, extra: &str) -> Config {
        // Test file systems are far smaller than the default min_free.
        let extra = if extra.contains("[transfer]") {
            extra.replace("[transfer]", "[transfer]\nmin_free = \"1MB\"")
        } else {
            format!("{extra}\n[transfer]\nmin_free = \"1MB\"\n")
        };
        let path = self.dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "dest_root = {:?}\ntarget_id = {TARGET:?}\nstate_dir = {:?}\n{extra_top}\n[source]\nroot = {:?}\n{extra}\n",
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

    /// Simulate runs into data/ with ont-runsim on a fake clock.
    fn simulate(&self, runs: &[(&str, &str)], extra: &str) -> Vec<PathBuf> {
        let mut toml = format!("seed = 3\nhours = 0.5\n{extra}\n[fake]\nsize_scale = 0.0002\n");
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
    }

    fn sim_manifest(&self, run: &Path) -> PathBuf {
        ont_runsim::status::manifests_dir(&self.p("data")).join(format!(
            "{}.sha256",
            run.file_name().unwrap().to_string_lossy()
        ))
    }

    fn detected(&self, cfg: &Config, run: &Path) -> DetectedRun {
        detect(cfg, &self.fs)
            .unwrap()
            .runs
            .into_iter()
            .find(|r| r.path == std::fs::canonicalize(run).unwrap())
            .expect("run detected")
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|r| {
            r.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn all_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
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

// ---------------------------------------------------------------------------
// Detection, mapping, selection
// ---------------------------------------------------------------------------

/// Verifies: SRS-SEL-006
#[test]
fn detects_minknow_runs_by_marker_and_completion() {
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1"), ("exp2", "s2")], "");
    std::fs::create_dir_all(env.p("data/exp3/s3/not_a_run/pod5")).unwrap();
    std::fs::create_dir_all(env.p("data/lost+found/x/y/z")).unwrap();
    std::fs::write(env.p("data/lost+found/x/y/z/sample_sheet_a.csv"), "x").unwrap();
    let cfg = env.cfg("", "");
    let det = detect(&cfg, &env.fs).unwrap();
    assert_eq!(det.runs.len(), 2, "{:?}", det.runs);
    for (r, path) in det.runs.iter().zip(&runs) {
        assert_eq!(r.path, std::fs::canonicalize(path).unwrap());
        assert!(r.marker.starts_with("sample_sheet_"));
        assert!(
            r.complete
                .as_deref()
                .is_some_and(|c| c.starts_with("final_summary_"))
        );
    }
}

/// Verifies: SRS-SEL-006
#[test]
fn detects_runs_without_sample_sheet_with_an_extra_marker_and_any_depth() {
    let env = Env::new();
    let run = env.p("data/projectA/batch1/sub/run_x");
    std::fs::create_dir_all(run.join("pod5")).unwrap();
    std::fs::write(run.join("pod5/a.pod5"), "x").unwrap();
    // A nested run inside it is not reported separately.
    std::fs::create_dir_all(run.join("inner")).unwrap();
    std::fs::write(run.join("inner/sample_sheet_b.csv"), "x").unwrap();
    let cfg = env.cfg(
        "",
        "layout = \"any-depth\"\nrun_markers = [\"sample_sheet_*.csv\", \"pod5/\"]",
    );
    let det = detect(&cfg, &env.fs).unwrap();
    assert_eq!(det.runs.len(), 1, "{:?}", det.runs);
    assert_eq!(det.runs[0].marker, "pod5");
    assert!(det.runs[0].complete.is_none());
    // In the MinKNOW layout the same folder is too deep to be a run.
    let cfg = env.cfg("", "run_markers = [\"pod5/\"]");
    assert!(detect(&cfg, &env.fs).unwrap().runs.is_empty());
}

/// Verifies: SRS-SEL-006, SRS-SEL-004
#[cfg(unix)]
#[test]
fn symlinked_folders_are_not_followed_during_detection() {
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1")], "");
    std::os::unix::fs::symlink(env.p("data/exp1"), env.p("data/link_to_exp1")).unwrap();
    let det = detect(&env.cfg("", ""), &env.fs).unwrap();
    assert_eq!(det.runs.len(), 1);
    assert_eq!(det.runs[0].path, std::fs::canonicalize(&runs[0]).unwrap());
    assert!(
        det.warnings.iter().any(|w| w.contains("link_to_exp1")),
        "{:?}",
        det.warnings
    );
}

/// Verifies: SRS-SEL-007
#[test]
fn dest_starts_at_levels() {
    let rel = Path::new("EXP01/S1/20261113_0956_1A_PAW12345_9f3c2a1b");
    assert_eq!(dest_rel(rel, DestLevel::Experiment).unwrap(), rel);
    assert_eq!(
        dest_rel(rel, DestLevel::Sample).unwrap(),
        Path::new("S1/20261113_0956_1A_PAW12345_9f3c2a1b")
    );
    assert_eq!(
        dest_rel(rel, DestLevel::Run).unwrap(),
        Path::new("20261113_0956_1A_PAW12345_9f3c2a1b")
    );
    assert!(dest_rel(Path::new("only_run"), DestLevel::Sample).is_err());
    assert!(dest_rel(Path::new(""), DestLevel::Run).is_err());
    assert!(dest_rel(Path::new("a/../b"), DestLevel::Run).is_err());
}

/// Property: whatever the relative path, the mapped destination stays below
/// `dest_root` or is refused.
///
/// Verifies: SRS-SEL-002
#[test]
fn mapping_never_escapes_dest_root() {
    let base = Path::new("/mnt/share/ont");
    let parts = [
        "a", "..", ".", "", "/", "b\u{7}", "c d", "/etc", "x/../..", "...",
    ];
    let mut seed: u64 = 0x5EED;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..20_000 {
        let n = 1 + next() % 5;
        let rel: String = (0..n)
            .map(|_| parts[usize::try_from(next() % parts.len() as u64).unwrap()])
            .collect::<Vec<_>>()
            .join("/");
        for level in [DestLevel::Experiment, DestLevel::Sample, DestLevel::Run] {
            if let Ok(d) = dest_rel(Path::new(&rel), level) {
                let full = safe_join(base, &d).unwrap();
                assert!(
                    full.starts_with(base) && full != base,
                    "{rel:?} -> {full:?}"
                );
            }
        }
        if let Ok(full) = safe_join(base, Path::new(&rel)) {
            assert!(full.starts_with(base), "{rel:?} -> {full:?}");
            assert!(
                !full
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
            );
        }
    }
}

/// Verifies: SRS-SEL-001, SRS-SEL-003, SRS-SEL-004, SRS-SEL-005
#[test]
fn selection_applies_globs_and_skips_links_and_reserved_names() {
    let env = Env::new();
    let run = env.p("data/e/s/run");
    for f in [
        "sample_sheet_x.csv",
        "pod5/a.pod5",
        "pod5/sub/b.pod5",
        "fastq_pass/c.fastq.gz",
        "notes.tmp",
        ".hidden",
        "sequencing_summary_x.txt",
        "COMPLETE",
        "bell\u{7}name.txt",
    ] {
        let p = run.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, f).unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc", run.join("link")).unwrap();
    let cfg = env.cfg(
        "",
        "[selection]\ninclude = [\"**\"]\nexclude = [\"**/.*\", \"**/*.tmp\", \"fastq_pass/**\"]\ndefer_until_complete = [\"sequencing_summary_*.txt\"]",
    );
    let m = Markers::new(&cfg.source.run_markers).unwrap();
    let sel = select(&env.fs, &run, &cfg.selection, &m).unwrap();
    let rels: Vec<&str> = sel.files.iter().map(|f| f.rel.as_str()).collect();
    assert_eq!(
        rels,
        vec![
            "bell\u{7}name.txt",
            "pod5/a.pod5",
            "pod5/sub/b.pod5",
            "sample_sheet_x.csv",
            "sequencing_summary_x.txt"
        ]
    );
    assert!(
        sel.files
            .iter()
            .find(|f| f.rel.starts_with("sequencing"))
            .unwrap()
            .deferred
    );
    assert_eq!(
        sel.excluded,
        vec![".hidden", "fastq_pass/c.fastq.gz", "notes.tmp"]
    );
    assert_eq!(sel.reserved, vec!["COMPLETE"]);
    #[cfg(unix)]
    assert_eq!(sel.symlinks, vec!["link"]);
}

// ---------------------------------------------------------------------------
// Offload
// ---------------------------------------------------------------------------

/// Verifies: SRS-SEL-001, SRS-VER-001, SRS-VER-002, SRS-CPY-001
#[test]
fn offloaded_run_matches_the_simulator_manifest() {
    let env = Env::new();
    let runs = env.simulate(
        &[("exp1", "s1")],
        "[synthetic]\nbam_pass_batch = \"50MB\"\nbarcodes = 2",
    );
    let cfg = env.cfg("", "");
    let r = env.detected(&cfg, &runs[0]);
    let rep = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap();
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(rep.copied, rep.files);
    let dest = env.p("share").join("exp1/s1").join(r.name());
    assert_eq!(rep.dest, dest);
    let check = ont_runsim::check::check(&dest, &env.sim_manifest(&runs[0]), &[]).unwrap();
    assert!(check.ok(), "{check:?}");
    // Our manifest lists exactly the same digests as the simulator's.
    let ours =
        journal::parse_manifest(&std::fs::read_to_string(dest.join(MANIFEST)).unwrap()).unwrap();
    let theirs = ont_runsim::check::read_manifest(&env.sim_manifest(&runs[0])).unwrap();
    assert_eq!(ours, theirs);
    let complete = std::fs::read_to_string(dest.join(COMPLETE)).unwrap();
    assert!(complete.contains(&rep.manifest_sha256.clone().unwrap()));
    let run_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dest.join(RUN_JSON)).unwrap()).unwrap();
    assert_eq!(run_json["files"], rep.files);
    assert_eq!(run_json["dest_starts_at"], "experiment");
    assert!(
        !all_files(&dest)
            .iter()
            .any(|f| copy::is_temp_name(f.rsplit('/').next().unwrap()))
    );
    // A second invocation does nothing.
    let again = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap();
    assert!(again.already_complete);
}

/// Verifies: SRS-SEL-007
#[test]
fn dest_starts_at_sample_and_run_place_the_run_higher() {
    for (level, expect) in [("sample", "s1"), ("run", "")] {
        let env = Env::new();
        let runs = env.simulate(&[("exp1", "s1")], "");
        let cfg = env.cfg(&format!("dest_starts_at = \"{level}\""), "");
        let r = env.detected(&cfg, &runs[0]);
        let rep = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap();
        let want = if expect.is_empty() {
            env.p("share").join(r.name())
        } else {
            env.p("share").join(expect).join(r.name())
        };
        assert_eq!(rep.dest, want);
        assert!(want.join(COMPLETE).is_file());
        assert!(!env.p("share/exp1").exists());
    }
}

/// Verifies: SRS-SEL-007
#[test]
fn two_runs_mapped_to_one_destination_are_not_mixed() {
    let env = Env::new();
    let a = env.p("data/expA/s1/20260101_1200_1A_PAW00001_aaaaaaaa");
    let b = env.p("data/expB/s1/20260101_1200_1A_PAW00001_aaaaaaaa");
    for (run, body) in [(&a, "A"), (&b, "B")] {
        std::fs::create_dir_all(run.join("pod5")).unwrap();
        std::fs::write(run.join("sample_sheet_x.csv"), body).unwrap();
        std::fs::write(run.join("final_summary_x.txt"), body).unwrap();
        std::fs::write(run.join("pod5/x.pod5"), body).unwrap();
    }
    let cfg = env.cfg("dest_starts_at = \"run\"", "");
    let g = env.guard(&cfg);
    offload(&cfg, &env.fs, &g, &env.detected(&cfg, &a), false).unwrap();
    let err = offload(&cfg, &env.fs, &g, &env.detected(&cfg, &b), false).unwrap_err();
    assert!(matches!(err, Error::Integrity(_)), "{err}");
    let dest = env.p("share/20260101_1200_1A_PAW00001_aaaaaaaa/pod5/x.pod5");
    assert_eq!(std::fs::read_to_string(dest).unwrap(), "A");
}

/// Verifies: SRS-GRD-001
#[test]
fn guard_refusals_write_nothing() {
    let cases: Vec<(&str, Box<dyn Fn(&Env)>)> = vec![
        (
            "missing sentinel",
            Box::new(|e: &Env| std::fs::remove_file(e.p("share").join(SENTINEL)).unwrap()),
        ),
        (
            "wrong target",
            Box::new(|e: &Env| std::fs::write(e.p("share").join(SENTINEL), "other-share").unwrap()),
        ),
        (
            "fstype",
            Box::new(|e: &Env| {
                let t = std::fs::read_to_string(&e.mountinfo)
                    .unwrap()
                    .replace("nfs4", "ext4");
                std::fs::write(&e.mountinfo, t).unwrap();
            }),
        ),
    ];
    for (label, break_it) in cases {
        let env = Env::new();
        let runs = env.simulate(&[("exp1", "s1")], "");
        let cfg = env.cfg("", "");
        let r = env.detected(&cfg, &runs[0]);
        break_it(&env);
        let err = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap_err();
        assert!(matches!(err, Error::Guard(_)), "{label}: {err}");
        assert!(
            !env.p("share/exp1").exists(),
            "{label}: something was written"
        );
    }
    // Same device as the source (share not mounted): refused.
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1")], "");
    let cfg = env.cfg("", "");
    let r = env.detected(&cfg, &runs[0]);
    env.fs.clear();
    let err = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap_err();
    assert!(err.to_string().contains("same device"), "{err}");
    assert!(!env.p("share/exp1").exists());
}

/// Verifies: SRS-GRD-002, SRS-RES-004
#[test]
fn sentinel_lost_mid_run_stops_writes_and_the_run_resumes_later() {
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1")], "");
    let cfg = env.cfg("", "");
    let r = env.detected(&cfg, &runs[0]);
    let g = env.guard(&cfg);
    // The sentinel disappears after 25 reads (the guard reads it per chunk and per commit).
    env.fs.add(Rule::after(
        Op::Read,
        SENTINEL,
        Fault::Errno(errno::ENOENT),
        25,
    ));
    let err = offload(&cfg, &env.fs, &g, &r, false).unwrap_err();
    assert!(matches!(err, Error::Guard(_)), "{err}");
    let dest = env.p("share/exp1/s1").join(r.name());
    let partial: Vec<String> = all_files(&dest);
    assert!(!partial.is_empty() && !partial.contains(&COMPLETE.to_owned()));
    assert!(
        !partial
            .iter()
            .any(|f| copy::is_temp_name(f.rsplit('/').next().unwrap())),
        "{partial:?}"
    );

    env.fs.clear();
    env.fs.add(Rule::always(
        Op::Metadata,
        "/share",
        Fault::Device(9_999_999),
    ));
    let rep = offload(&cfg, &env.fs, &g, &r, false).unwrap();
    assert!(rep.skipped > 0, "committed files are not copied again");
    assert_eq!(rep.skipped + rep.copied, rep.files);
    assert!(
        ont_runsim::check::check(&dest, &env.sim_manifest(&runs[0]), &[])
            .unwrap()
            .ok()
    );
}

/// Verifies: SRS-GRD-003
#[cfg(unix)]
#[test]
fn repointed_destination_symlink_is_refused() {
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1")], "");
    std::fs::create_dir(env.p("share2")).unwrap();
    std::fs::write(env.p("share2").join(SENTINEL), TARGET).unwrap();
    let link = env.p("nas");
    std::os::unix::fs::symlink(env.p("share"), &link).unwrap();
    let path = env.dir.path().join("config.toml");
    let cfg = env.cfg("", "");
    let text = std::fs::read_to_string(&path).unwrap().replace(
        &format!("dest_root = {:?}", env.p("share").to_str().unwrap()),
        &format!("dest_root = {:?}", link.to_str().unwrap()),
    );
    std::fs::write(&path, text).unwrap();
    let cfg2 = config::load(&path, &env.fs).unwrap().config;
    assert_eq!(cfg2.dest_root.resolved, cfg.dest_root.resolved);
    let r = env.detected(&cfg2, &runs[0]);
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(env.p("share2"), &link).unwrap();
    let err = offload(&cfg2, &env.fs, &env.guard(&cfg2), &r, false).unwrap_err();
    assert!(err.to_string().contains("recorded at start-up"), "{err}");
    assert_eq!(names(&env.p("share2")), vec![SENTINEL.to_owned()]);
}

/// Verifies: SRS-CPY-002
#[test]
fn foreign_destination_file_is_a_conflict_unless_identical_with_hash() {
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1")], "");
    let cfg = env.cfg("", "");
    let r = env.detected(&cfg, &runs[0]);
    let dest = env.p("share/exp1/s1").join(r.name());
    std::fs::create_dir_all(dest.join("pod5")).unwrap();
    let victim = std::fs::read_dir(runs[0].join("pod5"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let foreign = dest.join("pod5").join(victim.file_name());
    std::fs::write(&foreign, b"someone else's data").unwrap();
    let rep = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap();
    assert_eq!(rep.failed.len(), 1, "{:?}", rep.failed);
    assert!(rep.failed[0].1.contains("not written by ont-offload"));
    assert_eq!(std::fs::read(&foreign).unwrap(), b"someone else's data");
    assert!(!dest.join(COMPLETE).exists());

    // An identical file is adopted with on_existing = "hash".
    std::fs::copy(victim.path(), &foreign).unwrap();
    let cfg = env.cfg("", "[transfer]\non_existing = \"hash\"");
    let rep = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap();
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert!(dest.join(COMPLETE).is_file());
}

/// Verifies: SRS-CPY-001, SRS-RES-004
#[test]
fn failed_rename_leaves_no_final_file_and_stale_temps_are_removed() {
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1")], "");
    let cfg = env.cfg("", "[transfer]\nmax_attempts = 1");
    let r = env.detected(&cfg, &runs[0]);
    let dest = env.p("share/exp1/s1").join(r.name());
    // Leftover from a crash before rename.
    std::fs::create_dir_all(dest.join("pod5")).unwrap();
    std::fs::write(
        dest.join("pod5").join(copy::temp_name("x.pod5")),
        b"partial",
    )
    .unwrap();
    env.fs
        .add(Rule::always(Op::Rename, ".pod5", Fault::Errno(1)));
    let rep = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap();
    assert!(!rep.failed.is_empty());
    assert!(
        names(&dest.join("pod5")).is_empty(),
        "no final or temp pod5 files: {:?}",
        names(&dest.join("pod5"))
    );
    assert!(!dest.join(COMPLETE).exists());
}

/// A filesystem that appends to one source file right after it is opened,
/// the first time only (a file still being written).
struct AppendOnce {
    target: PathBuf,
    done: AtomicBool,
}

impl crate::fsio::Fs for AppendOnce {
    fn metadata(&self, p: &Path) -> io::Result<FileStat> {
        RealFs.metadata(p)
    }
    fn symlink_metadata(&self, p: &Path) -> io::Result<FileStat> {
        RealFs.symlink_metadata(p)
    }
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        RealFs.canonicalize(p)
    }
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        RealFs.read(p)
    }
    fn open_read(&self, p: &Path) -> io::Result<Box<dyn Read + Send>> {
        let r = RealFs.open_read(p)?;
        if p == self.target && !self.done.swap(true, Ordering::SeqCst) {
            RealFs.append_sync(p, b"more data written while copying")?;
        }
        Ok(r)
    }
    fn create_new(&self, p: &Path) -> io::Result<Box<dyn FsFile>> {
        RealFs.create_new(p)
    }
    fn rename(&self, a: &Path, b: &Path) -> io::Result<()> {
        RealFs.rename(a, b)
    }
    fn remove_file(&self, p: &Path) -> io::Result<()> {
        RealFs.remove_file(p)
    }
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        RealFs.create_dir_all(p)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<DirEntry>> {
        RealFs.read_dir(p)
    }
    fn sync_dir(&self, p: &Path) -> io::Result<()> {
        RealFs.sync_dir(p)
    }
    fn append_sync(&self, p: &Path, d: &[u8]) -> io::Result<()> {
        RealFs.append_sync(p, d)
    }
    fn set_mode(&self, p: &Path, m: u32) -> io::Result<()> {
        RealFs.set_mode(p, m)
    }
}

fn opts(verify: config::VerifyMode) -> CopyOptions {
    CopyOptions {
        chunk_size: 4096,
        verify,
        on_existing: config::OnExisting::Fail,
        file_mode: 0o640,
        preserve_mtime: true,
        min_free: 0,
    }
}

/// Verifies: SRS-LIVE-002
#[test]
fn source_change_during_copy_discards_the_copy() {
    let env = Env::new();
    let cfg = env.cfg("", "");
    let src = env.p("data/f.bin");
    std::fs::write(&src, vec![7u8; 10_000]).unwrap();
    let dest = env.p("share/f.bin");
    let fs = AppendOnce {
        target: src.clone(),
        done: AtomicBool::new(false),
    };
    // Same device as source on the real fs: use a guard without device rule via the FaultyFs.
    let g = env.guard(&cfg);
    let faulty = FaultyFs::new(Arc::new(fs));
    faulty.add(Rule::always(
        Op::Metadata,
        "/share",
        Fault::Device(9_999_999),
    ));
    let err = copy::copy_file(
        &faulty,
        &g,
        &src,
        &dest,
        false,
        &opts(config::VerifyMode::Inflight),
    )
    .unwrap_err();
    assert!(
        matches!(err, CopyFail::Retry(ref m) if m.contains("source changed")),
        "{err}"
    );
    assert!(!dest.exists());
    assert_eq!(
        names(&env.p("share")),
        vec![SENTINEL.to_owned()],
        "temp removed"
    );
    // The next attempt copies the grown file.
    let c = copy::copy_file(
        &faulty,
        &g,
        &src,
        &dest,
        false,
        &opts(config::VerifyMode::Inflight),
    )
    .unwrap();
    assert_eq!(c.size, 10_000 + 31);
    assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&src).unwrap());
}

/// Verifies: SRS-VER-001
#[test]
fn short_writes_and_read_back_bit_flips_are_caught() {
    let env = Env::new();
    let cfg = env.cfg("", "");
    let g = env.guard(&cfg);
    let src = env.p("data/f.bin");
    let body: Vec<u8> = (0..50_000u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    std::fs::write(&src, &body).unwrap();

    env.fs
        .add(Rule::always(Op::Write, ".partial", Fault::ShortWrite));
    let c = copy::copy_file(
        &env.fs,
        &g,
        &src,
        &env.p("share/a.bin"),
        false,
        &opts(config::VerifyMode::Inflight),
    )
    .unwrap();
    assert_eq!(std::fs::read(env.p("share/a.bin")).unwrap(), body);
    assert_eq!(c.sha256, journal::sha256_hex(&body));

    env.fs.add(Rule::times(
        Op::OpenRead,
        ".partial",
        Fault::BitFlip(1234),
        1,
    ));
    let err = copy::copy_file(
        &env.fs,
        &g,
        &src,
        &env.p("share/b.bin"),
        false,
        &opts(config::VerifyMode::Readback),
    )
    .unwrap_err();
    assert!(
        matches!(err, CopyFail::Mismatch(ref m) if m.contains("read-back mismatch")),
        "{err}"
    );
    assert!(!env.p("share/b.bin").exists());
    copy::copy_file(
        &env.fs,
        &g,
        &src,
        &env.p("share/b.bin"),
        false,
        &opts(config::VerifyMode::Readback),
    )
    .unwrap();
    assert_eq!(std::fs::read(env.p("share/b.bin")).unwrap(), body);
}

/// Verifies: SRS-CLI-004
#[test]
fn share_outage_mid_run_asks_for_a_restart() {
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1")], "");
    let cfg = env.cfg("", "");
    let r = env.detected(&cfg, &runs[0]);
    env.fs.add(Rule::after(
        Op::CreateNew,
        "/share/",
        Fault::Errno(errno::ESTALE),
        3,
    ));
    let err = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap_err();
    assert!(matches!(err, Error::RestartNeeded(_)), "{err}");
    assert_eq!(err.exit_code(), 75);
}

/// Verifies: SRS-SEL-006
#[test]
fn incomplete_run_needs_the_completed_flag() {
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1")], "[faults]\nabort_after = \"20m\"");
    let cfg = env.cfg("", "");
    let r = env.detected(&cfg, &runs[0]);
    assert!(r.complete.is_none());
    let err = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap_err();
    assert!(err.to_string().contains("--completed"), "{err}");
    let rep = offload(&cfg, &env.fs, &env.guard(&cfg), &r, true).unwrap();
    assert!(rep.failed.is_empty());
    let run_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(rep.dest.join(RUN_JSON)).unwrap()).unwrap();
    assert_eq!(run_json["assumed_complete"], true);
}

/// Verifies: SRS-LOG-003
#[test]
fn commit_log_line_names_run_file_size_digest_and_duration() {
    let c = copy::Copied {
        size: 42,
        sha256: "ab".repeat(32),
        mtime_ns: 0,
        adopted: false,
    };
    let line = offload::commit_message("0123456789abcdef", "pod5/x\u{7}.pod5", &c, 1.5);
    assert!(line.contains("run=0123456789abcdef"));
    assert!(line.contains("file=pod5/x\\u{7}.pod5"), "{line}");
    assert!(line.contains("size=42") && line.contains(&"ab".repeat(32)) && line.contains("1.50s"));
    assert!(!line.contains('\u{7}'));
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// Verifies: SRS-VER-003, SRS-VER-004
#[test]
fn verify_writes_receipt_only_when_everything_matches() {
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1")], "");
    let cfg = env.cfg("", "");
    let r = env.detected(&cfg, &runs[0]);
    let rep = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false).unwrap();
    let dest = rep.dest;

    let ok = journal::verify(&env.fs, &dest, true).unwrap();
    assert!(ok.source_checked);
    let receipt: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dest.join(VERIFIED)).unwrap()).unwrap();
    assert_eq!(receipt["result"], "ok");
    assert_eq!(receipt["files"], rep.files);
    assert_eq!(
        receipt["manifest_sha256"],
        journal::sha256_hex(&std::fs::read(dest.join(MANIFEST)).unwrap())
    );
    for key in ["host", "verified_at", "bytes"] {
        assert!(receipt.get(key).is_some(), "{key}");
    }
    std::fs::remove_file(dest.join(VERIFIED)).unwrap();

    // One flipped byte: mismatch, exit 4, no receipt.
    let victim = dest.join(
        all_files(&dest)
            .into_iter()
            .find(|f| f.ends_with(".pod5"))
            .unwrap(),
    );
    let mut b = std::fs::read(&victim).unwrap();
    b[0] ^= 1;
    std::fs::write(&victim, b).unwrap();
    let err = journal::verify(&env.fs, &dest, false).unwrap_err();
    assert_eq!(err.exit_code(), 4);
    assert!(!dest.join(VERIFIED).exists());
}

/// Verifies: SRS-VER-003
#[test]
fn verify_reports_extra_missing_and_source_differences() {
    let env = Env::new();
    let runs = env.simulate(&[("exp1", "s1")], "");
    let cfg = env.cfg("", "");
    let r = env.detected(&cfg, &runs[0]);
    let dest = offload(&cfg, &env.fs, &env.guard(&cfg), &r, false)
        .unwrap()
        .dest;
    std::fs::write(dest.join("stray.txt"), "x").unwrap();
    assert!(journal::verify(&env.fs, &dest, false).is_err());
    std::fs::remove_file(dest.join("stray.txt")).unwrap();
    // The source changed after the offload.
    let src_file = runs[0].join(
        all_files(&runs[0])
            .into_iter()
            .find(|f| f.ends_with(".pod5"))
            .unwrap(),
    );
    std::fs::write(&src_file, b"changed").unwrap();
    assert!(journal::verify(&env.fs, &dest, false).is_ok());
    std::fs::remove_file(dest.join(VERIFIED)).unwrap();
    let err = journal::verify(&env.fs, &dest, true).unwrap_err();
    assert!(err.to_string().contains("differ at the source"), "{err}");
    std::fs::remove_file(dest.join(COMPLETE)).unwrap();
    assert!(
        journal::verify(&env.fs, &dest, false)
            .unwrap_err()
            .to_string()
            .contains("not complete")
    );
}
