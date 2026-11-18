// SPDX-License-Identifier: AGPL-3.0-or-later
//! `verify`: re-hash a destination run against its manifest and, only when
//! everything matches, write the `VERIFIED.json` receipt.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::copy::{hash_file, is_temp_name};
use crate::error::{Error, Result};
use crate::fsio::{FileKind, Fs};
use crate::logging::escape_control;

use super::{COMPLETE, MANIFEST, RESERVED_NAMES, RUN_JSON, VERIFIED, parse_manifest, sha256_hex};

/// Outcome of [`verify`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct VerifyReport {
    /// Destination run folder.
    pub run: String,
    /// Files in the manifest.
    pub files: usize,
    /// Bytes hashed.
    pub bytes: u64,
    /// SHA-256 of the manifest.
    pub manifest_sha256: String,
    /// Missing at the destination.
    pub missing: Vec<String>,
    /// Different content at the destination.
    pub changed: Vec<String>,
    /// At the destination but not in the manifest.
    pub extra: Vec<String>,
    /// Source copy checked too (`--source`), with its differences.
    pub source_checked: bool,
    /// Source files missing or different.
    pub source_mismatch: Vec<String>,
    /// `COMPLETE` was missing.
    pub incomplete: bool,
}

impl VerifyReport {
    /// Everything matches.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
            && self.changed.is_empty()
            && self.extra.is_empty()
            && self.source_mismatch.is_empty()
            && !self.incomplete
    }
}

fn walk(fs: &dyn Fs, root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((d, prefix)) = stack.pop() {
        for e in fs.read_dir(&d)? {
            let name = e.name.to_string_lossy().into_owned();
            let rel = format!("{prefix}{name}");
            if e.kind == FileKind::Dir {
                stack.push((d.join(&e.name), format!("{rel}/")));
            } else if !((prefix.is_empty() && RESERVED_NAMES.contains(&name.as_str()))
                || is_temp_name(&name))
            {
                out.push(rel);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Re-hash a destination run against its manifest; with `check_source`,
/// also re-hash the source run recorded in `run.json`. Writes
/// `VERIFIED.json` only when every check passes.
///
/// Implements: SRS-VER-003, SRS-VER-004
///
/// # Errors
/// [`Error::Integrity`] on any difference (the report is logged first);
/// I/O errors.
pub fn verify(fs: &dyn Fs, run: &Path, check_source: bool) -> Result<VerifyReport> {
    let shown = escape_control(&run.to_string_lossy());
    let manifest_bytes = fs
        .read(&run.join(MANIFEST))
        .map_err(|e| Error::Integrity(format!("{shown}: cannot read {MANIFEST}: {e}")))?;
    let manifest = parse_manifest(&String::from_utf8_lossy(&manifest_bytes))?;
    let mut rep = VerifyReport {
        run: run.to_string_lossy().into_owned(),
        files: manifest.len(),
        manifest_sha256: sha256_hex(&manifest_bytes),
        incomplete: fs.symlink_metadata(&run.join(COMPLETE)).is_err(),
        ..VerifyReport::default()
    };
    for (rel, want) in &manifest {
        let p = super::super::run::safe_join(run, Path::new(rel)).map_err(Error::Integrity)?;
        match hash_file(fs, &p, 8 << 20) {
            Ok((len, got)) => {
                rep.bytes += len;
                if got != *want {
                    rep.changed.push(rel.clone());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => rep.missing.push(rel.clone()),
            Err(e) => return Err(e.into()),
        }
    }
    rep.extra = walk(fs, run)?
        .into_iter()
        .filter(|r| !manifest.contains_key(r))
        .collect();

    if check_source {
        rep.source_checked = true;
        let run_json: serde_json::Value = serde_json::from_slice(
            &fs.read(&run.join(RUN_JSON))
                .map_err(|e| Error::Integrity(format!("{shown}: cannot read {RUN_JSON}: {e}")))?,
        )
        .map_err(|e| Error::Integrity(format!("{shown}: {RUN_JSON}: {e}")))?;
        let src = run_json
            .get("source")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Integrity(format!("{shown}: {RUN_JSON} names no source")))?;
        let src = PathBuf::from(src);
        for (rel, want) in &manifest {
            let p = super::super::run::safe_join(&src, Path::new(rel)).map_err(Error::Integrity)?;
            match hash_file(fs, &p, 8 << 20) {
                Ok((_, got)) if got == *want => {}
                Ok(_) | Err(_) => rep.source_mismatch.push(rel.clone()),
            }
        }
    }

    if !rep.ok() {
        for (label, list) in [
            ("missing", &rep.missing),
            ("changed", &rep.changed),
            ("extra", &rep.extra),
            ("source differs", &rep.source_mismatch),
        ] {
            for f in list {
                log::error!("verify {shown}: {label}: {}", escape_control(f));
            }
        }
        if rep.incomplete {
            log::error!("verify {shown}: {COMPLETE} is missing; the run was not finished");
        }
        return Err(Error::Integrity(format!(
            "{shown}: {} missing, {} changed, {} extra{}{}",
            rep.missing.len(),
            rep.changed.len(),
            rep.extra.len(),
            if rep.source_checked {
                format!(", {} differ at the source", rep.source_mismatch.len())
            } else {
                String::new()
            },
            if rep.incomplete { ", not complete" } else { "" }
        )));
    }

    let receipt = serde_json::json!({
        "result": "ok",
        "tool": "ont-offload",
        "version": env!("CARGO_PKG_VERSION"),
        "verified_at": super::now_rfc3339(),
        "host": super::host_name(fs),
        "run": rep.run,
        "manifest_sha256": rep.manifest_sha256,
        "files": rep.files,
        "bytes": rep.bytes,
        "source_checked": rep.source_checked,
    });
    let text = serde_json::to_string_pretty(&receipt).unwrap_or_default() + "\n";
    super::write_atomic(fs, run, VERIFIED, text.as_bytes())?;
    log::info!(
        "verify {shown}: OK, {} files, {} bytes; {VERIFIED} written",
        rep.files,
        rep.bytes
    );
    Ok(rep)
}
