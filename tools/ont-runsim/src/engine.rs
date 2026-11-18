// SPDX-License-Identifier: AGPL-3.0-or-later
//! The writing engine: creates, grows and closes planned files as simulated
//! time passes, and records the SHA-256 of every file in the run's manifest.
//!
//! [`Engine::advance_to`] is clock-free so tests can drive it on a fake
//! clock; [`simulate`] drives it on the wall clock at the scenario speed.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::plan::{Content, RunPlan};
use crate::rng::SplitMix64;
use crate::scenario::Scenario;
use crate::status::{self, EventLog, RunStatus, Status};
use crate::units::{bytes, hms};
use crate::{Error, Result, STATE_DIR, io_err, write_atomic};

const CHUNK: usize = 1 << 20;

/// Produces the bytes of one file, sequentially.
enum Gen {
    Random(SplitMix64, bool),
    Zero,
    Text(Arc<Vec<u8>>),
    Replay(File, u64, PathBuf),
}

impl Gen {
    fn new(content: &Content) -> Result<Self> {
        Ok(match content {
            Content::Random { seed, printable } => Self::Random(SplitMix64::new(*seed), *printable),
            Content::Zero => Self::Zero,
            Content::Text(t) => Self::Text(Arc::clone(t)),
            Content::Replay(p) => {
                let f = File::open(p).map_err(io_err("open replay source", p))?;
                let len = f.metadata().map_err(io_err("stat", p))?.len();
                Self::Replay(f, len.max(1), p.clone())
            }
        })
    }

    /// Fill `buf` with the bytes at `offset..offset + buf.len()`; calls are sequential.
    fn fill(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        match self {
            Self::Random(rng, false) => rng.fill(buf),
            Self::Random(rng, true) => {
                rng.fill(buf);
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = if (offset + i as u64) % 81 == 80 {
                        b'\n'
                    } else {
                        HEX[usize::from(*b & 15)]
                    };
                }
            }
            Self::Zero => buf.fill(0),
            Self::Text(t) => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX).min(t.len());
                let end = (start + buf.len()).min(t.len());
                buf[..end - start].copy_from_slice(&t[start..end]);
                buf[end - start..].fill(b'\n');
            }
            Self::Replay(f, len, path) => {
                let mut filled = 0;
                while filled < buf.len() {
                    let pos = (offset + filled as u64) % *len;
                    f.seek(SeekFrom::Start(pos)).map_err(io_err("seek", path))?;
                    let want =
                        (buf.len() - filled).min(usize::try_from(*len - pos).unwrap_or(usize::MAX));
                    let n = f
                        .read(&mut buf[filled..filled + want])
                        .map_err(io_err("read", path))?;
                    if n == 0 {
                        return Err(Error::Scenario(format!(
                            "replay source {} shrank",
                            path.display()
                        )));
                    }
                    filled += n;
                }
            }
        }
        Ok(())
    }
}

struct OpenFile {
    idx: usize,
    file: File,
    path: PathBuf,
    written: u64,
    hasher: Sha256,
    source: Gen,
}

impl OpenFile {
    fn write_to(&mut self, target: u64, buf: &mut [u8]) -> Result<u64> {
        let mut added = 0;
        while self.written < target {
            let n = usize::try_from(target - self.written)
                .unwrap_or(CHUNK)
                .min(buf.len());
            self.source.fill(self.written, &mut buf[..n])?;
            self.file
                .write_all(&buf[..n])
                .map_err(io_err("write", &self.path))?;
            self.hasher.update(&buf[..n]);
            self.written += n as u64;
            added += n as u64;
        }
        Ok(added)
    }
}

/// Phase of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Not started yet.
    Waiting,
    /// Writing files.
    Sequencing,
    /// All files written.
    Complete,
    /// Stopped by the abort fault.
    Aborted,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Sequencing => "sequencing",
            Self::Complete => "complete",
            Self::Aborted => "aborted",
        }
    }
}

struct RunState {
    plan: RunPlan,
    dir: PathBuf,
    next: usize,
    open: Vec<OpenFile>,
    manifest: BTreeMap<String, String>,
    rewrites_done: Vec<bool>,
    phase: Phase,
    files_closed: usize,
    bytes_written: u64,
    dirty: bool,
}

/// Writes the planned runs below an output folder.
pub struct Engine {
    out: PathBuf,
    runs: Vec<RunState>,
    events: EventLog,
    buf: Vec<u8>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("out", &self.out)
            .finish_non_exhaustive()
    }
}

fn hex(d: &[u8]) -> String {
    use std::fmt::Write as _;
    d.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Prepare an output folder: refuse folders the simulator did not create,
/// refuse while another simulation is running there, create the state folder.
///
/// # Errors
/// [`Error::Refused`] or I/O errors.
pub fn prepare_out(out: &Path) -> Result<()> {
    if out.file_name().is_none() || out.parent().is_none() {
        return Err(Error::Refused(format!(
            "{} cannot be the output folder",
            out.display()
        )));
    }
    if out.exists() {
        let meta = std::fs::symlink_metadata(out).map_err(io_err("stat", out))?;
        if !meta.is_dir() {
            return Err(Error::Refused(format!("{} is not a folder", out.display())));
        }
        let state = status::state_dir(out);
        let empty = std::fs::read_dir(out)
            .map_err(io_err("read folder", out))?
            .next()
            .is_none();
        if !empty && !state.is_dir() {
            return Err(Error::Refused(format!(
                "{} is not empty and was not created by ont-runsim (no {STATE_DIR}); use a new or empty folder",
                out.display()
            )));
        }
        if let Ok(s) = Status::read(out) {
            if s.is_live() {
                return Err(Error::Refused(format!(
                    "a simulation is already running in {} (pid {})",
                    out.display(),
                    s.pid
                )));
            }
        }
    }
    let m = status::manifests_dir(out);
    std::fs::create_dir_all(&m).map_err(io_err("create folder", &m))
}

impl Engine {
    /// Create run folders and register them in `runs.txt` (before any data is written).
    ///
    /// # Errors
    /// [`Error::Refused`] when a run folder exists; I/O errors.
    pub fn new(out: &Path, plans: Vec<RunPlan>) -> Result<Self> {
        let list = status::runs_list(out);
        let mut runs = Vec::with_capacity(plans.len());
        for plan in plans {
            let dir = out.join(plan.id.rel_dir());
            if dir.exists() {
                return Err(Error::Refused(format!(
                    "run folder {} already exists",
                    dir.display()
                )));
            }
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&list)
                .map_err(io_err("open", &list))?;
            writeln!(f, "{}", plan.id.rel_dir().display()).map_err(io_err("write", &list))?;
            let n = plan.rewrites.len();
            runs.push(RunState {
                plan,
                dir,
                next: 0,
                open: Vec::new(),
                manifest: BTreeMap::new(),
                rewrites_done: vec![false; n],
                phase: Phase::Waiting,
                files_closed: 0,
                bytes_written: 0,
                dirty: true,
            });
        }
        Ok(Self {
            out: out.to_owned(),
            runs,
            events: EventLog::new(out),
            buf: vec![0; CHUNK],
        })
    }

    /// Absolute run folders.
    #[must_use]
    pub fn run_dirs(&self) -> Vec<PathBuf> {
        self.runs.iter().map(|r| r.dir.clone()).collect()
    }

    /// Phases of the runs.
    #[must_use]
    pub fn phases(&self) -> Vec<Phase> {
        self.runs.iter().map(|r| r.phase).collect()
    }

    /// True when every run is complete or aborted.
    #[must_use]
    pub fn done(&self) -> bool {
        self.runs
            .iter()
            .all(|r| matches!(r.phase, Phase::Complete | Phase::Aborted))
    }

    /// Simulated second at which everything is written.
    #[must_use]
    pub fn total_seconds(&self) -> f64 {
        self.runs.iter().map(|r| r.plan.end_s()).fold(0.0, f64::max)
    }

    /// Next simulated second at which something happens after `t`
    /// (`t` itself while files are growing).
    #[must_use]
    pub fn next_event_after(&self, t: f64) -> f64 {
        let mut next = f64::INFINITY;
        for r in &self.runs {
            if matches!(r.phase, Phase::Complete | Phase::Aborted) {
                continue;
            }
            if !r.open.is_empty() {
                return t;
            }
            if r.phase == Phase::Waiting {
                next = next.min(r.plan.start_s);
            }
            if let Some(f) = r.plan.files.get(r.next) {
                next = next.min(f.created_s);
            }
            for (i, w) in r.plan.rewrites.iter().enumerate() {
                if !r.rewrites_done[i] {
                    next = next.min(w.at_s);
                }
            }
            if let Some(a) = r.plan.abort_s {
                next = next.min(a);
            }
        }
        next
    }

    /// Bring every run to simulated second `t`.
    ///
    /// # Errors
    /// I/O errors.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    pub fn advance_to(&mut self, t: f64) -> Result<()> {
        for r in &mut self.runs {
            if matches!(r.phase, Phase::Complete | Phase::Aborted) || t < r.plan.start_s {
                continue;
            }
            let name = r.plan.id.folder_name();
            if r.phase == Phase::Waiting {
                std::fs::create_dir_all(&r.dir).map_err(io_err("create folder", &r.dir))?;
                r.phase = Phase::Sequencing;
                self.events.emit(
                    r.plan.start_s,
                    "run_started",
                    Some(&name),
                    &r.dir.display().to_string(),
                )?;
            }
            let limit = r.plan.abort_s.map_or(t, |a| t.min(a));

            // Open files whose time has come.
            while let Some(f) = r.plan.files.get(r.next) {
                if f.created_s > limit {
                    break;
                }
                let path = r.dir.join(&f.rel);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(io_err("create folder", parent))?;
                }
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(io_err("create", &path))?;
                r.open.push(OpenFile {
                    idx: r.next,
                    file,
                    path,
                    written: 0,
                    hasher: Sha256::new(),
                    source: Gen::new(&f.content)?,
                });
                r.next += 1;
            }

            // Grow open files; close the finished ones.
            let mut i = 0;
            while i < r.open.len() {
                let f = &r.plan.files[r.open[i].idx];
                let target = if limit >= f.closed_s || f.closed_s <= f.created_s {
                    f.size
                } else {
                    let frac = (limit - f.created_s) / (f.closed_s - f.created_s);
                    ((f.size as f64 * frac) as u64).min(f.size)
                };
                let added = r.open[i].write_to(target, &mut self.buf)?;
                r.bytes_written += added;
                if limit >= f.closed_s {
                    let of = r.open.swap_remove(i);
                    of.file.sync_data().map_err(io_err("sync", &of.path))?;
                    r.manifest.insert(f.rel.clone(), hex(&of.hasher.finalize()));
                    r.files_closed += 1;
                    r.dirty = true;
                } else {
                    i += 1;
                }
            }

            // Rewrite faults.
            for (k, w) in r.plan.rewrites.iter().enumerate() {
                if r.rewrites_done[k] || w.at_s > limit {
                    continue;
                }
                let f = &r.plan.files[w.file];
                if r.open.iter().any(|o| o.idx == w.file) || w.file >= r.next {
                    continue;
                }
                let path = r.dir.join(&f.rel);
                let file = File::create(&path).map_err(io_err("rewrite", &path))?;
                let mut of = OpenFile {
                    idx: w.file,
                    file,
                    path,
                    written: 0,
                    hasher: Sha256::new(),
                    source: Gen::new(&Content::Random {
                        seed: w.seed,
                        printable: false,
                    })?,
                };
                r.bytes_written += of.write_to(f.size, &mut self.buf)?;
                of.file.sync_data().map_err(io_err("sync", &of.path))?;
                r.manifest.insert(f.rel.clone(), hex(&of.hasher.finalize()));
                r.rewrites_done[k] = true;
                r.dirty = true;
                self.events
                    .emit(w.at_s, "fault_rewrite", Some(&name), &f.rel)?;
            }

            // Abort fault: leave partial files as they are, record what is on disk.
            if r.plan.abort_s.is_some_and(|a| t >= a) {
                for of in r.open.drain(..) {
                    of.file.sync_data().map_err(io_err("sync", &of.path))?;
                    let rel = r.plan.files[of.idx].rel.clone();
                    r.manifest.insert(rel, hex(&of.hasher.finalize()));
                }
                r.phase = Phase::Aborted;
                r.dirty = true;
                self.events.emit(
                    t,
                    "run_aborted",
                    Some(&name),
                    "fault: abort_after; no final summary",
                )?;
                continue;
            }

            if r.next == r.plan.files.len()
                && r.open.is_empty()
                && r.rewrites_done.iter().all(|d| *d)
            {
                r.phase = Phase::Complete;
                r.dirty = true;
                self.events.emit(
                    t,
                    "run_complete",
                    Some(&name),
                    &format!("{} files, {}", r.files_closed, bytes(r.bytes_written)),
                )?;
            }
        }
        self.write_manifests()
    }

    /// Write changed manifests (`sha256sum -c` format, relative to the run folder).
    ///
    /// # Errors
    /// I/O errors.
    pub fn write_manifests(&mut self) -> Result<()> {
        let dir = status::manifests_dir(&self.out);
        for r in self.runs.iter_mut().filter(|r| r.dirty) {
            let mut text = String::new();
            for (path, digest) in &r.manifest {
                text.push_str(digest);
                text.push_str("  ");
                text.push_str(path);
                text.push('\n');
            }
            write_atomic(
                &dir.join(format!("{}.sha256", r.plan.id.folder_name())),
                text.as_bytes(),
            )?;
            r.dirty = false;
        }
        Ok(())
    }

    /// Build a status snapshot.
    #[must_use]
    pub fn status(
        &self,
        state: &str,
        started: &str,
        speed: f64,
        sim_seconds: f64,
        message: Option<String>,
    ) -> Status {
        let runs: Vec<RunStatus> = self
            .runs
            .iter()
            .map(|r| RunStatus {
                name: r.plan.id.folder_name(),
                path: r.dir.display().to_string(),
                state: r.phase.as_str().into(),
                files_closed: r.files_closed,
                files_planned: r.plan.files.len(),
                bytes_written: r.bytes_written,
            })
            .collect();
        Status {
            tool: "ont-runsim".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            simulated: true,
            pid: std::process::id(),
            state: state.into(),
            out: self.out.display().to_string(),
            started: started.into(),
            updated: status::now_rfc3339(),
            speed,
            sim_seconds,
            total_seconds: self.total_seconds(),
            bytes_written: runs.iter().map(|r| r.bytes_written).sum(),
            files_closed: runs.iter().map(|r| r.files_closed).sum(),
            files_planned: runs.iter().map(|r| r.files_planned).sum(),
            message,
            runs,
        }
    }

    /// Event log of this output folder.
    #[must_use]
    pub fn events(&self) -> &EventLog {
        &self.events
    }
}

/// Free bytes for unprivileged users on the filesystem holding `path`.
///
/// # Errors
/// I/O errors.
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Result<u64> {
    let st = rustix::fs::statvfs(path).map_err(|e| Error::Io {
        op: "statvfs",
        path: path.to_owned(),
        source: e.into(),
    })?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Free bytes (not available on this platform: unlimited).
///
/// # Errors
/// Never.
#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Result<u64> {
    Ok(u64::MAX)
}

/// Options for [`simulate`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Stop when free space on the output disk falls below this.
    pub keep_free: u64,
    /// Print a progress line to stderr every this often (`None`: quiet).
    pub progress_every: Option<Duration>,
}

/// Outcome of [`simulate`].
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Final snapshot.
    pub status: Status,
}

/// Run a scenario on the wall clock.
///
/// # Errors
/// [`Error::Refused`] for an unsafe output folder, [`Error::DiskFull`],
/// I/O errors. The status file records the reason.
#[allow(clippy::cast_precision_loss)]
pub fn simulate(sc: &Scenario, out: &Path, opts: &Options) -> Result<Outcome> {
    prepare_out(out)?;
    let plans = crate::plan::build(sc, SystemTime::now())?;
    let mut engine = Engine::new(out, plans)?;
    let started = status::now_rfc3339();
    let total = engine.total_seconds();
    engine.events().emit(
        0.0,
        "simulation_started",
        None,
        &format!(
            "{} run(s), {} simulated, speed {}x, seed {}",
            engine.runs.len(),
            hms(total),
            sc.speed,
            sc.seed
        ),
    )?;
    let t0 = Instant::now();
    let mut last_progress = Instant::now();
    loop {
        let sim = t0.elapsed().as_secs_f64() * sc.speed;
        let free = free_bytes(out)?;
        if free < opts.keep_free {
            let err = Error::DiskFull {
                free,
                keep: opts.keep_free,
            };
            engine
                .events()
                .emit(sim, "simulation_stopped", None, &err.to_string())?;
            engine
                .status("stopped", &started, sc.speed, sim, Some(err.to_string()))
                .write(out)?;
            return Err(err);
        }
        if let Err(e) = engine.advance_to(sim) {
            let _ = engine
                .status("failed", &started, sc.speed, sim, Some(e.to_string()))
                .write(out);
            return Err(e);
        }
        let done = engine.done();
        let state = if done { "finished" } else { "running" };
        let snapshot = engine.status(state, &started, sc.speed, sim.min(total), None);
        snapshot.write(out)?;
        if let Some(every) = opts.progress_every {
            if done || last_progress.elapsed() >= every {
                last_progress = Instant::now();
                eprintln!(
                    "ont-runsim: simulated {} of {} ({:.0}%), {} of {} files, {} written",
                    hms(sim.min(total)),
                    hms(total),
                    100.0 * sim.min(total) / total.max(1.0),
                    snapshot.files_closed,
                    snapshot.files_planned,
                    bytes(snapshot.bytes_written)
                );
            }
        }
        if done {
            engine.events().emit(
                sim,
                "simulation_finished",
                None,
                &bytes(snapshot.bytes_written),
            )?;
            return Ok(Outcome { status: snapshot });
        }
        let next = engine.next_event_after(sim);
        let wait = ((next - sim) / sc.speed).clamp(0.02, 1.0);
        std::thread::sleep(Duration::from_secs_f64(wait));
    }
}
