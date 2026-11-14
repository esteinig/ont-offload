// SPDX-License-Identifier: AGPL-3.0-or-later
//! `FaultyFs`: a fault-injecting [`Fs`] for tests.
//!
//! Wraps another filesystem and applies [`Rule`]s: return an OS error, report
//! a different device id, accept only part of each write, flip a bit in data
//! that is read, or delay the call. Only compiled for unit tests and with the
//! `fault-injection` feature.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::{DirEntry, FileStat, Fs, FsFile};

/// The operation a rule applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Every operation.
    Any,
    /// [`Fs::metadata`].
    Metadata,
    /// [`Fs::symlink_metadata`].
    SymlinkMetadata,
    /// [`Fs::canonicalize`].
    Canonicalize,
    /// [`Fs::read`].
    Read,
    /// [`Fs::open_read`] and reads from the returned stream.
    OpenRead,
    /// [`Fs::create_new`].
    CreateNew,
    /// Writes to a file returned by [`Fs::create_new`].
    Write,
    /// [`FsFile::sync_data`].
    SyncData,
    /// [`Fs::rename`] (matched on the source path).
    Rename,
    /// [`Fs::remove_file`].
    RemoveFile,
    /// [`Fs::remove_dir`].
    RemoveDir,
    /// [`Fs::create_dir_all`].
    CreateDirAll,
    /// [`Fs::read_dir`].
    ReadDir,
    /// [`Fs::sync_dir`].
    SyncDir,
    /// [`Fs::set_mode`].
    SetMode,
    /// [`Fs::append_sync`].
    AppendSync,
    /// [`Fs::free_space`].
    FreeSpace,
}

/// What happens when a rule fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Fail with this raw OS error (see [`super::errno`]).
    Errno(i32),
    /// Report this device id from metadata calls.
    Device(u64),
    /// Accept at most half of each write buffer (at least one byte).
    ShortWrite,
    /// Flip the lowest bit of the byte at this offset of data read.
    BitFlip(u64),
    /// Sleep before performing the call normally.
    Delay(Duration),
    /// Report this many free bytes from [`Fs::free_space`].
    Free(u64),
}

/// One fault-injection rule.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Operation to match.
    pub op: Op,
    /// Substring the path must contain (empty matches every path).
    pub path_contains: String,
    /// Fault to inject.
    pub fault: Fault,
    /// How many more times the rule fires; `None` means forever.
    pub remaining: Option<u32>,
    /// Matching calls to let through before the rule starts firing.
    pub skip: u32,
}

impl Rule {
    /// A rule that fires on every matching call.
    #[must_use]
    pub fn always(op: Op, path_contains: &str, fault: Fault) -> Self {
        Self {
            op,
            path_contains: path_contains.to_owned(),
            fault,
            remaining: None,
            skip: 0,
        }
    }

    /// A rule that lets `k` matching calls through, then fires on every later one.
    #[must_use]
    pub fn after(op: Op, path_contains: &str, fault: Fault, k: u32) -> Self {
        Self {
            skip: k,
            ..Self::always(op, path_contains, fault)
        }
    }

    /// A rule that fires on the next `n` matching calls only.
    #[must_use]
    pub fn times(op: Op, path_contains: &str, fault: Fault, n: u32) -> Self {
        Self {
            remaining: Some(n),
            ..Self::always(op, path_contains, fault)
        }
    }

    fn matches(&self, op: Op, path: &Path) -> bool {
        (self.op == Op::Any || self.op == op)
            && self.remaining != Some(0)
            && path.to_string_lossy().contains(&self.path_contains)
    }
}

type Rules = Arc<Mutex<Vec<Rule>>>;

/// Fault-injecting filesystem wrapper.
///
/// Implements: SRS-FSIO-001
#[derive(Clone)]
pub struct FaultyFs {
    inner: Arc<dyn Fs>,
    rules: Rules,
}

impl std::fmt::Debug for FaultyFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultyFs").finish_non_exhaustive()
    }
}

/// Faults that fired for one call, in rule order.
fn fire(rules: &Rules, op: Op, path: &Path) -> Vec<Fault> {
    let mut rules = rules.lock().unwrap_or_else(PoisonError::into_inner);
    let mut fired = Vec::new();
    for rule in rules.iter_mut().filter(|r| r.matches(op, path)) {
        if rule.skip > 0 {
            rule.skip -= 1;
            continue;
        }
        if let Some(n) = rule.remaining.as_mut() {
            *n -= 1;
        }
        fired.push(rule.fault);
    }
    fired
}

/// Apply delays and return the first error fault, if any.
fn pre(faults: &[Fault]) -> io::Result<()> {
    for f in faults {
        match *f {
            Fault::Delay(d) => std::thread::sleep(d),
            Fault::Errno(code) => return Err(io::Error::from_raw_os_error(code)),
            _ => {}
        }
    }
    Ok(())
}

fn patch_stat(mut st: FileStat, faults: &[Fault]) -> FileStat {
    for f in faults {
        if let Fault::Device(dev) = *f {
            st.dev = dev;
        }
    }
    st
}

fn flips(faults: &[Fault]) -> Vec<u64> {
    faults
        .iter()
        .filter_map(|f| match *f {
            Fault::BitFlip(o) => Some(o),
            _ => None,
        })
        .collect()
}

impl FaultyFs {
    /// Wrap `inner` with no rules.
    #[must_use]
    pub fn new(inner: Arc<dyn Fs>) -> Self {
        Self {
            inner,
            rules: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Add a rule.
    pub fn add(&self, rule: Rule) {
        self.rules
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(rule);
    }

    /// Remove every rule (for example when the simulated outage ends).
    pub fn clear(&self) {
        self.rules
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    fn check(&self, op: Op, path: &Path) -> io::Result<Vec<Fault>> {
        let faults = fire(&self.rules, op, path);
        pre(&faults)?;
        Ok(faults)
    }
}

impl Fs for FaultyFs {
    fn metadata(&self, path: &Path) -> io::Result<FileStat> {
        let faults = self.check(Op::Metadata, path)?;
        Ok(patch_stat(self.inner.metadata(path)?, &faults))
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileStat> {
        let faults = self.check(Op::SymlinkMetadata, path)?;
        Ok(patch_stat(self.inner.symlink_metadata(path)?, &faults))
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.check(Op::Canonicalize, path)?;
        self.inner.canonicalize(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let faults = self.check(Op::Read, path)?;
        let mut data = self.inner.read(path)?;
        for off in flips(&faults) {
            if let Some(b) = usize::try_from(off).ok().and_then(|o| data.get_mut(o)) {
                *b ^= 1;
            }
        }
        Ok(data)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let faults = self.check(Op::OpenRead, path)?;
        Ok(Box::new(FaultyReader {
            inner: self.inner.open_read(path)?,
            rules: Arc::clone(&self.rules),
            path: path.to_owned(),
            pos: 0,
            flips: flips(&faults),
        }))
    }

    fn read_at(&self, path: &Path, offset: u64, max: usize) -> io::Result<Vec<u8>> {
        self.check(Op::OpenRead, path)?;
        self.inner.read_at(path, offset, max)
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn FsFile>> {
        self.check(Op::CreateNew, path)?;
        Ok(Box::new(FaultyFile {
            inner: self.inner.create_new(path)?,
            rules: Arc::clone(&self.rules),
            path: path.to_owned(),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check(Op::Rename, from)?;
        self.inner.rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.check(Op::RemoveFile, path)?;
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.check(Op::RemoveDir, path)?;
        self.inner.remove_dir(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.check(Op::CreateDirAll, path)?;
        self.inner.create_dir_all(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.check(Op::ReadDir, path)?;
        self.inner.read_dir(path)
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.check(Op::SyncDir, path)?;
        self.inner.sync_dir(path)
    }

    fn append_sync(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.check(Op::AppendSync, path)?;
        self.inner.append_sync(path, data)
    }

    fn set_mode(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.check(Op::SetMode, path)?;
        self.inner.set_mode(path, mode)
    }

    fn free_space(&self, path: &Path) -> io::Result<u64> {
        let faults = self.check(Op::FreeSpace, path)?;
        let real = self.inner.free_space(path)?;
        Ok(faults
            .iter()
            .rev()
            .find_map(|f| match *f {
                Fault::Free(n) => Some(n),
                _ => None,
            })
            .unwrap_or(real))
    }
}

struct FaultyReader {
    inner: Box<dyn Read + Send>,
    rules: Rules,
    path: PathBuf,
    pos: u64,
    flips: Vec<u64>,
}

impl Read for FaultyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let faults = fire(&self.rules, Op::OpenRead, &self.path);
        pre(&faults)?;
        let n = self.inner.read(buf)?;
        let start = self.pos;
        self.pos += n as u64;
        for &off in &self.flips {
            if off >= start && off < self.pos {
                if let Some(b) = usize::try_from(off - start)
                    .ok()
                    .and_then(|i| buf.get_mut(i))
                {
                    *b ^= 1;
                }
            }
        }
        Ok(n)
    }
}

struct FaultyFile {
    inner: Box<dyn FsFile>,
    rules: Rules,
    path: PathBuf,
}

impl Write for FaultyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let faults = fire(&self.rules, Op::Write, &self.path);
        pre(&faults)?;
        if faults.contains(&Fault::ShortWrite) && buf.len() > 1 {
            return self.inner.write(&buf[..buf.len() / 2]);
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl FsFile for FaultyFile {
    fn sync_data(&mut self) -> io::Result<()> {
        let faults = fire(&self.rules, Op::SyncData, &self.path);
        pre(&faults)?;
        self.inner.sync_data()
    }

    fn set_modified(&mut self, time: std::time::SystemTime) -> io::Result<()> {
        self.inner.set_modified(time)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ErrorClass, RealFs, classify, errno};
    use super::*;

    fn setup() -> (tempfile::TempDir, FaultyFs) {
        let dir = tempfile::tempdir().unwrap();
        (dir, FaultyFs::new(Arc::new(RealFs)))
    }

    /// Verifies: SRS-FSIO-001
    #[test]
    fn injected_errno_is_classified_and_expires() {
        let (dir, fs) = setup();
        fs.add(Rule::times(
            Op::Metadata,
            "",
            Fault::Errno(errno::ENOTCONN),
            2,
        ));
        for _ in 0..2 {
            let err = fs.metadata(dir.path()).unwrap_err();
            assert_eq!(err.raw_os_error(), Some(errno::ENOTCONN));
            assert_eq!(classify(&err), ErrorClass::Outage);
        }
        assert!(fs.metadata(dir.path()).is_ok(), "rule used up");
    }

    /// Verifies: SRS-FSIO-001
    #[test]
    fn device_override_and_path_filter() {
        let (dir, fs) = setup();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        fs.add(Rule::always(Op::Metadata, "/a", Fault::Device(4242)));
        assert_eq!(fs.metadata(&a).unwrap().dev, 4242);
        assert_ne!(fs.metadata(&b).unwrap().dev, 4242);
    }

    /// Verifies: SRS-FSIO-001
    #[test]
    fn bit_flip_on_read_and_stream() {
        let (dir, fs) = setup();
        let p = dir.path().join("f");
        std::fs::write(&p, vec![0u8; 100_000]).unwrap();
        fs.add(Rule::always(Op::Read, "", Fault::BitFlip(3)));
        fs.add(Rule::times(Op::OpenRead, "", Fault::BitFlip(70_000), 1));
        let data = fs.read(&p).unwrap();
        assert_eq!(data.iter().filter(|b| **b != 0).count(), 1);
        assert_eq!(data[3], 1);
        let mut streamed = Vec::new();
        fs.open_read(&p)
            .unwrap()
            .read_to_end(&mut streamed)
            .unwrap();
        assert_eq!(streamed.len(), 100_000);
        assert_eq!(streamed[70_000], 1);
        assert_eq!(streamed.iter().filter(|b| **b != 0).count(), 1);
    }

    /// Verifies: SRS-FSIO-001
    #[test]
    fn short_writes_and_write_errors() {
        let (dir, fs) = setup();
        let p = dir.path().join("w");
        fs.add(Rule::always(Op::Write, "", Fault::ShortWrite));
        {
            let mut f = fs.create_new(&p).unwrap();
            f.write_all(b"0123456789").unwrap();
            f.sync_data().unwrap();
        }
        assert_eq!(std::fs::read(&p).unwrap(), b"0123456789");
        fs.clear();
        fs.add(Rule::times(Op::SyncData, "", Fault::Errno(errno::EIO), 1));
        let q = dir.path().join("q");
        let mut f = fs.create_new(&q).unwrap();
        f.write_all(b"x").unwrap();
        assert_eq!(f.sync_data().unwrap_err().raw_os_error(), Some(errno::EIO));
    }
}
