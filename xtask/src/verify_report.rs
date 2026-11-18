// SPDX-License-Identifier: AGPL-3.0-or-later
//! `cargo xtask verify-report`: an end-to-end execution with a verification
//! report as its evidence. ont-runsim writes runs into a work folder while
//! `ont-offload watch --until-idle` offloads them to a share on another
//! filesystem (a tmpfs by default); a second `watch` session picks up what
//! the first left (a restart), `verify` writes the receipts, and
//! `report verify --sim` checks everything against the simulator's own
//! manifests. The PDF and its JSON record go to `--out`; the runs, state and
//! log of the execution stay in `target/verify-report-work`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use std::{env, fs};

use super::{Res, cargo, root};

pub const USAGE: &str = "\
cargo xtask verify-report [options]
  --out DIR          evidence folder (default target/verify-report)
  --share DIR        parent of the temporary share, on another filesystem (default /dev/shm)
  --fstype T         filesystem type of the share, for the guard (default tmpfs)
  --preset P         ont-runsim preset (default quick-2h)
  --speed N          simulated seconds per second (default 240)
  --size-scale X     file size factor (default 0.001)
  --operator NAME    operator named in the report
  --keep             keep the share folder afterwards";

struct Opts {
    out: PathBuf,
    share: PathBuf,
    fstype: String,
    preset: String,
    speed: String,
    size_scale: String,
    operator: String,
    keep: bool,
}

fn opts(args: &[&str]) -> Result<Opts, String> {
    let mut o = Opts {
        out: root().join("target/verify-report"),
        share: PathBuf::from("/dev/shm"),
        fstype: "tmpfs".into(),
        preset: "quick-2h".into(),
        speed: "240".into(),
        size_scale: "0.001".into(),
        operator: env::var("USER").unwrap_or_else(|_| "cargo xtask".into()),
        keep: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if *a == "--keep" {
            o.keep = true;
            continue;
        }
        let v = it
            .next()
            .map(|v| (*v).to_owned())
            .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))?;
        match *a {
            "--out" => o.out = PathBuf::from(v),
            "--share" => o.share = PathBuf::from(v),
            "--fstype" => o.fstype = v,
            "--preset" => o.preset = v,
            "--speed" => o.speed = v,
            "--size-scale" => o.size_scale = v,
            "--operator" => o.operator = v,
            _ => return Err(format!("unknown option {a}\n\n{USAGE}")),
        }
    }
    Ok(o)
}

fn bin(name: &str) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| root().join("target"), PathBuf::from)
        .join("debug")
        .join(name)
}

fn show(c: &Command) {
    let args: Vec<_> = c.get_args().map(|a| a.to_string_lossy()).collect();
    eprintln!("$ {} {}", c.get_program().to_string_lossy(), args.join(" "));
}

/// Run and return the exit code (`None` if killed by a signal).
fn code(mut c: Command) -> Result<Option<i32>, String> {
    show(&c);
    c.status()
        .map(|s| s.code())
        .map_err(|e| format!("cannot run {:?}: {e}", c.get_program()))
}

fn spawn(mut c: Command) -> Result<Child, String> {
    show(&c);
    c.spawn()
        .map_err(|e| format!("cannot run {:?}: {e}", c.get_program()))
}

/// Folders below `dir` that hold a file named like `prefix*`.
fn find(dir: &Path, test: &dyn Fn(&str) -> bool, found: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            find(&p, test, found);
        } else if test(&e.file_name().to_string_lossy()) && !found.contains(&dir.to_path_buf()) {
            found.push(dir.to_path_buf());
        }
    }
}

fn config(o: &Opts, work: &Path, share: &Path) -> String {
    format!(
        "# Written by cargo xtask verify-report.\n\
         dest_root = {:?}\ntarget_id = \"verify-report\"\nstate_dir = {:?}\n\
         [guard]\nallowed_fstypes = [{:?}]\n\
         [transfer]\nmin_free = \"1MB\"\nsettle = \"2s\"\nsettle_after_complete = \"5s\"\n\
         [bandwidth]\nworkers = 4\n\
         [outage]\nsource_free_alert = \"0B\"\n\
         [state]\nfree_alert = \"0B\"\n\
         [logging]\nfile = {:?}\n\
         [report]\nsite = \"cargo xtask verify-report\"\n\
         [source]\nroot = {:?}\n",
        share.to_string_lossy(),
        work.join("state").to_string_lossy(),
        o.fstype,
        work.join("log/ont-offload.log").to_string_lossy(),
        work.join("data").to_string_lossy(),
    )
}

pub fn verify_report(args: &[&str]) -> Res {
    let o = opts(args)?;
    cargo(&[
        "build",
        "-p",
        "ont-offload",
        "-p",
        "ont-runsim",
        "--features",
        "ont-offload/report",
    ])?;
    let (offload, runsim) = (bin("ont-offload"), bin("ont-runsim"));

    // A fresh work folder (runs, state, log) beside the build; the
    // evidence folder only receives reports and keeps earlier ones.
    let work = root().join("target/verify-report-work");
    if work.exists() {
        fs::remove_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    }
    for d in ["data", "state", "log"] {
        fs::create_dir_all(work.join(d)).map_err(|e| format!("{}: {e}", work.display()))?;
    }
    let share = o
        .share
        .join(format!("ont-offload-verify-{}", std::process::id()));
    fs::create_dir_all(&share).map_err(|e| format!("{}: {e}", share.display()))?;
    fs::write(share.join(".ont-offload-target"), "verify-report\n")
        .map_err(|e| format!("{}: {e}", share.display()))?;
    let cfg = work.join("ont-offload.toml");
    fs::write(&cfg, config(&o, &work, &share)).map_err(|e| format!("{}: {e}", cfg.display()))?;
    let result = execute(&o, &offload, &runsim, &work, &share, &cfg);
    if o.keep {
        eprintln!("xtask: share kept at {}", share.display());
    } else {
        let _ = fs::remove_dir_all(&share);
    }
    result
}

fn execute(o: &Opts, offload: &Path, runsim: &Path, work: &Path, share: &Path, cfg: &Path) -> Res {
    let ont = || {
        let mut c = Command::new(offload);
        c.arg("--config").arg(cfg);
        c
    };
    let data = work.join("data");

    // 1. Runs are written while the daemon watches.
    let mut sim = Command::new(runsim);
    sim.args(["run", "--preset", &o.preset, "--speed", &o.speed])
        .args([
            "--size-scale",
            &o.size_scale,
            "--keep-free",
            "0B",
            "--quiet",
            "--out",
        ])
        .arg(&data);
    let mut sim = spawn(sim)?;
    let t0 = Instant::now();
    let mut seen = Vec::new();
    while seen.is_empty() {
        if let Ok(Some(s)) = sim.try_wait() {
            if !s.success() {
                return Err(format!("ont-runsim failed ({s})"));
            }
            break;
        }
        if t0.elapsed() > Duration::from_secs(120) {
            let _ = sim.kill();
            return Err("ont-runsim wrote no run within 2 minutes".into());
        }
        std::thread::sleep(Duration::from_millis(200));
        find(&data, &|n| n.starts_with("sample_sheet_"), &mut seen);
    }
    let mut w = ont();
    w.args(["watch", "--until-idle"]);
    let first = code(w)?;
    let s = sim.wait().map_err(|e| format!("ont-runsim: {e}"))?;
    if !s.success() {
        return Err(format!("ont-runsim failed ({s})"));
    }

    // 2. A second session: a restart, and anything the first left.
    let mut w = ont();
    w.args(["watch", "--until-idle"]);
    let second = code(w)?;
    eprintln!("xtask: watch sessions exited with {first:?} and {second:?}");

    // 3. Receipts for every completed run.
    let mut done = Vec::new();
    find(share, &|n| n == "COMPLETE", &mut done);
    for run in &done {
        let mut v = ont();
        v.arg("verify").arg(run);
        let _ = code(v)?;
    }

    // 4. The report: its exit code is the verdict.
    let report = o.out.clone();
    let mut r = ont();
    r.args(["report", "verify", "--sim"])
        .arg(&data)
        .args(["--protocol", "simulation", "--operator", &o.operator])
        .args(["--format", "pdf,json", "--out"])
        .arg(&report);
    match code(r)? {
        Some(0) => {
            eprintln!("xtask: PASS; evidence in {}", report.display());
            Ok(())
        }
        Some(3) => Err(format!("verification FAIL; see {}", report.display())),
        Some(5) => Err(format!(
            "verification INCONCLUSIVE; see {}",
            report.display()
        )),
        other => Err(format!("report verify exited with {other:?}")),
    }
}
