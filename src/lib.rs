// SPDX-License-Identifier: AGPL-3.0-or-later
//! ont-offload: verified offload of ONT sequencing runs to mounted network storage.
//!
//! Module boundaries follow the design plan and the software items in
//! `compliance/architecture/`; behaviour is added release by release (see the
//! staged implementation plan in the docs). v0.1 provides the filesystem
//! layer ([`fsio`]) and configuration validation ([`config`], `check-config`).

pub mod bench;
pub mod cleanup;
pub mod cli;
pub mod config;
pub mod copy;
pub mod error;
pub mod fsio;
pub mod guard;
pub mod journal;
pub mod logging;
pub mod run;
pub mod sched;
pub mod state;

pub use error::{Error, Result};

use std::io::Write;
use std::path::{Path, PathBuf};

use cli::{Cli, Command};
use fsio::{Fs, RealFs};
use guard::Guard;
use logging::escape_control;

/// Dispatch a parsed command.
///
/// Implements: SRS-CLI-001
///
/// # Errors
/// Returns the command's error; `main` maps it to an exit code.
pub fn execute(cli: &Cli) -> Result<()> {
    log::debug!("command: {:?}", cli.command);
    let out = &mut std::io::stdout();
    match &cli.command {
        Command::CheckConfig => check_config(&cli.global.config, &RealFs, None, out),
        Command::Run { path, completed } => {
            run_one(&cli.global.config, &RealFs, None, path, *completed, out)
        }
        Command::Verify { run, source } => verify_run(&RealFs, run, *source, out),
        Command::Watch { until_idle } => watch(
            &cli.global.config,
            std::sync::Arc::new(RealFs),
            None,
            *until_idle,
        ),
        Command::Status { json } => status(&cli.global.config, &RealFs, *json, out),
        Command::Bench {
            size,
            max_streams,
            no_chunks,
        } => {
            let size = config::units::parse_size(size).map_err(Error::Config)?;
            bench_share(
                &cli.global.config,
                &RealFs,
                None,
                size,
                *max_streams,
                !*no_chunks,
                out,
            )
        }
        other @ Command::Cleanup { .. } => Err(Error::NotImplemented {
            command: other.name(),
            planned: other.planned(),
        }),
    }
}

fn guard_for(cfg: &config::Config, mountinfo: Option<&Path>) -> Guard {
    let g = Guard::new(cfg);
    match mountinfo {
        Some(m) => g.with_mountinfo(m),
        None => g,
    }
}

/// `check-config`: validate the configuration, check the destination guard,
/// and list every detected run with its marker and destination.
///
/// `mountinfo` overrides `/proc/self/mountinfo` (tests).
///
/// Implements: SRS-CFG-004, SRS-SEL-006, SRS-GRD-001
///
/// # Errors
/// [`Error::Config`] listing every problem, [`Error::Guard`] when the
/// destination is refused (after the runs are listed); I/O errors.
pub fn check_config(
    path: &Path,
    fs: &dyn Fs,
    mountinfo: Option<&Path>,
    out: &mut dyn Write,
) -> Result<()> {
    let loaded = config::load(path, fs)?;
    let cfg = &loaded.config;
    for w in &loaded.warnings {
        log::warn!("{w}");
    }
    writeln!(
        out,
        "configuration: {}",
        escape_control(&path.to_string_lossy())
    )?;
    write!(out, "{}", cfg.summary())?;
    let det = run::detect(cfg, fs)?;
    for w in &det.warnings {
        log::warn!("{w}");
    }
    writeln!(out, "runs detected: {}", det.runs.len())?;
    for r in &det.runs {
        let mapped = run::dest_rel(&r.rel, cfg.dest_starts_at).map_or_else(
            |e| format!("NOT MAPPED: {e}"),
            |d| escape_control(&cfg.dest_root.resolved.join(d).to_string_lossy()),
        );
        writeln!(
            out,
            "  {}  [{}; {}] -> {mapped}",
            escape_control(&r.rel.to_string_lossy()),
            escape_control(&r.marker),
            r.complete.as_deref().map_or_else(
                || "running or no complete marker".to_owned(),
                |c| format!("complete: {}", escape_control(c))
            )
        )?;
    }
    let guard = guard_for(cfg, mountinfo);
    match guard.check(fs) {
        Ok(m) => writeln!(
            out,
            "destination guard OK: sentinel matches, {} mounted from {} at {}",
            m.fstype,
            escape_control(&m.source),
            escape_control(&m.point.to_string_lossy())
        )?,
        Err(e) => {
            writeln!(out, "destination guard FAILED: {e}")?;
            return Err(e);
        }
    }
    writeln!(
        out,
        "configuration OK ({} warning(s))",
        loaded.warnings.len()
    )?;
    Ok(())
}

/// `run <path> [--completed]`: offload one completed run in the foreground.
///
/// Implements: SRS-SEL-006, SRS-GRD-001, SRS-CLI-004
///
/// # Errors
/// As [`run::offload`]; [`Error::Failed`] when files failed.
pub fn run_one(
    config_path: &Path,
    fs: &dyn Fs,
    mountinfo: Option<&Path>,
    path: &Path,
    completed: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let loaded = config::load(config_path, fs)?;
    let cfg = &loaded.config;
    for w in &loaded.warnings {
        log::warn!("{w}");
    }
    let shown = escape_control(&path.to_string_lossy());
    let real: PathBuf = fs
        .canonicalize(path)
        .map_err(|e| Error::Config(format!("run folder {shown}: {e}")))?;
    let root = &cfg.source.root.resolved;
    if !real.starts_with(root) || real == *root {
        return Err(Error::Config(format!(
            "run folder {shown} is not below source.root {}",
            escape_control(&root.to_string_lossy())
        )));
    }
    let run_m = run::Markers::new(&cfg.source.run_markers)?;
    let done_m = run::Markers::new(&cfg.source.complete_markers)?;
    let detected = run::inspect(fs, root, &real, &run_m, &done_m)?.ok_or_else(|| {
        Error::Config(format!(
            "{shown} is not a run: none of {} found in it",
            cfg.source.run_markers.join(", ")
        ))
    })?;
    let guard = guard_for(cfg, mountinfo);
    let _lock = sched::lock::InstanceLock::acquire(fs, &cfg.state_dir.resolved)?;
    let report = run::offload(cfg, fs, &guard, &detected, completed)?;
    writeln!(
        out,
        "{} -> {}: {}",
        escape_control(&detected.rel.to_string_lossy()),
        escape_control(&report.dest.to_string_lossy()),
        if report.already_complete {
            "already complete".to_owned()
        } else if report.failed.is_empty() {
            format!(
                "COMPLETE ({} files, {} copied, {} already there)",
                report.files, report.copied, report.skipped
            )
        } else {
            format!("{} of {} files FAILED", report.failed.len(), report.files)
        }
    )?;
    for (f, why) in &report.failed {
        writeln!(
            out,
            "  failed: {}: {}",
            escape_control(f),
            escape_control(why)
        )?;
    }
    if report.failed.is_empty() {
        Ok(())
    } else {
        Err(Error::Failed(report.failed.len()))
    }
}

/// `verify <run> [--source]`: re-hash a destination run against its manifest.
///
/// Implements: SRS-VER-003, SRS-VER-004
///
/// # Errors
/// [`Error::Integrity`] on any difference.
pub fn verify_run(fs: &dyn Fs, run: &Path, source: bool, out: &mut dyn Write) -> Result<()> {
    let rep = journal::verify(fs, run, source)?;
    writeln!(
        out,
        "{}: OK, {} files, {} bytes match manifest {}{}",
        escape_control(&run.to_string_lossy()),
        rep.files,
        rep.bytes,
        rep.manifest_sha256,
        if rep.source_checked {
            "; source matches too"
        } else {
            ""
        }
    )?;
    Ok(())
}

/// `watch [--until-idle]`: the offload daemon.
///
/// Takes the state folder lock, logs the version, the effective settings
/// and (on the first check) the destination's mount, then runs the
/// controller until stopped. SIGHUP re-reads `rate_limit`. The journal is
/// consistent at every moment, so stopping the process (SIGTERM, SIGKILL,
/// power loss) is always safe; the next start resumes.
///
/// Implements: SRS-RES-007, SRS-PAR-001, SRS-RES-006, SRS-RATE-003
///
/// # Errors
/// [`Error::Config`] or [`Error::Guard`] at start-up, [`Error::RestartNeeded`]
/// when every worker is stalled, [`Error::Failed`] with `until_idle`.
pub fn watch(
    config_path: &Path,
    fs: std::sync::Arc<dyn Fs>,
    mountinfo: Option<&Path>,
    until_idle: bool,
) -> Result<()> {
    let loaded = config::load(config_path, &*fs)?;
    for w in &loaded.warnings {
        log::warn!("{w}");
    }
    let cfg = loaded.config;
    let _lock = sched::lock::InstanceLock::acquire(&*fs, &cfg.state_dir.resolved)?;
    let guard = std::sync::Arc::new(guard_for(&cfg, mountinfo));
    log::info!(
        "ont-offload {} watching {} -> {}; effective configuration from {}:\n{}",
        env!("CARGO_PKG_VERSION"),
        escape_control(&cfg.source.root.resolved.to_string_lossy()),
        escape_control(&cfg.dest_root.resolved.to_string_lossy()),
        escape_control(&config_path.to_string_lossy()),
        cfg.summary().trim_end()
    );
    // Rescan at least twice per settle window, at most every 10 s.
    let scan_every = (cfg.transfer.settle / 2).clamp(
        std::time::Duration::from_secs(1),
        std::time::Duration::from_secs(10),
    );
    let opts = sched::watch::Options {
        until_idle,
        scan_every,
        tick: scan_every.min(std::time::Duration::from_secs(1)),
        ..sched::watch::Options::default()
    };
    let reload = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(unix)]
    if let Err(e) =
        signal_hook::flag::register(signal_hook::consts::SIGHUP, std::sync::Arc::clone(&reload))
    {
        log::warn!("cannot handle SIGHUP ({e}); changing rate_limit needs a restart");
    }
    let mut w = sched::watch::Watcher::new(
        cfg,
        fs,
        guard,
        std::sync::Arc::new(sched::RealClock::new()),
        opts,
        sched::notify::Notifier::from_env(),
    )?
    .with_reload(config_path, reload);
    w.run()
}

/// `status [--json]`: print the snapshot that `watch` keeps in
/// `state_dir/state.json`. A snapshot older than three intervals (at least
/// 30 s) is flagged as stale: the daemon is not running or is stuck.
///
/// Implements: SRS-STATE-001, SRS-CLI-001
///
/// # Errors
/// [`Error::Config`] for a bad configuration or a missing or unreadable
/// snapshot.
pub fn status(config_path: &Path, fs: &dyn Fs, json: bool, out: &mut dyn Write) -> Result<()> {
    let cfg = config::load(config_path, fs)?.config;
    let dir = &cfg.state_dir.resolved;
    if json {
        let path = dir.join(state::STATE_FILE);
        let bytes = fs.read(&path).map_err(|e| {
            Error::Config(format!(
                "cannot read {}: {e}; is `ont-offload watch` running?",
                escape_control(&path.to_string_lossy())
            ))
        })?;
        out.write_all(&bytes)?;
        return Ok(());
    }
    let snap = state::read_snapshot(fs, dir)
        .map_err(|e| Error::Config(format!("{e}; is `ont-offload watch` running?")))?;
    write!(
        out,
        "{}",
        state::describe(&snap, std::time::SystemTime::now())
    )?;
    Ok(())
}

/// `bench`: measure the share and suggest settings.
///
/// Implements: SRS-PAR-002
///
/// # Errors
/// As [`bench::run`].
#[allow(clippy::too_many_arguments)]
pub fn bench_share(
    config_path: &Path,
    fs: &dyn Fs,
    mountinfo: Option<&Path>,
    size: u64,
    max_streams: usize,
    chunks: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let cfg = config::load(config_path, fs)?.config;
    let guard = guard_for(&cfg, mountinfo);
    let opts = bench::BenchOptions {
        size,
        max_streams,
        chunk_size: cfg.transfer.chunk_size,
        chunks,
    };
    bench::run(
        fs,
        &guard,
        &cfg.state_dir.resolved,
        &opts,
        cfg.transfer.min_free,
        out,
    )?;
    Ok(())
}
