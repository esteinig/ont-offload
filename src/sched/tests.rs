// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the rate limiter, outage tracking, the instance lock, systemd
//! notification and the `watch` controller, on a fake clock with runs
//! written over time by ont-runsim and faults injected with `FaultyFs`.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::lock::{InstanceLock, LOCK};
use super::notify::Notifier;
use super::outage::{Event, Tracker};
use super::rate::RateLimiter;
use super::watch::{Options, Watcher};
use super::{Clock, FakeClock, RealClock};
use crate::config::units::Rate;
use crate::config::{self, Config};
use crate::error::Error;
use crate::fsio::faulty::{Fault, FaultyFs, Op, Rule};
use crate::fsio::{RealFs, errno};
use crate::guard::{Guard, SENTINEL};
use crate::journal::COMPLETE;

const TARGET: &str = "lab-share-01";

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

// ---------------------------------------------------------------------------
// Rate limiter
// ---------------------------------------------------------------------------

/// Verifies: SRS-RATE-001, SRS-PAR-001
#[test]
fn rate_limit_holds_for_all_workers_together_on_a_fake_clock() {
    let clock = Arc::new(FakeClock::new());
    let rate = 1_000_000u64;
    let burst = 64 * 1024u64;
    let chunk = 16 * 1024u64;
    let lim = RateLimiter::new(Rate::BytesPerSec(rate), burst, clock.clone());
    // Four workers; each sends a chunk as soon as the limiter lets it.
    let mut next = [Duration::ZERO; 4];
    let mut sent: Vec<(Duration, u64)> = Vec::new();
    while sent.len() < 2000 {
        let (w, t) = next
            .iter()
            .copied()
            .enumerate()
            .min_by_key(|(_, t)| *t)
            .unwrap();
        clock.set(t);
        let at = t + lim.reserve(chunk);
        sent.push((at, chunk));
        next[w] = at;
    }
    sent.sort();
    let mut total = 0u64;
    for (at, n) in &sent {
        total += n;
        let allowed = burst as f64 + rate as f64 * at.as_secs_f64();
        assert!(
            total as f64 <= allowed + 1.0,
            "{total} bytes by {at:?} > {allowed}"
        );
    }
    // And the limit is used, not just respected.
    let end = sent.last().unwrap().0.as_secs_f64();
    assert!(total as f64 / end > 0.95 * rate as f64);
}

/// Verifies: SRS-RATE-001, SRS-PAR-001
#[test]
fn rate_limit_holds_across_real_threads() {
    let clock: Arc<dyn Clock> = Arc::new(RealClock::new());
    let rate = 4_000_000u64;
    let burst = 256 * 1024;
    let lim = Arc::new(RateLimiter::new(Rate::BytesPerSec(rate), burst, clock));
    let t0 = std::time::Instant::now();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let lim = Arc::clone(&lim);
            std::thread::spawn(move || {
                for _ in 0..16 {
                    lim.acquire(64 * 1024);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let total = 4 * 16 * 64 * 1024u64;
    let min = (total - burst) as f64 / rate as f64;
    assert!(
        t0.elapsed().as_secs_f64() >= min * 0.98,
        "{:?} < {min}",
        t0.elapsed()
    );
    let unlimited = RateLimiter::new(Rate::Unlimited, 0, Arc::new(FakeClock::new()));
    assert_eq!(unlimited.reserve(u64::MAX), Duration::ZERO);
}

// ---------------------------------------------------------------------------
// Outage tracker
// ---------------------------------------------------------------------------

fn outage_cfg() -> config::Outage {
    config::Outage {
        check_every_max: s(300),
        warn_after: s(600),
        alert_after: s(3600),
        remind_every: s(3600),
        log_every: s(900),
        stable_before_resume: s(60),
        source_free_alert: 0,
        notify_command: None,
    }
}

/// Verifies: SRS-RES-002, SRS-RES-005, SRS-LOG-004
#[test]
fn twelve_hour_outage_is_rechecked_with_capped_backoff_and_logged_sparingly() {
    let mut t = Tracker::new(&outage_cfg());
    let mut events = t.failure(s(0), "EIO", 7);
    let mut checks = Vec::new();
    let mut now = s(0);
    while now < s(12 * 3600) {
        now += s(1);
        events.extend(t.tick(now, 7));
        if t.check_due(now) {
            checks.push(now);
            events.extend(t.failure(now, "ESTALE", 7));
        }
    }
    // Backoff doubles from 1 s and is capped at check_every_max.
    assert_eq!(&checks[..5], &[s(1), s(3), s(7), s(15), s(31)]);
    assert!(checks.windows(2).all(|w| w[1] - w[0] <= s(300)));
    assert_eq!(t.backoff(), Some(s(300)));
    // One start, a summary every 15 min, one warning, an alert and hourly reminders.
    let count = |f: &dyn Fn(&Event) -> bool| events.iter().filter(|e| f(e)).count();
    assert_eq!(count(&|e| matches!(e, Event::Started { .. })), 1);
    assert_eq!(count(&|e| matches!(e, Event::Summary { .. })), 48);
    assert_eq!(count(&|e| matches!(e, Event::Warn { .. })), 1);
    assert_eq!(
        count(&|e| matches!(
            e,
            Event::Alert {
                reminder: false,
                ..
            }
        )),
        1
    );
    assert_eq!(
        count(&|e| matches!(e, Event::Alert { reminder: true, .. })),
        11
    );
    assert!(
        events.len() < 70,
        "{} events for a 12 h outage",
        events.len()
    );
    // Recovery: the guard has to pass for stable_before_resume.
    let end = now;
    assert_eq!(t.success(end, 7), vec![Event::Recovering]);
    assert!(t.is_down());
    let ended = t.success(end + s(60), 7);
    assert!(matches!(ended.as_slice(), [Event::Ended { down, .. }] if *down == s(12 * 3600 + 60)));
    assert!(!t.is_down());
    for e in ended {
        let (level, msg) = e.message();
        assert_eq!(level, log::Level::Info);
        assert!(msg.contains("12h 1m"), "{msg}");
    }
}

/// Verifies: SRS-RES-005, SRS-LOG-004
#[test]
fn flapping_destination_does_not_resume_until_stable() {
    let mut t = Tracker::new(&outage_cfg());
    assert_eq!(t.failure(s(0), "EIO", 0).len(), 1);
    assert_eq!(t.success(s(10), 0), vec![Event::Recovering]);
    assert!(t.success(s(40), 0).is_empty());
    // Fails again inside the stability window: still the same outage, no new start line.
    assert!(t.failure(s(50), "EIO", 0).is_empty());
    assert!(t.is_down());
    assert_eq!(t.success(s(100), 0), vec![Event::Recovering]);
    assert!(t.is_down());
    assert!(matches!(
        t.success(s(160), 0).as_slice(),
        [Event::Ended { failures: 2, .. }]
    ));
}

// ---------------------------------------------------------------------------
// Instance lock and systemd notification
// ---------------------------------------------------------------------------

/// Verifies: SRS-RES-007
#[test]
fn one_process_per_state_folder() {
    let dir = tempfile::tempdir().unwrap();
    // pid 1 is always alive: refused.
    std::fs::write(dir.path().join(LOCK), "1\n").unwrap();
    let err = InstanceLock::acquire(&RealFs, dir.path()).unwrap_err();
    assert!(
        matches!(&err, Error::Config(m) if m.contains("pid 1")),
        "{err}"
    );
    // A lock left by a process that is gone is taken over and removed on drop.
    std::fs::write(dir.path().join(LOCK), "4194999\n").unwrap();
    {
        let _l = InstanceLock::acquire(&RealFs, dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join(LOCK)).unwrap();
        assert_eq!(text.trim(), std::process::id().to_string());
    }
    assert!(!dir.path().join(LOCK).exists());
}

/// Verifies: SRS-RES-003
#[cfg(unix)]
#[test]
fn systemd_ready_and_watchdog_notifications() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notify.sock");
    let sock = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let n = Notifier::to(&path);
    let mut buf = [0u8; 512];
    n.ready("2 runs\n");
    let k = sock.recv(&mut buf).unwrap();
    assert_eq!(&buf[..k], b"READY=1\nSTATUS=2 runs ");
    n.watchdog("ok");
    let k = sock.recv(&mut buf).unwrap();
    assert_eq!(&buf[..k], b"WATCHDOG=1\nSTATUS=ok");
    // Without a socket nothing happens.
    Notifier::default().ready("x");
}

// ---------------------------------------------------------------------------
// The watch controller on a fake clock
// ---------------------------------------------------------------------------

struct Env {
    dir: tempfile::TempDir,
    fs: FaultyFs,
    mountinfo: PathBuf,
    clock: Arc<FakeClock>,
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
        let env = Self {
            dir,
            fs,
            mountinfo,
            clock: Arc::new(FakeClock::new()),
        };
        env.base_rules();
        env
    }

    /// The share is "another device".
    fn base_rules(&self) {
        self.fs.add(Rule::always(
            Op::Metadata,
            "/share",
            Fault::Device(9_999_999),
        ));
    }

    fn p(&self, rel: &str) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap().join(rel)
    }

    fn cfg(&self, extra_top: &str, extra: &str) -> Config {
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

    fn watcher(&self, cfg: Config) -> Watcher {
        let guard = Guard::new(&cfg).with_mountinfo(&self.mountinfo);
        Watcher::new(
            cfg,
            Arc::new(self.fs.clone()),
            Arc::new(guard),
            self.clock.clone(),
            Options {
                scan_every: s(10),
                probe_timeout: s(30),
                tick: Duration::from_millis(10),
                until_idle: false,
            },
            Notifier::default(),
        )
        .unwrap()
    }

    fn engine(&self, toml: &str) -> ont_runsim::engine::Engine {
        let sc = ont_runsim::scenario::Scenario::from_toml(toml, None).unwrap();
        let out = self.p("data");
        ont_runsim::engine::prepare_out(&out).unwrap();
        let plans = ont_runsim::plan::build(&sc, SystemTime::UNIX_EPOCH).unwrap();
        ont_runsim::engine::Engine::new(&out, plans).unwrap()
    }

    fn dest_of(&self, run: &Path) -> PathBuf {
        self.p("share")
            .join(run.strip_prefix(self.p("data")).unwrap())
    }

    /// Check a destination run against the simulator's manifest.
    fn matches_simulator(&self, run: &Path) {
        let manifest = ont_runsim::status::manifests_dir(&self.p("data")).join(format!(
            "{}.sha256",
            run.file_name().unwrap().to_string_lossy()
        ));
        let rep = ont_runsim::check::check(&self.dest_of(run), &manifest, &[]).unwrap();
        assert!(rep.ok(), "{}: {rep:?}", run.display());
        // And our own manifest agrees.
        let ours = crate::journal::parse_manifest(
            &std::fs::read_to_string(self.dest_of(run).join("manifest.sha256")).unwrap(),
        )
        .unwrap();
        let theirs = ont_runsim::check::read_manifest(&manifest).unwrap();
        assert_eq!(ours, theirs);
    }
}

/// Drive the simulator and the controller together, `step` simulated
/// seconds at a time, from `from` to `to`. `each` runs before every step.
fn drive(
    env: &Env,
    w: &mut Watcher,
    engine: &mut ont_runsim::engine::Engine,
    from: u64,
    to: u64,
    step: u64,
    mut each: impl FnMut(u64, &mut Watcher),
) {
    let mut t = from;
    while t <= to {
        engine.advance_to(t as f64).unwrap();
        env.clock.set(s(t));
        each(t, w);
        w.step(s(t)).unwrap();
        w.quiesce(s(t), s(20)).unwrap();
        t += step;
    }
}

fn finish(env: &Env, engine: &mut ont_runsim::engine::Engine) {
    engine.write_manifests().unwrap();
    let _ = env;
}

/// Verifies: SRS-LIVE-001, SRS-LIVE-003, SRS-LIVE-002, SRS-PAR-001, SRS-VER-002
#[test]
fn live_runs_with_rewrites_and_late_files_offload_exactly() {
    let env = Env::new();
    let cfg = env.cfg("", "[bandwidth]\nworkers = 3\n");
    let mut engine = env.engine(
        "seed = 11\nhours = 1\n[fake]\nsize_scale = 0.0002\n\
         [faults]\nrewrite_closed = 3\nlate_files = 2\n\
         [[runs]]\nexperiment = \"exp1\"\nsample = \"s1\"\n\
         [[runs]]\nexperiment = \"exp2\"\nsample = \"s2\"\nstart_after = \"10m\"\n",
    );
    let mut w = env.watcher(cfg);
    let total = engine.total_seconds() as u64;
    let mut live_commits = 0;
    drive(&env, &mut w, &mut engine, 0, total + 900, 10, |t, w| {
        if t + 10 < total / 2 {
            live_commits = w.stats.committed;
        }
    });
    finish(&env, &mut engine);
    // Files were copied while the runs were still being written.
    assert!(live_commits > 0);
    let runs = engine.run_dirs();
    assert_eq!(runs.len(), 2);
    for r in &runs {
        env.matches_simulator(r);
        let manifest = std::fs::read_to_string(env.dest_of(r).join("manifest.sha256")).unwrap();
        assert!(
            manifest.contains("other_reports/late_file_1.txt"),
            "late file missing"
        );
    }
    assert_eq!(w.stats.completed_runs, 2);
    assert_eq!(w.stats.failed_files, 0);
    assert!(w.idle());
}

/// Verifies: SRS-LIVE-003
#[test]
fn aborted_run_is_never_marked_complete() {
    let env = Env::new();
    let cfg = env.cfg("", "");
    let mut engine = env.engine(
        "seed = 4\nhours = 1\n[fake]\nsize_scale = 0.0002\n[faults]\nabort_after = \"20m\"\n\
         [[runs]]\nexperiment = \"e\"\nsample = \"s\"\n",
    );
    let mut w = env.watcher(cfg);
    drive(&env, &mut w, &mut engine, 0, 3 * 3600, 30, |_, _| {});
    let run = &engine.run_dirs()[0];
    assert!(w.stats.committed > 0);
    assert!(!env.dest_of(run).join(COMPLETE).exists());
    assert_eq!(w.stats.completed_runs, 0);
    assert!(!w.idle());
}

/// Verifies: SRS-RES-002, SRS-RES-005, SRS-LOG-004, SRS-RES-001
#[test]
fn twelve_hour_live_run_through_outages_and_error_bursts_offloads_exactly() {
    let env = Env::new();
    let cfg = env.cfg("", "[bandwidth]\nworkers = 4\n");
    let mut engine = env.engine(
        "seed = 21\nhours = 12\n[fake]\nsize_scale = 0.00005\n\
         [faults]\nrewrite_closed = 2\nlate_files = 1\n\
         [[runs]]\nexperiment = \"exp12\"\nsample = \"s1\"\n",
    );
    let mut w = env.watcher(cfg);
    let total = engine.total_seconds() as u64;
    let mut down_seen = false;
    let fs = env.fs.clone();
    drive(&env, &mut w, &mut engine, 0, total + 1200, 20, |t, w| {
        match t {
            // A 3 h outage: every call on the share fails with EIO.
            7200 => fs.add(Rule::always(Op::Any, "/share", Fault::Errno(errno::EIO))),
            18000 => {
                fs.clear();
                env.base_rules();
            }
            // A burst of ESTALE on writes, then ENOSPC once.
            25000 => fs.add(Rule::times(
                Op::Write,
                "/share",
                Fault::Errno(errno::ESTALE),
                5,
            )),
            30000 => fs.add(Rule::times(
                Op::CreateNew,
                "/share",
                Fault::Errno(errno::ENOSPC),
                1,
            )),
            // Read-back is off: a short write is caught by the size check and retried.
            35000 => fs.add(Rule::times(Op::Write, "/share", Fault::ShortWrite, 2)),
            _ => {}
        }
        if (9000..18000).contains(&t) {
            assert!(w.is_down(), "not paused at {t}");
            down_seen = true;
        }
    });
    finish(&env, &mut engine);
    assert!(down_seen);
    assert!(w.stats.outages >= 2, "{:?}", w.stats);
    assert_eq!(w.stats.failed_files, 0, "{:?}", w.stats);
    assert_eq!(w.stats.completed_runs, 1);
    env.matches_simulator(&engine.run_dirs()[0]);
}

/// Verifies: SRS-RES-006, SRS-GRD-001
#[test]
fn starts_while_the_share_is_missing_and_resumes_when_it_is_mounted() {
    let env = Env::new();
    let sentinel = env.p("share").join(SENTINEL);
    std::fs::remove_file(&sentinel).unwrap();
    let cfg = env.cfg("", "");
    let mut engine = env.engine(
        "seed = 2\nhours = 0.5\n[fake]\nsize_scale = 0.0002\n[[runs]]\nexperiment = \"e\"\nsample = \"s\"\n",
    );
    let mut w = env.watcher(cfg);
    let total = engine.total_seconds() as u64;
    drive(&env, &mut w, &mut engine, 0, total + 600, 10, |_, _| {});
    assert!(w.is_down());
    assert_eq!(w.stats.committed, 0);
    let written: Vec<_> = std::fs::read_dir(env.p("share")).unwrap().collect();
    assert!(written.is_empty(), "share written while unavailable");
    // Mounted: after stable_before_resume (60 s) everything is copied.
    std::fs::write(&sentinel, format!("{TARGET}\n")).unwrap();
    drive(
        &env,
        &mut w,
        &mut engine,
        total + 610,
        total + 1800,
        10,
        |_, _| {},
    );
    finish(&env, &mut engine);
    assert!(!w.is_down());
    env.matches_simulator(&engine.run_dirs()[0]);
}

/// Verifies: SRS-RES-006
#[test]
fn share_of_another_target_at_start_is_refused() {
    let env = Env::new();
    std::fs::write(env.p("share").join(SENTINEL), "other-lab\n").unwrap();
    let cfg = env.cfg("", "");
    let mut w = env.watcher(cfg);
    w.step(s(0)).unwrap();
    let err = w.quiesce(s(0), s(10)).unwrap_err();
    assert!(
        matches!(&err, Error::Guard(m) if m.contains("other-lab")),
        "{err}"
    );
    assert_eq!(err.exit_code(), crate::error::exit::CONFIG);
}

fn one_completed_run(env: &Env) -> PathBuf {
    let mut engine = env.engine(
        "seed = 9\nhours = 0.5\n[fake]\nsize_scale = 0.0002\n[[runs]]\nexperiment = \"e\"\nsample = \"s\"\n",
    );
    let mut t = 0.0;
    while !engine.done() {
        engine.advance_to(t).unwrap();
        t += 300.0;
    }
    engine.write_manifests().unwrap();
    engine.run_dirs()[0].clone()
}

/// Verifies: SRS-RES-003
#[test]
fn stalled_worker_is_reported_and_all_workers_stalled_needs_a_restart() {
    for workers in [2u32, 1] {
        let env = Env::new();
        one_completed_run(&env);
        let cfg = env.cfg("", &format!("[bandwidth]\nworkers = {workers}\n"));
        let mut w = env.watcher(cfg);
        for t in [0, 10] {
            env.clock.set(s(t));
            w.step(s(t)).unwrap();
            w.quiesce(s(t), s(10)).unwrap();
        }
        // The next write blocks (a hung share) for 1.5 s of real time.
        env.fs.add(Rule::times(
            Op::Write,
            "/share",
            Fault::Delay(Duration::from_millis(1500)),
            1,
        ));
        env.clock.set(s(50));
        w.step(s(50)).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        env.clock.set(s(200));
        let r = w.step(s(200));
        assert_eq!(w.stats.stalls, 1, "workers = {workers}");
        if workers == 1 {
            let err = r.unwrap_err();
            assert_eq!(err.exit_code(), crate::error::exit::RESTART, "{err}");
        } else {
            r.unwrap();
        }
        w.quiesce(s(200), s(10)).unwrap();
    }
}

/// Verifies: SRS-RES-001, SRS-CPY-002
#[test]
fn errors_are_handled_by_class() {
    let env = Env::new();
    let run = one_completed_run(&env);
    let cfg = env.cfg("", "[transfer]\nmax_attempts = 3\nverify = \"readback\"\n");
    // Permanent: permission denied on one file, every time.
    env.fs.add(Rule::always(
        Op::CreateNew,
        ".final_summary_",
        Fault::Errno(errno::EACCES),
    ));
    // Integrity: one bit flip on read-back, then fine.
    env.fs
        .add(Rule::times(Op::OpenRead, "/share", Fault::BitFlip(3), 1));
    let mut w = env.watcher(cfg);
    let mut t = 0;
    while t < 3600 {
        env.clock.set(s(t));
        w.step(s(t)).unwrap();
        w.quiesce(s(t), s(20)).unwrap();
        t += 30;
    }
    // The final summary failed after max_attempts, the bit flip was retried;
    // no outage, nothing completed, and nothing more to do.
    assert_eq!(w.stats.failed_files, 1, "{:?}", w.stats);
    assert!(w.stats.committed > 1);
    assert_eq!(w.stats.outages, 0);
    assert_eq!(w.stats.completed_runs, 0);
    assert!(!env.dest_of(&run).join(COMPLETE).exists());
    assert!(w.idle());
    // Capacity: ENOSPC pauses like an outage instead of failing files.
    env.fs.clear();
    env.base_rules();
    let env2 = Env::new();
    one_completed_run(&env2);
    env2.fs.add(Rule::times(
        Op::CreateNew,
        "/share",
        Fault::Errno(errno::ENOSPC),
        1,
    ));
    let mut w2 = env2.watcher(env2.cfg("", ""));
    let mut t = 0;
    while t < 3600 {
        env2.clock.set(s(t));
        w2.step(s(t)).unwrap();
        w2.quiesce(s(t), s(20)).unwrap();
        t += 30;
    }
    assert_eq!(w2.stats.outages, 1);
    assert_eq!(w2.stats.failed_files, 0);
    assert_eq!(w2.stats.completed_runs, 1);
}

/// Verifies: SRS-RES-004, SRS-SEL-007
#[test]
fn restart_resumes_from_the_journal_and_completed_runs_are_left_alone() {
    let env = Env::new();
    let run = one_completed_run(&env);
    let cfg = env.cfg("", "");
    {
        let mut w = env.watcher(cfg.clone());
        // Stop half way: only the first few commits happen before the "kill".
        let mut t = 0;
        while w.stats.committed < 3 {
            env.clock.set(s(t));
            w.step(s(t)).unwrap();
            w.quiesce(s(t), s(20)).unwrap();
            t += 10;
        }
    }
    let mut w = env.watcher(cfg.clone());
    let mut t = 0;
    while !w.idle() && t < 3600 {
        env.clock.set(s(10_000 + t));
        w.step(s(10_000 + t)).unwrap();
        w.quiesce(s(10_000 + t), s(20)).unwrap();
        t += 10;
    }
    assert_eq!(w.stats.completed_runs, 1);
    let total_files = crate::journal::parse_manifest(
        &std::fs::read_to_string(env.dest_of(&run).join("manifest.sha256")).unwrap(),
    )
    .unwrap()
    .len();
    assert!(
        w.stats.committed < total_files,
        "files copied again after restart"
    );
    env.matches_simulator(&run);
    // A third start finds the run complete and copies nothing.
    let mut w = env.watcher(cfg);
    for t in [20_000, 20_010, 20_100] {
        env.clock.set(s(t));
        w.step(s(t)).unwrap();
        w.quiesce(s(t), s(20)).unwrap();
    }
    assert_eq!(w.stats.committed, 0);
    assert!(w.idle());
}
