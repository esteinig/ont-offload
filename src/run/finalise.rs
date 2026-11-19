// SPDX-License-Identifier: AGPL-3.0-or-later
//! The end of a run: size check of every committed file, then
//! `manifest.sha256`, `run.json` and `COMPLETE`, in that order. Shared by
//! `run` and `watch`.

use std::fmt::Write as _;
use std::path::Path;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fsio::Fs;
use crate::guard::Guard;
use crate::journal::{
    self, COMPLETE, Journal, MANIFEST, RUN_JSON, manifest_line, sha256_hex, write_atomic,
};
use crate::logging::escape_control;

use super::{DetectedRun, Selected, safe_join};

/// Everything written at the end of a run, prepared from the journal.
#[derive(Debug, Clone)]
pub struct Outputs {
    /// `manifest.sha256` text.
    pub manifest: String,
    /// SHA-256 of the manifest.
    pub manifest_sha256: String,
    /// `run.json` text.
    pub run_json: String,
    /// Relative path and committed size of every file.
    pub sizes: Vec<(String, u64)>,
}

/// Prepare the outputs of a run whose selected files are all committed.
///
/// `mode` is `"run"` or `"watch"`, recorded in `run.json`.
///
/// Implements: SRS-VER-002
///
/// # Errors
/// [`Error::Integrity`] when a selected file is not in the journal.
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    cfg: &Config,
    fs: &dyn Fs,
    run: &DetectedRun,
    dest: &Path,
    j: &Journal,
    sel: &Selected,
    started: &str,
    mode: &str,
) -> Result<Outputs> {
    let mut manifest = String::new();
    let mut sizes = Vec::with_capacity(sel.files.len());
    for f in &sel.files {
        let c = j.files.get(&f.rel).ok_or_else(|| {
            Error::Integrity(format!("{} is not in the journal", escape_control(&f.rel)))
        })?;
        manifest.push_str(&manifest_line(&c.sha256, &f.rel));
        sizes.push((f.rel.clone(), c.size));
    }
    let manifest_sha256 = sha256_hex(manifest.as_bytes());
    let run_json = serde_json::json!({
        "tool": "ont-offload",
        "version": env!("CARGO_PKG_VERSION"),
        "mode": mode,
        "run_id": j.id,
        "run_name": run.name(),
        "source": run.path.to_string_lossy(),
        "destination": dest.to_string_lossy(),
        "dest_starts_at": cfg.sources.get(run.source).map_or("experiment", |s| s.dest_starts_at.as_str()),
        "source_name": cfg.sources.get(run.source).map(|s| s.name.as_str()),
        "target_id": cfg.target_id,
        "host": journal::host_name(fs),
        "run_marker": run.marker,
        "complete_marker": run.complete,
        "assumed_complete": run.complete.is_none(),
        "started": started,
        "finished": journal::now_rfc3339(),
        "files": sel.files.len(),
        "bytes": sel.bytes(),
        "verify": format!("{:?}", cfg.transfer.verify).to_lowercase(),
        "manifest_sha256": manifest_sha256,
        "excluded": sel.excluded,
        "symlinks_not_followed": sel.symlinks,
        "special_files": sel.special,
        "nested_runs": sel.nested_runs,
        "invalid_names": sel.invalid_names,
        "reserved_names": sel.reserved,
    });
    let run_json = serde_json::to_string_pretty(&run_json).unwrap_or_default() + "\n";
    Ok(Outputs {
        manifest,
        manifest_sha256,
        run_json,
        sizes,
    })
}

/// Check every committed file's size on the destination, then write
/// `manifest.sha256`, `run.json` and `COMPLETE` last, with the guard checked
/// before the first and the last write.
///
/// Implements: SRS-VER-002, SRS-GRD-002
///
/// # Errors
/// [`Error::Integrity`] for a size mismatch, [`Error::Guard`], I/O errors.
pub fn write(fs: &dyn Fs, guard: &Guard, dest: &Path, out: &Outputs) -> Result<()> {
    for (rel, size) in &out.sizes {
        let target = safe_join(dest, Path::new(rel)).map_err(Error::Config)?;
        let len = fs.metadata(&target)?.len;
        if len != *size {
            return Err(Error::Integrity(format!(
                "{} has {len} bytes on the destination, {size} were committed",
                escape_control(rel),
            )));
        }
    }
    guard.check(fs)?;
    write_atomic(fs, dest, MANIFEST, out.manifest.as_bytes())?;
    write_atomic(fs, dest, RUN_JSON, out.run_json.as_bytes())?;
    guard.check(fs)?;
    let mut complete = String::new();
    let _ = writeln!(complete, "manifest_sha256={}", out.manifest_sha256);
    write_atomic(fs, dest, COMPLETE, complete.as_bytes())?;
    Ok(())
}

/// Withdraw a completed run before it is offloaded again: `COMPLETE`
/// first, then `VERIFIED.json`, so that at no moment does the share say
/// "complete" or "verified" for a run that is about to change. The
/// manifest and `run.json` stay until they are rewritten.
///
/// Implements: SRS-LIVE-004
///
/// # Errors
/// [`Error::Guard`], I/O errors other than a missing file.
pub fn withdraw(fs: &dyn Fs, guard: &Guard, dest: &Path) -> Result<()> {
    guard.check(fs)?;
    for name in [COMPLETE, journal::VERIFIED] {
        match fs.remove_file(&dest.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    fs.sync_dir(dest)?;
    Ok(())
}
