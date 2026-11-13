// SPDX-License-Identifier: AGPL-3.0-or-later
//! Run discovery, destination mapping, file selection and the offload of one
//! run (SWI-RUN).
//!
//! - [`detect`] finds runs below `source.root` by layout and run markers.
//! - [`dest_rel`] maps a run to its folder below `dest_root` by `dest_starts_at`.
//! - [`select`] lists the files of a run to copy, without following symlinks.
//! - [`offload`] copies one completed run and writes its manifest.

pub mod finalise;
pub mod lifecycle;
pub mod offload;
pub mod select;

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::config::{Config, DestLevel, Layout};
use crate::error::{Error, Result};
use crate::fsio::{DirEntry, FileKind, Fs};
use crate::logging::escape_control;

pub use offload::{OffloadReport, offload};
pub use select::{Selected, SelectedFile, select};

/// Build a glob set with `/`-aware matching (`*` does not cross folders).
///
/// # Errors
/// [`Error::Config`] for an invalid pattern (already rejected by config validation).
pub fn globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let g = GlobBuilder::new(p.trim_end_matches('/'))
            .literal_separator(true)
            .build()
            .map_err(|e| Error::Config(format!("pattern {p:?}: {e}")))?;
        b.add(g);
    }
    b.build().map_err(|e| Error::Config(e.to_string()))
}

/// Matches file names against marker patterns; a pattern ending in `/` matches folders only.
#[derive(Debug, Clone)]
pub struct Markers {
    files: GlobSet,
    dirs: GlobSet,
    patterns: Vec<String>,
}

impl Markers {
    /// Compile marker patterns.
    ///
    /// # Errors
    /// [`Error::Config`] for an invalid pattern.
    pub fn new(patterns: &[String]) -> Result<Self> {
        let (dirs, files): (Vec<String>, Vec<String>) =
            patterns.iter().cloned().partition(|p| p.ends_with('/'));
        Ok(Self {
            files: globset(&files)?,
            dirs: globset(&dirs)?,
            patterns: patterns.to_vec(),
        })
    }

    /// The first entry of a folder listing that matches, as written on disk.
    #[must_use]
    pub fn find<'a>(&self, entries: &'a [DirEntry]) -> Option<&'a DirEntry> {
        entries.iter().find(|e| self.matches(e))
    }

    /// Whether one entry matches.
    #[must_use]
    pub fn matches(&self, e: &DirEntry) -> bool {
        let name = Path::new(&e.name);
        match e.kind {
            FileKind::File => self.files.is_match(name),
            FileKind::Dir => self.dirs.is_match(name),
            _ => false,
        }
    }

    /// The patterns.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

/// A run found below `source.root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedRun {
    /// Run folder (real path below the resolved source root).
    pub path: PathBuf,
    /// Run folder relative to the source root.
    pub rel: PathBuf,
    /// Name of the entry that matched a run marker.
    pub marker: String,
    /// Name of the entry that matched a complete marker, if any.
    pub complete: Option<String>,
}

impl DetectedRun {
    /// Folder name of the run.
    #[must_use]
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }
}

/// Result of [`detect`].
#[derive(Debug, Clone, Default)]
pub struct Detection {
    /// Runs, sorted by relative path.
    pub runs: Vec<DetectedRun>,
    /// Folders skipped and why (already escaped for display).
    pub warnings: Vec<String>,
}

/// Inspect one folder: is it a run, and is it complete?
///
/// # Errors
/// I/O errors listing the folder.
pub fn inspect(
    fs: &dyn Fs,
    root: &Path,
    dir: &Path,
    run_markers: &Markers,
    complete_markers: &Markers,
) -> Result<Option<DetectedRun>> {
    let entries = fs.read_dir(dir)?;
    let Some(marker) = run_markers.find(&entries) else {
        return Ok(None);
    };
    let rel = dir.strip_prefix(root).unwrap_or(dir).to_path_buf();
    Ok(Some(DetectedRun {
        path: dir.to_path_buf(),
        rel,
        marker: marker.name.to_string_lossy().into_owned(),
        complete: complete_markers
            .find(&entries)
            .map(|e| e.name.to_string_lossy().into_owned()),
    }))
}

/// Find runs below the resolved `source.root` by layout and run markers.
///
/// Symlinked folders are never followed, ignored patterns are skipped, and
/// in `any-depth` layout a run inside another run is not reported.
///
/// Implements: SRS-SEL-006
///
/// # Errors
/// I/O errors listing the source root.
pub fn detect(cfg: &Config, fs: &dyn Fs) -> Result<Detection> {
    let root = &cfg.source.root.resolved;
    let ignore = globset(&cfg.source.ignore)?;
    let runs_m = Markers::new(&cfg.source.run_markers)?;
    let done_m = Markers::new(&cfg.source.complete_markers)?;
    let mut out = Detection::default();
    let max_depth = match cfg.source.layout {
        Layout::Minknow => 3,
        Layout::AnyDepth => 12,
    };
    // (folder, depth below root)
    let mut stack = vec![(root.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 0 {
            let rel = dir.strip_prefix(root).unwrap_or(&dir);
            if ignore.is_match(rel) {
                continue;
            }
            let check = cfg.source.layout == Layout::AnyDepth || depth == 3;
            if check {
                match inspect(fs, root, &dir, &runs_m, &done_m) {
                    Ok(Some(run)) => {
                        out.runs.push(run);
                        continue; // never descend into a run
                    }
                    Ok(None) => {}
                    Err(e) => {
                        out.warnings.push(format!(
                            "cannot read {}: {e}",
                            escape_control(&dir.to_string_lossy())
                        ));
                        continue;
                    }
                }
            }
        }
        if depth >= max_depth {
            continue;
        }
        let entries = match fs.read_dir(&dir) {
            Ok(e) => e,
            Err(e) if depth == 0 => return Err(e.into()),
            Err(e) => {
                out.warnings.push(format!(
                    "cannot read {}: {e}",
                    escape_control(&dir.to_string_lossy())
                ));
                continue;
            }
        };
        for e in entries.into_iter().rev() {
            let child = dir.join(&e.name);
            match e.kind {
                FileKind::Dir => stack.push((child, depth + 1)),
                FileKind::Symlink => {
                    let rel = child.strip_prefix(root).unwrap_or(&child);
                    if !ignore.is_match(rel) && is_dir_link(fs, &child) {
                        out.warnings.push(format!(
                            "{} is a symlink to a folder; not followed",
                            escape_control(&rel.to_string_lossy())
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    out.runs.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

fn is_dir_link(fs: &dyn Fs, p: &Path) -> bool {
    fs.metadata(p).is_ok_and(|m| m.kind == FileKind::Dir)
}

/// Folder of a run below `dest_root`, from its path below `source.root` and
/// the `dest_starts_at` level.
///
/// `run` keeps the run folder only, `sample` adds its parent, `experiment`
/// keeps the whole path below the source root.
///
/// Implements: SRS-SEL-007, SRS-SEL-002
///
/// # Errors
/// A message when the path has too few folders for the level or contains a
/// component other than a plain name.
pub fn dest_rel(run_rel: &Path, level: DestLevel) -> std::result::Result<PathBuf, String> {
    let parts: Vec<&OsStr> = run_rel
        .components()
        .map(|c| match c {
            Component::Normal(n) => Ok(n),
            other => Err(format!(
                "{} contains {:?}, which is not a plain folder name",
                escape_control(&run_rel.to_string_lossy()),
                other
            )),
        })
        .collect::<std::result::Result<_, _>>()?;
    let keep = match level {
        DestLevel::Experiment => parts.len(),
        DestLevel::Sample => 2,
        DestLevel::Run => 1,
    };
    if parts.is_empty() || parts.len() < keep {
        return Err(format!(
            "{} has too few parent folders for dest_starts_at = \"{}\"",
            escape_control(&run_rel.to_string_lossy()),
            level.as_str()
        ));
    }
    Ok(parts[parts.len() - keep..].iter().collect())
}

/// Join relative parts below a base, refusing anything but plain names.
///
/// Implements: SRS-SEL-002
///
/// # Errors
/// A message naming the offending path.
pub fn safe_join(base: &Path, rel: &Path) -> std::result::Result<PathBuf, String> {
    if rel.as_os_str().is_empty() {
        return Err("empty relative path".into());
    }
    let mut out = base.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(n) => out.push(n),
            _ => {
                return Err(format!(
                    "{} is not a plain relative path",
                    escape_control(&rel.to_string_lossy())
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
