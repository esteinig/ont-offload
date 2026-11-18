// SPDX-License-Identifier: AGPL-3.0-or-later
//! Journal, destination claims, manifest, `run.json`, `COMPLETE` and the
//! `VERIFIED.json` receipt (SWI-JOURNAL).
//!
//! The journal of a run lives in `state_dir/runs/<run id>/journal.jsonl`:
//! one JSON object per line, appended and flushed to disk before the next
//! step. A torn last line (crash while appending) is ignored on load.

pub mod verify;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::DestLevel;
use crate::error::{Error, Result};
use crate::fsio::Fs;
use crate::logging::escape_control;

pub use verify::{VerifyReport, verify};

/// Name of the manifest in a destination run folder.
pub const MANIFEST: &str = "manifest.sha256";
/// Name of the run record in a destination run folder.
pub const RUN_JSON: &str = "run.json";
/// Name of the completion marker, written last.
pub const COMPLETE: &str = "COMPLETE";
/// Name of the verification receipt.
pub const VERIFIED: &str = "VERIFIED.json";
/// Top-level names ont-offload writes into a destination run folder.
pub const RESERVED_NAMES: &[&str] = &[MANIFEST, RUN_JSON, COMPLETE, VERIFIED];

/// Lowercase hex of bytes.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// SHA-256 of bytes as hex.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Stable id of a run: the first 16 hex digits of the SHA-256 of its source path.
#[must_use]
pub fn run_id(source: &Path) -> String {
    sha256_hex(source.as_os_str().as_encoded_bytes())[..16].to_owned()
}

/// RFC 3339 UTC time, seconds precision.
#[must_use]
pub fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

/// Nanoseconds since the Unix epoch (0 for earlier or unknown times,
/// saturating in the year 2554). A `u64` keeps the journal plain JSON.
#[must_use]
pub fn nanos(t: Option<SystemTime>) -> u64 {
    t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    /// The run claimed its destination folder.
    Claim {
        /// Source run folder.
        source: String,
        /// Destination run folder below `dest_root`.
        dest: String,
        /// Level used for the mapping.
        dest_starts_at: DestLevel,
        /// When.
        ts: String,
    },
    /// A file was committed at the destination.
    File {
        /// Path relative to the run folder.
        rel: String,
        /// Bytes.
        size: u64,
        /// Source modification time in ns.
        mtime_ns: u64,
        /// SHA-256 hex.
        sha256: String,
        /// When.
        ts: String,
    },
    /// Manifest, `run.json` and `COMPLETE` were written.
    Complete {
        /// SHA-256 of the manifest.
        manifest_sha256: String,
        /// When.
        ts: String,
    },
}

/// A committed file as recorded in the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    /// Bytes.
    pub size: u64,
    /// Source modification time in ns.
    pub mtime_ns: u64,
    /// SHA-256 hex.
    pub sha256: String,
}

/// The journal of one run.
#[derive(Debug, Clone)]
pub struct Journal {
    dir: PathBuf,
    /// Run id.
    pub id: String,
    /// Claim, if made.
    pub claim: Option<(String, String, DestLevel)>,
    /// Committed files by relative path (latest record wins).
    pub files: BTreeMap<String, Committed>,
    /// Manifest digest once complete.
    pub complete: Option<String>,
}

impl Journal {
    /// Open (or start) the journal of the run at `source` below `state_dir`.
    ///
    /// Implements: SRS-RES-004
    ///
    /// # Errors
    /// I/O errors other than a missing journal.
    pub fn open(fs: &dyn Fs, state_dir: &Path, source: &Path) -> Result<Self> {
        let id = run_id(source);
        let dir = state_dir.join("runs").join(&id);
        fs.create_dir_all(&dir)?;
        let mut j = Self {
            dir,
            id,
            claim: None,
            files: BTreeMap::new(),
            complete: None,
        };
        let text = match fs.read(&j.path()) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let lines: Vec<&str> = text.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(line) {
                Ok(r) => j.apply(r),
                // A torn final line from a crash while appending.
                Err(_) if i + 1 == lines.len() => {
                    log::warn!("journal {}: ignoring incomplete last line", j.id);
                }
                Err(e) => {
                    return Err(Error::Integrity(format!(
                        "journal {} line {} is corrupt: {e}",
                        j.path().display(),
                        i + 1
                    )));
                }
            }
        }
        Ok(j)
    }

    fn path(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }

    fn apply(&mut self, r: Record) {
        match r {
            Record::Claim {
                source,
                dest,
                dest_starts_at,
                ..
            } => self.claim = Some((source, dest, dest_starts_at)),
            Record::File {
                rel,
                size,
                mtime_ns,
                sha256,
                ..
            } => {
                self.files.insert(
                    rel,
                    Committed {
                        size,
                        mtime_ns,
                        sha256,
                    },
                );
                self.complete = None;
            }
            Record::Complete {
                manifest_sha256, ..
            } => self.complete = Some(manifest_sha256),
        }
    }

    /// Append a record and flush it to disk before returning.
    ///
    /// # Errors
    /// I/O errors.
    pub fn append(&mut self, fs: &dyn Fs, r: Record) -> Result<()> {
        let mut line = serde_json::to_vec(&r).map_err(|e| Error::Integrity(e.to_string()))?;
        line.push(b'\n');
        fs.append_sync(&self.path(), &line)?;
        self.apply(r);
        Ok(())
    }
}

/// Claim a destination folder for a run, or confirm an earlier claim.
///
/// Claims live in `state_dir/claims/`, one file per destination folder. A
/// destination already claimed by another source run is refused, and so is
/// a run whose recorded destination differs from the current mapping
/// (`dest_starts_at` changed while it was in progress).
///
/// Implements: SRS-SEL-007
///
/// # Errors
/// [`Error::Config`] for a changed mapping, [`Error::Failed`]-worthy
/// [`Error::Integrity`] for a destination claimed by another run; I/O errors.
pub fn claim(
    fs: &dyn Fs,
    state_dir: &Path,
    journal: &mut Journal,
    source: &Path,
    dest_rel: &Path,
    level: DestLevel,
) -> Result<()> {
    let src = source.to_string_lossy().into_owned();
    let dest = dest_rel.to_string_lossy().into_owned();
    if let Some((s, d, l)) = &journal.claim {
        if *d != dest {
            return Err(Error::Config(format!(
                "run {} was started with dest_starts_at = \"{}\" (destination {}); it is now \"{}\" (destination {}). \
                 Finish the run with the old setting or remove its journal {} to start over",
                escape_control(s),
                l.as_str(),
                escape_control(d),
                level.as_str(),
                escape_control(&dest),
                journal.dir.display()
            )));
        }
        return Ok(());
    }
    let dir = state_dir.join("claims");
    fs.create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", &sha256_hex(dest.as_bytes())[..24]));
    let body = serde_json::json!({ "dest": dest, "source": src, "run_id": journal.id });
    match fs.create_new(&path) {
        Ok(mut f) => {
            f.write_all(body.to_string().as_bytes())?;
            f.sync_data()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing: serde_json::Value =
                serde_json::from_slice(&fs.read(&path)?).unwrap_or_default();
            if existing.get("source").and_then(|v| v.as_str()) != Some(src.as_str()) {
                return Err(Error::Integrity(format!(
                    "destination {} is already claimed by run {}; not mixing data from {}",
                    escape_control(&dest),
                    escape_control(
                        existing
                            .get("source")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                    ),
                    escape_control(&src)
                )));
            }
        }
        Err(e) => return Err(e.into()),
    }
    journal.append(
        fs,
        Record::Claim {
            source: src,
            dest,
            dest_starts_at: level,
            ts: now_rfc3339(),
        },
    )
}

/// Escape a path for a `sha256sum` line: `\` and newline are escaped and the
/// line then starts with `\` (GNU coreutils convention).
#[must_use]
pub fn manifest_line(sha: &str, rel: &str) -> String {
    if rel.contains('\\') || rel.contains('\n') || rel.contains('\r') {
        let esc = rel
            .replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        format!("\\{sha}  {esc}\n")
    } else {
        format!("{sha}  {rel}\n")
    }
}

/// Parse a manifest into relative path -> digest.
///
/// # Errors
/// [`Error::Integrity`] for a malformed line.
pub fn parse_manifest(text: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (escaped, line) = line
            .strip_prefix('\\')
            .map_or((false, line), |rest| (true, rest));
        let Some((sha, name)) = line.split_once("  ") else {
            return Err(Error::Integrity(format!(
                "manifest line {} is malformed",
                n + 1
            )));
        };
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Integrity(format!(
                "manifest line {} has a bad digest",
                n + 1
            )));
        }
        let name = if escaped {
            let mut s = String::new();
            let mut it = name.chars();
            while let Some(c) = it.next() {
                if c == '\\' {
                    match it.next() {
                        Some('n') => s.push('\n'),
                        Some('r') => s.push('\r'),
                        Some(o) if o != '\\' => {
                            s.push('\\');
                            s.push(o);
                        }
                        Some(_) | None => s.push('\\'),
                    }
                } else {
                    s.push(c);
                }
            }
            s
        } else {
            name.to_owned()
        };
        out.insert(name, sha.to_ascii_lowercase());
    }
    Ok(out)
}

/// Write a small file atomically: temp name, flush, rename, flush the folder.
///
/// # Errors
/// I/O errors.
pub fn write_atomic(fs: &dyn Fs, dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    let tmp = dir.join(crate::copy::temp_name(name));
    {
        let mut f = fs.create_new(&tmp)?;
        f.write_all(data)?;
        f.sync_data()?;
    }
    fs.rename(&tmp, &dir.join(name))?;
    fs.sync_dir(dir)?;
    Ok(())
}

/// Host name for records (`unknown` if it cannot be read).
#[must_use]
pub fn host_name(fs: &dyn Fs) -> String {
    for p in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(b) = fs.read(Path::new(p)) {
            let s = String::from_utf8_lossy(&b).trim().to_owned();
            if !s.is_empty() {
                return s;
            }
        }
    }
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsio::RealFs;

    /// Verifies: SRS-VER-002
    #[test]
    fn manifest_lines_round_trip_with_escapes() {
        let sha = "a".repeat(64);
        let mut text = String::new();
        for name in ["pod5/a.pod5", "odd\\name", "line\nbreak"] {
            text.push_str(&manifest_line(&sha, name));
        }
        assert!(text.starts_with(&format!("{sha}  pod5/a.pod5\n")));
        let parsed = parse_manifest(&text).unwrap();
        assert_eq!(parsed.len(), 3);
        assert!(parsed.contains_key("line\nbreak"));
        assert!(parsed.contains_key("odd\\name"));
        assert!(parse_manifest("nonsense\n").is_err());
    }

    /// Verifies: SRS-RES-004
    #[test]
    fn journal_survives_a_torn_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new("/data/e/s/run1");
        let mut j = Journal::open(&RealFs, dir.path(), src).unwrap();
        j.append(
            &RealFs,
            Record::File {
                rel: "a".into(),
                size: 3,
                mtime_ns: 7,
                sha256: "x".into(),
                ts: now_rfc3339(),
            },
        )
        .unwrap();
        let path = j.path();
        RealFs
            .append_sync(&path, b"{\"type\":\"file\",\"rel\":\"b")
            .unwrap();
        let j2 = Journal::open(&RealFs, dir.path(), src).unwrap();
        assert_eq!(j2.files.len(), 1);
        assert_eq!(j2.files["a"].size, 3);
        assert_eq!(j2.id, run_id(src));
    }

    /// Verifies: SRS-SEL-007
    #[test]
    fn claims_refuse_a_second_run_and_a_changed_level() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let a = Path::new("/data/e1/s/run");
        let b = Path::new("/data/e2/s/run");
        let mut ja = Journal::open(&fs, dir.path(), a).unwrap();
        claim(
            &fs,
            dir.path(),
            &mut ja,
            a,
            Path::new("s/run"),
            DestLevel::Sample,
        )
        .unwrap();
        // Same run again: fine.
        let mut ja2 = Journal::open(&fs, dir.path(), a).unwrap();
        claim(
            &fs,
            dir.path(),
            &mut ja2,
            a,
            Path::new("s/run"),
            DestLevel::Sample,
        )
        .unwrap();
        // Another run with the same destination: refused.
        let mut jb = Journal::open(&fs, dir.path(), b).unwrap();
        let err = claim(
            &fs,
            dir.path(),
            &mut jb,
            b,
            Path::new("s/run"),
            DestLevel::Sample,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already claimed"), "{err}");
        // The first run with a different level: refused.
        let err = claim(
            &fs,
            dir.path(),
            &mut ja2,
            a,
            Path::new("e1/s/run"),
            DestLevel::Experiment,
        )
        .unwrap_err();
        assert!(err.to_string().contains("dest_starts_at"), "{err}");
    }
}
