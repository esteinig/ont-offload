// SPDX-License-Identifier: AGPL-3.0-or-later
//! File selection inside one run folder.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::Selection;
use crate::error::Result;
use crate::fsio::{FileKind, Fs};
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
    let include = globset(&sel.include)?;
    let exclude = globset(&sel.exclude)?;
    let defer = globset(&sel.defer_until_complete)?;
    let mut out = Selected::default();
    let mut stack: Vec<(PathBuf, String)> = vec![(run.to_path_buf(), String::new())];
    let mut first = true;
    while let Some((dir, prefix)) = stack.pop() {
        let entries = match fs.read_dir(&dir) {
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
            log::warn!(
                "{} inside run {} is a run itself; not copied",
                escape_control(&prefix),
                escape_control(&run.to_string_lossy())
            );
            continue;
        }
        first = false;
        for e in entries {
            let Some(name) = e.name.to_str() else {
                let shown = format!("{prefix}{}", escape_control(&e.name.to_string_lossy()));
                log::warn!("{shown}: name is not valid UTF-8; not copied");
                out.invalid_names.push(shown);
                continue;
            };
            let rel = format!("{prefix}{name}");
            if name.chars().any(char::is_control) {
                log::warn!(
                    "{}: file name contains control characters",
                    escape_control(&rel)
                );
            }
            match e.kind {
                FileKind::Dir => stack.push((dir.join(name), format!("{rel}/"))),
                FileKind::Symlink => {
                    log::warn!("{}: symbolic link; not followed", escape_control(&rel));
                    out.symlinks.push(escape_control(&rel));
                }
                FileKind::Other => out.special.push(escape_control(&rel)),
                FileKind::File => {
                    if prefix.is_empty() && RESERVED_NAMES.contains(&name) {
                        log::warn!(
                            "{}: name is reserved for ont-offload's own files; not copied",
                            escape_control(&rel)
                        );
                        out.reserved.push(rel);
                        continue;
                    }
                    let p = Path::new(&rel);
                    if !include.is_match(p) || exclude.is_match(p) {
                        out.excluded.push(rel);
                        continue;
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
