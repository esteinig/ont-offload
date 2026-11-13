// SPDX-License-Identifier: AGPL-3.0-or-later
//! Destination guard (SWI-GUARD).
//!
//! Nothing is written below `dest_root` unless all of these hold:
//!
//! 1. `dest_root` still resolves to the real path recorded at start-up;
//! 2. the sentinel file `dest_root/.ont-offload-target` contains `target_id`;
//! 3. the mount that holds `dest_root` (longest prefix in
//!    `/proc/self/mountinfo`) has an allowed filesystem type;
//! 4. `dest_root` is on a different device than `source.root`.
//!
//! [`Guard::check`] runs all four (before each commit); [`Guard::quick`]
//! re-reads the sentinel (between chunks).

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fsio::Fs;
use crate::logging::escape_control;

/// Name of the sentinel file at the top of `dest_root`.
pub const SENTINEL: &str = ".ont-offload-target";

/// Default mount table.
pub const MOUNTINFO: &str = "/proc/self/mountinfo";

/// The mount that holds the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Mount point.
    pub point: PathBuf,
    /// Filesystem type.
    pub fstype: String,
    /// Mount source (server and export).
    pub source: String,
}

/// Destination guard for one configuration.
#[derive(Debug, Clone)]
pub struct Guard {
    configured: PathBuf,
    resolved: PathBuf,
    source_dev: u64,
    target_id: String,
    allowed: Vec<String>,
    mountinfo: PathBuf,
}

/// Undo the octal escapes of mountinfo fields (`\040` for a space).
fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let digits = &b[i + 1..i + 4];
            if digits.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let text = std::str::from_utf8(digits).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(text, 8) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse mountinfo text into mounts.
#[must_use]
pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let point = left.split(' ').nth(4)?;
            let mut r = right.split(' ');
            let fstype = r.next()?;
            let source = r.next().unwrap_or("");
            Some(Mount {
                point: PathBuf::from(unescape(point)),
                fstype: fstype.to_owned(),
                source: unescape(source),
            })
        })
        .collect()
}

/// The mount with the longest mount point that contains `path`.
#[must_use]
pub fn mount_of<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|m| path.starts_with(&m.point))
        .max_by_key(|m| m.point.components().count())
}

impl Guard {
    /// Guard for a validated configuration, reading `/proc/self/mountinfo`.
    #[must_use]
    pub fn new(cfg: &Config) -> Self {
        Self {
            configured: cfg.dest_root.configured.clone(),
            resolved: cfg.dest_root.resolved.clone(),
            source_dev: cfg.source.root.dev,
            target_id: cfg.target_id.clone(),
            allowed: cfg.allowed_fstypes.clone(),
            mountinfo: PathBuf::from(MOUNTINFO),
        }
    }

    /// Read the mount table from another file (tests).
    #[must_use]
    pub fn with_mountinfo(mut self, path: &Path) -> Self {
        self.mountinfo = path.to_path_buf();
        self
    }

    /// Real path of `dest_root` recorded at start-up.
    #[must_use]
    pub fn dest(&self) -> &Path {
        &self.resolved
    }

    fn refuse(&self, why: impl std::fmt::Display) -> Error {
        Error::Guard(format!(
            "destination {} refused: {why}",
            escape_control(&self.configured.to_string_lossy())
        ))
    }

    /// Check that the sentinel names this target.
    ///
    /// Implements: SRS-GRD-001, SRS-GRD-002
    ///
    /// # Errors
    /// [`Error::Guard`] when the sentinel is missing, unreadable or names another target.
    pub fn quick(&self, fs: &dyn Fs) -> Result<()> {
        let path = self.resolved.join(SENTINEL);
        let bytes = fs.read(&path).map_err(|e| {
            self.refuse(format!(
                "sentinel {SENTINEL} cannot be read ({e}); is the share mounted?"
            ))
        })?;
        let found = String::from_utf8_lossy(&bytes);
        let found = found.trim();
        if found != self.target_id {
            return Err(self.refuse(format!(
                "sentinel names target {:?}, expected {:?}",
                escape_control(found),
                self.target_id
            )));
        }
        Ok(())
    }

    /// Run every guard check.
    ///
    /// Implements: SRS-GRD-001, SRS-GRD-002, SRS-GRD-003
    ///
    /// # Errors
    /// [`Error::Guard`] naming the first failed check.
    pub fn check(&self, fs: &dyn Fs) -> Result<Mount> {
        let now = fs
            .canonicalize(&self.configured)
            .map_err(|e| self.refuse(format!("cannot be resolved ({e})")))?;
        if now != self.resolved {
            return Err(self.refuse(format!(
                "now resolves to {} instead of {} recorded at start-up",
                escape_control(&now.to_string_lossy()),
                escape_control(&self.resolved.to_string_lossy())
            )));
        }
        self.quick(fs)?;
        let text = fs.read(&self.mountinfo).map_err(|e| {
            self.refuse(format!(
                "mount table {} cannot be read ({e})",
                self.mountinfo.display()
            ))
        })?;
        let mounts = parse_mountinfo(&String::from_utf8_lossy(&text));
        let mount = mount_of(&mounts, &self.resolved)
            .cloned()
            .ok_or_else(|| self.refuse("no mount found for it"))?;
        if !self.allowed.iter().any(|a| *a == mount.fstype) {
            return Err(self.refuse(format!(
                "it is on a {} filesystem mounted at {}; allowed: {}",
                mount.fstype,
                escape_control(&mount.point.to_string_lossy()),
                self.allowed.join(", ")
            )));
        }
        let dev = fs
            .metadata(&self.resolved)
            .map_err(|e| self.refuse(format!("cannot be read ({e})")))?
            .dev;
        if dev == self.source_dev {
            return Err(
                self.refuse("it is on the same device as source.root; is the share mounted?")
            );
        }
        Ok(mount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 0:50 / /mnt/nas rw,relatime shared:20 - nfs4 nas01:/export/ont rw,vers=4.2
41 22 0:51 / /mnt/my\\040share rw - cifs //srv/share rw
";

    /// Verifies: SRS-GRD-001
    #[test]
    fn mountinfo_longest_prefix_and_escapes() {
        let m = parse_mountinfo(SAMPLE);
        assert_eq!(m.len(), 3);
        assert_eq!(
            mount_of(&m, Path::new("/mnt/nas/ont/run")).unwrap().fstype,
            "nfs4"
        );
        assert_eq!(
            mount_of(&m, Path::new("/mnt/nasty")).unwrap().fstype,
            "ext4"
        );
        let s = mount_of(&m, Path::new("/mnt/my share/x")).unwrap();
        assert_eq!(
            (s.fstype.as_str(), s.source.as_str()),
            ("cifs", "//srv/share")
        );
    }
}
