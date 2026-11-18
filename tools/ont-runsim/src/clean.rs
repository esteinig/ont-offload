// SPDX-License-Identifier: AGPL-3.0-or-later
//! Remove what the simulator created, and nothing else.
//!
//! Only run folders listed in `<out>/.ont-runsim/runs.txt` are removed, only
//! when they are real folders inside `out` that carry the simulation label.
//! Empty experiment and sample folders are removed afterwards; anything else
//! someone put into `out` stays.

use std::path::{Component, Path, PathBuf};

use crate::status::{self, Status};
use crate::{Error, LABEL_FILE, Result, STATE_DIR, io_err};

/// What [`clean`] did.
#[derive(Debug, Clone, Default)]
pub struct Cleaned {
    /// Run folders removed.
    pub removed: Vec<PathBuf>,
    /// Listed run folders left in place, with the reason.
    pub skipped: Vec<(PathBuf, String)>,
    /// Whether the output folder itself was removed (it was empty).
    pub removed_out: bool,
}

fn safe_rel(rel: &Path) -> bool {
    !rel.as_os_str().is_empty() && rel.components().all(|c| matches!(c, Component::Normal(_)))
}

/// Remove the simulated runs below `out`.
///
/// # Errors
/// [`Error::Refused`] when `out` was not created by ont-runsim or a
/// simulation is still running there; I/O errors.
pub fn clean(out: &Path) -> Result<Cleaned> {
    let state = status::state_dir(out);
    if !std::fs::symlink_metadata(&state).is_ok_and(|m| m.is_dir()) {
        return Err(Error::Refused(format!(
            "{} has no {STATE_DIR} folder; it was not created by ont-runsim, nothing removed",
            out.display()
        )));
    }
    if let Ok(s) = Status::read(out) {
        if s.is_live() {
            return Err(Error::Refused(format!(
                "a simulation is running in {} (pid {})",
                out.display(),
                s.pid
            )));
        }
    }
    let list = status::runs_list(out);
    let text = match std::fs::read_to_string(&list) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_err("read", &list)(e)),
    };
    let mut done = Cleaned::default();
    let mut parents = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let rel = PathBuf::from(line.trim());
        let dir = out.join(&rel);
        let reason = if !safe_rel(&rel) {
            Some("not a plain relative path")
        } else if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            Some("missing or not a folder")
        } else if !dir.join(LABEL_FILE).is_file() {
            Some("no simulation label file")
        } else {
            None
        };
        if let Some(r) = reason {
            if r != "missing or not a folder" || dir.exists() {
                done.skipped.push((dir, r.to_owned()));
            }
            continue;
        }
        std::fs::remove_dir_all(&dir).map_err(io_err("remove", &dir))?;
        let mut p = rel.parent().map(Path::to_path_buf);
        while let Some(parent) = p.filter(|x| !x.as_os_str().is_empty()) {
            parents.push(out.join(&parent));
            p = parent.parent().map(Path::to_path_buf);
        }
        done.removed.push(dir);
    }
    // Deepest first; remove_dir only succeeds on empty folders.
    parents.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    parents.dedup();
    for p in parents {
        let _ = std::fs::remove_dir(&p);
    }
    if done.skipped.is_empty() {
        std::fs::remove_dir_all(&state).map_err(io_err("remove", &state))?;
        done.removed_out = std::fs::remove_dir(out).is_ok();
    }
    Ok(done)
}
