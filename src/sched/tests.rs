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
use crate::state::Kind;
use crate::state::schema_check::{load, validate};

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
        // Test file systems are far smaller than the default min_free.
        let extra = if extra.contains("[transfer]") {
            extra.replace("[transfer]", "[transfer]\nmin_free = \"1MB\"")
        } else {
            format!("{extra}\n[transfer]\nmin_free = \"1MB\"\n")
        };
        // Nor do they have the default source_free_alert to spare.
        let extra = if extra.contains("[outage]") {
            extra
        } else {
            format!("{extra}\n[outage]\nsource_free_alert = \"0B\"\n")
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
                online_check_every: s(60),
                cleanup_every: s(3600),
                cleanup_first_after: s(300),
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

impl Env {
    /// As [`Env::matches_simulator`], for a run changed after the simulator
    /// wrote its manifest: `added` files are extra, `changed` files differ;
    /// both must be in our manifest with their current digest.
    fn matches_simulator_except(&self, run: &Path, added: &[&str], changed: &[&PathBuf]) {
        let manifest = ont_runsim::status::manifests_dir(&self.p("data")).join(format!(
            "{}.sha256",
            run.file_name().unwrap().to_string_lossy()
        ));
        let rep = ont_runsim::check::check(&self.dest_of(run), &manifest, &[]).unwrap();
        let changed_rel: Vec<String> = changed
            .iter()
            .map(|p| p.strip_prefix(run).unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(rep.missing.is_empty(), "{rep:?}");
        let mut extra = rep.extra.clone();
        extra.sort();
        let mut want: Vec<String> = added.iter().map(|s| (*s).to_owned()).collect();
        want.sort();
        assert_eq!(extra, want, "{rep:?}");
        let mut got = rep.changed.clone();
        got.sort();
        assert_eq!(got, changed_rel, "{rep:?}");
        let ours = crate::journal::parse_manifest(
            &std::fs::read_to_string(self.dest_of(run).join("manifest.sha256")).unwrap(),
        )
        .unwrap();
        for rel in added.iter().map(|s| (*s).to_owned()).chain(changed_rel) {
            let (_, sha) = crate::copy::hash_file(&RealFs, &run.join(&rel), 1 << 20).unwrap();
            assert_eq!(ours.get(&rel), Some(&sha), "{rel}");
        }
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

// ---------------------------------------------------------------------------
// v0.4: live state, share claims, fairness, completed runs, bounds
// ---------------------------------------------------------------------------

fn events(env: &Env) -> Vec<crate::state::Event> {
    std::fs::read_to_string(env.p("state").join(crate::state::EVENTS_FILE))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn kinds(env: &Env) -> Vec<Kind> {
    events(env).iter().map(|e| e.kind).collect()
}

/// Step the controller alone (the source does not change) from `from` to
/// `to` in steps of `step` simulated seconds.
fn steps(env: &Env, w: &mut Watcher, from: u64, to: u64, step: u64) {
    let mut t = from;
    while t <= to {
        env.clock.set(s(t));
        w.step(s(t)).unwrap();
        w.quiesce(s(t), s(20)).unwrap();
        t += step;
    }
}

fn assert_schema_valid(w: &mut Watcher, env: &Env, now: Duration) {
    let state = load("state-v1.json");
    let snap = serde_json::to_value(w.snapshot(now)).unwrap();
    assert_eq!(validate(&state, &snap), Vec::<String>::new(), "{snap:#}");
    let event = load("event-v1.json");
    for e in events(env) {
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(validate(&event, &v), Vec::<String>::new(), "{v}");
    }
}

/// Verifies: SRS-SEL-008, SRS-STATE-002
#[test]
fn a_second_instance_cannot_write_into_a_run_claimed_on_the_share() {
    let env = Env::new();
    let run = one_completed_run(&env);
    let mut a = env.watcher(env.cfg("", ""));
    steps(&env, &mut a, 0, 1200, 30);
    assert_eq!(a.stats.completed_runs, 1);
    let claim = env.dest_of(&run).join(crate::journal::SHARE_CLAIM);
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&claim).unwrap()).unwrap();
    assert_eq!(body["source"], run.to_str().unwrap());
    assert_eq!(body["target_id"], TARGET);
    env.matches_simulator(&run);

    // A second sequencer (other source root, other state folder) with a run
    // of the same name, mapped to the same destination folder.
    let rel = run.strip_prefix(env.p("data")).unwrap().to_path_buf();
    let other = env.p("data2").join(&rel);
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    let status = std::process::Command::new("cp")
        .args(["-a", run.to_str().unwrap(), other.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    let victim = std::fs::read_dir(&other)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_file() && p.to_string_lossy().contains("final_summary"))
        .unwrap();
    std::fs::write(&victim, b"another run entirely\n").unwrap();
    std::fs::create_dir_all(env.p("state2")).unwrap();
    let cfg_path = env.dir.path().join("config.toml");
    let text = std::fs::read_to_string(&cfg_path)
        .unwrap()
        .replace(
            env.p("state").to_str().unwrap(),
            env.p("state2").to_str().unwrap(),
        )
        .replace(
            &format!("root = {:?}", env.p("data").to_str().unwrap()),
            &format!("root = {:?}", env.p("data2").to_str().unwrap()),
        );
    let cfg_path2 = env.dir.path().join("config2.toml");
    std::fs::write(&cfg_path2, text).unwrap();
    let cfg2 = config::load(&cfg_path2, &env.fs).unwrap().config;
    let before = std::fs::read(env.dest_of(&run).join(victim.file_name().unwrap())).unwrap();
    let mut b = env.watcher(cfg2.clone());
    steps(&env, &mut b, 0, 1200, 30);
    assert_eq!(b.stats.failed_runs, 1, "{:?}", b.stats);
    assert_eq!(b.stats.committed, 0);
    assert_eq!(
        std::fs::read(env.dest_of(&run).join(victim.file_name().unwrap())).unwrap(),
        before,
        "the first run's copy was changed"
    );
    let snap = b.snapshot(s(1200));
    assert!(
        snap.alerts
            .iter()
            .any(|a| a.kind == Kind::RunFailed && a.message.contains("claimed on the share")),
        "{:?}",
        snap.alerts
    );
    assert!(snap.runs.iter().any(|r| r.phase == "failed"));
    // `run` refuses too.
    let guard = Guard::new(&cfg2).with_mountinfo(&env.mountinfo);
    let det = crate::run::detect(&cfg2, &env.fs).unwrap();
    let err = crate::run::offload(&cfg2, &env.fs, &guard, &det.runs[0], true).unwrap_err();
    assert!(err.to_string().contains("claimed on the share"), "{err}");
}

/// Verifies: SRS-PAR-003, SRS-STATE-001
#[test]
fn workers_are_shared_across_runs_in_turn() {
    let env = Env::new();
    let mut engine = env.engine(
        "seed = 3\nhours = 1\n[fake]\nsize_scale = 0.0001\n\
         [[runs]]\nexperiment = \"a\"\nsample = \"big\"\n\
         [[runs]]\nexperiment = \"b\"\nsample = \"small\"\n",
    );
    let mut t = 0.0;
    while !engine.done() {
        engine.advance_to(t).unwrap();
        t += 300.0;
    }
    engine.write_manifests().unwrap();
    let mut w = env.watcher(env.cfg("", "[bandwidth]\nworkers = 1\n"));
    steps(&env, &mut w, 0, 3600, 10);
    let commits: Vec<String> = events(&env)
        .into_iter()
        .filter(|e| e.kind == Kind::FileCommitted)
        .map(|e| e.run.unwrap())
        .collect();
    let first: Vec<&String> = commits.iter().take(10).collect();
    let a = first.iter().filter(|r| r.starts_with("a/")).count();
    let b = first.iter().filter(|r| r.starts_with("b/")).count();
    // Source-path order would give run "a" all ten.
    assert!(a >= 4 && b >= 4, "first ten commits: {first:?}");
    assert_eq!(w.stats.completed_runs, 2);
    for r in engine.run_dirs() {
        env.matches_simulator(&r);
    }
}

/// Verifies: SRS-LIVE-004, SRS-STATE-002
#[test]
fn a_completed_run_that_changes_is_withdrawn_before_it_is_copied_again() {
    let env = Env::new();
    let run = one_completed_run(&env);
    let mut w = env.watcher(env.cfg("", "full_rescan_every = \"1h\"\n"));
    steps(&env, &mut w, 0, 1200, 30);
    assert_eq!(w.stats.completed_runs, 1);
    assert!(w.runs.is_empty());
    assert_eq!(w.completed.len(), 1);
    let dest = env.dest_of(&run);
    std::fs::write(dest.join(crate::journal::VERIFIED), "{}").unwrap();
    // Re-processing writes a new report into the completed run.
    std::fs::create_dir_all(run.join("other_reports")).unwrap();
    std::fs::write(run.join("other_reports/reprocessed.txt"), "new\n").unwrap();
    steps(&env, &mut w, 1210, 1210, 10);
    assert_eq!(w.stats.reopened_runs, 1);
    assert!(!dest.join(COMPLETE).exists(), "COMPLETE still there");
    assert!(!dest.join(crate::journal::VERIFIED).exists());
    assert!(!dest.join("other_reports/reprocessed.txt").exists());
    // After settling the run is complete again, with the new file.
    steps(&env, &mut w, 1220, 2400, 30);
    assert_eq!(w.stats.completed_runs, 2);
    let manifest = std::fs::read_to_string(dest.join("manifest.sha256")).unwrap();
    assert!(manifest.contains("other_reports/reprocessed.txt"));
    let k = kinds(&env);
    let reopened = k.iter().position(|k| *k == Kind::RunReopened).unwrap();
    let complete: Vec<usize> = k
        .iter()
        .enumerate()
        .filter(|(_, k)| **k == Kind::RunComplete)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(complete.len(), 2);
    assert!(complete[0] < reopened && reopened < complete[1]);
    // The journal says so too: Reopen before the new file's record.
    let j = std::fs::read_to_string(
        crate::journal::Journal::open(&env.fs, &env.p("state"), &run)
            .unwrap()
            .path(),
    )
    .unwrap();
    // Compacted on completion: no Reopen left, one record per file.
    assert!(!j.contains("\"reopen\""), "{j}");
    // An in-place change (same folder times) is found by the periodic full check.
    let pod5 = std::fs::read_dir(run.join("pod5"))
        .map(|d| d.map(|e| e.unwrap().path()).next().unwrap())
        .unwrap();
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&pod5)
        .unwrap();
    std::io::Write::write_all(&mut f, b"more").unwrap();
    drop(f);
    steps(&env, &mut w, 2410, 2500, 30);
    assert_eq!(w.stats.reopened_runs, 1, "noticed before the full check");
    steps(&env, &mut w, 2510, 6000, 30);
    assert_eq!(w.stats.reopened_runs, 2);
    assert_eq!(w.stats.completed_runs, 3);
    env.matches_simulator_except(&run, &["other_reports/reprocessed.txt"], &[&pod5]);
}

/// Verifies: SRS-RES-008, SRS-LIVE-004
#[test]
fn completed_runs_drop_their_file_lists_compact_their_journals_and_retire() {
    let env = Env::new();
    let mut engine = env.engine(
        "seed = 5\nhours = 0.5\n[fake]\nsize_scale = 0.0001\n\
         [[runs]]\nexperiment = \"e1\"\nsample = \"s\"\n[[runs]]\nexperiment = \"e2\"\nsample = \"s\"\n\
         [[runs]]\nexperiment = \"e3\"\nsample = \"s\"\n[[runs]]\nexperiment = \"e4\"\nsample = \"s\"\n",
    );
    let mut w = env.watcher(env.cfg("", "[transfer]\nwatch_completed_for = \"1h\"\n"));
    let total = engine.total_seconds() as u64;
    drive(&env, &mut w, &mut engine, 0, total + 900, 20, |_, _| {});
    finish(&env, &mut engine);
    assert_eq!(w.stats.completed_runs, 4);
    assert!(w.runs.is_empty(), "file lists kept after completion");
    assert_eq!(w.completed.len(), 4);
    for r in engine.run_dirs() {
        env.matches_simulator(&r);
        let j = crate::journal::Journal::open(&env.fs, &env.p("state"), &r).unwrap();
        // Claim, one record per file, completion: rewrites and retries are gone.
        assert_eq!(j.records(), j.files.len() + 2, "journal not compacted");
    }
    let snap = w.snapshot(s(total + 900));
    assert_eq!(
        snap.runs.iter().filter(|r| r.phase == "complete").count(),
        4
    );
    steps(&env, &mut w, total + 1000, total + 1000 + 3700, 100);
    assert!(w.completed.is_empty());
    assert_eq!(w.stats.retired_runs, 4);
    assert_eq!(
        kinds(&env)
            .iter()
            .filter(|k| **k == Kind::RunRetired)
            .count(),
        4
    );
    assert!(w.snapshot(s(total + 5000)).runs.is_empty());
    // A restart after the window does not watch them at all.
    std::thread::sleep(Duration::from_millis(1100));
    let mut w2 = env.watcher(env.cfg("", "[transfer]\nwatch_completed_for = \"1s\"\n"));
    steps(&env, &mut w2, 0, 100, 10);
    assert!(w2.completed.is_empty() && w2.runs.is_empty());
    assert_eq!(w2.stats.committed, 0);
}

/// Verifies: SRS-RES-009
#[test]
fn rescans_list_only_folders_that_changed_and_miss_nothing() {
    let env = Env::new();
    let mut engine = env.engine(
        "seed = 8\nhours = 2\n[fake]\nsize_scale = 0.0001\n[synthetic]\nbarcodes = 6\n\
         [[runs]]\nexperiment = \"e\"\nsample = \"s\"\n",
    );
    engine.advance_to(3600.0).unwrap();
    let run = engine.run_dirs()[0].clone();
    let mut w = env.watcher(env.cfg("", "full_rescan_every = \"10m\"\n"));
    steps(&env, &mut w, 0, 300, 10);
    assert!(w.stats.committed > 0);
    // Folder times must be older than the racy window before they are trusted.
    std::thread::sleep(Duration::from_millis(2100));
    steps(&env, &mut w, 310, 330, 10);
    let dirs = crate::run::dir_signature(&env.fs, &run).unwrap().len() as u64;
    let (l0, s0) = w.scan_work();
    steps(&env, &mut w, 340, 340, 10);
    let (l1, s1) = w.scan_work();
    assert_eq!(l1 - l0, 0, "unchanged folders listed again");
    assert!(
        s1 - s0 <= dirs + w.backlog() as u64,
        "{} status calls for {dirs} folders and {} waiting files",
        s1 - s0,
        w.backlog()
    );
    // A new file is found by listing only its folder.
    std::fs::write(run.join("new_report.txt"), "x").unwrap();
    steps(&env, &mut w, 350, 350, 10);
    let (l2, _) = w.scan_work();
    assert_eq!(l2 - l1, 1);
    assert!(
        w.runs
            .values()
            .any(|r| r.life.files.contains_key("new_report.txt"))
    );
    // Every full_rescan_every, everything is listed again.
    steps(&env, &mut w, 360, 940, 10);
    let (l3, _) = w.scan_work();
    assert!(
        l3 - l2 >= dirs,
        "no full rescan: {} listings for {dirs} folders",
        l3 - l2
    );
    // And the run still offloads exactly.
    let until = engine.total_seconds() as u64 + 900;
    drive(&env, &mut w, &mut engine, 950, until, 20, |_, _| {});
    finish(&env, &mut engine);
    assert_eq!(w.stats.completed_runs, 1);
    env.matches_simulator_except(&run, &["new_report.txt"], &[]);
}

/// Verifies: SRS-RES-010, SRS-STATE-002
#[test]
fn share_below_min_free_pauses_writes_until_space_returns() {
    let env = Env::new();
    let run = one_completed_run(&env);
    env.fs
        .add(Rule::always(Op::FreeSpace, "/share", Fault::Free(500_000)));
    let mut w = env.watcher(env.cfg("", ""));
    steps(&env, &mut w, 0, 300, 10);
    assert!(w.is_down());
    assert_eq!(w.stats.committed, 0);
    assert!(
        !env.dest_of(&run).exists() || std::fs::read_dir(env.dest_of(&run)).unwrap().count() == 0
    );
    let snap = w.snapshot(s(300));
    assert_eq!(snap.destination.free_bytes, Some(500_000));
    assert!(snap.alerts.iter().any(|a| a.kind == Kind::CapacityLow));
    assert_schema_valid(&mut w, &env, s(300));
    env.fs.clear();
    env.base_rules();
    steps(&env, &mut w, 310, 1800, 30);
    assert!(!w.is_down());
    assert_eq!(w.stats.completed_runs, 1);
    let k = kinds(&env);
    assert!(
        k.contains(&Kind::CapacityLow) && k.contains(&Kind::CapacityOk),
        "{k:?}"
    );
    let left = w.snapshot(s(1800)).alerts;
    assert!(left.is_empty(), "{left:?}");
    env.matches_simulator(&run);
}

/// Verifies: SRS-STATE-003
#[test]
fn low_source_space_raises_an_alert_and_clears() {
    let env = Env::new();
    one_completed_run(&env);
    env.fs
        .add(Rule::always(Op::FreeSpace, "/data", Fault::Free(1_000_000)));
    let mut w = env.watcher(env.cfg("", "[outage]\nsource_free_alert = \"1GB\"\n"));
    steps(&env, &mut w, 0, 60, 10);
    let snap = w.snapshot(s(60));
    assert_eq!(snap.source_free_bytes, Some(1_000_000));
    assert!(
        snap.alerts.iter().any(|a| a.kind == Kind::SourceSpaceLow),
        "{:?}",
        snap.alerts
    );
    env.fs.clear();
    env.base_rules();
    steps(&env, &mut w, 70, 120, 10);
    assert!(
        !w.snapshot(s(120))
            .alerts
            .iter()
            .any(|a| a.kind == Kind::SourceSpaceLow)
    );
    let k = kinds(&env);
    assert_eq!(k.iter().filter(|k| **k == Kind::SourceSpaceLow).count(), 1);
    assert!(k.contains(&Kind::SourceSpaceOk));
}

/// Verifies: SRS-RATE-003
#[test]
fn rate_limit_changes_on_reload_and_a_bad_file_changes_nothing() {
    let env = Env::new();
    let cfg = env.cfg("", "[bandwidth]\nrate_limit = \"10MB/s\"\n");
    let path = env.dir.path().join("config.toml");
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut w = env.watcher(cfg).with_reload(&path, Arc::clone(&flag));
    assert_eq!(w.rate_limit(), Some(10_000_000));
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("10MB/s", "20MB/s")).unwrap();
    flag.store(true, std::sync::atomic::Ordering::SeqCst);
    w.step(s(0)).unwrap();
    assert_eq!(w.rate_limit(), Some(20_000_000));
    assert!(!flag.load(std::sync::atomic::Ordering::SeqCst));
    std::fs::write(&path, text.replace("10MB/s", "fast")).unwrap();
    w.reload_now(&path);
    assert_eq!(w.rate_limit(), Some(20_000_000));
    let snap = w.snapshot(s(1));
    assert_eq!(snap.rate.limit_bytes_per_s, Some(20_000_000));
}

/// A seed run outside the source tree, for replayed content.
fn seed_run(env: &Env) -> PathBuf {
    let sc = ont_runsim::scenario::Scenario::from_toml(
        "seed = 77\nhours = 0.5\n[fake]\nsize_scale = 0.0001\n[[runs]]\nexperiment = \"seed\"\nsample = \"s\"\n",
        None,
    )
    .unwrap();
    let out = env.p("seed");
    ont_runsim::engine::prepare_out(&out).unwrap();
    let plans = ont_runsim::plan::build(&sc, SystemTime::UNIX_EPOCH).unwrap();
    let mut e = ont_runsim::engine::Engine::new(&out, plans).unwrap();
    let mut t = 0.0;
    while !e.done() {
        e.advance_to(t).unwrap();
        t += 300.0;
    }
    e.run_dirs()[0].clone()
}

/// The v0.4 acceptance soak on the fake clock: 24 h of replayed content in
/// two overlapping runs, with an outage, an error burst and the state file
/// checked against the schema throughout. The dashboard model reads the
/// state folder every 5 simulated minutes, as `tui` would, and must show
/// every alerting event by the next refresh and never call the data stale.
///
/// Verifies: SRS-LIVE-001, SRS-LIVE-003, SRS-PAR-003, SRS-RES-002, SRS-RES-008, SRS-RES-009, SRS-STATE-001, SRS-STATE-002, SRS-STATE-003, SRS-TUI-002, SRS-TUI-003, SRS-TUI-005
#[test]
fn replay_soak_24h_offloads_exactly_with_valid_state() {
    let env = Env::new();
    let seed = seed_run(&env);
    let mut engine = env.engine(&format!(
        "seed = 24\nhours = 24\ncontent = \"replay\"\n[fake]\nsize_scale = 0.0001\n[replay]\nrun = {:?}\n\
         [synthetic]\nbatch_every = \"30m\"\n\
         [faults]\nrewrite_closed = 2\nlate_files = 1\n\
         [[runs]]\nexperiment = \"soak\"\nsample = \"a\"\n\
         [[runs]]\nexperiment = \"soak\"\nsample = \"b\"\nstart_after = \"6h\"\n",
        seed.to_str().unwrap()
    ));
    let mut w = env.watcher(env.cfg("", "[bandwidth]\nworkers = 3\n"));
    let total = engine.total_seconds() as u64;
    let fs = env.fs.clone();
    let mut checked = 0;
    let mut dash = crate::tui::model::Model::new(&env.p("state"), None);
    let mut shown: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut due: Vec<String> = Vec::new();
    let mut saw_outage_alert = false;
    drive(&env, &mut w, &mut engine, 0, total + 1800, 60, |t, w| {
        if t > 0 && t % 300 == 0 {
            // The snapshot and events carry wall-clock times, so the
            // dashboard reads them at the real time.
            dash.refresh(&RealFs, std::time::SystemTime::now());
            assert!(
                dash.health.as_ref().unwrap().live(),
                "t={t}: {:?}",
                dash.health
            );
            shown.extend(
                dash.alerts
                    .iter()
                    .chain(&dash.cleared)
                    .map(|a| a.key.clone()),
            );
            saw_outage_alert |= dash.alerts.iter().any(|a| a.kind == Kind::OutageAlert);
            for k in &due {
                assert!(shown.contains(k), "t={t}: alert {k} not shown");
            }
            due = events(&env)
                .iter()
                .filter(|e| e.kind.alerts())
                .map(|e| format!("event:{}:{}", e.ts, e.seq))
                .collect();
        }
        match t {
            // A 2 h outage in the night.
            36_000 => fs.add(Rule::always(
                Op::Any,
                "/share",
                Fault::Errno(errno::ENOTCONN),
            )),
            43_200 => {
                fs.clear();
                env.base_rules();
            }
            60_000 => fs.add(Rule::times(
                Op::Write,
                "/share",
                Fault::Errno(errno::EIO),
                3,
            )),
            _ => {}
        }
        if t % 7200 == 0 {
            assert_schema_valid(w, &env, s(t));
            checked += 1;
        }
    });
    finish(&env, &mut engine);
    assert!(checked >= 12);
    assert_eq!(w.stats.failed_files, 0, "{:?}", w.stats);
    assert_eq!(w.stats.completed_runs, 2);
    assert!(w.stats.outages >= 1);
    for r in engine.run_dirs() {
        env.matches_simulator(&r);
    }
    let k = kinds(&env);
    for want in [
        Kind::OutageStarted,
        Kind::OutageWarning,
        Kind::OutageAlert,
        Kind::OutageEnded,
        Kind::Paused,
        Kind::Resumed,
    ] {
        assert!(k.contains(&want), "{want:?} missing");
    }
    assert_schema_valid(&mut w, &env, s(total + 1800));
    assert!(saw_outage_alert, "the outage alert was on the dashboard");
    assert!(
        dash.alerts.iter().all(|a| a.kind != Kind::OutageAlert),
        "and cleared"
    );
    assert_eq!(dash.missed_events, 0);
}

// ---------------------------------------------------------------------
// v0.5: the hourly cleanup pass
// ---------------------------------------------------------------------

/// Storage-side verification, as a cron job on the receiving side does:
/// every complete destination run without a receipt gets one.
fn storage_verify_all(env: &Env, runs: &[PathBuf]) -> usize {
    let mut n = 0;
    for r in runs {
        let d = env.dest_of(r);
        if d.join(COMPLETE).exists() && !d.join(crate::journal::VERIFIED).exists() {
            crate::journal::verify(&RealFs, &d, false).unwrap();
            n += 1;
        }
    }
    n
}

fn cleanup_cfg(env: &Env, delete: &str) -> Config {
    env.cfg(
        "",
        &format!(
            "[transfer]\nwatch_completed_for = \"3h\"\n[cleanup]\ndelete = {delete:?}\ndelete_after = \"1h\"\n"
        ),
    )
}

/// The daemon's pass: dry run first, never a run still watched, then a
/// real deletion after a restart, and a deleted run is never offloaded
/// again even when its folder reappears.
///
/// Verifies: SRS-DEL-001, SRS-DEL-006, SRS-STATE-001, SRS-STATE-002
#[test]
fn hourly_cleanup_pass_leaves_watched_runs_and_deletes_retired_ones() {
    let env = Env::new();
    let mut engine = env.engine(
        "seed = 9\nhours = 0.5\n[fake]\nsize_scale = 0.0002\n[[runs]]\nexperiment = \"e\"\nsample = \"s\"\n",
    );
    let mut w = env.watcher(cleanup_cfg(&env, "dry-run"));
    let total = engine.total_seconds() as u64;
    drive(&env, &mut w, &mut engine, 0, total + 600, 30, |_, _| {});
    finish(&env, &mut engine);
    let run = engine.run_dirs()[0].clone();
    assert_eq!(w.stats.completed_runs, 1);
    assert_eq!(storage_verify_all(&env, &[run.clone()]), 1);

    // Still watched (3 h): every pass leaves it alone.
    steps(&env, &mut w, total + 660, total + 2 * 3600, 60);
    let snap = w.snapshot(s(total + 2 * 3600));
    let c = snap.cleanup.clone().expect("cleanup state");
    assert_eq!(c.mode, "dry-run");
    assert!(c.last_pass_at.is_some());
    assert_eq!(c.would_delete, 0, "{c:?}");
    assert!(kinds(&env).contains(&Kind::DeletionDryRun));
    // Retired: the next dry run says it would delete it, and deletes nothing.
    steps(&env, &mut w, total + 2 * 3600 + 60, total + 5 * 3600, 60);
    assert_eq!(
        w.snapshot(s(total + 5 * 3600))
            .cleanup
            .unwrap()
            .would_delete,
        1
    );
    assert!(run.exists());
    env.matches_simulator(&run);
    assert_schema_valid(&mut w, &env, s(total + 5 * 3600));
    drop(w);

    // Restart with deletion on: the run is too old to be watched, so the
    // first pass deletes it.
    let t0 = total + 5 * 3600;
    let mut w = env.watcher(cleanup_cfg(&env, "on"));
    steps(&env, &mut w, t0, t0 + 900, 30);
    assert!(!run.exists(), "deleted");
    assert_eq!(w.stats.deleted_runs, 1);
    assert!(kinds(&env).contains(&Kind::RunDeleted));
    assert!(env.dest_of(&run).join(crate::journal::DELETED).exists());
    env.matches_simulator(&run);
    let snap = w.snapshot(s(t0 + 900));
    assert_eq!(snap.totals.deleted_runs, 1);
    assert_eq!(snap.cleanup.as_ref().unwrap().deleted_runs, 1);
    assert_schema_valid(&mut w, &env, s(t0 + 900));
    let reports = std::fs::read_to_string(env.p("state").join(crate::cleanup::REPORTS)).unwrap();
    assert!(reports.lines().count() >= 4, "every pass is reported");

    // The folder comes back with a run marker: not offloaded again.
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("sample_sheet_again.csv"), b"x").unwrap();
    let discovered = kinds(&env)
        .iter()
        .filter(|k| **k == Kind::RunDiscovered)
        .count();
    let mut w = env.watcher(cleanup_cfg(&env, "on"));
    steps(&env, &mut w, t0 + 960, t0 + 1200, 30);
    assert!(w.runs.is_empty() && w.completed.is_empty());
    assert_eq!(w.stats.committed, 0);
    assert_eq!(w.stats.failed_runs, 1, "a person is alerted");
    assert!(
        w.snapshot(s(t0 + 1200))
            .alerts
            .iter()
            .any(|a| a.message.contains("deleted by cleanup"))
    );
    assert_eq!(
        kinds(&env)
            .iter()
            .filter(|k| **k == Kind::RunDiscovered)
            .count(),
        discovered
    );
}

/// A run refused by a gate raises one alert (one `notify_command`), shown
/// until the problem is gone; a failure while deleting stops the pass with
/// an alert, and the next pass resumes.
///
/// Verifies: SRS-DEL-004, SRS-DEL-006, SRS-STATE-003
#[test]
fn refused_and_stopped_deletions_alert_once_and_clear() {
    let env = Env::new();
    let run = one_completed_run(&env);
    let mut w = env.watcher(env.cfg(
        "",
        "[transfer]\nwatch_completed_for = \"1s\"\n[cleanup]\ndelete = \"on\"\ndelete_after = \"1s\"\n",
    ));
    // Offload it (the run is complete; watch_completed_for is tiny).
    steps(&env, &mut w, 0, 600, 10);
    assert_eq!(w.stats.completed_runs, 1);
    storage_verify_all(&env, &[run.clone()]);
    // Change one destination file's size: refused.
    let dest = env.dest_of(&run);
    let pod5 = std::fs::read_dir(dest.join("pod5"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .next()
        .unwrap();
    let full = std::fs::read(&pod5).unwrap();
    std::fs::write(&pod5, &full[1..]).unwrap();
    steps(&env, &mut w, 900, 300 + 3 * 3600, 300);
    let refused = kinds(&env)
        .iter()
        .filter(|k| **k == Kind::DeletionRefused)
        .count();
    assert_eq!(
        refused,
        1,
        "one alert, not one per pass: {}",
        std::fs::read_to_string(env.p("state").join(crate::cleanup::REPORTS)).unwrap_or_default()
    );
    let snap = w.snapshot(s(300 + 3 * 3600));
    assert!(
        snap.alerts.iter().any(|a| a.kind == Kind::DeletionRefused),
        "{:?}",
        snap.alerts
    );
    assert!(run.exists());
    // Repaired: the alert clears and deletion starts, but removing a
    // file fails after a few: stopped, with an alert.
    std::fs::write(&pod5, &full).unwrap();
    env.fs.add(Rule::after(
        Op::RemoveFile,
        "/data/",
        Fault::Errno(errno::EACCES),
        5,
    ));
    let t = 300 + 4 * 3600;
    steps(&env, &mut w, t, t + 3600, 300);
    env.fs.clear();
    env.base_rules();
    assert!(
        kinds(&env).contains(&Kind::DeletionStopped),
        "{:?}",
        kinds(&env)
    );
    let snap = w.snapshot(s(t + 3600));
    assert!(!snap.alerts.iter().any(|a| a.kind == Kind::DeletionRefused));
    // Resumed on a later pass once the share is back.
    steps(&env, &mut w, t + 3900, t + 4 * 3600, 300);
    assert!(!run.exists());
    assert_eq!(w.stats.deleted_runs, 1);
    let snap = w.snapshot(s(t + 4 * 3600));
    assert!(
        snap.alerts.iter().all(|a| a.kind != Kind::DeletionStopped),
        "{:?}",
        snap.alerts
    );
}

/// The v0.5 qualification soak on the fake clock: 72 h, 200 runs of an
/// hour each, a new one every 22 minutes, hourly storage-side verification by a "cron job", hourly
/// cleanup passes. Memory stays bounded (only live and watched runs are
/// held), every run is offloaded exactly, and every deleted source run has
/// a verified, intact copy, `DELETED.json` and a deletion record per file.
///
/// The journal's completion times are real time while the controller's
/// clock is fake, so `delete_after` is not what this soak measures (the
/// gate tests do); `watch_completed_for` is.
///
/// Verifies: SRS-DEL-002, SRS-DEL-003, SRS-DEL-006, SRS-RES-008, SRS-RES-009, SRS-STATE-001
#[test]
fn soak_72h_with_hourly_verify_and_cleanup() {
    let env = Env::new();
    let mut toml = "seed = 72\nhours = 1\n[fake]\nsize_scale = 0.00002\n".to_owned();
    for i in 0..200 {
        toml.push_str(&format!(
            "[[runs]]\nexperiment = \"soak\"\nsample = \"r{i:03}\"\nstart_after = \"{}m\"\n",
            i * 22
        ));
    }
    let mut engine = env.engine(&toml);
    let mut w = env.watcher(env.cfg(
        "",
        "[bandwidth]\nworkers = 2\n[transfer]\nwatch_completed_for = \"1h\"\n\
         [cleanup]\ndelete = \"on\"\ndelete_after = \"1h\"\n",
    ));
    let total = engine.total_seconds() as u64;
    assert!(total >= 72 * 3600, "{total}");
    let runs = engine.run_dirs();
    let mut held_max = 0;
    let mut verified = 0;
    drive(
        &env,
        &mut w,
        &mut engine,
        0,
        total + 4 * 3600,
        120,
        |t, w| {
            if t % 3600 == 0 {
                verified += storage_verify_all(&env, &runs);
            }
            held_max = held_max.max(w.runs.len() + w.completed.len());
            if t % (12 * 3600) == 0 {
                assert_schema_valid(w, &env, s(t));
            }
        },
    );
    finish(&env, &mut engine);
    verified += storage_verify_all(&env, &runs);
    // A few live runs and those watched for an hour: memory does not
    // grow with the number of runs.
    assert!(held_max <= 10, "held {held_max} runs at once");
    assert_eq!(w.stats.completed_runs, 200);
    assert_eq!(w.stats.failed_files, 0);
    assert_eq!(verified, 200);
    let log = std::fs::read_to_string(env.p("state").join(crate::cleanup::DELETIONS)).unwrap();
    let recs: Vec<crate::cleanup::DeletionRecord> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let mut deleted = 0;
    for r in &runs {
        env.matches_simulator(r);
        let dest = env.dest_of(r);
        if r.exists() {
            assert!(!dest.join(crate::journal::DELETED).exists());
            continue;
        }
        deleted += 1;
        assert!(dest.join(crate::journal::VERIFIED).exists());
        assert!(dest.join(crate::journal::DELETED).exists());
        let id = crate::journal::run_id(r);
        let manifest = crate::journal::parse_manifest(
            &std::fs::read_to_string(dest.join("manifest.sha256")).unwrap(),
        )
        .unwrap();
        let listed: std::collections::BTreeSet<&str> = recs
            .iter()
            .filter(|d| d.run_id == id)
            .map(|d| d.rel.as_str())
            .collect();
        assert_eq!(listed.len(), manifest.len());
    }
    assert!(deleted >= 195, "only {deleted} of 200 deleted");
    assert_eq!(w.stats.deleted_runs, deleted);
    assert_schema_valid(&mut w, &env, s(total + 4 * 3600));
}

impl Env {
    /// MinKNOW runs in `data` and user datasets in `transfer` (layout
    /// `ready`, into `dest_root/transfer`), with `extra` settings.
    fn two_sources(&self, extra: &str) -> Config {
        std::fs::create_dir_all(self.p("transfer")).unwrap();
        let path = self.dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "dest_root = {:?}\ntarget_id = {TARGET:?}\nstate_dir = {:?}\n\
                 [[source]]\nroot = {:?}\n\
                 [[source]]\nroot = {:?}\nlayout = \"ready\"\ndest = \"transfer\"\n\
                 [outage]\nsource_free_alert = \"0B\"\n{extra}\n",
                self.p("share").to_str().unwrap(),
                self.p("state").to_str().unwrap(),
                self.p("data").to_str().unwrap(),
                self.p("transfer").to_str().unwrap(),
            ),
        )
        .unwrap();
        config::load(&path, &self.fs).unwrap().config
    }
}

/// One daemon offloads a live MinKNOW run and a user dataset dropped with
/// READY into one share, shows each run with its source, and its cleanup
/// pass deletes the run but never the dataset.
///
/// Verifies: SRS-SRC-001, SRS-SRC-002, SRS-SRC-004, SRS-SRC-005, SRS-STATE-001
#[test]
fn runs_and_user_datasets_offload_side_by_side_and_only_runs_are_deleted() {
    let env = Env::new();
    let cfg = env.two_sources(
        "[transfer]\nmin_free = \"1MB\"\nwatch_completed_for = \"30m\"\n\
         [cleanup]\ndelete = \"on\"\ndelete_after = \"0s\"\n",
    );
    let mut engine = env.engine(
        "seed = 9\nhours = 0.5\n[fake]\nsize_scale = 0.0002\n[[runs]]\nexperiment = \"e\"\nsample = \"s\"\n",
    );
    let mut w = env.watcher(cfg);
    let set = env.p("transfer/alice/set1");
    let files = [
        ("reads/a.fastq", "aaaa"),
        ("reads/deeper/b.fastq", "bbbbbb"),
        ("notes.txt", "n"),
    ];
    let total = engine.total_seconds() as u64;
    drive(&env, &mut w, &mut engine, 0, total + 600, 30, |t, w| {
        if t == 300 {
            for (rel, text) in files {
                let p = set.join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, text).unwrap();
            }
        }
        if t == 600 {
            assert!(
                !w.runs.keys().any(|k| k.starts_with(&set)),
                "not offloaded before READY"
            );
            std::fs::write(set.join("READY"), b"").unwrap();
        }
    });
    finish(&env, &mut engine);
    let run = engine.run_dirs()[0].clone();
    assert_eq!(w.stats.completed_runs, 2, "{:?}", w.stats);
    env.matches_simulator(&run);
    let dest = env.p("share/transfer/alice/set1");
    for (rel, text) in files {
        assert_eq!(
            std::fs::read_to_string(dest.join(rel)).unwrap(),
            text,
            "{rel}"
        );
    }
    assert!(dest.join(COMPLETE).exists() && dest.join("READY").exists());

    let snap = w.snapshot(s(total + 600));
    let mut names: Vec<(String, Option<String>)> = snap
        .runs
        .iter()
        .map(|r| (r.name.clone(), r.source_name.clone()))
        .collect();
    names.sort();
    let run_rel = run
        .strip_prefix(env.p("data"))
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        names,
        vec![
            (format!("data/{run_rel}"), Some("data".to_owned())),
            (
                "transfer/alice/set1".to_owned(),
                Some("transfer".to_owned())
            ),
        ]
    );
    assert_schema_valid(&mut w, &env, s(total + 600));

    // Both retire, both copies are verified on the storage side, and the
    // hourly passes delete the run only.
    steps(&env, &mut w, total + 660, total + 3000, 60);
    assert_eq!(storage_verify_all(&env, &[run.clone()]), 1);
    crate::journal::verify(&RealFs, &dest, false).unwrap();
    steps(&env, &mut w, total + 3060, total + 3 * 3600, 60);
    assert!(!run.exists(), "the MinKNOW run is deleted");
    assert_eq!(w.stats.deleted_runs, 1);
    assert!(
        set.join("notes.txt").exists(),
        "the user dataset is never deleted"
    );
    env.matches_simulator(&run);
}
