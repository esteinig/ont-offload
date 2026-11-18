// SPDX-License-Identifier: AGPL-3.0-or-later
//! File selection inside one run folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Selection;
use crate::error::Result;
use crate::fsio::{DirEntry, FileKind, Fs};
use crate::journal::RESERVED_NAMES;
use crate::logging::escape_control;

use super::{Markers, globset};

/// One file to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedFile {
    /// Path relative to the run folder, with `/` separators.
    pub rel: String,
    /// Size when listed.
    pub size: u64,
    /// Modification time when listed.
    pub modified: Option<SystemTime>,
    /// Matches `defer_until_complete`.
    pub deferred: bool,
}

/// What [`select`] found in a run folder.
#[derive(Debug, Clone, Default)]
pub struct Selected {
    /// Files to copy, sorted by path.
    pub files: Vec<SelectedFile>,
    /// Files left out by `include` / `exclude`.
    pub excluded: Vec<String>,
    /// Symbolic links, never followed (escaped for display).
    pub symlinks: Vec<String>,
    /// Sockets, FIFOs, devices (escaped for display).
    pub special: Vec<String>,
    /// Sub-folders that are runs themselves, not copied.
    pub nested_runs: Vec<String>,
    /// Names that are not valid UTF-8 (escaped for display), not copied.
    pub invalid_names: Vec<String>,
    /// Top-level files whose names ont-offload writes itself, not copied.
    pub reserved: Vec<String>,
}

impl Selected {
    /// Total bytes of the selected files.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// Directory listings kept between scans of one run, so a rescan lists
/// only folders whose modification time changed and checks only files
/// that are not already committed as they are.
#[derive(Debug, Clone, Default)]
pub struct ScanCache {
    dirs: BTreeMap<PathBuf, (SystemTime, Vec<DirEntry>)>,
    /// Folder listings read (cumulative).
    pub listed: u64,
    /// File and folder status calls (cumulative).
    pub stats: u64,
}

impl ScanCache {
    /// Forget every listing: the next scan is a full one.
    pub fn clear(&mut self) {
        self.dirs.clear();
    }
}

/// A listing is reused only if the folder changed longer ago than this:
/// a change within the same timestamp tick as the listing would otherwise
/// go unnoticed.
const RACY: Duration = Duration::from_secs(2);

/// [`select`] using and updating `cache`. `known` returns the previous
/// entry of a file that is committed as it is; such a file in an unchanged
/// folder is not checked again. Warnings are logged only for folders that
/// were listed afresh.
///
/// Implements: SRS-RES-009, SRS-SEL-001
///
/// # Errors
/// As [`select`].
pub fn select_cached(
    fs: &dyn Fs,
    run: &Path,
    sel: &Selection,
    run_markers: &Markers,
    cache: &mut ScanCache,
    known: &dyn Fn(&str) -> Option<SelectedFile>,
) -> Result<Selected> {
    walk(fs, run, sel, run_markers, Some(cache), known)
}

/// List the files of a run to copy.
///
/// Walks the run folder without following symlinks, applies `include`,
/// `exclude` and `defer_until_complete` (globs relative to the run folder,
/// `*` does not cross `/`), and records everything left out with the
/// reason. Names with control characters are copied but warned about.
///
/// Implements: SRS-SEL-001, SRS-SEL-003, SRS-SEL-004, SRS-SEL-005
///
/// # Errors
/// I/O errors listing the run folder itself (errors below it are skipped with a warning).
pub fn select(fs: &dyn Fs, run: &Path, sel: &Selection, run_markers: &Markers) -> Result<Selected> {
    walk(fs, run, sel, run_markers, None, &|_| None)
}

#[allow(clippy::too_many_lines)]
fn walk(
    fs: &dyn Fs,
    run: &Path,
    sel: &Selection,
    run_markers: &Markers,
    mut cache: Option<&mut ScanCache>,
    known: &dyn Fn(&str) -> Option<SelectedFile>,
) -> Result<Selected> {
    let include = globset(&sel.include)?;
    let exclude = globset(&sel.exclude)?;
    let defer = globset(&sel.defer_until_complete)?;
    let mut out = Selected::default();
    let mut stack: Vec<(PathBuf, String)> = vec![(run.to_path_buf(), String::new())];
    let mut first = true;
    while let Some((dir, prefix)) = stack.pop() {
        let mut cached = false;
        let listing = match cache.as_deref_mut() {
            None => fs.read_dir(&dir),
            Some(c) => {
                c.stats += 1;
                let mtime = fs.symlink_metadata(&dir).ok().and_then(|m| m.modified);
                match (mtime, c.dirs.get(&dir)) {
                    (Some(m), Some((was, entries))) if *was == m => {
                        cached = true;
                        Ok(entries.clone())
                    }
                    _ => {
                        c.listed += 1;
                        let r = fs.read_dir(&dir);
                        match (&r, mtime) {
                            (Ok(entries), Some(m))
                                if SystemTime::now()
                                    .duration_since(m)
                                    .is_ok_and(|age| age >= RACY) =>
                            {
                                c.dirs.insert(dir.clone(), (m, entries.clone()));
                            }
                            _ => {
                                c.dirs.remove(&dir);
                            }
                        }
                        r
                    }
                }
            }
        };
        let quiet = cached;
        let entries = match listing {
            Ok(e) => e,
            Err(e) if first => return Err(e.into()),
            Err(e) => {
                log::warn!(
                    "cannot list {}: {e}; its files are not copied",
                    escape_control(&dir.to_string_lossy())
                );
                continue;
            }
        };
        if !first && run_markers.find(&entries).is_some() {
            out.nested_runs.push(escape_control(&prefix));
            if !quiet {
                log::warn!(
                    "{} inside run {} is a run itself; not copied",
                    escape_control(&prefix),
                    escape_control(&run.to_string_lossy())
                );
            }
            continue;
        }
        first = false;
        for e in entries {
            let Some(name) = e.name.to_str() else {
                let shown = format!("{prefix}{}", escape_control(&e.name.to_string_lossy()));
                if !quiet {
                    log::warn!("{shown}: name is not valid UTF-8; not copied");
                }
                out.invalid_names.push(shown);
                continue;
            };
            let rel = format!("{prefix}{name}");
            if !quiet && name.chars().any(char::is_control) {
                log::warn!(
                    "{}: file name contains control characters",
                    escape_control(&rel)
                );
            }
            match e.kind {
                FileKind::Dir => stack.push((dir.join(name), format!("{rel}/"))),
                FileKind::Symlink => {
                    if !quiet {
                        log::warn!("{}: symbolic link; not followed", escape_control(&rel));
                    }
                    out.symlinks.push(escape_control(&rel));
                }
                FileKind::Other => out.special.push(escape_control(&rel)),
                FileKind::File => {
                    if prefix.is_empty() && RESERVED_NAMES.contains(&name) {
                        if !quiet {
                            log::warn!(
                                "{}: name is reserved for ont-offload's own files; not copied",
                                escape_control(&rel)
                            );
                        }
                        out.reserved.push(rel);
                        continue;
                    }
                    let p = Path::new(&rel);
                    if !include.is_match(p) || exclude.is_match(p) {
                        out.excluded.push(rel);
                        continue;
                    }
                    if cached {
                        if let Some(prev) = known(&rel) {
                            out.files.push(SelectedFile {
                                deferred: defer.is_match(p),
                                ..prev
                            });
                            continue;
                        }
                    }
                    if let Some(c) = cache.as_deref_mut() {
                        c.stats += 1;
                    }
                    let st = match fs.symlink_metadata(&dir.join(name)) {
                        Ok(st) => st,
                        Err(err) => {
                            log::warn!("{}: {err}; skipped", escape_control(&rel));
                            continue;
                        }
                    };
                    out.files.push(SelectedFile {
                        deferred: defer.is_match(p),
                        rel,
                        size: st.len,
                        modified: st.modified,
                    });
                }
            }
        }
    }
    out.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    out.excluded.sort();
    Ok(out)
}

/// A digest of a selection: every selected file's path, size and
/// modification time. Two selections with the same fingerprint list the
/// same files as they were.
///
/// Implements: SRS-LIVE-004
#[must_use]
pub fn fingerprint(sel: &Selected) -> String {
    let mut text = String::new();
    for f in &sel.files {
        text.push_str(&format!(
            "{}\t{}\t{}\n",
            f.rel,
            f.size,
            crate::journal::nanos(f.modified)
        ));
    }
    crate::journal::sha256_hex(text.as_bytes())
}

/// Every folder of a run (not following symlinks) with its modification
/// time. A file added, removed or renamed changes the time of its folder,
/// so comparing these few values notices most changes to a completed run
/// without listing its files.
///
/// Implements: SRS-LIVE-004, SRS-RES-009
///
/// # Errors
/// I/O errors on the run folder itself.
pub fn dir_signature(fs: &dyn Fs, run: &Path) -> Result<Vec<(PathBuf, Option<SystemTime>)>> {
    let mut out = Vec::new();
    let mut stack = vec![run.to_path_buf()];
    let mut first = true;
    while let Some(dir) = stack.pop() {
        let m = match fs.symlink_metadata(&dir) {
            Ok(m) => m,
            Err(e) if first => return Err(e.into()),
            Err(_) => continue,
        };
        first = false;
        out.push((dir.clone(), m.modified));
        if let Ok(entries) = fs.read_dir(&dir) {
            for e in entries {
                if e.kind == FileKind::Dir {
                    stack.push(dir.join(&e.name));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Whether any folder in `sig` changed or went away (status calls only).
#[must_use]
pub fn signature_changed(fs: &dyn Fs, sig: &[(PathBuf, Option<SystemTime>)]) -> bool {
    sig.iter().any(|(dir, was)| {
        fs.symlink_metadata(dir)
            .map_or(true, |m| m.modified != *was)
    })
}
