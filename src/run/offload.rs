// SPDX-License-Identifier: AGPL-3.0-or-later
//! Offload of one completed run (v0.2: one file at a time).
//!
//! Guard, claim the destination, select files, copy each one that the
//! journal does not already record as committed, check every committed
//! file's size, then write `manifest.sha256`, `run.json` and `COMPLETE` last.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::Config;
use crate::copy::{self, CopyFail, CopyOptions};
use crate::error::{Error, Result};
use crate::fsio::{ErrorClass, FileKind, Fs, classify};
use crate::guard::Guard;
use crate::journal::{self, COMPLETE, Journal, Record, claim_on_share};
use crate::logging::escape_control;

use super::{DetectedRun, Markers, dest_rel, finalise, safe_join, select};

/// Outcome of [`offload`].
#[derive(Debug, Clone, Default)]
pub struct OffloadReport {
    /// Run id.
    pub run_id: String,
    /// Destination run folder.
    pub dest: PathBuf,
    /// Files selected.
    pub files: usize,
    /// Bytes selected.
    pub bytes: u64,
    /// Files copied in this invocation.
    pub copied: usize,
    /// Files already committed earlier (resume).
    pub skipped: usize,
    /// Files that failed, with the reason.
    pub failed: Vec<(String, String)>,
    /// The run was already complete; nothing was done.
    pub already_complete: bool,
    /// SHA-256 of the manifest when complete.
    pub manifest_sha256: Option<String>,
}

/// The log line for a committed file: run id, path (escaped), size, digest
/// and duration.
///
/// Implements: SRS-LOG-003
#[must_use]
pub fn commit_message(run_id: &str, rel: &str, c: &copy::Copied, secs: f64) -> String {
    format!(
        "committed run={run_id} file={} size={} sha256={} in {secs:.2}s{}",
        escape_control(rel),
        c.size,
        c.sha256,
        if c.adopted {
            " (identical file adopted)"
        } else {
            ""
        }
    )
}

/// Copy one completed run to the destination.
///
/// `assume_completed` treats the run as finished without a complete marker
/// (`run --completed`).
///
/// Implements: SRS-SEL-001, SRS-SEL-002, SRS-SEL-007, SRS-SEL-008, SRS-RES-004, SRS-VER-002, SRS-LOG-003
///
/// # Errors
/// [`Error::Guard`] when the guard fails (nothing more is written),
/// [`Error::Config`] for a run that is not complete or cannot be mapped,
/// [`Error::RestartNeeded`] when the share fails mid-run (safe to resume),
/// [`Error::Integrity`] for a destination claimed by another run or a size
/// mismatch after commit, I/O errors.
#[allow(clippy::too_many_lines)]
pub fn offload(
    cfg: &Config,
    fs: &dyn Fs,
    guard: &Guard,
    run: &DetectedRun,
    assume_completed: bool,
) -> Result<OffloadReport> {
    let name = escape_control(&run.rel.to_string_lossy());
    if run.complete.is_none() && !assume_completed {
        return Err(Error::Config(format!(
            "run {name} has no complete marker ({}); offloading live runs comes with `watch` in v0.3. \
             Use `run --completed` if the run has finished",
            cfg.source.complete_markers.join(", ")
        )));
    }
    let rel = dest_rel(&run.rel, cfg.dest_starts_at).map_err(Error::Config)?;
    let dest = safe_join(guard.dest(), &rel).map_err(Error::Config)?;
    guard.check(fs)?;

    let state = &cfg.state_dir.resolved;
    let mut j = Journal::open(fs, state, &run.path)?;
    if let Some(ts) = &j.deleting {
        return Err(Error::Config(format!(
            "run {name}: its source was deleted by cleanup (started {ts}); it is not offloaded again. \
             To offload a new run at this path, move its journal {} away",
            escape_control(&j.path().to_string_lossy())
        )));
    }
    let mut report = OffloadReport {
        run_id: j.id.clone(),
        dest: dest.clone(),
        ..OffloadReport::default()
    };
    journal::claim(fs, state, &mut j, &run.path, &rel, cfg.dest_starts_at)?;
    let started = journal::now_rfc3339();
    let t0 = Instant::now();

    let markers = Markers::new(&cfg.source.run_markers)?;
    let sel = select(fs, &run.path, &cfg.selection, &markers)?;
    report.files = sel.files.len();
    report.bytes = sel.bytes();

    if let Some(m) = &j.complete {
        if fs.symlink_metadata(&dest.join(COMPLETE)).is_ok() {
            let all = sel.files.iter().all(|f| {
                j.files
                    .get(&f.rel)
                    .is_some_and(|c| c.size == f.size && c.mtime_ns == journal::nanos(f.modified))
            });
            if all {
                log::info!(
                    "run {name}: already complete at {}",
                    escape_control(&dest.to_string_lossy())
                );
                report.already_complete = true;
                report.manifest_sha256 = Some(m.clone());
                return Ok(report);
            }
            log::warn!(
                "run {name}: source changed since it was completed; withdrawing COMPLETE and offloading again"
            );
        }
    }

    claim_on_share(fs, guard, &dest, &run.path, &j.id, &cfg.target_id).map_err(|e| match e {
        CopyFail::Conflict(m) => Error::Integrity(m),
        CopyFail::Guard(e) => e,
        CopyFail::Io(e) => Error::Io(e),
        other => Error::Integrity(other.to_string()),
    })?;
    if j.complete.is_some() {
        super::finalise::withdraw(fs, guard, &dest)?;
        j.append(
            fs,
            Record::Reopen {
                ts: journal::now_rfc3339(),
            },
        )?;
    }
    fs.create_dir_all(&dest)?;
    let removed = copy::remove_stale_temps(fs, &dest)?;
    if removed > 0 {
        log::info!("run {name}: removed {removed} stale temp file(s) from an earlier attempt");
    }
    log::info!(
        "run {name} (id {}): {} files, {} bytes -> {}",
        j.id,
        sel.files.len(),
        sel.bytes(),
        escape_control(&dest.to_string_lossy())
    );

    let opts = CopyOptions {
        chunk_size: usize::try_from(cfg.transfer.chunk_size).unwrap_or(8 << 20),
        verify: cfg.transfer.verify,
        on_existing: cfg.transfer.on_existing,
        file_mode: cfg.transfer.file_mode,
        preserve_mtime: cfg.transfer.preserve_mtime,
        min_free: cfg.transfer.min_free,
    };
    for f in &sel.files {
        let src = safe_join(&run.path, Path::new(&f.rel)).map_err(Error::Config)?;
        let target = safe_join(&dest, Path::new(&f.rel)).map_err(Error::Config)?;
        let shown = escape_control(&f.rel);
        let prev = j.files.get(&f.rel).cloned();
        if let Some(c) = &prev {
            let same_source = c.size == f.size && c.mtime_ns == journal::nanos(f.modified);
            let present = fs
                .symlink_metadata(&target)
                .is_ok_and(|d| d.kind == FileKind::File && d.len == c.size);
            if same_source && present {
                report.skipped += 1;
                continue;
            }
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            let start = Instant::now();
            match copy::copy_file(fs, guard, &src, &target, prev.is_some(), &opts) {
                Ok(c) => {
                    j.append(
                        fs,
                        Record::File {
                            rel: f.rel.clone(),
                            size: c.size,
                            mtime_ns: c.mtime_ns,
                            sha256: c.sha256.clone(),
                            ts: journal::now_rfc3339(),
                        },
                    )?;
                    log::info!("{}", commit_message(&j.id, &f.rel, &c, copy::secs(start)));
                    report.copied += 1;
                    break;
                }
                Err(CopyFail::Guard(e)) => {
                    log::error!("run {name}: stopping all writes: {e}");
                    return Err(e);
                }
                Err(CopyFail::Conflict(m)) => {
                    log::error!("run {name}: {shown}: {m}");
                    report.failed.push((f.rel.clone(), m));
                    break;
                }
                Err(CopyFail::Io(e)) if classify(&e) == ErrorClass::Outage => {
                    return Err(Error::RestartNeeded(format!(
                        "run {name}: the share failed while copying {shown} ({e}); \
                         committed files are kept and the run resumes where it stopped"
                    )));
                }
                Err(CopyFail::Io(e)) if classify(&e) == ErrorClass::NoSpace => {
                    return Err(Error::Failed(1)).inspect_err(|_| {
                        log::error!("run {name}: destination is full while copying {shown}: {e}");
                    });
                }
                Err(e) => {
                    if attempt >= cfg.transfer.max_attempts {
                        log::error!("run {name}: {shown}: failed after {attempt} attempt(s): {e}");
                        report.failed.push((f.rel.clone(), e.to_string()));
                        break;
                    }
                    log::warn!("run {name}: {shown}: attempt {attempt} failed ({e}); retrying");
                }
            }
        }
    }

    if !report.failed.is_empty() {
        log::error!(
            "run {name}: {} file(s) failed; manifest and COMPLETE not written",
            report.failed.len()
        );
        return Ok(report);
    }

    // Size check of every committed file, then manifest, run.json, COMPLETE.
    let out = finalise::prepare(cfg, fs, run, &dest, &j, &sel, &started, "run")?;
    finalise::write(fs, guard, &dest, &out)?;
    let manifest_sha = out.manifest_sha256;
    j.append(
        fs,
        Record::Complete {
            manifest_sha256: manifest_sha.clone(),
            ts: journal::now_rfc3339(),
        },
    )?;
    if let Err(e) = j.compact(fs) {
        log::warn!("run {name}: cannot compact the journal: {e}");
    }
    log::info!(
        "run {name}: COMPLETE, {} files ({} copied, {} already there), {} bytes in {:.1}s, manifest sha256={manifest_sha}",
        report.files,
        report.copied,
        report.skipped,
        report.bytes,
        copy::secs(t0)
    );
    report.manifest_sha256 = Some(manifest_sha);
    Ok(report)
}
