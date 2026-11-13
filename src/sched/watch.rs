// SPDX-License-Identifier: AGPL-3.0-or-later
//! The `watch` controller: discovers runs, claims their destinations on the
//! share, tracks their lifecycle, hands stable files to the workers fairly
//! across runs, handles outages and stalls, finalises runs, keeps completed
//! runs under watch for `watch_completed_for`, and publishes the live state.
//! One step at a time on a [`Clock`], so the whole controller runs on a fake
//! clock in tests.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime};

use serde_json::json;

use crate::config::{self, Config};
use crate::copy::{CopyFail, CopyOptions};
use crate::error::{Error, Result};
use crate::fsio::{ErrorClass, Fs, classify};
use crate::guard::Guard;
use crate::journal::{self, Journal, Record};
use crate::logging::escape_control;
use crate::run::lifecycle::{Lifecycle, Phase, committed_as};
use crate::run::offload::commit_message;
use crate::run::{
    self, DetectedRun, Markers, ScanCache, Selected, SelectedFile, finalise, safe_join,
};
use crate::state::{self, Kind, StateWriter};

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
    /// Check the destination (guard and free space) this often while it is
    /// available, so the state shows its free space and a lost share is
    /// noticed between copies.
    pub online_check_every: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scan_every: Duration::from_secs(10),
            probe_timeout: Duration::from_secs(30),
            tick: Duration::from_secs(1),
            until_idle: false,
            online_check_every: Duration::from_secs(60),
        }
    }
}

/// Counters for the state snapshot, the status line and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Files committed.
    pub committed: usize,
    /// Bytes committed.
    pub committed_bytes: u64,
    /// Files that failed for good.
    pub failed_files: usize,
    /// Runs completed.
    pub completed_runs: usize,
    /// Runs that cannot be completed (failed files, integrity, claim).
    pub failed_runs: usize,
    /// Completed runs offloaded again because the source changed.
    pub reopened_runs: usize,
    /// Completed runs no longer watched.
    pub retired_runs: usize,
    /// Copies whose destination did not match.
    pub mismatches: usize,
    /// Outages seen.
    pub outages: usize,
    /// Stalls seen.
    pub stalls: usize,
}

/// What must happen on the share before files of a run are copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// Write the claim file in the destination run folder.
    Claim,
    Claiming,
    /// Remove `COMPLETE` and `VERIFIED.json` (the run is offloaded again).
    Withdraw,
    Withdrawing,
    /// Copy.
    Open,
}

/// One run being offloaded.
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
    /// The run cannot be completed in this process.
    pub broken: Option<String>,
    phase: Phase,
    gate: Gate,
    withdraw: bool,
    cache: ScanCache,
    last_full: Duration,
    full_checked: bool,
    mismatches: u64,
}

/// A completed run, kept under watch with a few values instead of its
/// file list.
///
/// Implements: SRS-LIVE-004, SRS-RES-008
#[derive(Debug, Clone)]
pub struct DoneRun {
    /// As detected.
    pub det: DetectedRun,
    /// Destination run folder.
    pub dest: PathBuf,
    /// Run id.
    pub id: String,
    /// Manifest digest.
    pub manifest_sha256: String,
    /// Files in the manifest.
    pub files: u64,
    /// Bytes in the manifest.
    pub bytes: u64,
    /// Clock reading at completion.
    pub completed: Duration,
    sig: Vec<(PathBuf, Option<SystemTime>)>,
    fingerprint: String,
    last_full: Duration,
}

/// What a newly detected run turned out to be.
enum Tracked {
    Live(Box<LiveRun>),
    Done(Box<DoneRun>),
    Retired,
}

struct ProbeResult {
    result: std::result::Result<(), String>,
    wrong_target: Option<String>,
    free: Option<u64>,
    capacity_low: bool,
    mount: Option<crate::guard::Mount>,
}

struct Probe {
    rx: Receiver<ProbeResult>,
    started: Duration,
    timed_out: bool,
}

/// Destination facts from the last check.
#[derive(Debug, Clone, Default)]
struct DestInfo {
    ok: bool,
    free: Option<u64>,
    fstype: Option<String>,
    capacity_low: bool,
    outage_since: Option<String>,
    last_check: Option<Duration>,
}

/// The controller.
///
/// Implements: SRS-LIVE-001, SRS-LIVE-003, SRS-LIVE-004, SRS-PAR-001,
/// SRS-PAR-003, SRS-RES-001, SRS-RES-002, SRS-RES-003, SRS-RES-005,
/// SRS-RES-006, SRS-RES-008, SRS-RES-009, SRS-SEL-008, SRS-STATE-001
pub struct Watcher {
    cfg: Config,
    fs: Arc<dyn Fs>,
    guard: Arc<Guard>,
    clock: Arc<dyn Clock>,
    opts: Options,
    pool: Pool,
    rate: Arc<RateLimiter>,
    outage: Tracker,
    markers: Markers,
    /// Runs being offloaded, by source path.
    pub runs: BTreeMap<PathBuf, LiveRun>,
    /// Completed runs under watch, by source path.
    pub completed: BTreeMap<PathBuf, DoneRun>,
    retired: BTreeSet<PathBuf>,
    ignored: BTreeSet<PathBuf>,
    warned: BTreeSet<String>,
    last_scan: Option<Duration>,
    probe: Option<Probe>,
    started_up: bool,
    stalled: BTreeSet<(usize, Duration)>,
    notifier: Notifier,
    state: StateWriter,
    started_at: String,
    dest_info: DestInfo,
    source_free: Option<u64>,
    source_low: bool,
    alerts: BTreeMap<String, state::Alert>,
    recent: VecDeque<(Duration, u64)>,
    cursor: Option<PathBuf>,
    reload: Option<(PathBuf, Arc<AtomicBool>)>,
    /// Counters.
    pub stats: Stats,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("runs", &self.runs.len())
            .field("completed", &self.completed.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

fn level_str(level: log::Level) -> &'static str {
    match level {
        log::Level::Error => "error",
        log::Level::Warn => "warning",
        _ => "info",
    }
}

fn rel_of(det: &DetectedRun) -> String {
    det.rel.to_string_lossy().into_owned()
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
            min_free: cfg.transfer.min_free,
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
        let state = StateWriter::new(
            &*fs,
            &cfg.state_dir.resolved,
            cfg.state.interval,
            cfg.state.events_max_size,
            cfg.outage.notify_command.clone(),
        );
        Ok(Self {
            cfg,
            fs,
            guard,
            clock,
            opts,
            pool,
            rate,
            outage,
            markers,
            runs: BTreeMap::new(),
            completed: BTreeMap::new(),
            retired: BTreeSet::new(),
            ignored: BTreeSet::new(),
            warned: BTreeSet::new(),
            last_scan: None,
            probe: None,
            started_up: false,
            stalled: BTreeSet::new(),
            notifier,
            state,
            started_at: journal::now_rfc3339(),
            dest_info: DestInfo::default(),
            source_free: None,
            source_low: false,
            alerts: BTreeMap::new(),
            recent: VecDeque::new(),
            cursor: None,
            reload: None,
            stats: Stats::default(),
        })
    }

    /// Re-read `rate_limit` from `config_path` whenever `flag` is set
    /// (by SIGHUP).
    #[must_use]
    pub fn with_reload(mut self, config_path: &Path, flag: Arc<AtomicBool>) -> Self {
        self.reload = Some((config_path.to_path_buf(), flag));
        self
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

    /// Folder listings and status calls made by the incremental scans of
    /// the runs being offloaded (diagnostics and tests).
    #[must_use]
    pub fn scan_work(&self) -> (u64, u64) {
        self.runs
            .values()
            .fold((0, 0), |(l, s), r| (l + r.cache.listed, s + r.cache.stats))
    }

    /// The current rate limit in bytes per second.
    #[must_use]
    pub fn rate_limit(&self) -> Option<u64> {
        self.rate.limit()
    }

    /// Files waiting to be committed, over all runs.
    #[must_use]
    pub fn backlog(&self) -> usize {
        self.runs
            .values()
            .map(|r| r.life.outstanding(&r.journal.files))
            .sum()
    }

    fn backlog_bytes(r: &LiveRun) -> u64 {
        r.life
            .files
            .iter()
            .filter(|(rel, t)| !committed_as(&r.journal.files, rel, t))
            .map(|(_, t)| t.size)
            .sum()
    }

    /// One line for systemd and logs.
    #[must_use]
    pub fn status_line(&self) -> String {
        let mut s = format!(
            "{} run(s) active, {} complete under watch; {} file(s) waiting, {} copying",
            self.runs.len(),
            self.completed.len(),
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
                r.broken.is_some()
                    || (!r.failed.is_empty()
                        && r.gate == Gate::Open
                        && r.life.complete_since.is_some()
                        && r.life.outstanding(&r.journal.files) <= r.failed.len())
            })
    }

    // -----------------------------------------------------------------
    // Events and alerts
    // -----------------------------------------------------------------

    /// Log a state change and append it to `events.jsonl`.
    ///
    /// Implements: SRS-STATE-002
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        kind: Kind,
        level: log::Level,
        run: Option<&str>,
        file: Option<&str>,
        msg: &str,
        data: serde_json::Value,
    ) {
        log::log!(level, "{msg}");
        self.state
            .event(&*self.fs, kind, level, run, file, msg, data);
    }

    fn raise(&mut self, key: &str, kind: Kind, level: log::Level, run: Option<&str>, msg: &str) {
        let since = self
            .alerts
            .get(key)
            .map_or_else(journal::now_rfc3339, |a| a.since.clone());
        self.alerts.insert(
            key.to_owned(),
            state::Alert {
                kind,
                level: level_str(level).into(),
                since,
                run: run.map(escape_control),
                message: escape_control(msg),
            },
        );
    }

    fn clear(&mut self, key: &str) {
        self.alerts.remove(key);
    }

    fn outage_event(&mut self, e: &Event) {
        let (level, msg) = e.message();
        let (kind, data) = match e {
            Event::Started { reason, backlog } => (
                Kind::OutageStarted,
                json!({"reason": reason, "backlog_files": backlog}),
            ),
            Event::Summary {
                down,
                failures,
                reason,
                backlog,
            } => (
                Kind::OutageSummary,
                json!({"duration_s": down.as_secs(), "checks": failures, "reason": reason, "backlog_files": backlog}),
            ),
            Event::Warn { down } => (Kind::OutageWarning, json!({"duration_s": down.as_secs()})),
            Event::Alert { down, reminder } => (
                if *reminder {
                    Kind::OutageReminder
                } else {
                    Kind::OutageAlert
                },
                json!({"duration_s": down.as_secs()}),
            ),
            Event::Recovering => (Kind::OutageRecovering, json!({})),
            Event::Ended {
                down,
                failures,
                backlog,
            } => (
                Kind::OutageEnded,
                json!({"duration_s": down.as_secs(), "checks": failures, "backlog_files": backlog}),
            ),
        };
        self.emit(kind, level, None, None, &msg, data);
        match e {
            Event::Warn { .. } => self.raise("outage", kind, log::Level::Warn, None, &msg),
            Event::Alert { .. } => {
                self.raise("outage", Kind::OutageAlert, log::Level::Error, None, &msg);
            }
            Event::Ended { .. } => self.clear("outage"),
            _ => {}
        }
    }

    // -----------------------------------------------------------------
    // Destination checks
    // -----------------------------------------------------------------

    fn set_paused(&mut self, paused: bool, why: &str) {
        if self.pool.paused() == paused {
            return;
        }
        self.pool.set_paused(paused);
        if paused {
            self.emit(
                Kind::Paused,
                log::Level::Info,
                None,
                None,
                &format!("workers paused: {why}"),
                json!({"reason": why}),
            );
        } else {
            self.emit(
                Kind::Resumed,
                log::Level::Info,
                None,
                None,
                "workers resumed",
                json!({}),
            );
        }
    }

    fn outage_failure(&mut self, now: Duration, reason: &str) {
        let first = !self.outage.is_down();
        self.dest_info.ok = false;
        if first {
            self.stats.outages += 1;
            self.dest_info.outage_since = Some(journal::now_rfc3339());
        }
        let backlog = self.backlog();
        for e in self.outage.failure(now, reason, backlog) {
            self.outage_event(&e);
        }
        self.set_paused(true, reason);
    }

    fn start_probe(&mut self, now: Duration) {
        if self.probe.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let fs = Arc::clone(&self.fs);
        let guard = Arc::clone(&self.guard);
        let min_free = self.cfg.transfer.min_free;
        let spawned = std::thread::Builder::new()
            .name("guard-probe".into())
            .spawn(move || {
                let checked = guard.check(&*fs);
                let mut res = ProbeResult {
                    result: Ok(()),
                    wrong_target: None,
                    free: None,
                    capacity_low: false,
                    mount: None,
                };
                match checked {
                    Ok(m) => {
                        res.free = fs.free_space(guard.dest()).ok();
                        res.mount = Some(m);
                        if let Some(free) = res.free.filter(|f| *f < min_free) {
                            res.capacity_low = true;
                            res.result = Err(format!(
                                "destination has {} free, below min_free {}",
                                config::units::Bytes(free),
                                config::units::Bytes(min_free)
                            ));
                        }
                    }
                    Err(e) => {
                        res.result = Err(e.to_string());
                        res.wrong_target = guard.wrong_target(&*fs);
                    }
                }
                let _ = tx.send(res);
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
        let online_due = self.started_up
            && !self.outage.is_down()
            && self
                .dest_info
                .last_check
                .is_none_or(|t| now.saturating_sub(t) >= self.opts.online_check_every);
        if self.probe.is_none() && (!self.started_up || self.outage.check_due(now) || online_due) {
            self.start_probe(now);
        }
        Ok(())
    }

    /// Implements: SRS-RES-010, SRS-STATE-003
    #[allow(clippy::too_many_lines)]
    fn probe_result(&mut self, now: Duration, res: ProbeResult) -> Result<()> {
        let first = !self.started_up;
        self.started_up = true;
        self.dest_info.last_check = Some(now);
        if res.free.is_some() {
            self.dest_info.free = res.free;
        }
        if let Some(m) = &res.mount {
            self.dest_info.fstype = Some(m.fstype.clone());
            if first {
                log::info!(
                    "destination {} passes the guard: {} {} mounted at {} ({})",
                    escape_control(&self.guard.dest().to_string_lossy()),
                    escape_control(&m.fstype),
                    escape_control(&m.source),
                    escape_control(&m.point.to_string_lossy()),
                    escape_control(&m.options)
                );
            }
        }
        if res.capacity_low != self.dest_info.capacity_low {
            self.dest_info.capacity_low = res.capacity_low;
            let free = res.free.unwrap_or(0);
            if res.capacity_low {
                let msg = res.result.clone().err().unwrap_or_default();
                self.emit(
                    Kind::CapacityLow,
                    log::Level::Error,
                    None,
                    None,
                    &msg,
                    json!({"free_bytes": free, "min_free": self.cfg.transfer.min_free}),
                );
                self.raise("capacity", Kind::CapacityLow, log::Level::Error, None, &msg);
            } else if res.result.is_ok() {
                self.emit(
                    Kind::CapacityOk,
                    log::Level::Info,
                    None,
                    None,
                    &format!("destination has {} free again", config::units::Bytes(free)),
                    json!({"free_bytes": free}),
                );
                self.clear("capacity");
            }
        }
        match res.result {
            Ok(()) => {
                if first && !self.outage.is_down() {
                    self.dest_info.ok = true;
                    let msg = format!(
                        "destination {} ready",
                        escape_control(&self.guard.dest().to_string_lossy())
                    );
                    self.emit(
                        Kind::DestinationOk,
                        log::Level::Info,
                        None,
                        None,
                        &msg,
                        json!({"free_bytes": res.free}),
                    );
                    return Ok(());
                }
                let backlog = self.backlog();
                for e in self.outage.success(now, backlog) {
                    self.outage_event(&e);
                }
                if !self.outage.is_down() {
                    self.dest_info.ok = true;
                    self.dest_info.outage_since = None;
                    self.clear("guard");
                    self.set_paused(false, "");
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
                if !self.outage.is_down() && !res.capacity_low {
                    self.emit(
                        Kind::GuardFailed,
                        log::Level::Warn,
                        None,
                        None,
                        &format!("destination check failed: {msg}"),
                        json!({"reason": msg, "other_target": res.wrong_target}),
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

    // -----------------------------------------------------------------
    // Scanning
    // -----------------------------------------------------------------

    /// Detect runs, refresh every unfinished run's files (only changed
    /// folders, with a full rescan every `full_rescan_every`), and check
    /// completed runs for changes. Reads only the source and the state
    /// folder.
    ///
    /// # Errors
    /// The source root cannot be listed; journal I/O errors.
    pub fn scan(&mut self, now: Duration) -> Result<()> {
        self.last_scan = Some(now);
        self.source_space();
        let det = run::detect(&self.cfg, &*self.fs)?;
        for w in det.warnings {
            self.warn_once(&w);
        }
        let mut present = BTreeSet::new();
        for d in det.runs {
            present.insert(d.path.clone());
            if self.ignored.contains(&d.path) || self.retired.contains(&d.path) {
                continue;
            }
            if let Some(c) = self.completed.get_mut(&d.path) {
                c.det = d;
                continue;
            }
            if !self.runs.contains_key(&d.path) {
                match self.track(now, &d) {
                    Ok(Tracked::Live(r)) => {
                        self.runs.insert(d.path.clone(), *r);
                    }
                    Ok(Tracked::Done(c)) => {
                        self.completed.insert(d.path.clone(), *c);
                        continue;
                    }
                    Ok(Tracked::Retired) => {
                        self.retired.insert(d.path.clone());
                        continue;
                    }
                    Err(e) => {
                        let rel = rel_of(&d);
                        let msg = format!("run {}: {e}; not offloaded", escape_control(&rel));
                        self.emit(
                            Kind::RunFailed,
                            log::Level::Error,
                            Some(&rel),
                            None,
                            &msg,
                            json!({"reason": e.to_string()}),
                        );
                        self.raise(
                            &format!("run:{rel}"),
                            Kind::RunFailed,
                            log::Level::Error,
                            Some(&rel),
                            &msg,
                        );
                        self.ignored.insert(d.path.clone());
                        self.stats.failed_runs += 1;
                        continue;
                    }
                }
            }
            self.refresh(now, d);
        }
        self.watch_completed(now, &present)?;
        Ok(())
    }

    /// Re-select one live run's files, listing only folders that changed.
    ///
    /// Implements: SRS-RES-009
    fn refresh(&mut self, now: Duration, d: DetectedRun) {
        let full_every = self.cfg.source.full_rescan_every;
        let Some(r) = self.runs.get_mut(&d.path) else {
            return;
        };
        r.det = d;
        if now.saturating_sub(r.last_full) >= full_every {
            r.cache.clear();
            r.last_full = now;
        }
        let LiveRun {
            det,
            journal,
            life,
            cache,
            ..
        } = r;
        let known = |rel: &str| -> Option<SelectedFile> {
            let t = life.files.get(rel)?;
            committed_as(&journal.files, rel, t).then(|| SelectedFile {
                rel: rel.to_owned(),
                size: t.size,
                modified: Some(SystemTime::UNIX_EPOCH + Duration::from_nanos(t.mtime_ns)),
                deferred: false,
            })
        };
        let sel = run::select_cached(
            &*self.fs,
            &det.path,
            &self.cfg.selection,
            &self.markers,
            cache,
            &known,
        );
        match sel {
            Ok(sel) => {
                if r.life.observe(now, &sel, r.det.complete.is_some()) > 0 {
                    r.full_checked = false;
                }
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

    /// Free space on the source against `source_free_alert`.
    ///
    /// Implements: SRS-STATE-003
    fn source_space(&mut self) {
        let free = self.fs.free_space(&self.cfg.source.root.resolved).ok();
        self.source_free = free.filter(|f| *f != u64::MAX);
        let limit = self.cfg.outage.source_free_alert;
        let Some(free) = self.source_free else {
            return;
        };
        let low = limit > 0 && free < limit;
        if low == self.source_low {
            return;
        }
        self.source_low = low;
        if low {
            let msg = format!(
                "source disk has {} free, below source_free_alert {}; sequencing may stop if nothing leaves it",
                config::units::Bytes(free),
                config::units::Bytes(limit)
            );
            self.emit(
                Kind::SourceSpaceLow,
                log::Level::Error,
                None,
                None,
                &msg,
                json!({"free_bytes": free, "limit": limit}),
            );
            self.raise(
                "source_space",
                Kind::SourceSpaceLow,
                log::Level::Error,
                None,
                &msg,
            );
        } else {
            self.emit(
                Kind::SourceSpaceOk,
                log::Level::Info,
                None,
                None,
                &format!("source disk has {} free again", config::units::Bytes(free)),
                json!({"free_bytes": free}),
            );
            self.clear("source_space");
        }
    }

    /// Check completed runs for changes; reopen changed ones; retire runs
    /// past `watch_completed_for` or gone from the source.
    ///
    /// Implements: SRS-LIVE-004, SRS-RES-008
    fn watch_completed(&mut self, now: Duration, present: &BTreeSet<PathBuf>) -> Result<()> {
        let window = self.cfg.transfer.watch_completed_for;
        let full_every = self.cfg.source.full_rescan_every;
        let keys: Vec<PathBuf> = self.completed.keys().cloned().collect();
        for key in keys {
            let Some(c) = self.completed.get_mut(&key) else {
                continue;
            };
            let rel = rel_of(&c.det);
            if now.saturating_sub(c.completed) >= window || !present.contains(&key) {
                let gone = !present.contains(&key);
                self.completed.remove(&key);
                self.retired.insert(key);
                self.stats.retired_runs += 1;
                let msg = if gone {
                    format!(
                        "run {}: no longer on the source; not watched any more",
                        escape_control(&rel)
                    )
                } else {
                    format!(
                        "run {}: complete for {}; not watched any more",
                        escape_control(&rel),
                        human(window)
                    )
                };
                self.emit(
                    Kind::RunRetired,
                    log::Level::Info,
                    Some(&rel),
                    None,
                    &msg,
                    json!({}),
                );
                continue;
            }
            let full_due = now.saturating_sub(c.last_full) >= full_every;
            if !full_due && !run::signature_changed(&*self.fs, &c.sig) {
                continue;
            }
            let sel = match run::select(&*self.fs, &key, &self.cfg.selection, &self.markers) {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("run {}: cannot list files: {e}", escape_control(&rel));
                    self.warn_once(&msg);
                    continue;
                }
            };
            if run::fingerprint(&sel) == c.fingerprint {
                c.sig = run::dir_signature(&*self.fs, &key).unwrap_or_default();
                c.last_full = now;
                continue;
            }
            if let Some(c) = self.completed.remove(&key) {
                self.reopen(now, c)?;
            }
        }
        Ok(())
    }

    /// A completed run changed at the source: withdraw `COMPLETE` and
    /// `VERIFIED.json` first, then offload it again.
    ///
    /// Implements: SRS-LIVE-004
    fn reopen(&mut self, now: Duration, c: DoneRun) -> Result<()> {
        let rel = rel_of(&c.det);
        let journal = Journal::open(&*self.fs, &self.cfg.state_dir.resolved, &c.det.path)?;
        let mut r = Self::live_run(now, &c.det, c.dest.clone(), journal);
        r.gate = Gate::Withdraw;
        r.withdraw = true;
        self.stats.reopened_runs += 1;
        let msg = format!(
            "run {}: changed at the source after it was completed; withdrawing COMPLETE and offloading again",
            escape_control(&rel)
        );
        self.emit(
            Kind::RunReopened,
            log::Level::Warn,
            Some(&rel),
            None,
            &msg,
            json!({"manifest_sha256": c.manifest_sha256}),
        );
        let key = c.det.path.clone();
        self.runs.insert(key, r);
        self.refresh(now, c.det);
        Ok(())
    }

    fn live_run(now: Duration, d: &DetectedRun, dest: PathBuf, journal: Journal) -> LiveRun {
        LiveRun {
            det: d.clone(),
            dest,
            journal,
            life: Lifecycle::new(now),
            sel: None,
            started: journal::now_rfc3339(),
            inflight: BTreeSet::new(),
            attempts: BTreeMap::new(),
            failed: BTreeMap::new(),
            finalising: None,
            broken: None,
            phase: Phase::Live,
            gate: Gate::Claim,
            withdraw: false,
            cache: ScanCache::default(),
            last_full: now,
            full_checked: false,
            mismatches: 0,
        }
    }

    fn done_run(
        &self,
        now: Duration,
        det: &DetectedRun,
        destination: PathBuf,
        j: &Journal,
        sel: &Selected,
        age: Duration,
    ) -> DoneRun {
        DoneRun {
            det: det.clone(),
            dest: destination,
            id: j.id.clone(),
            manifest_sha256: j.complete.clone().unwrap_or_default(),
            files: sel.files.len() as u64,
            bytes: sel.bytes(),
            completed: now.saturating_sub(age),
            sig: run::dir_signature(&*self.fs, &det.path).unwrap_or_default(),
            fingerprint: run::fingerprint(sel),
            last_full: now,
        }
    }

    fn track(&mut self, now: Duration, d: &DetectedRun) -> Result<Tracked> {
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
        let name = escape_control(&d.rel.to_string_lossy());
        let mut withdraw = false;
        if j.complete.is_some() {
            let sel = run::select(&*self.fs, &d.path, &self.cfg.selection, &self.markers)?;
            let same = sel.files.iter().all(|f| {
                j.files
                    .get(&f.rel)
                    .is_some_and(|c| c.size == f.size && c.mtime_ns == journal::nanos(f.modified))
            });
            if same {
                let age = j
                    .completed_at
                    .as_deref()
                    .and_then(|t| humantime::parse_rfc3339_weak(t).ok())
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .unwrap_or_default();
                if age >= self.cfg.transfer.watch_completed_for {
                    log::info!("run {name}: already complete");
                    return Ok(Tracked::Retired);
                }
                log::info!(
                    "run {name}: already complete; watching it for changes for another {}",
                    human(self.cfg.transfer.watch_completed_for.saturating_sub(age))
                );
                return Ok(Tracked::Done(Box::new(
                    self.done_run(now, d, dest, &j, &sel, age),
                )));
            }
            log::warn!(
                "run {name}: source changed since it was completed; withdrawing COMPLETE and offloading again"
            );
            withdraw = true;
            self.stats.reopened_runs += 1;
        }
        let mut r = Self::live_run(now, d, dest, j);
        r.withdraw = withdraw;
        let msg = format!(
            "run {name} (id {}): detected by {}, {} -> {}",
            r.journal.id,
            escape_control(&d.marker),
            d.complete.as_deref().map_or_else(
                || "live".to_owned(),
                |c| format!("complete marker {}", escape_control(c))
            ),
            escape_control(&r.dest.to_string_lossy())
        );
        let rel = rel_of(d);
        self.emit(
            if withdraw {
                Kind::RunReopened
            } else {
                Kind::RunDiscovered
            },
            if withdraw {
                log::Level::Warn
            } else {
                log::Level::Info
            },
            Some(&rel),
            None,
            &msg,
            json!({"run_id": r.journal.id, "destination": r.dest.to_string_lossy()}),
        );
        Ok(Tracked::Live(Box::new(r)))
    }

    // -----------------------------------------------------------------
    // Scheduling
    // -----------------------------------------------------------------

    /// Hand claims, stable files and finished runs to the workers. Files
    /// are taken from the runs in turn (one each, oldest waiting file first
    /// within a run), starting after the run served last, so a large run
    /// cannot hold every worker while another run waits.
    ///
    /// Implements: SRS-PAR-003, SRS-SEL-008, SRS-LIVE-004, SRS-RES-009
    #[allow(clippy::too_many_lines)]
    fn schedule(&mut self, now: Duration) -> Result<()> {
        let cap = self.pool.workers() * 2;
        let settle = self.cfg.transfer.settle;
        let after_complete = self.cfg.transfer.settle_after_complete;
        let mut in_flight = self.pool.in_flight();
        let mut jobs = Vec::new();
        let mut queues: Vec<(PathBuf, VecDeque<String>)> = Vec::new();
        let mut phase_events = Vec::new();
        let mut need_full = Vec::new();
        for (key, r) in &mut self.runs {
            if r.broken.is_some() {
                continue;
            }
            match r.gate {
                Gate::Claim if in_flight < cap => {
                    r.gate = Gate::Claiming;
                    jobs.push(Job::Claim {
                        run: key.clone(),
                        dest: r.dest.clone(),
                        run_id: r.journal.id.clone(),
                        target_id: self.cfg.target_id.clone(),
                    });
                    in_flight += 1;
                    continue;
                }
                Gate::Withdraw if in_flight < cap => {
                    r.gate = Gate::Withdrawing;
                    jobs.push(Job::Withdraw {
                        run: key.clone(),
                        dest: r.dest.clone(),
                    });
                    in_flight += 1;
                    continue;
                }
                Gate::Open => {}
                _ => continue,
            }
            let phase = r.life.phase(now, after_complete, &r.journal.files);
            if phase != r.phase {
                phase_events.push((rel_of(&r.det), phase, r.life.outstanding(&r.journal.files)));
                r.phase = phase;
            }
            let mut ready: Vec<(Duration, String)> = r
                .life
                .ready(now, settle, &r.journal.files)
                .into_iter()
                .filter(|rel| !r.inflight.contains(rel) && !r.failed.contains_key(rel))
                .map(|rel| (r.life.files.get(&rel).map_or(now, |t| t.since), rel))
                .collect();
            ready.sort();
            if !ready.is_empty() {
                queues.push((key.clone(), ready.into_iter().map(|(_, rel)| rel).collect()));
            }
            if phase == Phase::Finalising
                && r.inflight.is_empty()
                && r.failed.is_empty()
                && r.finalising.is_none()
            {
                if r.full_checked {
                    if in_flight < cap {
                        if let Some(sel) = &r.sel {
                            let out = finalise::prepare(
                                &self.cfg, &*self.fs, &r.det, &r.dest, &r.journal, sel, &r.started,
                                "watch",
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
                } else {
                    need_full.push(key.clone());
                }
            }
        }
        // Round robin over the runs, starting after the one served last.
        let start = self
            .cursor
            .as_ref()
            .map_or(0, |c| queues.iter().position(|(k, _)| k > c).unwrap_or(0));
        queues.rotate_left(start);
        let mut served = None;
        'fill: loop {
            let mut any = false;
            for (key, q) in &mut queues {
                if in_flight >= cap {
                    break 'fill;
                }
                let Some(rel) = q.pop_front() else { continue };
                any = true;
                let Some(r) = self.runs.get_mut(key) else {
                    continue;
                };
                let src = safe_join(&r.det.path, Path::new(&rel)).map_err(Error::Config)?;
                let dst = safe_join(&r.dest, Path::new(&rel)).map_err(Error::Config)?;
                let size = r.life.files.get(&rel).map_or(0, |t| t.size);
                r.inflight.insert(rel.clone());
                jobs.push(Job::Copy {
                    run: key.clone(),
                    ours: r.journal.files.contains_key(&rel),
                    rel,
                    src,
                    dst,
                    size,
                });
                in_flight += 1;
                served = Some(key.clone());
            }
            if !any {
                break;
            }
        }
        if served.is_some() {
            self.cursor = served;
        }
        for j in jobs {
            self.pool.submit(j);
        }
        for (rel, phase, waiting) in phase_events {
            let msg = format!(
                "run {}: {} ({waiting} file(s) waiting)",
                escape_control(&rel),
                phase.as_str()
            );
            self.emit(
                Kind::RunStateChanged,
                log::Level::Info,
                Some(&rel),
                None,
                &msg,
                json!({"state": phase.as_str(), "waiting_files": waiting}),
            );
        }
        // A full listing before the manifest is written, so no change hidden
        // by the folder cache is left out.
        for key in need_full {
            if let Some(r) = self.runs.get_mut(&key) {
                r.cache.clear();
                r.last_full = now;
                r.full_checked = true;
                let det = r.det.clone();
                self.refresh(now, det);
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Outcomes
    // -----------------------------------------------------------------

    /// Act on one finished job.
    ///
    /// Implements: SRS-RES-001, SRS-STATE-002
    ///
    /// # Errors
    /// Journal I/O errors.
    #[allow(clippy::too_many_lines)]
    pub fn handle(&mut self, o: Outcome, now: Duration) -> Result<()> {
        let key = o.job.run().clone();
        let max_attempts = self.cfg.transfer.max_attempts;
        let mut outage: Option<String> = None;
        let mut events: Vec<(Kind, log::Level, Option<String>, String, serde_json::Value)> =
            Vec::new();
        let mut alert: Option<(String, Kind, String)> = None;
        let mut finished = false;
        {
            let Some(r) = self.runs.get_mut(&key) else {
                return Ok(());
            };
            let rel_run = rel_of(&r.det);
            let name = escape_control(&rel_run);
            match (o.job, o.result) {
                (Job::Claim { .. }, result) => match result {
                    Ok(_) => {
                        r.gate = if r.withdraw {
                            Gate::Withdraw
                        } else {
                            Gate::Open
                        };
                    }
                    Err(Failure::Copy(CopyFail::Conflict(m))) => {
                        let msg = format!("run {name}: {m}");
                        r.broken = Some(m.clone());
                        self.stats.failed_runs += 1;
                        events.push((
                            Kind::RunFailed,
                            log::Level::Error,
                            None,
                            msg.clone(),
                            json!({"reason": m}),
                        ));
                        alert = Some((format!("run:{rel_run}"), Kind::RunFailed, msg));
                    }
                    Err(Failure::Copy(CopyFail::Guard(e))) => {
                        r.gate = Gate::Claim;
                        outage = Some(e.to_string());
                    }
                    Err(Failure::Copy(CopyFail::Io(e)))
                        if matches!(classify(&e), ErrorClass::Outage | ErrorClass::NoSpace) =>
                    {
                        r.gate = Gate::Claim;
                        outage = Some(format!("{e} while claiming run {name}"));
                    }
                    Err(e) => {
                        let msg = format!("run {name}: cannot claim the destination: {e}");
                        r.broken = Some(e.to_string());
                        self.stats.failed_runs += 1;
                        events.push((
                            Kind::RunFailed,
                            log::Level::Error,
                            None,
                            msg.clone(),
                            json!({"reason": e.to_string()}),
                        ));
                        alert = Some((format!("run:{rel_run}"), Kind::RunFailed, msg));
                    }
                },
                (Job::Withdraw { .. }, result) => match result {
                    Ok(_) => {
                        r.journal.append(
                            &*self.fs,
                            Record::Reopen {
                                ts: journal::now_rfc3339(),
                            },
                        )?;
                        r.withdraw = false;
                        r.gate = Gate::Open;
                        log::info!("run {name}: COMPLETE withdrawn from the destination");
                    }
                    Err(Failure::Finalise(Error::Guard(e))) => {
                        r.gate = Gate::Withdraw;
                        outage = Some(e);
                    }
                    Err(Failure::Finalise(Error::Io(e)))
                        if matches!(classify(&e), ErrorClass::Outage | ErrorClass::NoSpace) =>
                    {
                        r.gate = Gate::Withdraw;
                        outage = Some(format!("{e} while withdrawing COMPLETE of run {name}"));
                    }
                    Err(e) => {
                        let msg = format!("run {name}: cannot withdraw COMPLETE: {e}");
                        r.broken = Some(e.to_string());
                        self.stats.failed_runs += 1;
                        events.push((
                            Kind::RunFailed,
                            log::Level::Error,
                            None,
                            msg.clone(),
                            json!({"reason": e.to_string()}),
                        ));
                        alert = Some((format!("run:{rel_run}"), Kind::RunFailed, msg));
                    }
                },
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
                            r.attempts.remove(&rel);
                            self.stats.committed += 1;
                            self.stats.committed_bytes += c.size;
                            self.recent.push_back((now, c.size));
                            events.push((
                                Kind::FileCommitted,
                                log::Level::Info,
                                Some(rel.clone()),
                                commit_message(&r.journal.id, &rel, &c, o.secs),
                                json!({"size": c.size, "sha256": c.sha256, "seconds": o.secs, "adopted": c.adopted}),
                            ));
                        }
                        Ok(_) | Err(Failure::Finalise(_)) => {}
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
                                let msg = format!("run {name}: {shown}: {m}");
                                events.push((
                                    Kind::FileFailed,
                                    log::Level::Error,
                                    Some(rel.clone()),
                                    msg,
                                    json!({"reason": m}),
                                ));
                                r.failed.insert(rel, m);
                                self.stats.failed_files += 1;
                                alert = Some((
                                    format!("failed:{rel_run}"),
                                    Kind::FileFailed,
                                    format!(
                                        "run {name}: {} file(s) failed; the run is not completed",
                                        r.failed.len()
                                    ),
                                ));
                            }
                            other => {
                                if let CopyFail::Mismatch(m) = &other {
                                    r.mismatches += 1;
                                    self.stats.mismatches += 1;
                                    events.push((
                                        Kind::IntegrityMismatch,
                                        log::Level::Warn,
                                        Some(rel.clone()),
                                        format!("run {name}: {shown}: {m}"),
                                        json!({"reason": m}),
                                    ));
                                }
                                let n = r.attempts.entry(rel.clone()).or_insert(0);
                                *n += 1;
                                if *n >= max_attempts {
                                    let msg = format!(
                                        "run {name}: {shown}: failed after {n} attempt(s): {other}"
                                    );
                                    events.push((
                                        Kind::FileFailed,
                                        log::Level::Error,
                                        Some(rel.clone()),
                                        msg,
                                        json!({"reason": other.to_string(), "attempts": *n}),
                                    ));
                                    r.failed.insert(rel, other.to_string());
                                    self.stats.failed_files += 1;
                                    alert = Some((
                                        format!("failed:{rel_run}"),
                                        Kind::FileFailed,
                                        format!(
                                            "run {name}: {} file(s) failed; the run is not completed",
                                            r.failed.len()
                                        ),
                                    ));
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
                            if let Err(e) = r.journal.compact(&*self.fs) {
                                log::warn!("run {name}: cannot compact the journal: {e}");
                            }
                            let files = r.sel.as_ref().map_or(0, |s| s.files.len());
                            let bytes = r.sel.as_ref().map_or(0, Selected::bytes);
                            events.push((
                                Kind::RunComplete,
                                log::Level::Info,
                                None,
                                format!(
                                    "run {name}: COMPLETE, {files} files, {bytes} bytes -> {}, manifest sha256={sha}",
                                    escape_control(&r.dest.to_string_lossy())
                                ),
                                json!({"files": files, "bytes": bytes, "manifest_sha256": sha, "destination": r.dest.to_string_lossy()}),
                            ));
                            self.stats.completed_runs += 1;
                            finished = true;
                        }
                        Err(Failure::Finalise(Error::Guard(e))) => outage = Some(e),
                        Err(Failure::Finalise(Error::Io(e)))
                            if matches!(classify(&e), ErrorClass::Outage | ErrorClass::NoSpace) =>
                        {
                            outage = Some(format!("{e} while finalising run {name}"));
                        }
                        Err(e) => {
                            let msg = format!("run {name}: cannot be completed: {e}");
                            r.broken = Some(e.to_string());
                            self.stats.failed_runs += 1;
                            events.push((
                                Kind::RunFailed,
                                log::Level::Error,
                                None,
                                msg.clone(),
                                json!({"reason": e.to_string()}),
                            ));
                            alert = Some((format!("run:{rel_run}"), Kind::RunFailed, msg));
                        }
                    }
                }
            }
        }
        let rel_run = self
            .runs
            .get(&key)
            .map(|r| rel_of(&r.det))
            .unwrap_or_default();
        for (kind, level, file, msg, data) in events {
            self.emit(kind, level, Some(&rel_run), file.as_deref(), &msg, data);
        }
        if let Some((k, kind, msg)) = alert {
            self.raise(&k, kind, log::Level::Error, Some(&rel_run), &msg);
        }
        if finished {
            self.complete(now, &key);
        }
        if let Some(reason) = outage {
            self.outage_failure(now, &reason);
        }
        Ok(())
    }

    /// Move a completed run out of the live set: its journal, selection and
    /// lifecycle are dropped and only its fingerprint is kept.
    ///
    /// Implements: SRS-RES-008, SRS-LIVE-004
    fn complete(&mut self, now: Duration, key: &Path) {
        let Some(r) = self.runs.remove(key) else {
            return;
        };
        self.clear(&format!("run:{}", rel_of(&r.det)));
        let sel = r.sel.unwrap_or_default();
        let c = self.done_run(now, &r.det, r.dest, &r.journal, &sel, Duration::ZERO);
        self.completed.insert(key.to_path_buf(), c);
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
        let stuck: BTreeSet<usize> = self.stalled.iter().map(|(i, _)| *i).collect();
        let stale: Vec<String> = self
            .alerts
            .keys()
            .filter(|k| {
                k.strip_prefix("stall:")
                    .and_then(|i| i.parse::<usize>().ok())
                    .is_some_and(|i| !stuck.contains(&i))
            })
            .cloned()
            .collect();
        for k in stale {
            self.clear(&k);
        }
        for (i, b) in &busy {
            let idle = now.saturating_sub(b.last_progress);
            if idle >= limit && self.stalled.insert((*i, b.started)) {
                self.stats.stalls += 1;
                let run = self
                    .runs
                    .get(&b.run)
                    .map_or_else(|| b.run.to_string_lossy().into_owned(), |r| rel_of(&r.det));
                let msg = format!(
                    "worker {i} stalled: no progress for {} on {} of run {} ({} bytes written)",
                    human(idle),
                    escape_control(&b.label),
                    escape_control(&run),
                    b.bytes
                );
                self.emit(
                    Kind::WorkerStalled,
                    log::Level::Error,
                    Some(&run),
                    Some(&b.label),
                    &msg,
                    json!({"worker": i, "idle_s": idle.as_secs(), "bytes": b.bytes}),
                );
                self.raise(
                    &format!("stall:{i}"),
                    Kind::WorkerStalled,
                    log::Level::Error,
                    Some(&run),
                    &msg,
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

    // -----------------------------------------------------------------
    // Reload and state
    // -----------------------------------------------------------------

    /// Apply a changed `rate_limit` after SIGHUP. Other changes need a
    /// restart and are reported as such; an invalid file changes nothing.
    ///
    /// Implements: SRS-RATE-003
    pub fn reload_now(&mut self, config_path: &Path) {
        match config::load(config_path, &*self.fs) {
            Ok(loaded) => {
                let mut new = loaded.config;
                let old_rate = self.cfg.bandwidth.rate_limit;
                let new_rate = new.bandwidth.rate_limit;
                if old_rate == new_rate {
                    log::info!("reload: rate limit unchanged ({old_rate})");
                } else {
                    self.rate.set_rate(new_rate);
                    self.cfg.bandwidth.rate_limit = new_rate;
                    log::info!("reload: rate limit changed from {old_rate} to {new_rate}");
                }
                new.bandwidth.rate_limit = new_rate;
                let mut cur = self.cfg.clone();
                cur.bandwidth.rate_limit = new_rate;
                if format!("{new:?}") != format!("{cur:?}") {
                    log::warn!("reload: other settings changed; they take effect after a restart");
                }
            }
            Err(e) => log::error!("reload refused, keeping the current settings: {e}"),
        }
    }

    /// Bytes per second committed over the last minute.
    fn current_rate(&mut self, now: Duration) -> u64 {
        let window = Duration::from_secs(60);
        while self
            .recent
            .front()
            .is_some_and(|(t, _)| now.saturating_sub(*t) > window)
        {
            self.recent.pop_front();
        }
        let bytes: u64 = self.recent.iter().map(|(_, b)| b).sum();
        let span = now.min(window).as_secs().max(1);
        bytes / span
    }

    /// The state snapshot at clock reading `now`.
    ///
    /// Implements: SRS-STATE-001, SRS-STATE-003
    #[allow(clippy::too_many_lines)]
    pub fn snapshot(&mut self, now: Duration) -> state::Snapshot {
        let current = self.current_rate(now);
        let busy = self.pool.busy();
        let limit = self.cfg.transfer.stall_timeout;
        let mut runs = Vec::new();
        let mut backlog_bytes_all = 0;
        for r in self.runs.values() {
            let backlog_files = r.life.outstanding(&r.journal.files) as u64;
            let backlog_bytes = Self::backlog_bytes(r);
            backlog_bytes_all += backlog_bytes;
            let files = r.life.files.len() as u64;
            let sel = r.sel.as_ref();
            runs.push(state::RunState {
                run_id: r.journal.id.clone(),
                name: escape_control(&rel_of(&r.det)),
                source: escape_control(&r.det.path.to_string_lossy()),
                destination: escape_control(&r.dest.to_string_lossy()),
                phase: if r.broken.is_some() {
                    "failed".into()
                } else {
                    match r.gate {
                        Gate::Claim | Gate::Claiming => "claiming".into(),
                        Gate::Withdraw | Gate::Withdrawing => "withdrawing".into(),
                        Gate::Open => r.phase.as_str().into(),
                    }
                },
                files,
                bytes: r.life.files.values().map(|t| t.size).sum(),
                committed_files: files - backlog_files,
                backlog_files,
                backlog_bytes,
                copying: r.inflight.len() as u64,
                failed: r
                    .failed
                    .iter()
                    .map(|(f, why)| state::FailedFile {
                        file: escape_control(f),
                        reason: escape_control(why),
                    })
                    .collect(),
                integrity_mismatches: r.mismatches,
                skipped: sel
                    .map(|s| s.symlinks.iter().chain(&s.special).cloned().collect())
                    .unwrap_or_default(),
                eta_s: (current > 0 && backlog_bytes > 0).then(|| backlog_bytes / current),
                complete_marker: r.det.complete.as_deref().map(escape_control),
                manifest_sha256: None,
                error: r.broken.as_deref().map(escape_control),
            });
        }
        for c in self.completed.values() {
            runs.push(state::RunState {
                run_id: c.id.clone(),
                name: escape_control(&rel_of(&c.det)),
                source: escape_control(&c.det.path.to_string_lossy()),
                destination: escape_control(&c.dest.to_string_lossy()),
                phase: "complete".into(),
                files: c.files,
                bytes: c.bytes,
                committed_files: c.files,
                backlog_files: 0,
                backlog_bytes: 0,
                copying: 0,
                failed: Vec::new(),
                integrity_mismatches: 0,
                skipped: Vec::new(),
                eta_s: None,
                complete_marker: c.det.complete.as_deref().map(escape_control),
                manifest_sha256: Some(c.manifest_sha256.clone()),
                error: None,
            });
        }
        let outage = self.outage.info(now).map(|i| state::OutageState {
            since: self.dest_info.outage_since.clone().unwrap_or_default(),
            duration_s: i.duration.as_secs(),
            reason: escape_control(&i.reason),
            level: i.level.into(),
            checks: i.failures,
            next_check_s: i.next_check.as_secs(),
            recovering: i.recovering,
            backlog_files: self.backlog() as u64,
            backlog_bytes: backlog_bytes_all,
            stuck_workers: self.stalled.len() as u64,
        });
        let mut alerts: Vec<state::Alert> = self.alerts.values().cloned().collect();
        alerts.sort_by(|a, b| a.since.cmp(&b.since));
        state::Snapshot {
            schema: state::STATE_SCHEMA.into(),
            schema_version: state::SCHEMA_VERSION,
            tool_version: env!("CARGO_PKG_VERSION").into(),
            host: journal::host_name(&*self.fs),
            pid: std::process::id(),
            started_at: self.started_at.clone(),
            updated_at: journal::now_rfc3339(),
            interval_s: self.cfg.state.interval.as_secs().max(1),
            status: if !self.started_up {
                "starting"
            } else if self.outage.is_down() {
                "outage"
            } else {
                "running"
            }
            .into(),
            target_id: self.cfg.target_id.clone(),
            source_root: escape_control(&self.cfg.source.root.resolved.to_string_lossy()),
            destination: state::Destination {
                root: escape_control(&self.guard.dest().to_string_lossy()),
                fstype: self.dest_info.fstype.clone(),
                status: if !self.started_up {
                    "starting"
                } else if self.outage.is_down() {
                    "unavailable"
                } else {
                    "ok"
                }
                .into(),
                guard_ok: self.dest_info.ok,
                free_bytes: self.dest_info.free,
                outage,
            },
            source_free_bytes: self.source_free,
            workers: state::Workers {
                count: self.pool.workers() as u64,
                paused: self.pool.paused(),
                busy: busy
                    .iter()
                    .map(|(i, b)| {
                        let idle = now.saturating_sub(b.last_progress);
                        state::WorkerState {
                            worker: *i as u64,
                            run: self.runs.get(&b.run).map_or_else(
                                || escape_control(&b.run.to_string_lossy()),
                                |r| escape_control(&rel_of(&r.det)),
                            ),
                            task: escape_control(&b.label),
                            bytes: b.bytes,
                            bytes_total: b.total,
                            idle_s: idle.as_secs(),
                            stalled: !limit.is_zero() && idle >= limit,
                        }
                    })
                    .collect(),
            },
            rate: state::Rate {
                limit_bytes_per_s: self.rate.limit(),
                current_bytes_per_s: current,
            },
            totals: state::Totals {
                committed_files: self.stats.committed as u64,
                committed_bytes: self.stats.committed_bytes,
                failed_files: self.stats.failed_files as u64,
                completed_runs: self.stats.completed_runs as u64,
                failed_runs: self.stats.failed_runs as u64,
                reopened_runs: self.stats.reopened_runs as u64,
                outages: self.stats.outages as u64,
                stalls: self.stats.stalls as u64,
            },
            runs,
            alerts,
        }
    }

    /// Write `state.json` when it is due (or now, with `force`).
    ///
    /// Implements: SRS-STATE-001
    pub fn publish(&mut self, now: Duration, force: bool) {
        if !force && !self.state.due(now) {
            return;
        }
        let snap = self.snapshot(now);
        if let Err(e) = self.state.snapshot(&*self.fs, now, &snap) {
            log::warn!("cannot write {}: {e}", state::STATE_FILE);
        }
    }

    /// One controller step at clock reading `now`.
    ///
    /// # Errors
    /// [`Error::Guard`] when the share belongs to another target at start-up,
    /// [`Error::RestartNeeded`] when every worker is stalled, source and
    /// journal I/O errors.
    pub fn step(&mut self, now: Duration) -> Result<()> {
        if let Some((path, flag)) = &self.reload {
            if flag.swap(false, Ordering::SeqCst) {
                let path = path.clone();
                self.reload_now(&path);
            }
        }
        while let Some(o) = self.pool.recv_timeout(Duration::ZERO) {
            self.handle(o, now)?;
        }
        self.probe_step(now)?;
        let backlog = self.backlog();
        for e in self.outage.tick(now, backlog) {
            self.outage_event(&e);
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
        self.publish(now, false);
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
        let started = self.clock.now();
        self.emit(
            Kind::Started,
            log::Level::Info,
            None,
            None,
            &format!("ont-offload {} watching", env!("CARGO_PKG_VERSION")),
            json!({"pid": std::process::id()}),
        );
        self.publish(started, true);
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
                self.publish(now, true);
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
