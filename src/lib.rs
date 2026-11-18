// SPDX-License-Identifier: AGPL-3.0-or-later
//! ont-offload: verified offload of ONT sequencing runs to mounted network storage.
//!
//! Module boundaries follow the design plan and the software items in
//! `compliance/architecture/`; behaviour is added release by release (see the
//! staged implementation plan in the docs). v0.1 provides the filesystem
//! layer ([`fsio`]) and configuration validation ([`config`], `check-config`).

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
        other => Err(Error::NotImplemented {
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
