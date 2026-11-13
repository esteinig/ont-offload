// SPDX-License-Identifier: AGPL-3.0-or-later
//! The `watch` controller: discovers runs, tracks their lifecycle, hands
//! stable files to the workers, handles outages and stalls, and finalises
//! runs. One step at a time on a [`Clock`], so the whole controller runs on
//! a fake clock in tests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use crate::config::Config;
use crate::copy::{CopyFail, CopyOptions};
use crate::error::{Error, Result};
use crate::fsio::{ErrorClass, Fs, classify};
use crate::guard::Guard;
use crate::journal::{self, Journal, Record};
use crate::logging::escape_control;
use crate::run::lifecycle::{Lifecycle, Phase};
use crate::run::offload::commit_message;
use crate::run::{self, DetectedRun, Markers, Selected, finalise, safe_join};

use super::notify::Notifier;
use super::outage::{Event, Tracker};
use super::pool::{Done, Failure, Job, Outcome, Pool};
use super::rate::RateLimiter;
use super::{Clock, human};

/// Controller settings that are not in the configuration file.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Rescan the source this often.
    pub scan_every: Duration,
    /// A destination check that takes longer than this counts as failed.
    pub probe_timeout: Duration,
    /// Real-time wait between controller steps when nothing happens.
    pub tick: Duration,
    /// Exit once every detected run is complete or failed (`watch --until-idle`).
    pub until_idle: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scan_every: Duration::from_secs(10),
            probe_timeout: Duration::from_secs(30),
            tick: Duration::from_secs(1),
            until_idle: false,
        }
    }
}

/// Counters for the status line and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Files committed.
    pub committed: usize,
    /// Files that failed for good.
    pub failed_files: usize,
    /// Runs completed.
    pub completed_runs: usize,
    /// Runs that cannot be completed (failed files, integrity, claim).
    pub failed_runs: usize,
    /// Outages seen.
    pub outages: usize,
    /// Stalls seen.
    pub stalls: usize,
}

/// One tracked run.
#[derive(Debug)]
pub struct LiveRun {
    /// As detected (refreshed every scan).
    pub det: DetectedRun,
    /// Destination run folder.
    pub dest: PathBuf,
    /// The run's journal (owned by the controller).
    pub journal: Journal,
    /// Lifecycle tracking.
    pub life: Lifecycle,
    /// Latest selection.
    pub sel: Option<Selected>,
    started: String,
    inflight: BTreeSet<String>,
    attempts: BTreeMap<String, u32>,
    /// Files that failed for good, with the reason.
    pub failed: BTreeMap<String, String>,
    finalising: Option<String>,
    /// `COMPLETE` written.
    pub done: bool,
    /// The run cannot be completed in this process.
    pub broken: Option<String>,
    phase: Phase,
}

struct ProbeResult {
    result: std::result::Result<(), String>,
    wrong_target: Option<String>,
}

struct Probe {
    rx: Receiver<ProbeResult>,
    started: Duration,
    timed_out: bool,
}

/// The controller.
///
/// Implements: SRS-LIVE-001, SRS-LIVE-003, SRS-PAR-001, SRS-RES-001,
/// SRS-RES-002, SRS-RES-003, SRS-RES-005, SRS-RES-006
pub struct Watcher {
    cfg: Config,
    fs: Arc<dyn Fs>,
    guard: Arc<Guard>,
    clock: Arc<dyn Clock>,
    opts: Options,
    pool: Pool,
    outage: Tracker,
    markers: Markers,
    /// Tracked runs by source path.
    pub runs: BTreeMap<PathBuf, LiveRun>,
    ignored: BTreeSet<PathBuf>,
    warned: BTreeSet<String>,
    last_scan: Option<Duration>,
    probe: Option<Probe>,
    started_up: bool,
    stalled: BTreeSet<(usize, Duration)>,
    notifier: Notifier,
    /// Counters.
    pub stats: Stats,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("runs", &self.runs.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

fn log_event(e: &Event) {
    let (level, msg) = e.message();
    log::log!(level, "{msg}");
}

impl Watcher {
    /// A controller with its worker pool. Nothing is checked or written
    /// until the first [`Watcher::step`].
    ///
    /// # Errors
    /// Invalid markers; a worker thread cannot be started.
    pub fn new(
        cfg: Config,
        fs: Arc<dyn Fs>,
        guard: Arc<Guard>,
        clock: Arc<dyn Clock>,
        opts: Options,
        notifier: Notifier,
    ) -> Result<Self> {
        let chunk = usize::try_from(cfg.transfer.chunk_size).unwrap_or(8 << 20);
        let copy_opts = CopyOptions {
            chunk_size: chunk,
            verify: cfg.transfer.verify,
            on_existing: cfg.transfer.on_existing,
            file_mode: cfg.transfer.file_mode,
            preserve_mtime: cfg.transfer.preserve_mtime,
        };
        let rate = Arc::new(RateLimiter::new(
            cfg.bandwidth.rate_limit,
            cfg.transfer.chunk_size.max(1 << 20),
            Arc::clone(&clock),
        ));
        let pool = Pool::new(
            usize::try_from(cfg.bandwidth.workers).unwrap_or(1),
            &fs,
            &guard,
            copy_opts,
            &rate,
            &clock,
        )?;
        let markers = Markers::new(&cfg.source.run_markers)?;
        let outage = Tracker::new(&cfg.outage);
        Ok(Self {
            cfg,
            fs,
            guard,
            clock,
            opts,
            pool,
            outage,
            markers,
            runs: BTreeMap::new(),
            ignored: BTreeSet::new(),
            warned: BTreeSet::new(),
            last_scan: None,
            probe: None,
            started_up: false,
            stalled: BTreeSet::new(),
            notifier,
            stats: Stats::default(),
        })
    }

    /// Writes are paused by an outage.
    #[must_use]
    pub fn is_down(&self) -> bool {
        self.outage.is_down()
    }

    /// Jobs with the workers.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.pool.in_flight()
    }

    /// Files waiting to be committed, over all runs.
    #[must_use]
    pub fn backlog(&self) -> usize {
        self.runs
            .values()
            .filter(|r| !r.done)
            .map(|r| r.life.outstanding(&r.journal.files))
            .sum()
    }

    /// One line for systemd and logs.
    #[must_use]
    pub fn status_line(&self) -> String {
        let active = self.runs.values().filter(|r| !r.done).count();
        let mut s = format!(
            "{} run(s), {active} active, {} complete; {} file(s) waiting, {} copying",
            self.runs.len(),
            self.stats.completed_runs,
            self.backlog(),
            self.pool.in_flight()
        );
        if let Some(d) = self.outage.down_for(self.clock.now()) {
            s.push_str(&format!("; destination unavailable for {}", human(d)));
        }
        s
    }

    /// Every detected run is complete or cannot progress, and nothing is
    /// being copied.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.started_up
            && self.last_scan.is_some()
            && !self.outage.is_down()
            && self.pool.in_flight() == 0
            && self.runs.values().all(|r| {
                r.done
                    || r.broken.is_some()
                    || (!r.failed.is_empty()
                        && r.life.complete_since.is_some()
                        && r.life.outstanding(&r.journal.files) <= r.failed.len())
            })
    }

    fn outage_failure(&mut self, now: Duration, reason: &str) {
        let first = !self.outage.is_down();
        self.pool.set_paused(true);
        let backlog = self.backlog();
        for e in self.outage.failure(now, reason, backlog) {
            log_event(&e);
        }
        if first {
            self.stats.outages += 1;
        }
    }

    fn start_probe(&mut self, now: Duration) {
        if self.probe.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let fs = Arc::clone(&self.fs);
        let guard = Arc::clone(&self.guard);
        let spawned = std::thread::Builder::new()
            .name("guard-probe".into())
            .spawn(move || {
                let result = guard.check(&*fs).map(|_| ()).map_err(|e| e.to_string());
                let wrong_target = if result.is_err() {
                    guard.wrong_target(&*fs)
                } else {
                    None
                };
                let _ = tx.send(ProbeResult {
                    result,
                    wrong_target,
                });
            });
        match spawned {
            Ok(_) => {
                self.probe = Some(Probe {
                    rx,
                    started: now,
                    timed_out: false,
                });
            }
            Err(e) => self.outage_failure(now, &format!("cannot start a destination check: {e}")),
        }
    }

    /// Collect the probe result; start a new probe when one is due.
    fn probe_step(&mut self, now: Duration) -> Result<()> {
        if let Some(p) = &mut self.probe {
            match p.rx.try_recv() {
                Ok(res) => {
                    self.probe = None;
                    self.probe_result(now, res)?;
                }
                Err(TryRecvError::Empty) => {
                    if !p.timed_out && now.saturating_sub(p.started) >= self.opts.probe_timeout {
                        p.timed_out = true;
                        let msg = format!(
                            "destination check did not answer within {}",
                            human(self.opts.probe_timeout)
                        );
                        self.started_up = true;
                        self.outage_failure(now, &msg);
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.probe = None;
                    self.outage_failure(now, "destination check ended without a result");
                }
            }
        }
        if self.probe.is_none() && (!self.started_up || self.outage.check_due(now)) {
            self.start_probe(now);
        }
        Ok(())
    }

    fn probe_result(&mut self, now: Duration, res: ProbeResult) -> Result<()> {
        let first = !self.started_up;
        self.started_up = true;
        match res.result {
            Ok(()) => {
                if first && !self.outage.is_down() {
                    log::info!(
                        "destination {} passes the guard",
                        escape_control(&self.guard.dest().to_string_lossy())
                    );
                    return Ok(());
                }
                let backlog = self.backlog();
                for e in self.outage.success(now, backlog) {
                    log_event(&e);
                }
                if !self.outage.is_down() {
                    self.pool.set_paused(false);
                }
            }
            Err(msg) => {
                if first {
                    if let Some(found) = res.wrong_target {
                        return Err(Error::Guard(format!(
                            "{msg}; the share at {} belongs to target {:?}, not {:?}; refusing to start",
                            escape_control(&self.guard.dest().to_string_lossy()),
                            escape_control(&found),
                            self.cfg.target_id
                        )));
                    }
                    log::warn!(
                        "starting while the destination is unavailable; runs wait on the local disk"
                    );
                }
                self.outage_failure(now, &msg);
            }
        }
        Ok(())
    }

    fn warn_once(&mut self, msg: &str) {
        if self.warned.insert(msg.to_owned()) {
            log::warn!("{msg}");
        }
    }

    /// Detect runs and refresh every unfinished run's files. Reads only the
    /// source and the state folder.
    ///
    /// # Errors
    /// The source root cannot be listed; journal I/O errors.
    pub fn scan(&mut self, now: Duration) -> Result<()> {
        self.last_scan = Some(now);
        let det = run::detect(&self.cfg, &*self.fs)?;
        for w in det.warnings {
            self.warn_once(&w);
        }
        for d in det.runs {
            if self.ignored.contains(&d.path) {
                continue;
            }
            if !self.runs.contains_key(&d.path) {
                match self.track(now, &d) {
                    Ok(r) => {
                        self.runs.insert(d.path.clone(), r);
                    }
                    Err(e) => {
                        log::error!(
                            "run {}: {e}; not offloaded",
                            escape_control(&d.rel.to_string_lossy())
                        );
                        self.ignored.insert(d.path.clone());
                        self.stats.failed_runs += 1;
                        continue;
                    }
                }
            }
            let Some(r) = self.runs.get_mut(&d.path) else {
                continue;
            };
            if r.done {
                continue;
            }
            r.det = d;
            match run::select(&*self.fs, &r.det.path, &self.cfg.selection, &self.markers) {
                Ok(sel) => {
                    r.life.observe(now, &sel, r.det.complete.is_some());
                    r.sel = Some(sel);
                }
                Err(e) => {
                    let msg = format!(
                        "run {}: cannot list files: {e}",
                        escape_control(&r.det.rel.to_string_lossy())
                    );
                    self.warn_once(&msg);
                }
            }
        }
        Ok(())
    }

    fn track(&mut self, now: Duration, d: &DetectedRun) -> Result<LiveRun> {
        let rel = run::dest_rel(&d.rel, self.cfg.dest_starts_at).map_err(Error::Config)?;
        let dest = safe_join(self.guard.dest(), &rel).map_err(Error::Config)?;
        let state = &self.cfg.state_dir.resolved;
        let mut j = Journal::open(&*self.fs, state, &d.path)?;
        journal::claim(
            &*self.fs,
            state,
            &mut j,
            &d.path,
            &rel,
            self.cfg.dest_starts_at,
        )?;
        let mut r = LiveRun {
            det: d.clone(),
            dest,
            journal: j,
            life: Lifecycle::new(now),
            sel: None,
            started: journal::now_rfc3339(),
            inflight: BTreeSet::new(),
            attempts: BTreeMap::new(),
            failed: BTreeMap::new(),
            finalising: None,
            done: false,
            broken: None,
            phase: Phase::Live,
        };
        let name = escape_control(&d.rel.to_string_lossy());
        if r.journal.complete.is_some() {
            let sel = run::select(&*self.fs, &d.path, &self.cfg.selection, &self.markers)?;
            let same = sel.files.iter().all(|f| {
                r.journal
                    .files
                    .get(&f.rel)
                    .is_some_and(|c| c.size == f.size && c.mtime_ns == journal::nanos(f.modified))
            });
            if same {
                log::info!("run {name}: already complete");
                r.done = true;
                return Ok(r);
            }
            log::warn!("run {name}: source changed since it was completed; offloading again");
        }
        log::info!(
            "run {name} (id {}): detected by {}, {} -> {}",
            r.journal.id,
            escape_control(&d.marker),
            d.complete.as_deref().map_or_else(
                || "live".to_owned(),
                |c| format!("complete marker {}", escape_control(c))
            ),
            escape_control(&r.dest.to_string_lossy())
        );
        Ok(r)
    }

    /// Hand stable files and finished runs to the workers.
    fn schedule(&mut self, now: Duration) -> Result<()> {
        let cap = self.pool.workers() * 2;
        let settle = self.cfg.transfer.settle;
        let after_complete = self.cfg.transfer.settle_after_complete;
        let mut jobs = Vec::new();
        let mut in_flight = self.pool.in_flight();
        for (key, r) in &mut self.runs {
            if r.done || r.broken.is_some() {
                continue;
            }
            let name = escape_control(&r.det.rel.to_string_lossy());
            let phase = r.life.phase(now, after_complete, &r.journal.files);
            if phase != r.phase {
                log::info!(
                    "run {name}: {} ({} file(s) waiting)",
                    phase.as_str(),
                    r.life.outstanding(&r.journal.files)
                );
                r.phase = phase;
            }
            for rel in r.life.ready(now, settle, &r.journal.files) {
                if in_flight >= cap {
                    break;
                }
                if r.inflight.contains(&rel) || r.failed.contains_key(&rel) {
                    continue;
                }
                let src = safe_join(&r.det.path, Path::new(&rel)).map_err(Error::Config)?;
                let dst = safe_join(&r.dest, Path::new(&rel)).map_err(Error::Config)?;
                r.inflight.insert(rel.clone());
                jobs.push(Job::Copy {
                    run: key.clone(),
                    ours: r.journal.files.contains_key(&rel),
                    rel,
                    src,
                    dst,
                });
                in_flight += 1;
            }
            if phase == Phase::Finalising
                && r.inflight.is_empty()
                && r.failed.is_empty()
                && r.finalising.is_none()
                && in_flight < cap
            {
                if let Some(sel) = &r.sel {
                    let out = finalise::prepare(
                        &self.cfg, &*self.fs, &r.det, &r.dest, &r.journal, sel, &r.started, "watch",
                    )?;
                    r.finalising = Some(out.manifest_sha256.clone());
                    jobs.push(Job::Finalise {
                        run: key.clone(),
                        dest: r.dest.clone(),
                        outputs: Box::new(out),
                    });
                    in_flight += 1;
                }
            }
        }
        for j in jobs {
            self.pool.submit(j);
        }
        Ok(())
    }

    /// Act on one finished job.
    ///
    /// Implements: SRS-RES-001
    ///
    /// # Errors
    /// Journal I/O errors.
    #[allow(clippy::too_many_lines)]
    pub fn handle(&mut self, o: Outcome, now: Duration) -> Result<()> {
        let key = o.job.run().clone();
        let max_attempts = self.cfg.transfer.max_attempts;
        let mut outage: Option<String> = None;
        {
            let Some(r) = self.runs.get_mut(&key) else {
                return Ok(());
            };
            let name = escape_control(&r.det.rel.to_string_lossy());
            match (o.job, o.result) {
                (Job::Copy { rel, .. }, result) => {
                    r.inflight.remove(&rel);
                    let shown = escape_control(&rel);
                    match result {
                        Ok(Done::Copied(c)) => {
                            r.journal.append(
                                &*self.fs,
                                Record::File {
                                    rel: rel.clone(),
                                    size: c.size,
                                    mtime_ns: c.mtime_ns,
                                    sha256: c.sha256.clone(),
                                    ts: journal::now_rfc3339(),
                                },
                            )?;
                            log::info!("{}", commit_message(&r.journal.id, &rel, &c, o.secs));
                            r.attempts.remove(&rel);
                            self.stats.committed += 1;
                        }
                        Ok(Done::Finalised) | Err(Failure::Finalise(_)) => {}
                        Err(Failure::Copy(f)) => match f {
                            CopyFail::Paused => {}
                            CopyFail::Guard(e) => outage = Some(e.to_string()),
                            CopyFail::Io(e) if classify(&e) == ErrorClass::Outage => {
                                outage = Some(format!("{e} while copying {shown}"));
                            }
                            CopyFail::Io(e) if classify(&e) == ErrorClass::NoSpace => {
                                outage =
                                    Some(format!("destination full ({e}) while copying {shown}"));
                            }
                            CopyFail::Conflict(m) => {
                                log::error!("run {name}: {shown}: {m}");
                                r.failed.insert(rel, m);
                                self.stats.failed_files += 1;
                            }
                            other => {
                                let n = r.attempts.entry(rel.clone()).or_insert(0);
                                *n += 1;
                                if *n >= max_attempts {
                                    log::error!(
                                        "run {name}: {shown}: failed after {n} attempt(s): {other}"
                                    );
                                    r.failed.insert(rel, other.to_string());
                                    self.stats.failed_files += 1;
                                } else {
                                    log::warn!(
                                        "run {name}: {shown}: attempt {n} failed ({other}); retrying"
                                    );
                                }
                            }
                        },
                    }
                }
                (Job::Finalise { .. }, result) => {
                    let sha = r.finalising.take().unwrap_or_default();
                    match result {
                        Ok(_) => {
                            r.journal.append(
                                &*self.fs,
                                Record::Complete {
                                    manifest_sha256: sha.clone(),
                                    ts: journal::now_rfc3339(),
                                },
                            )?;
                            let files = r.sel.as_ref().map_or(0, |s| s.files.len());
                            let bytes = r.sel.as_ref().map_or(0, Selected::bytes);
                            log::info!(
                                "run {name}: COMPLETE, {files} files, {bytes} bytes -> {}, manifest sha256={sha}",
                                escape_control(&r.dest.to_string_lossy())
                            );
                            r.done = true;
                            self.stats.completed_runs += 1;
                        }
                        Err(Failure::Finalise(Error::Guard(e))) => outage = Some(e),
                        Err(Failure::Finalise(Error::Io(e)))
                            if matches!(classify(&e), ErrorClass::Outage | ErrorClass::NoSpace) =>
                        {
                            outage = Some(format!("{e} while finalising run {name}"));
                        }
                        Err(e) => {
                            log::error!("run {name}: cannot be completed: {e}");
                            r.broken = Some(e.to_string());
                            self.stats.failed_runs += 1;
                        }
                    }
                }
            }
        }
        if let Some(reason) = outage {
            self.outage_failure(now, &reason);
        }
        Ok(())
    }

    /// Report workers without progress for `stall_timeout`.
    ///
    /// Implements: SRS-RES-003
    ///
    /// # Errors
    /// [`Error::RestartNeeded`] when every worker is stalled.
    pub fn check_stalls(&mut self, now: Duration) -> Result<()> {
        let limit = self.cfg.transfer.stall_timeout;
        if limit.is_zero() {
            return Ok(());
        }
        let busy = self.pool.busy();
        let current: BTreeSet<(usize, Duration)> =
            busy.iter().map(|(i, b)| (*i, b.started)).collect();
        self.stalled.retain(|k| current.contains(k));
        for (i, b) in &busy {
            let idle = now.saturating_sub(b.last_progress);
            if idle >= limit && self.stalled.insert((*i, b.started)) {
                self.stats.stalls += 1;
                log::error!(
                    "worker {i} stalled: no progress for {} on {} of run {} ({} bytes written)",
                    human(idle),
                    escape_control(&b.label),
                    escape_control(&b.run.to_string_lossy()),
                    b.bytes
                );
            }
        }
        if !self.stalled.is_empty() && self.stalled.len() >= self.pool.workers() {
            return Err(Error::RestartNeeded(format!(
                "all {} worker(s) are stalled (no progress for {}); committed files are kept",
                self.pool.workers(),
                human(limit)
            )));
        }
        Ok(())
    }

    /// One controller step at clock reading `now`.
    ///
    /// # Errors
    /// [`Error::Guard`] when the share belongs to another target at start-up,
    /// [`Error::RestartNeeded`] when every worker is stalled, source and
    /// journal I/O errors.
    pub fn step(&mut self, now: Duration) -> Result<()> {
        while let Some(o) = self.pool.recv_timeout(Duration::ZERO) {
            self.handle(o, now)?;
        }
        self.probe_step(now)?;
        let backlog = self.backlog();
        for e in self.outage.tick(now, backlog) {
            log_event(&e);
        }
        if self.started_up
            && self
                .last_scan
                .is_none_or(|t| now.saturating_sub(t) >= self.opts.scan_every)
        {
            self.scan(now)?;
        }
        if self.started_up && !self.outage.is_down() {
            self.schedule(now)?;
        }
        self.check_stalls(now)?;
        Ok(())
    }

    /// Wait up to `timeout` (real time) for one job to finish and handle it.
    ///
    /// # Errors
    /// As [`Watcher::handle`].
    pub fn wait(&mut self, timeout: Duration) -> Result<bool> {
        match self.pool.recv_timeout(timeout) {
            Some(o) => {
                let now = self.clock.now();
                self.handle(o, now)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Wait (real time, at most `limit`) until no job and no destination
    /// check is pending, handling every result at clock reading `now`. Lets
    /// tests on a fake clock step deterministically.
    ///
    /// # Errors
    /// As [`Watcher::handle`] and [`Watcher::step`].
    pub fn quiesce(&mut self, now: Duration, limit: Duration) -> Result<()> {
        let t0 = std::time::Instant::now();
        while t0.elapsed() < limit {
            if self.pool.in_flight() > 0 {
                if let Some(o) = self.pool.recv_timeout(Duration::from_millis(10)) {
                    self.handle(o, now)?;
                }
            } else if self.probe.is_some() {
                std::thread::sleep(Duration::from_millis(1));
                self.probe_step(now)?;
            } else {
                break;
            }
        }
        Ok(())
    }

    /// Run until stopped (or, with `until_idle`, until [`Watcher::idle`]).
    ///
    /// # Errors
    /// As [`Watcher::step`]; [`Error::Failed`] with `until_idle` when files
    /// or runs failed.
    pub fn run(&mut self) -> Result<()> {
        let mut ready = false;
        let mut last_watchdog = std::time::Instant::now();
        loop {
            let now = self.clock.now();
            self.step(now)?;
            let status = self.status_line();
            if ready {
                // WatchdogSec is minutes; every few seconds is plenty.
                if last_watchdog.elapsed() >= Duration::from_secs(5) {
                    self.notifier.watchdog(&status);
                    last_watchdog = std::time::Instant::now();
                }
            } else if self.started_up {
                self.notifier.ready(&status);
                ready = true;
            }
            if self.opts.until_idle && self.idle() {
                self.notifier.stopping();
                log::info!("idle: {status}");
                let failed = self.stats.failed_files + self.stats.failed_runs;
                return if failed == 0 {
                    Ok(())
                } else {
                    Err(Error::Failed(failed))
                };
            }
            self.wait(self.opts.tick)?;
        }
    }
}
