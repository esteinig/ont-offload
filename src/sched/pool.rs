// SPDX-License-Identifier: AGPL-3.0-or-later
//! Worker pool: `workers` threads take jobs (claim a destination on the
//! share, copy one file, write the end-of-run outputs, or withdraw
//! `COMPLETE` from a run that changed) and report outcomes. Each worker publishes its
//! progress so the controller can detect stalls; all workers share one rate
//! limiter and one pause flag.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::copy::{self, Copied, CopyFail, CopyOptions, Hooks};
use crate::error::Error;
use crate::fsio::Fs;
use crate::guard::Guard;
use crate::run::finalise::{self, Outputs};

use super::Clock;
use super::rate::RateLimiter;

/// Work for one worker.
#[derive(Debug, Clone)]
pub enum Job {
    /// Write the claim file in the destination run folder on the share.
    Claim {
        /// Run key (source run path).
        run: PathBuf,
        /// Destination run folder.
        dest: PathBuf,
        /// Run id.
        run_id: String,
        /// This instance's target id.
        target_id: String,
    },
    /// Remove `COMPLETE` and `VERIFIED.json` from a completed run that
    /// changed at the source.
    Withdraw {
        /// Run key (source run path).
        run: PathBuf,
        /// Destination run folder.
        dest: PathBuf,
    },
    /// Copy one file.
    Copy {
        /// Run key (source run path).
        run: PathBuf,
        /// Path relative to the run.
        rel: String,
        /// Source file.
        src: PathBuf,
        /// Destination file.
        dst: PathBuf,
        /// The journal records an earlier commit of this path.
        ours: bool,
        /// Size when selected.
        size: u64,
    },
    /// Size check, then `manifest.sha256`, `run.json` and `COMPLETE`.
    Finalise {
        /// Run key (source run path).
        run: PathBuf,
        /// Destination run folder.
        dest: PathBuf,
        /// Prepared outputs.
        outputs: Box<Outputs>,
    },
}

impl Job {
    /// Run key.
    #[must_use]
    pub fn run(&self) -> &PathBuf {
        match self {
            Self::Copy { run, .. }
            | Self::Finalise { run, .. }
            | Self::Claim { run, .. }
            | Self::Withdraw { run, .. } => run,
        }
    }

    /// Short description for logs.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Copy { rel, .. } => rel.clone(),
            Self::Finalise { .. } => "manifest, run.json and COMPLETE".to_owned(),
            Self::Claim { .. } => "claim on the share".to_owned(),
            Self::Withdraw { .. } => "withdraw COMPLETE".to_owned(),
        }
    }

    /// File size for a copy.
    #[must_use]
    pub fn size(&self) -> Option<u64> {
        match self {
            Self::Copy { size, .. } => Some(*size),
            _ => None,
        }
    }
}

/// What a job achieved.
#[derive(Debug, Clone)]
pub enum Done {
    /// A file was committed.
    Copied(Copied),
    /// The run's outputs were written.
    Finalised,
    /// The destination is claimed for this run.
    Claimed,
    /// `COMPLETE` and `VERIFIED.json` were removed.
    Withdrawn,
}

/// Why a job did not finish.
#[derive(Debug)]
pub enum Failure {
    /// Copy failure.
    Copy(CopyFail),
    /// Failure writing the outputs.
    Finalise(Error),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Copy(e) => write!(f, "{e}"),
            Self::Finalise(e) => write!(f, "{e}"),
        }
    }
}

/// A finished job.
#[derive(Debug)]
pub struct Outcome {
    /// Worker index.
    pub worker: usize,
    /// The job.
    pub job: Job,
    /// Result.
    pub result: Result<Done, Failure>,
    /// Seconds the job took.
    pub secs: f64,
}

/// A worker's current job.
#[derive(Debug, Clone)]
pub struct Busy {
    /// Job label.
    pub label: String,
    /// Run key.
    pub run: PathBuf,
    /// When the job started.
    pub started: Duration,
    /// Last progress (start, or the last chunk).
    pub last_progress: Duration,
    /// Bytes written so far.
    pub bytes: u64,
    /// File size for a copy.
    pub total: Option<u64>,
}

type Slot = Mutex<Option<Busy>>;

struct Shared {
    fs: Arc<dyn Fs>,
    guard: Arc<Guard>,
    opts: CopyOptions,
    rate: Arc<RateLimiter>,
    clock: Arc<dyn Clock>,
    paused: Arc<AtomicBool>,
    slots: Arc<Vec<Slot>>,
}

struct WorkerHooks<'a> {
    shared: &'a Shared,
    slot: &'a Slot,
}

impl Hooks for WorkerHooks<'_> {
    fn chunk(&self, n: usize) -> Result<(), CopyFail> {
        if self.shared.paused.load(Ordering::SeqCst) {
            return Err(CopyFail::Paused);
        }
        self.shared.rate.acquire(n as u64);
        let now = self.shared.clock.now();
        if let Some(b) = self
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            b.last_progress = now;
            b.bytes += n as u64;
        }
        if self.shared.paused.load(Ordering::SeqCst) {
            return Err(CopyFail::Paused);
        }
        Ok(())
    }
}

fn work(shared: &Shared, idx: usize, job: &Job) -> Result<Done, Failure> {
    if shared.paused.load(Ordering::SeqCst) {
        return Err(Failure::Copy(CopyFail::Paused));
    }
    let hooks = WorkerHooks {
        shared,
        slot: &shared.slots[idx],
    };
    match job {
        Job::Copy { src, dst, ours, .. } => copy::copy_file_with(
            &*shared.fs,
            &shared.guard,
            src,
            dst,
            *ours,
            &shared.opts,
            &hooks,
        )
        .map(Done::Copied)
        .map_err(Failure::Copy),
        Job::Finalise { dest, outputs, .. } => finalise::write(
            &*shared.fs,
            &shared.guard,
            dest,
            outputs,
            shared.opts.modes.record.as_ref(),
        )
        .map(|()| Done::Finalised)
        .map_err(Failure::Finalise),
        Job::Claim {
            run,
            dest,
            run_id,
            target_id,
        } => crate::journal::claim_on_share(
            &*shared.fs,
            &shared.guard,
            dest,
            run,
            run_id,
            target_id,
            &shared.opts.modes,
        )
        .map(|()| Done::Claimed)
        .map_err(Failure::Copy),
        Job::Withdraw { dest, .. } => finalise::withdraw(&*shared.fs, &shared.guard, dest)
            .map(|()| Done::Withdrawn)
            .map_err(Failure::Finalise),
    }
}

/// The worker threads and their queues.
///
/// Implements: SRS-PAR-001, SRS-RES-003
pub struct Pool {
    jobs: Option<Sender<Job>>,
    outcomes: Receiver<Outcome>,
    slots: Arc<Vec<Slot>>,
    paused: Arc<AtomicBool>,
    in_flight: usize,
    workers: usize,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("workers", &self.workers)
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// Start `workers` threads.
    ///
    /// # Errors
    /// A thread cannot be started.
    pub fn new(
        workers: usize,
        fs: &Arc<dyn Fs>,
        guard: &Arc<Guard>,
        opts: &CopyOptions,
        rate: &Arc<RateLimiter>,
        clock: &Arc<dyn Clock>,
    ) -> std::io::Result<Self> {
        let workers = workers.max(1);
        let (jtx, jrx) = mpsc::channel::<Job>();
        let (otx, orx) = mpsc::channel::<Outcome>();
        let jrx = Arc::new(Mutex::new(jrx));
        let slots: Arc<Vec<Slot>> = Arc::new((0..workers).map(|_| Mutex::new(None)).collect());
        let paused = Arc::new(AtomicBool::new(false));
        for idx in 0..workers {
            let shared = Shared {
                fs: Arc::clone(fs),
                guard: Arc::clone(guard),
                opts: opts.clone(),
                rate: Arc::clone(rate),
                clock: Arc::clone(clock),
                paused: Arc::clone(&paused),
                slots: Arc::clone(&slots),
            };
            let jrx = Arc::clone(&jrx);
            let otx = otx.clone();
            std::thread::Builder::new()
                .name(format!("worker-{idx}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let rx = jrx.lock().unwrap_or_else(PoisonError::into_inner);
                            match rx.recv() {
                                Ok(j) => j,
                                Err(_) => return,
                            }
                        };
                        let now = shared.clock.now();
                        *shared.slots[idx]
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(Busy {
                            label: job.label(),
                            run: job.run().clone(),
                            started: now,
                            last_progress: now,
                            bytes: 0,
                            total: job.size(),
                        });
                        let t0 = std::time::Instant::now();
                        let result = work(&shared, idx, &job);
                        *shared.slots[idx]
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = None;
                        let outcome = Outcome {
                            worker: idx,
                            job,
                            result,
                            secs: t0.elapsed().as_secs_f64(),
                        };
                        if otx.send(outcome).is_err() {
                            return;
                        }
                    }
                })?;
        }
        Ok(Self {
            jobs: Some(jtx),
            outcomes: orx,
            slots,
            paused,
            in_flight: 0,
            workers,
        })
    }

    /// Number of workers.
    #[must_use]
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Jobs submitted and not yet reported.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Queue a job.
    pub fn submit(&mut self, job: Job) {
        if let Some(tx) = &self.jobs {
            if tx.send(job).is_ok() {
                self.in_flight += 1;
            }
        }
    }

    /// Pause or resume all writes.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    /// Whether writes are paused.
    #[must_use]
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// The next outcome, waiting at most `timeout`.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Option<Outcome> {
        let r = if timeout.is_zero() {
            self.outcomes.try_recv().ok()
        } else {
            match self.outcomes.recv_timeout(timeout) {
                Ok(o) => Some(o),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
            }
        };
        if r.is_some() {
            self.in_flight = self.in_flight.saturating_sub(1);
        }
        r
    }

    /// Busy workers with their current job.
    #[must_use]
    pub fn busy(&self) -> Vec<(usize, Busy)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                s.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                    .map(|b| (i, b))
            })
            .collect()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // Workers end when the queue closes; a worker blocked on a hung
        // share is left to the process exit rather than joined.
        self.jobs = None;
    }
}
