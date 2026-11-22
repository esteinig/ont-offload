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

pub use verify::{VerifyReport, compare, verify};

/// Name of the manifest in a destination run folder.
pub const MANIFEST: &str = "manifest.sha256";
/// Name of the run record in a destination run folder.
pub const RUN_JSON: &str = "run.json";
/// Name of the completion marker, written last.
pub const COMPLETE: &str = "COMPLETE";
/// Name of the verification receipt.
pub const VERIFIED: &str = "VERIFIED.json";
/// Name of the deletion record, written after the source run was deleted.
pub const DELETED: &str = "DELETED.json";
/// Top-level names ont-offload writes into a destination run folder.
pub const RESERVED_NAMES: &[&str] = &[MANIFEST, RUN_JSON, COMPLETE, VERIFIED, SHARE_CLAIM, DELETED];

/// Claim file in each destination run folder (SRS-SEL-008).
pub const SHARE_CLAIM: &str = ".ont-offload-claim.json";

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
    /// The source changed after completion; `COMPLETE` was withdrawn.
    Reopen {
        /// When.
        ts: String,
    },
    /// Gated deletion of the source run started: from here on the run is
    /// never offloaded again.
    Deleting {
        /// When.
        ts: String,
    },
    /// Every listed source file was deleted and `DELETED.json` written.
    Deleted {
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
    /// When the run was completed (RFC 3339), if complete.
    pub completed_at: Option<String>,
    /// When gated deletion of the source started (RFC 3339), if it did.
    pub deleting: Option<String>,
    /// When gated deletion finished (RFC 3339), if it did.
    pub deleted_at: Option<String>,
    /// When the claim was first made (RFC 3339); kept by compaction.
    pub claimed_at: Option<String>,
    /// When each file was committed (RFC 3339); kept by compaction.
    pub committed_at: BTreeMap<String, String>,
    records: usize,
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
            completed_at: None,
            deleting: None,
            deleted_at: None,
            claimed_at: None,
            committed_at: BTreeMap::new(),
            records: 0,
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

    /// Every journal in `state_dir`, found through the source recorded in
    /// its claim (journals without a claim are skipped). Used by cleanup,
    /// which must also find runs whose run marker it already deleted.
    ///
    /// Implements: SRS-DEL-002
    ///
    /// # Errors
    /// I/O errors listing `state_dir/runs`; a corrupt journal.
    pub fn list(fs: &dyn Fs, state_dir: &Path) -> Result<Vec<Self>> {
        let runs = state_dir.join("runs");
        let entries = match fs.read_dir(&runs) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for e in entries {
            let path = runs.join(&e.name).join("journal.jsonl");
            let Ok(bytes) = fs.read(&path) else { continue };
            let text = String::from_utf8_lossy(&bytes);
            let source = text
                .lines()
                .find_map(|l| match serde_json::from_str::<Record>(l) {
                    Ok(Record::Claim { source, .. }) => Some(source),
                    _ => None,
                });
            if let Some(source) = source {
                let j = Self::open(fs, state_dir, Path::new(&source))?;
                if j.id == e.name.to_string_lossy() {
                    out.push(j);
                }
            }
        }
        Ok(out)
    }

    /// Records in the journal file (for compaction and tests).
    #[must_use]
    pub fn records(&self) -> usize {
        self.records
    }

    /// Rewrite the journal as one claim, one record per file and the
    /// completion record, atomically. Each record keeps the time it was
    /// first written, so the journal still dates every commit.
    ///
    /// Implements: SRS-RES-008, SRS-LOG-006
    ///
    /// # Errors
    /// I/O errors (the old journal is kept).
    pub fn compact(&mut self, fs: &dyn Fs) -> Result<()> {
        let ts = now_rfc3339();
        let mut out = Vec::new();
        let mut push = |r: &Record| -> Result<()> {
            out.extend(serde_json::to_vec(r).map_err(|e| Error::Integrity(e.to_string()))?);
            out.push(b'\n');
            Ok(())
        };
        let mut n = 0;
        if let Some((source, dest, level)) = &self.claim {
            push(&Record::Claim {
                source: source.clone(),
                dest: dest.clone(),
                dest_starts_at: *level,
                ts: self.claimed_at.clone().unwrap_or_else(|| ts.clone()),
            })?;
            n += 1;
        }
        for (rel, c) in &self.files {
            push(&Record::File {
                rel: rel.clone(),
                size: c.size,
                mtime_ns: c.mtime_ns,
                sha256: c.sha256.clone(),
                ts: self
                    .committed_at
                    .get(rel)
                    .cloned()
                    .unwrap_or_else(|| ts.clone()),
            })?;
            n += 1;
        }
        if let Some(m) = &self.complete {
            push(&Record::Complete {
                manifest_sha256: m.clone(),
                ts: self.completed_at.clone().unwrap_or_else(|| ts.clone()),
            })?;
            n += 1;
        }
        if let Some(ts) = &self.deleting {
            push(&Record::Deleting { ts: ts.clone() })?;
            n += 1;
        }
        if let Some(ts) = &self.deleted_at {
            push(&Record::Deleted { ts: ts.clone() })?;
            n += 1;
        }
        write_atomic(fs, &self.dir, "journal.jsonl", &out)?;
        self.records = n;
        Ok(())
    }

    pub(crate) fn path(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }

    fn apply(&mut self, r: Record) {
        self.records += 1;
        match r {
            Record::Claim {
                source,
                dest,
                dest_starts_at,
                ts,
            } => {
                if self.claim.is_none() {
                    self.claimed_at = Some(ts);
                }
                self.claim = Some((source, dest, dest_starts_at));
            }
            Record::File {
                rel,
                size,
                mtime_ns,
                sha256,
                ts,
            } => {
                self.committed_at.insert(rel.clone(), ts);
                self.files.insert(
                    rel,
                    Committed {
                        size,
                        mtime_ns,
                        sha256,
                    },
                );
                self.complete = None;
                self.completed_at = None;
            }
            Record::Complete {
                manifest_sha256,
                ts,
            } => {
                self.complete = Some(manifest_sha256);
                self.completed_at = Some(ts);
            }
            Record::Reopen { .. } => {
                self.complete = None;
                self.completed_at = None;
            }
            Record::Deleting { ts } => self.deleting = Some(ts),
            Record::Deleted { ts } => self.deleted_at = Some(ts),
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
    nested_claim(fs, &dir, dest_rel, &src)?;
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

/// Refuse a destination folder that holds, or lies inside, the folder of
/// another run (for example a dataset marked `READY` around a subfolder
/// that was already offloaded as a dataset of its own): the two would
/// write into each other's files.
///
/// Implements: SRS-SRC-003
fn nested_claim(fs: &dyn Fs, claims: &Path, dest: &Path, src: &str) -> Result<()> {
    for e in fs.read_dir(claims)? {
        let Ok(bytes) = fs.read(&claims.join(&e.name)) else {
            continue;
        };
        let other: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let (Some(d), Some(s)) = (
            other.get("dest").and_then(|v| v.as_str()),
            other.get("source").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let d = Path::new(d);
        if s == src || d == dest {
            continue;
        }
        if d.starts_with(dest) || dest.starts_with(d) {
            return Err(Error::Integrity(format!(
                "destination {} {} the destination {} of run {}; not mixing the two. \
                 Mark only the top folder of a dataset, or move one of them",
                escape_control(&dest.to_string_lossy()),
                if d.starts_with(dest) {
                    "holds"
                } else {
                    "lies inside"
                },
                escape_control(&d.to_string_lossy()),
                escape_control(s)
            )));
        }
    }
    Ok(())
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
    write_atomic_mode(fs, dir, name, data, None)
}

/// [`write_atomic`], giving the file `mode` (when given) before the rename.
///
/// # Errors
/// I/O errors.
pub fn write_atomic_mode(
    fs: &dyn Fs,
    dir: &Path,
    name: &str,
    data: &[u8],
    mode: Option<&crate::config::mode::Mode>,
) -> Result<()> {
    let tmp = dir.join(crate::copy::temp_name(name));
    {
        let mut f = fs.create_new(&tmp)?;
        f.write_all(data)?;
        f.sync_data()?;
    }
    if let Some(m) = mode {
        if let Err(e) = m.set(fs, &tmp, false) {
            let _ = fs.remove_file(&tmp);
            return Err(e.into());
        }
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

/// Claim a destination run folder on the share itself: create
/// [`SHARE_CLAIM`] with create-new semantics, or accept an existing claim
/// made for the same source. A claim for another source, by any host or
/// state folder, refuses the run.
///
/// The folders created for it get `modes.dir` and the claim `modes.record`
/// (when set).
///
/// Implements: SRS-SEL-008, SRS-CPY-003
///
/// # Errors
/// [`crate::copy::CopyFail::Conflict`] for another run's claim,
/// [`crate::copy::CopyFail::Guard`], I/O errors.
pub fn claim_on_share(
    fs: &dyn Fs,
    guard: &crate::guard::Guard,
    dest: &Path,
    source: &Path,
    run_id: &str,
    target_id: &str,
    modes: &crate::config::mode::OutputModes,
) -> std::result::Result<(), crate::copy::CopyFail> {
    use crate::copy::CopyFail;
    guard.check(fs).map_err(CopyFail::Guard)?;
    crate::copy::create_dirs(fs, dest, modes.dir.as_ref())?;
    let path = dest.join(SHARE_CLAIM);
    let src = source.to_string_lossy().into_owned();
    let body = serde_json::json!({
        "tool": "ont-offload",
        "version": env!("CARGO_PKG_VERSION"),
        "target_id": target_id,
        "host": host_name(fs),
        "source": src,
        "run_id": run_id,
        "claimed_at": now_rfc3339(),
    });
    match fs.create_new(&path) {
        Ok(mut f) => {
            f.write_all((body.to_string() + "\n").as_bytes())?;
            f.sync_data()?;
            drop(f);
            if let Some(m) = &modes.record {
                m.set(fs, &path, false)?;
            }
            fs.sync_dir(dest)?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing: serde_json::Value =
                serde_json::from_slice(&fs.read(&path)?).unwrap_or_default();
            let theirs = existing
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            if theirs == src {
                Ok(())
            } else {
                Err(CopyFail::Conflict(format!(
                    "destination {} is claimed on the share by run {} (host {}); not mixing data from {}",
                    escape_control(&dest.to_string_lossy()),
                    escape_control(theirs),
                    escape_control(existing.get("host").and_then(|v| v.as_str()).unwrap_or("?")),
                    escape_control(&src)
                )))
            }
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsio::RealFs;

    /// Verifies: SRS-LOG-006, SRS-RES-008
    #[test]
    fn compaction_keeps_the_time_of_each_commit() {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new("/data/exp/s/run1");
        let mut j = Journal::open(&RealFs, dir.path(), src).unwrap();
        j.append(
            &RealFs,
            Record::Claim {
                source: src.to_string_lossy().into_owned(),
                dest: "exp/s/run1".into(),
                dest_starts_at: DestLevel::default(),
                ts: "2026-01-02T03:04:05Z".into(),
            },
        )
        .unwrap();
        for (rel, ts) in [
            ("a.pod5", "2026-01-02T04:00:00Z"),
            ("a.pod5", "2026-01-02T05:00:00Z"),
            ("b.pod5", "2026-01-02T06:00:00Z"),
        ] {
            j.append(
                &RealFs,
                Record::File {
                    rel: rel.into(),
                    size: 1,
                    mtime_ns: 1,
                    sha256: "0".repeat(64),
                    ts: ts.into(),
                },
            )
            .unwrap();
        }
        j.compact(&RealFs).unwrap();
        let j = Journal::open(&RealFs, dir.path(), src).unwrap();
        assert_eq!(j.records(), 3);
        assert_eq!(j.claimed_at.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert_eq!(j.committed_at["a.pod5"], "2026-01-02T05:00:00Z");
        assert_eq!(j.committed_at["b.pod5"], "2026-01-02T06:00:00Z");
    }

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
