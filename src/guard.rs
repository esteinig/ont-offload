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
    /// Mount options (per-mount and superblock), as in the mount table.
    pub options: String,
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
            let mut l = left.split(' ');
            let point = l.nth(4)?;
            let mount_opts = l.next().unwrap_or("");
            let mut r = right.split(' ');
            let fstype = r.next()?;
            let source = r.next().unwrap_or("");
            let super_opts = r.next().unwrap_or("");
            Some(Mount {
                point: PathBuf::from(unescape(point)),
                fstype: fstype.to_owned(),
                source: unescape(source),
                options: [mount_opts, super_opts]
                    .iter()
                    .filter(|o| !o.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" "),
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

    /// The target named by a readable sentinel that differs from ours: a
    /// share that is mounted but belongs to another target.
    ///
    /// Implements: SRS-RES-006
    #[must_use]
    pub fn wrong_target(&self, fs: &dyn Fs) -> Option<String> {
        let bytes = fs.read(&self.resolved.join(SENTINEL)).ok()?;
        let found = String::from_utf8_lossy(&bytes).trim().to_owned();
        (found != self.target_id).then_some(found)
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

/// The identity this process writes as: effective user and group id
/// (`None` where the platform has none).
#[must_use]
pub fn process_ids() -> Option<(u32, u32)> {
    #[cfg(unix)]
    {
        Some((
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        ))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// What a write probe on the share found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// User and group this process writes as.
    pub process: Option<(u32, u32)>,
    /// Owner user id of the probe file as the share reports it.
    pub uid: u32,
    /// Owner group id of the probe file.
    pub gid: u32,
    /// Permission bits of the probe file after `transfer.file_mode` was set.
    pub mode: u32,
    /// The mode that was asked for.
    pub wanted_mode: u32,
}

impl Probe {
    /// Warnings for the operator: running as root, files owned by another
    /// user than this process (`root_squash`, `all_squash`, a fixed CIFS uid),
    /// and a mode the share does not keep.
    ///
    /// Implements: SRS-GRD-004
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if let Some((uid, _)) = self.process {
            if uid == 0 {
                w.push(
                    "running as root: run the service as a dedicated user that owns the files \
                     on the share (see the deployment guide)"
                        .to_owned(),
                );
            }
            if uid != self.uid {
                w.push(format!(
                    "files written to the share are owned by uid {} gid {}, not by this process \
                     (uid {uid}): the server maps users (root_squash, all_squash or a fixed CIFS \
                     uid); check that the receiving side can read them",
                    self.uid, self.gid
                ));
            }
        }
        if self.mode != self.wanted_mode {
            w.push(format!(
                "the share keeps mode {:04o} instead of transfer.file_mode {:04o}; check who can \
                 read the copies",
                self.mode, self.wanted_mode
            ));
        }
        w
    }
}

/// Write, sync, inspect and remove a small file at the top of the share,
/// as this process: proves the service user can write there and shows who
/// will own the copies. Run only after [`Guard::check`] passed.
///
/// Implements: SRS-GRD-004
///
/// # Errors
/// [`Error::Guard`] when the file cannot be written, inspected or removed.
pub fn probe_write(dest: &Path, file_mode: u32, fs: &dyn Fs) -> Result<Probe> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let path = dest.join(format!(".ont-offload-probe-{}-{nanos}", std::process::id()));
    let who = process_ids().map_or_else(String::new, |(u, g)| format!(" as uid {u} gid {g}"));
    let fail = |what: &str, e: std::io::Error| {
        Error::Guard(format!(
            "cannot {what} {}{who}: {e}",
            escape_control(&path.to_string_lossy())
        ))
    };
    let mut f = fs.create_new(&path).map_err(|e| fail("create", e))?;
    let written = f
        .write_all(b"ont-offload write probe\n")
        .and_then(|()| f.sync_data());
    drop(f);
    let result = written
        .map_err(|e| fail("write", e))
        .and_then(|()| {
            fs.set_mode(&path, file_mode)
                .map_err(|e| fail("set the mode of", e))
        })
        .and_then(|()| fs.metadata(&path).map_err(|e| fail("inspect", e)));
    let removed = fs.remove_file(&path).map_err(|e| fail("remove", e));
    let st = result?;
    removed?;
    Ok(Probe {
        process: process_ids(),
        uid: st.uid,
        gid: st.gid,
        mode: st.mode & 0o7777,
        wanted_mode: file_mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsio::RealFs;
    use crate::fsio::faulty::{Fault, FaultyFs, Op, Rule};

    /// Verifies: SRS-GRD-004
    #[test]
    fn write_probe_reports_owner_and_mode_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = probe_write(dir.path(), 0o640, &RealFs).unwrap();
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "probe removed"
        );
        #[cfg(unix)]
        {
            assert_eq!(p.process.map(|(u, _)| u), Some(p.uid));
            assert_eq!(p.mode, 0o640);
            let root = p.uid == 0;
            assert_eq!(p.warnings().len(), usize::from(root), "{:?}", p.warnings());
        }
        // A share that refuses writes is an error naming the user.
        let fs = FaultyFs::new(std::sync::Arc::new(RealFs));
        fs.add(Rule::always(
            Op::CreateNew,
            "",
            Fault::Errno(crate::fsio::errno::EACCES),
        ));
        let e = probe_write(dir.path(), 0o640, &fs).unwrap_err();
        assert!(
            matches!(e, Error::Guard(ref m) if m.contains("cannot create")),
            "{e}"
        );
        // A failed write still removes the probe.
        let fs = FaultyFs::new(std::sync::Arc::new(RealFs));
        fs.add(Rule::always(
            Op::Write,
            "",
            Fault::Errno(crate::fsio::errno::ENOSPC),
        ));
        assert!(probe_write(dir.path(), 0o640, &fs).is_err());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "probe removed after a failure"
        );
    }

    /// Verifies: SRS-GRD-004
    #[test]
    fn squashed_owner_root_and_lost_mode_are_warned() {
        let base = Probe {
            process: Some((1001, 1001)),
            uid: 1001,
            gid: 1001,
            mode: 0o640,
            wanted_mode: 0o640,
        };
        assert!(base.warnings().is_empty());
        let squashed = Probe {
            process: Some((0, 0)),
            uid: 65534,
            gid: 65534,
            ..base.clone()
        };
        let w = squashed.warnings();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("running as root"));
        assert!(w[1].contains("uid 65534") && w[1].contains("root_squash"));
        let cifs = Probe {
            mode: 0o755,
            ..base
        };
        assert!(cifs.warnings()[0].contains("mode 0755 instead of transfer.file_mode 0640"));
    }

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
