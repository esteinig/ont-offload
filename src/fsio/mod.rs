// SPDX-License-Identifier: AGPL-3.0-or-later
//! Filesystem abstraction (SWI-FSIO).
//!
//! Every filesystem call that ont-offload makes goes through the [`Fs`] trait.
//! [`RealFs`] is the production implementation; it adds nothing and hides
//! nothing, so the operating system error codes (`ENOTCONN`, `ETIMEDOUT`,
//! `ESTALE`, `EIO`, `ENOSPC`, ...) reach the caller unchanged and can be
//! classified with [`classify`]. `FaultyFs` (unit tests and the
//! `fault-injection` feature only) wraps another `Fs` and injects faults.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(any(test, feature = "fault-injection"))]
pub mod faulty;

/// What kind of filesystem object a path refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link (only reported by [`Fs::symlink_metadata`]).
    Symlink,
    /// Anything else: socket, FIFO, device.
    Other,
}

/// The subset of file metadata ont-offload uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    /// Size in bytes.
    pub len: u64,
    /// Modification time, if the platform reports one.
    pub modified: Option<SystemTime>,
    /// Object kind.
    pub kind: FileKind,
    /// Device id (0 where the platform has none).
    pub dev: u64,
    /// Inode number (0 where the platform has none).
    pub ino: u64,
}

impl FileStat {
    fn from_metadata(m: &fs::Metadata) -> Self {
        let ft = m.file_type();
        let kind = if ft.is_symlink() {
            FileKind::Symlink
        } else if ft.is_dir() {
            FileKind::Dir
        } else if ft.is_file() {
            FileKind::File
        } else {
            FileKind::Other
        };
        let (dev, ino) = dev_ino(m);
        Self {
            len: m.len(),
            modified: m.modified().ok(),
            kind,
            dev,
            ino,
        }
    }
}

#[cfg(unix)]
fn dev_ino(m: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}

#[cfg(not(unix))]
fn dev_ino(_m: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// File name as stored on disk (may not be valid UTF-8).
    pub name: OsString,
    /// Object kind, without following symlinks.
    pub kind: FileKind,
}

/// A file opened for writing through [`Fs::create_new`].
pub trait FsFile: Write + Send {
    /// Flush file data to stable storage (`fdatasync`).
    ///
    /// # Errors
    /// Returns the operating system error.
    fn sync_data(&mut self) -> io::Result<()>;

    /// Set the file's modification time.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn set_modified(&mut self, time: SystemTime) -> io::Result<()>;
}

impl FsFile for fs::File {
    fn sync_data(&mut self) -> io::Result<()> {
        fs::File::sync_data(self)
    }

    fn set_modified(&mut self, time: SystemTime) -> io::Result<()> {
        fs::File::set_modified(self, time)
    }
}

/// Every filesystem operation ont-offload performs.
///
/// Implementations must return the operating system's `io::Error` unchanged
/// (same kind and raw OS error), so that outages can be told apart from
/// other failures.
///
/// Implements: SRS-FSIO-001
pub trait Fs: Send + Sync {
    /// Metadata, following symlinks.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn metadata(&self, path: &Path) -> io::Result<FileStat>;

    /// Metadata of the path itself, not following a final symlink.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn symlink_metadata(&self, path: &Path) -> io::Result<FileStat>;

    /// Absolute path with every symlink resolved.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;

    /// Read a whole (small) file.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// Open a file for streaming reads.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;

    /// Create a new file for writing; fails if it exists.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn FsFile>>;

    /// Rename a file within one filesystem.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Remove a file.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Create a directory and its missing parents.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// List a directory, sorted by name.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;

    /// Flush a directory's entries to stable storage (after a rename).
    ///
    /// # Errors
    /// Returns the operating system error.
    fn sync_dir(&self, path: &Path) -> io::Result<()>;

    /// Append bytes to a file (created if missing) and flush them to stable storage.
    ///
    /// # Errors
    /// Returns the operating system error.
    fn append_sync(&self, path: &Path, data: &[u8]) -> io::Result<()>;

    /// Set permission bits (Unix; no-op elsewhere).
    ///
    /// # Errors
    /// Returns the operating system error.
    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()>;

    /// Bytes available to this process on the filesystem holding `path`
    /// (`u64::MAX` where it cannot be known; the default).
    ///
    /// # Errors
    /// The OS error.
    fn free_space(&self, _path: &Path) -> io::Result<u64> {
        Ok(u64::MAX)
    }
}

/// The production filesystem: plain `std::fs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl Fs for RealFs {
    fn metadata(&self, path: &Path) -> io::Result<FileStat> {
        fs::metadata(path).map(|m| FileStat::from_metadata(&m))
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileStat> {
        fs::symlink_metadata(path).map(|m| FileStat::from_metadata(&m))
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(fs::File::open(path)?))
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn FsFile>> {
        let f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Box::new(f))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let kind = if ft.is_symlink() {
                FileKind::Symlink
            } else if ft.is_dir() {
                FileKind::Dir
            } else if ft.is_file() {
                FileKind::File
            } else {
                FileKind::Other
            };
            out.push(DirEntry {
                name: entry.file_name(),
                kind,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    #[cfg(unix)]
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        fs::File::open(path)?.sync_all()
    }

    #[cfg(not(unix))]
    fn sync_dir(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    fn append_sync(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        f.write_all(data)?;
        f.sync_data()
    }

    #[cfg(unix)]
    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    #[cfg(not(unix))]
    fn set_mode(&self, _path: &Path, _mode: u32) -> io::Result<()> {
        Ok(())
    }

    #[cfg(unix)]
    fn free_space(&self, path: &Path) -> io::Result<u64> {
        let st = rustix::fs::statvfs(path).map_err(io::Error::from)?;
        Ok(st.f_bavail.saturating_mul(st.f_frsize))
    }

    #[cfg(not(unix))]
    fn free_space(&self, _path: &Path) -> io::Result<u64> {
        Ok(u64::MAX)
    }
}

/// Raw OS error numbers ont-offload treats specially (Linux values).
pub mod errno {
    /// I/O error.
    pub const EIO: i32 = 5;
    /// No such file or directory.
    pub const ENOENT: i32 = 2;
    /// Permission denied.
    pub const EACCES: i32 = 13;
    /// No space left on device.
    pub const ENOSPC: i32 = 28;
    /// Disk quota exceeded.
    pub const EDQUOT: i32 = 122;
    /// Transport endpoint is not connected.
    pub const ENOTCONN: i32 = 107;
    /// Connection timed out.
    pub const ETIMEDOUT: i32 = 110;
    /// Host is down.
    pub const EHOSTDOWN: i32 = 112;
    /// No route to host.
    pub const EHOSTUNREACH: i32 = 113;
    /// Stale file handle (NFS).
    pub const ESTALE: i32 = 116;
    /// Network is unreachable.
    pub const ENETUNREACH: i32 = 101;
    /// Connection reset by peer.
    pub const ECONNRESET: i32 = 104;
    /// Connection aborted.
    pub const ECONNABORTED: i32 = 103;
}

/// How the transfer engine should react to an I/O error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// The mount or network is unavailable: pause all workers and wait.
    Outage,
    /// The destination is full: pause and alert.
    NoSpace,
    /// The path does not exist (any more).
    NotFound,
    /// Permission denied: needs an operator.
    Permission,
    /// Anything else: retry the file a bounded number of times.
    Other,
}

/// Classify an I/O error by its OS error code (or kind when there is none).
///
/// Implements: SRS-FSIO-001
#[must_use]
pub fn classify(err: &io::Error) -> ErrorClass {
    if let Some(code) = err.raw_os_error() {
        return match code {
            errno::EIO
            | errno::ENOTCONN
            | errno::ETIMEDOUT
            | errno::EHOSTDOWN
            | errno::EHOSTUNREACH
            | errno::ESTALE
            | errno::ENETUNREACH
            | errno::ECONNRESET
            | errno::ECONNABORTED => ErrorClass::Outage,
            errno::ENOSPC | errno::EDQUOT => ErrorClass::NoSpace,
            errno::ENOENT => ErrorClass::NotFound,
            errno::EACCES => ErrorClass::Permission,
            _ => ErrorClass::Other,
        };
    }
    match err.kind() {
        io::ErrorKind::NotFound => ErrorClass::NotFound,
        io::ErrorKind::PermissionDenied => ErrorClass::Permission,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => ErrorClass::NoSpace,
        io::ErrorKind::TimedOut
        | io::ErrorKind::NotConnected
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted => ErrorClass::Outage,
        _ => ErrorClass::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies: SRS-FSIO-001
    #[test]
    fn real_fs_preserves_os_error_codes() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let err = RealFs.metadata(&missing).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(errno::ENOENT));
        assert_eq!(classify(&err), ErrorClass::NotFound);
    }

    /// Verifies: SRS-FSIO-001
    #[test]
    fn classify_outage_codes() {
        for code in [
            errno::EIO,
            errno::ENOTCONN,
            errno::ETIMEDOUT,
            errno::ESTALE,
            errno::EHOSTDOWN,
            errno::ENETUNREACH,
        ] {
            let err = io::Error::from_raw_os_error(code);
            assert_eq!(classify(&err), ErrorClass::Outage, "errno {code}");
        }
        assert_eq!(
            classify(&io::Error::from_raw_os_error(errno::ENOSPC)),
            ErrorClass::NoSpace
        );
        assert_eq!(
            classify(&io::Error::from_raw_os_error(errno::EACCES)),
            ErrorClass::Permission
        );
        assert_eq!(classify(&io::Error::other("x")), ErrorClass::Other);
    }

    /// Verifies: SRS-FSIO-001
    #[test]
    fn real_fs_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let sub = dir.path().join("a/b");
        fs.create_dir_all(&sub).unwrap();
        let tmp = sub.join("f.part");
        {
            let mut f = fs.create_new(&tmp).unwrap();
            f.write_all(b"hello").unwrap();
            f.sync_data().unwrap();
        }
        assert!(
            fs.create_new(&tmp).is_err(),
            "create_new must not overwrite"
        );
        let fin = sub.join("f");
        fs.rename(&tmp, &fin).unwrap();
        fs.sync_dir(&sub).unwrap();
        assert_eq!(fs.read(&fin).unwrap(), b"hello");
        let mut s = String::new();
        fs.open_read(&fin).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello");
        let st = fs.metadata(&fin).unwrap();
        assert_eq!((st.len, st.kind), (5, FileKind::File));
        let names: Vec<_> = fs.read_dir(&sub).unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].kind, FileKind::File);
        fs.remove_file(&fin).unwrap();
        assert!(fs.read_dir(&sub).unwrap().is_empty());
    }

    /// Verifies: SRS-FSIO-001
    #[cfg(unix)]
    #[test]
    fn symlink_metadata_does_not_follow() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            RealFs.symlink_metadata(&link).unwrap().kind,
            FileKind::Symlink
        );
        assert_eq!(RealFs.metadata(&link).unwrap().kind, FileKind::Dir);
        assert_eq!(
            RealFs.canonicalize(&link).unwrap(),
            RealFs.canonicalize(&target).unwrap()
        );
    }
}
