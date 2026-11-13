// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-file copy with in-flight SHA-256, optional read-back and atomic
//! commit (SWI-COPY).
//!
//! 1. Stat the source (S0).
//! 2. Stream it in chunks into `.<name>.ontoff-<nonce>.partial` in the final
//!    folder, hashing every byte as it is written; the sentinel is re-read
//!    after every chunk.
//! 3. Flush the data, stat the source again (S1): any change in size or
//!    modification time discards the copy.
//! 4. Check the temp file's size, optionally re-read it and compare digests.
//! 5. Run the full guard, then rename to the final name and flush the folder.
//!
//! A destination file that ont-offload did not write is never replaced.

use std::io::{Read, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::config::{OnExisting, VerifyMode};
use crate::error::Error;
use crate::fsio::{FileKind, Fs};
use crate::guard::Guard;
use crate::journal::hex;
use crate::logging::escape_control;

/// Marker in temp file names; only files with it are ever cleaned up.
pub const TEMP_TAG: &str = ".ontoff-";
/// Suffix of temp files.
pub const TEMP_SUFFIX: &str = ".partial";

static NONCE: AtomicU64 = AtomicU64::new(0);

/// A fresh temp file name for `name`: `.<name>.ontoff-<nonce>.partial`.
#[must_use]
pub fn temp_name(name: &str) -> String {
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!(
        ".{name}{TEMP_TAG}{:x}{t:08x}{n:x}{TEMP_SUFFIX}",
        std::process::id()
    )
}

/// Whether a file name is one of ont-offload's temp files.
#[must_use]
pub fn is_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.contains(TEMP_TAG) && name.ends_with(TEMP_SUFFIX)
}

/// Copy settings.
#[derive(Debug, Clone, Copy)]
pub struct CopyOptions {
    /// Chunk size in bytes.
    pub chunk_size: usize,
    /// Verification mode.
    pub verify: VerifyMode,
    /// Existing destination file policy.
    pub on_existing: OnExisting,
    /// Permission bits of committed files.
    pub file_mode: u32,
    /// Copy the source modification time.
    pub preserve_mtime: bool,
    /// Free space to keep on the destination (`transfer.min_free`).
    pub min_free: u64,
}

/// A committed copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    /// Bytes.
    pub size: u64,
    /// SHA-256 hex of the bytes read from the source and written.
    pub sha256: String,
    /// Source modification time (S0) in ns.
    pub mtime_ns: u64,
    /// Whether an identical file already at the destination was adopted.
    pub adopted: bool,
}

/// Why a copy did not commit.
#[derive(Debug)]
pub enum CopyFail {
    /// The source changed during the copy, or a check failed in a way a new
    /// attempt can fix (short write, read-back mismatch).
    Retry(String),
    /// What reached the destination differs from what was read (size
    /// check or read-back); retried like [`CopyFail::Retry`] and reported
    /// as an integrity mismatch.
    Mismatch(String),
    /// A destination file that ont-offload did not write is in the way.
    Conflict(String),
    /// The guard failed; stop writing.
    Guard(Error),
    /// Writes were paused (outage) or the copy was asked to stop; not an attempt.
    Paused,
    /// An I/O error.
    Io(std::io::Error),
}

impl std::fmt::Display for CopyFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retry(m) | Self::Mismatch(m) | Self::Conflict(m) => f.write_str(m),
            Self::Paused => f.write_str("paused"),
            Self::Guard(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for CopyFail {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Stream a file and return its size and SHA-256 hex.
///
/// # Errors
/// I/O errors.
pub fn hash_file(fs: &dyn Fs, path: &Path, chunk: usize) -> std::io::Result<(u64, String)> {
    let mut r = fs.open_read(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; chunk.max(4096)];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hex(&h.finalize())))
}

/// Read until `buf` is full or the stream ends.
fn fill(r: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Called by [`copy_file_with`] before each chunk is written: rate limiting,
/// progress for stall detection, and pausing.
pub trait Hooks {
    /// `n` bytes are about to be written. An error stops the copy (the temp
    /// file is removed).
    ///
    /// # Errors
    /// Usually [`CopyFail::Paused`].
    fn chunk(&self, n: usize) -> Result<(), CopyFail>;
}

/// No hooks.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl Hooks for NoHooks {
    fn chunk(&self, _n: usize) -> Result<(), CopyFail> {
        Ok(())
    }
}

/// [`copy_file_with`] without hooks.
///
/// # Errors
/// See [`CopyFail`].
pub fn copy_file(
    fs: &dyn Fs,
    guard: &Guard,
    src: &Path,
    dest: &Path,
    ours: bool,
    opts: &CopyOptions,
) -> Result<Copied, CopyFail> {
    copy_file_with(fs, guard, src, dest, ours, opts, &NoHooks)
}

/// Copy one file and commit it under its final name.
///
/// `ours` says the journal records a previous commit of this path by
/// ont-offload, which may then be replaced; any other existing file is a
/// conflict (or, with `on_existing = "hash"`, adopted when identical).
///
/// Implements: SRS-CPY-001, SRS-CPY-002, SRS-VER-001, SRS-LIVE-002, SRS-GRD-002
///
/// # Errors
/// See [`CopyFail`].
#[allow(clippy::too_many_lines)]
pub fn copy_file_with(
    fs: &dyn Fs,
    guard: &Guard,
    src: &Path,
    dest: &Path,
    ours: bool,
    opts: &CopyOptions,
    hooks: &dyn Hooks,
) -> Result<Copied, CopyFail> {
    let shown = escape_control(&dest.to_string_lossy());
    let s0 = fs.symlink_metadata(src)?;
    if s0.kind != FileKind::File {
        return Err(CopyFail::Conflict(format!(
            "{} is no longer a regular file",
            escape_control(&src.to_string_lossy())
        )));
    }
    let mtime_ns = crate::journal::nanos(s0.modified);
    let parent = dest
        .parent()
        .ok_or_else(|| CopyFail::Conflict(format!("{shown} has no parent folder")))?;
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CopyFail::Conflict(format!("{shown} has no usable file name")))?;

    match fs.symlink_metadata(dest) {
        Ok(existing) if !ours => {
            if opts.on_existing == OnExisting::Hash && existing.kind == FileKind::File {
                let (sl, ss) = hash_file(fs, src, opts.chunk_size)?;
                let (dl, ds) = hash_file(fs, dest, opts.chunk_size)?;
                if sl == dl && ss == ds {
                    return Ok(Copied {
                        size: sl,
                        sha256: ss,
                        mtime_ns,
                        adopted: true,
                    });
                }
            }
            return Err(CopyFail::Conflict(format!(
                "{shown} already exists and was not written by ont-offload; not overwritten"
            )));
        }
        Ok(_) | Err(_) => {}
    }

    guard.quick(fs).map_err(CopyFail::Guard)?;
    fs.create_dir_all(parent)?;
    check_free(fs, parent, s0.len, opts.min_free)?;
    let tmp = parent.join(temp_name(name));
    let result = (|| -> Result<Copied, CopyFail> {
        let mut out = fs.create_new(&tmp)?;
        let mut input = fs.open_read(src)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; opts.chunk_size.max(4096)];
        let mut total = 0u64;
        loop {
            let n = fill(&mut *input, &mut buf)?;
            if n == 0 {
                break;
            }
            hooks.chunk(n)?;
            h.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            total += n as u64;
            guard.quick(fs).map_err(CopyFail::Guard)?;
        }
        if opts.preserve_mtime {
            if let Some(t) = s0.modified {
                out.set_modified(t)?;
            }
        }
        out.sync_data()?;
        drop(out);
        let sha = hex(&h.finalize());

        let s1 = fs.symlink_metadata(src)?;
        if s1.len != s0.len || s1.modified != s0.modified || total != s0.len {
            return Err(CopyFail::Retry(format!(
                "source changed during the copy ({} -> {} bytes, {} read)",
                s0.len, s1.len, total
            )));
        }
        let written = fs.metadata(&tmp)?.len;
        if written != total {
            return Err(CopyFail::Mismatch(format!(
                "size check failed: {written} bytes on the destination, {total} read"
            )));
        }
        if opts.verify == VerifyMode::Readback {
            let (rl, rs) = hash_file(fs, &tmp, opts.chunk_size)?;
            if rl != total || rs != sha {
                return Err(CopyFail::Mismatch(format!(
                    "read-back mismatch: wrote {sha}, read back {rs}"
                )));
            }
        }
        fs.set_mode(&tmp, opts.file_mode)?;
        guard.check(fs).map_err(CopyFail::Guard)?;
        // A file that appeared under the final name during the copy is not ours.
        if !ours && fs.symlink_metadata(dest).is_ok() {
            return Err(CopyFail::Conflict(format!(
                "{shown} appeared during the copy; not overwritten"
            )));
        }
        fs.rename(&tmp, dest)?;
        fs.sync_dir(parent)?;
        Ok(Copied {
            size: total,
            sha256: sha,
            mtime_ns,
            adopted: false,
        })
    })();
    if result.is_err() {
        let _ = fs.remove_file(&tmp);
    }
    result
}

/// Refuse a copy that would leave less than `min_free` on the destination.
/// The error is classified as capacity (pause, like a full share).
///
/// Implements: SRS-RES-010
///
/// # Errors
/// [`CopyFail::Io`] with [`std::io::ErrorKind::StorageFull`].
pub fn check_free(fs: &dyn Fs, dir: &Path, size: u64, min_free: u64) -> Result<(), CopyFail> {
    if min_free == 0 {
        return Ok(());
    }
    let free = fs.free_space(dir)?;
    if free < min_free.saturating_add(size) {
        return Err(CopyFail::Io(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            format!(
                "{free} bytes free on the destination; copying {size} bytes would go below transfer.min_free ({min_free} bytes)"
            ),
        )));
    }
    Ok(())
}

/// Remove ont-offload's own temp files below a destination run folder.
///
/// Implements: SRS-RES-004
///
/// # Errors
/// I/O errors listing the top folder.
pub fn remove_stale_temps(fs: &dyn Fs, dir: &Path) -> std::io::Result<usize> {
    let mut removed = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match fs.read_dir(&d) {
            Ok(e) => e,
            Err(e) if d == dir => return Err(e),
            Err(_) => continue,
        };
        for e in entries {
            let p = d.join(&e.name);
            match e.kind {
                FileKind::Dir => stack.push(p),
                FileKind::File if e.name.to_str().is_some_and(is_temp_name) => {
                    fs.remove_file(&p)?;
                    removed += 1;
                }
                _ => {}
            }
        }
    }
    Ok(removed)
}

/// Seconds since `start`, for log lines.
#[must_use]
pub fn secs(start: Instant) -> f64 {
    start.elapsed().as_secs_f64()
}
