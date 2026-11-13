// SPDX-License-Identifier: AGPL-3.0-or-later
//! One ont-offload process per state folder: two processes writing the same
//! journals and claims could commit the same file twice or let two runs
//! share a destination.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fsio::Fs;
use crate::logging::escape_control;

/// Lock file name in `state_dir`.
pub const LOCK: &str = "ont-offload.lock";

/// Held for the life of the process; removed on drop.
#[derive(Debug)]
pub struct InstanceLock {
    path: PathBuf,
}

fn alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        Path::new("/proc").join(pid.to_string()).exists()
    } else {
        true
    }
}

impl InstanceLock {
    /// Take the lock, or fail when another live process holds it. A lock
    /// left by a process that no longer exists is taken over.
    ///
    /// Implements: SRS-RES-007
    ///
    /// # Errors
    /// [`Error::Config`] when another process holds the lock; I/O errors.
    pub fn acquire(fs: &dyn Fs, state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(LOCK);
        let me = std::process::id();
        for _ in 0..2 {
            match fs.create_new(&path) {
                Ok(mut f) => {
                    use std::io::Write as _;
                    writeln!(f, "{me}")?;
                    f.sync_data()?;
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let text = fs.read(&path).unwrap_or_default();
                    let pid = String::from_utf8_lossy(&text).trim().parse::<u32>().ok();
                    match pid {
                        Some(p) if p != me && alive(p) => {
                            return Err(Error::Config(format!(
                                "another ont-offload (pid {p}) is using state_dir {}; \
                                 only one may run per state folder",
                                escape_control(&state_dir.to_string_lossy())
                            )));
                        }
                        _ => {
                            log::warn!(
                                "removing stale lock {} (pid {})",
                                escape_control(&path.to_string_lossy()),
                                pid.map_or_else(|| "unreadable".to_owned(), |p| p.to_string())
                            );
                            fs.remove_file(&path)?;
                        }
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Config(format!(
            "cannot take lock {}",
            escape_control(&path.to_string_lossy())
        )))
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
