// SPDX-License-Identifier: AGPL-3.0-or-later
//! Gated source deletion (SWI-CLEANUP).
//!
//! A source run is deleted only when every gate passes, checked in this
//! order (cheap checks first):
//!
//! 1. **Finished**: the journal is complete, `COMPLETE` on the share holds
//!    the same manifest digest, and `delete_after` has passed since
//!    completion.
//! 2. **Share present**: the destination guard passes, the manifest on the
//!    share is the one the journal completed, every manifest entry is a
//!    committed file with the same digest, and every destination file has
//!    its committed size.
//! 3. **Content proof** (`gate`): `storage-verify` needs a `VERIFIED.json`
//!    from the receiving side that names the same manifest digest, file
//!    count and bytes and is newer than the completion; `readback` re-reads
//!    every destination file now (rate-limited); `both` needs both.
//! 4. **Source unchanged**: every source file still has its committed size
//!    and modification time, and with `rehash_source` its committed digest.
//!
//! A run that passes is deleted file by file: each file is checked again
//! (guard, destination size, source size and time) and appended to
//! `state_dir/deletions.jsonl` with its evidence before it is removed. Only
//! files in the manifest are deleted; folders are removed only when empty,
//! last; `DELETED.json` is written to the destination run folder and the
//! journal records the deletion. Any failure stops the pass. Every pass
//! appends a report to `state_dir/cleanup-reports.jsonl`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::units::Bytes;
use crate::config::{Config, DeleteMode, Gate};
use crate::error::{Error, Result};
use crate::fsio::{FileKind, Fs};
use crate::guard::Guard;
use crate::journal::{
    self, COMPLETE, Committed, DELETED, Journal, MANIFEST, Record, VERIFIED, parse_manifest,
    sha256_hex, write_atomic,
};
use crate::logging::escape_control;
use crate::run::safe_join;
use crate::sched::rate::RateLimiter;

/// Deletion log in `state_dir`: one line per deleted file, written before
/// the file is removed. Never rotated: it is the record of what was deleted.
pub const DELETIONS: &str = "deletions.jsonl";
/// Pass reports in `state_dir`: one line per cleanup pass.
pub const REPORTS: &str = "cleanup-reports.jsonl";
/// `schema` value of a pass report.
pub const REPORT_SCHEMA: &str = "ont-offload/cleanup-report/v1";

/// What happened to one run in a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Every gate passed and the source run was deleted.
    Deleted,
    /// Every gate passed; dry run, nothing deleted.
    WouldDelete,
    /// A gate is not met yet (too young, no receipt yet, share busy).
    Waiting,
    /// Every gate passed, but source free space is above
    /// `only_when_free_below` (or the pass stopped before this run).
    Held,
    /// A gate failed in a way that needs a person (digest or size
    /// mismatch, a changed source). Nothing of the run is deleted.
    Refused,
    /// Deletion started and stopped on an error; the rest of the pass was
    /// skipped. The next pass resumes the run.
    Stopped,
}

impl Outcome {
    /// The snake-case name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::WouldDelete => "would_delete",
            Self::Waiting => "waiting",
            Self::Held => "held",
            Self::Refused => "refused",
            Self::Stopped => "stopped",
        }
    }
}

/// Why a run may be deleted: recorded with every deleted file and in
/// `DELETED.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Gate setting used.
    pub gate: String,
    /// Manifest digest (journal, `COMPLETE` and share agree).
    pub manifest_sha256: String,
    /// When the run was completed.
    pub completed_at: String,
    /// `verified_at` of the storage-side receipt, when used.
    pub verified_at: Option<String>,
    /// Host that wrote the receipt, when used.
    pub verified_by: Option<String>,
    /// Every destination file was re-read and matched just now.
    pub readback: bool,
    /// Every source file was re-hashed and matched just now.
    pub source_rehashed: bool,
}

/// One run in a pass report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// Run id.
    pub run_id: String,
    /// Source run folder.
    pub source: String,
    /// Destination run folder.
    pub destination: String,
    /// Outcome.
    pub outcome: Outcome,
    /// Gate that is not met, or what happened.
    pub reason: String,
    /// Files in the manifest.
    pub files: u64,
    /// Bytes in the manifest.
    pub bytes: u64,
    /// Files deleted in this pass.
    pub deleted_files: u64,
    /// Entries left in the source run folder (not in the manifest).
    pub kept: Vec<String>,
    /// Evidence, once the gates passed.
    pub evidence: Option<Evidence>,
}

/// One cleanup pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Always [`REPORT_SCHEMA`].
    pub schema: String,
    /// ont-offload version.
    pub tool_version: String,
    /// Host name.
    pub host: String,
    /// When the pass started (RFC 3339).
    pub started_at: String,
    /// When it ended (RFC 3339).
    pub finished_at: String,
    /// `dry-run` or `delete`.
    pub mode: String,
    /// Gate setting.
    pub gate: String,
    /// Free bytes on the source when the pass started.
    pub source_free_bytes: Option<u64>,
    /// `only_when_free_below` in bytes, if set.
    pub only_when_free_below: Option<u64>,
    /// Runs considered, oldest completion first.
    pub runs: Vec<RunReport>,
    /// Why the pass stopped early, if it did.
    pub stopped: Option<String>,
}

impl Report {
    /// Runs with an outcome.
    #[must_use]
    pub fn count(&self, o: Outcome) -> usize {
        self.runs.iter().filter(|r| r.outcome == o).count()
    }

    /// Files deleted in the pass.
    #[must_use]
    pub fn deleted_files(&self) -> u64 {
        self.runs.iter().map(|r| r.deleted_files).sum()
    }

    /// Bytes of the runs deleted in the pass.
    #[must_use]
    pub fn deleted_bytes(&self) -> u64 {
        self.runs
            .iter()
            .filter(|r| r.outcome == Outcome::Deleted)
            .map(|r| r.bytes)
            .sum()
    }

    /// One line for logs and `cleanup` output.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut s = format!(
            "cleanup ({}, gate {}): {} deleted ({}), {} would be deleted, {} held, {} waiting, {} refused",
            self.mode,
            self.gate,
            self.count(Outcome::Deleted),
            Bytes(self.deleted_bytes()),
            self.count(Outcome::WouldDelete),
            self.count(Outcome::Held),
            self.count(Outcome::Waiting),
            self.count(Outcome::Refused),
        );
        if let Some(why) = &self.stopped {
            s.push_str("; STOPPED: ");
            s.push_str(why);
        }
        s
    }
}

/// One line of `deletions.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletionRecord {
    /// When (RFC 3339).
    pub ts: String,
    /// Run id.
    pub run_id: String,
    /// Source file (absolute).
    pub path: String,
    /// Path relative to the run.
    pub rel: String,
    /// Bytes.
    pub size: u64,
    /// Modification time in ns.
    pub mtime_ns: u64,
    /// SHA-256 hex.
    pub sha256: String,
    /// Destination copy.
    pub destination: String,
    /// Why it was safe.
    pub evidence: Evidence,
}

/// Result of the gates for one run.
enum Gated {
    Ready(Box<Plan>),
    Wait(String),
    Refuse(String),
}

/// A run that passed every gate.
struct Plan {
    source: PathBuf,
    dest: PathBuf,
    files: Vec<(String, Committed)>,
    bytes: u64,
    evidence: Evidence,
}

/// One cleanup pass over every completed run in `state_dir`.
pub struct Pass<'a> {
    /// Configuration.
    pub cfg: &'a Config,
    /// File system.
    pub fs: &'a dyn Fs,
    /// Destination guard.
    pub guard: &'a Guard,
    /// Wall-clock time used for `delete_after`.
    pub now: SystemTime,
    /// Delete (`true`) or only report.
    pub delete: bool,
    /// Only this source run folder.
    pub only: Option<PathBuf>,
    /// Run ids not to touch (still offloaded or watched by `watch`).
    pub skip: BTreeSet<String>,
    /// Shared rate limiter for read-back.
    pub rate: Option<&'a RateLimiter>,
    /// Stop between files when set (shutdown).
    pub stop: Option<&'a AtomicBool>,
    /// Append the report to [`REPORTS`].
    pub record: bool,
}

fn gate_name(g: Gate) -> &'static str {
    match g {
        Gate::StorageVerify => "storage-verify",
        Gate::Readback => "readback",
        Gate::Both => "both",
    }
}

/// The mode a pass runs in: `None` when deletion is off and no dry run was
/// asked for.
#[must_use]
pub fn mode(cfg: &Config, dry_run: bool) -> Option<bool> {
    match (cfg.cleanup.delete, dry_run) {
        (DeleteMode::Off, false) => None,
        (DeleteMode::On, false) => Some(true),
        _ => Some(false),
    }
}

fn stat_matches(fs: &dyn Fs, path: &Path, c: &Committed) -> std::result::Result<(), String> {
    match fs.symlink_metadata(path) {
        Ok(st) if st.kind != FileKind::File => Err("is no longer a regular file".into()),
        Ok(st) if st.len != c.size => {
            Err(format!("has {} bytes, {} were committed", st.len, c.size))
        }
        Ok(st) if journal::nanos(st.modified) != c.mtime_ns => {
            Err("modification time changed since it was committed".into())
        }
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn hash_limited(
    fs: &dyn Fs,
    path: &Path,
    chunk: usize,
    rate: Option<&RateLimiter>,
    stop: Option<&AtomicBool>,
) -> std::io::Result<(u64, String)> {
    let mut r = fs.open_read(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; chunk.max(4096)];
    let mut total = 0u64;
    loop {
        if stop.is_some_and(|s| s.load(Ordering::SeqCst)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "stopping",
            ));
        }
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if let Some(rate) = rate {
            rate.acquire(n as u64);
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, journal::hex(&h.finalize())))
}

impl Pass<'_> {
    fn stopping(&self) -> bool {
        self.stop.is_some_and(|s| s.load(Ordering::SeqCst))
    }

    fn chunk(&self) -> usize {
        usize::try_from(self.cfg.transfer.chunk_size).unwrap_or(8 << 20)
    }

    /// Run the pass.
    ///
    /// Implements: SRS-DEL-001, SRS-DEL-002, SRS-DEL-004, SRS-DEL-005
    ///
    /// # Errors
    /// I/O errors reading `state_dir`; a corrupt journal. Problems with a
    /// run or the share are in the report, not errors.
    #[allow(clippy::too_many_lines)]
    pub fn run(&self) -> Result<Report> {
        let state_dir = &self.cfg.state_dir.resolved;
        let mut report = Report {
            schema: REPORT_SCHEMA.into(),
            tool_version: env!("CARGO_PKG_VERSION").into(),
            host: journal::host_name(self.fs),
            started_at: journal::now_rfc3339(),
            finished_at: String::new(),
            mode: if self.delete { "delete" } else { "dry-run" }.into(),
            gate: gate_name(self.cfg.cleanup.gate).into(),
            source_free_bytes: self.fs.free_space(&self.cfg.source.root.resolved).ok(),
            only_when_free_below: self.cfg.cleanup.only_when_free_below,
            runs: Vec::new(),
            stopped: None,
        };
        // Gate 2, first half: never during an outage.
        if let Err(e) = self.guard.check(self.fs) {
            report.stopped = Some(format!("destination unavailable: {e}"));
            return self.finish(report);
        }
        let only_id = self.only.as_deref().map(journal::run_id);
        let mut journals: Vec<Journal> = Journal::list(self.fs, state_dir)?
            .into_iter()
            .filter(|j| {
                j.claim.is_some()
                    && j.complete.is_some()
                    && j.deleted_at.is_none()
                    && !self.skip.contains(&j.id)
                    && only_id.as_ref().is_none_or(|o| *o == j.id)
            })
            .collect();
        journals.sort_by(|a, b| a.completed_at.cmp(&b.completed_at).then(a.id.cmp(&b.id)));
        let logged = if journals.iter().any(|j| j.deleting.is_some()) {
            read_deletions(self.fs, state_dir)?
        } else {
            BTreeMap::new()
        };
        let empty = BTreeSet::new();

        let mut ready: Vec<(usize, Journal, Box<Plan>)> = Vec::new();
        for j in journals {
            if self.stopping() {
                report.stopped = Some("stopping".into());
                break;
            }
            let (source, dest) = self.paths(&j);
            let mut rr = RunReport {
                run_id: j.id.clone(),
                source: escape_control(&source.to_string_lossy()),
                destination: escape_control(&dest.to_string_lossy()),
                outcome: Outcome::Waiting,
                reason: String::new(),
                files: 0,
                bytes: 0,
                deleted_files: 0,
                kept: Vec::new(),
                evidence: None,
            };
            match self.check(&j, logged.get(&j.id).unwrap_or(&empty)) {
                Gated::Wait(why) => rr.reason = why,
                Gated::Refuse(why) => {
                    log::error!(
                        "cleanup: run {} is NOT deleted: {why}",
                        escape_control(&source.to_string_lossy())
                    );
                    rr.outcome = Outcome::Refused;
                    rr.reason = why;
                }
                Gated::Ready(plan) => {
                    rr.files = plan.files.len() as u64;
                    rr.bytes = plan.bytes;
                    rr.evidence = Some(plan.evidence.clone());
                    rr.outcome = Outcome::Held;
                    ready.push((report.runs.len(), j, plan));
                }
            }
            report.runs.push(rr);
        }

        // Free-space condition: oldest first, until the source has enough
        // room. A run already partly deleted is always finished.
        let mut free = report.source_free_bytes.unwrap_or(0);
        for (idx, mut j, plan) in ready {
            let rr = &mut report.runs[idx];
            if report.stopped.is_some() {
                rr.reason = "not attempted: the pass stopped".into();
                continue;
            }
            if let Some(limit) = self.cfg.cleanup.only_when_free_below {
                if j.deleting.is_none() && free >= limit {
                    rr.reason = format!(
                        "source has {} free, at least only_when_free_below = {}",
                        Bytes(free),
                        Bytes(limit)
                    );
                    continue;
                }
            }
            if !self.delete {
                rr.outcome = Outcome::WouldDelete;
                rr.reason = "every gate passed".into();
                free = free.saturating_add(plan.bytes);
                log::info!(
                    "cleanup (dry run): would delete {} ({} files, {})",
                    rr.source,
                    plan.files.len(),
                    Bytes(plan.bytes)
                );
                continue;
            }
            let done = logged.get(&j.id).unwrap_or(&empty);
            match self.delete_run(&mut j, &plan, done, rr) {
                Ok(()) => {
                    rr.outcome = Outcome::Deleted;
                    free = free.saturating_add(plan.bytes);
                }
                Err(why) => {
                    log::error!("cleanup: deleting {} stopped: {why}", rr.source);
                    rr.outcome = Outcome::Stopped;
                    rr.reason.clone_from(&why);
                    report.stopped = Some(format!("{}: {why}", rr.source));
                }
            }
        }
        self.finish(report)
    }

    fn finish(&self, mut report: Report) -> Result<Report> {
        report.finished_at = journal::now_rfc3339();
        log::info!("{}", report.summary());
        if self.record {
            let mut line =
                serde_json::to_vec(&report).map_err(|e| Error::Integrity(e.to_string()))?;
            line.push(b'\n');
            self.fs
                .append_sync(&self.cfg.state_dir.resolved.join(REPORTS), &line)?;
        }
        Ok(report)
    }

    fn paths(&self, j: &Journal) -> (PathBuf, PathBuf) {
        let (source, dest) = j
            .claim
            .as_ref()
            .map(|(s, d, _)| (s.as_str(), d.as_str()))
            .unwrap_or_default();
        let dest = safe_join(self.guard.dest(), Path::new(dest))
            .unwrap_or_else(|_| self.guard.dest().join("?"));
        (PathBuf::from(source), dest)
    }

    /// The four gates for one run.
    ///
    /// Implements: SRS-DEL-002, SRS-DEL-003
    #[allow(clippy::too_many_lines)]
    fn check(&self, j: &Journal, already: &BTreeSet<String>) -> Gated {
        let (source, dest) = self.paths(j);
        let Some(digest) = j.complete.clone() else {
            return Gated::Wait("not complete".into());
        };
        // 1. Finished and old enough.
        let completed_at = j.completed_at.clone().unwrap_or_default();
        let Ok(completed) = humantime::parse_rfc3339_weak(&completed_at) else {
            return Gated::Refuse(format!(
                "completion time {completed_at:?} in the journal cannot be read"
            ));
        };
        let age = self.now.duration_since(completed).unwrap_or_default();
        let after = self.cfg.cleanup.delete_after;
        if j.deleting.is_none() && age < after {
            return Gated::Wait(format!(
                "completed {} ago; delete_after is {}",
                humantime::format_duration(Duration::from_secs(age.as_secs())),
                humantime::format_duration(after)
            ));
        }
        match self.fs.read(&dest.join(COMPLETE)) {
            Ok(b) => {
                let text = String::from_utf8_lossy(&b);
                if text.trim() != format!("manifest_sha256={digest}") {
                    return Gated::Refuse(format!(
                        "{COMPLETE} on the share names another manifest than the journal"
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Gated::Wait(format!(
                    "{COMPLETE} is not on the share (withdrawn or not yet written)"
                ));
            }
            Err(e) => return Gated::Wait(format!("cannot read {COMPLETE}: {e}")),
        }

        // 2. Share present: the manifest and every file size.
        let manifest_bytes = match self.fs.read(&dest.join(MANIFEST)) {
            Ok(b) => b,
            Err(e) => return Gated::Refuse(format!("cannot read {MANIFEST} on the share: {e}")),
        };
        if sha256_hex(&manifest_bytes) != digest {
            return Gated::Refuse(format!(
                "{MANIFEST} on the share differs from the one the journal completed"
            ));
        }
        let manifest = match parse_manifest(&String::from_utf8_lossy(&manifest_bytes)) {
            Ok(m) => m,
            Err(e) => return Gated::Refuse(e.to_string()),
        };
        let mut files = Vec::with_capacity(manifest.len());
        let mut bytes = 0u64;
        for (rel, sha) in &manifest {
            let Some(c) = j.files.get(rel) else {
                return Gated::Refuse(format!(
                    "{} is in the manifest but not in the journal",
                    escape_control(rel)
                ));
            };
            if c.sha256 != *sha {
                return Gated::Refuse(format!(
                    "{}: the manifest and the journal disagree",
                    escape_control(rel)
                ));
            }
            let Ok(target) = safe_join(&dest, Path::new(rel)) else {
                return Gated::Refuse(format!("{} is not a plain path", escape_control(rel)));
            };
            match self.fs.metadata(&target) {
                Ok(st) if st.len == c.size => {}
                Ok(st) => {
                    return Gated::Refuse(format!(
                        "{} has {} bytes on the share, {} were committed",
                        escape_control(rel),
                        st.len,
                        c.size
                    ));
                }
                Err(e) => {
                    return Gated::Refuse(format!("{} on the share: {e}", escape_control(rel)));
                }
            }
            bytes += c.size;
            files.push((rel.clone(), c.clone()));
        }
        let mut evidence = Evidence {
            gate: gate_name(self.cfg.cleanup.gate).into(),
            manifest_sha256: digest.clone(),
            completed_at: completed_at.clone(),
            ..Evidence::default()
        };

        // 3. Content proof.
        let gate = self.cfg.cleanup.gate;
        if matches!(gate, Gate::StorageVerify | Gate::Both) {
            let receipt: serde_json::Value = match self.fs.read(&dest.join(VERIFIED)) {
                Ok(b) => match serde_json::from_slice(&b) {
                    Ok(v) => v,
                    Err(e) => return Gated::Refuse(format!("{VERIFIED} cannot be read: {e}")),
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Gated::Wait(format!(
                        "waiting for storage-side verification ({VERIFIED})"
                    ));
                }
                Err(e) => return Gated::Wait(format!("cannot read {VERIFIED}: {e}")),
            };
            let s = |k: &str| receipt.get(k).and_then(|v| v.as_str()).unwrap_or_default();
            let n = |k: &str| receipt.get(k).and_then(serde_json::Value::as_u64);
            if s("result") != "ok" {
                return Gated::Refuse(format!("{VERIFIED} does not say \"ok\""));
            }
            if s("manifest_sha256") != digest {
                return Gated::Refuse(format!(
                    "{VERIFIED} is for another manifest than the one completed"
                ));
            }
            if n("files") != Some(files.len() as u64) || n("bytes") != Some(bytes) {
                return Gated::Refuse(format!(
                    "{VERIFIED} names other file or byte counts than the manifest"
                ));
            }
            let verified_at = s("verified_at").to_owned();
            match humantime::parse_rfc3339_weak(&verified_at) {
                Ok(t) if t >= completed => {}
                Ok(_) => {
                    return Gated::Wait(format!(
                        "{VERIFIED} is older than the completion; waiting for a new verification"
                    ));
                }
                Err(_) => return Gated::Refuse(format!("{VERIFIED} has no readable verified_at")),
            }
            evidence.verified_at = Some(verified_at);
            evidence.verified_by = Some(escape_control(s("host")));
        }
        if matches!(gate, Gate::Readback | Gate::Both) {
            for (rel, c) in &files {
                let Ok(target) = safe_join(&dest, Path::new(rel)) else {
                    return Gated::Refuse(format!("{} is not a plain path", escape_control(rel)));
                };
                match hash_limited(self.fs, &target, self.chunk(), self.rate, self.stop) {
                    Ok((len, sha)) if len == c.size && sha == c.sha256 => {}
                    Ok(_) => {
                        return Gated::Refuse(format!(
                            "{} on the share does not match its committed digest",
                            escape_control(rel)
                        ));
                    }
                    Err(e) => {
                        return Gated::Wait(format!(
                            "read-back of {} failed: {e}",
                            escape_control(rel)
                        ));
                    }
                }
            }
            evidence.readback = true;
        }

        // 4. Source unchanged.
        for (rel, c) in &files {
            let Ok(path) = safe_join(&source, Path::new(rel)) else {
                return Gated::Refuse(format!("{} is not a plain path", escape_control(rel)));
            };
            if already.contains(rel)
                && matches!(self.fs.symlink_metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
            {
                continue;
            }
            if let Err(why) = stat_matches(self.fs, &path, c) {
                return Gated::Refuse(format!("source {}: {why}", escape_control(rel)));
            }
            if self.cfg.cleanup.rehash_source {
                match hash_limited(self.fs, &path, self.chunk(), None, self.stop) {
                    Ok((_, sha)) if sha == c.sha256 => {}
                    Ok(_) => {
                        return Gated::Refuse(format!(
                            "source {} no longer matches its committed digest",
                            escape_control(rel)
                        ));
                    }
                    Err(e) => {
                        return Gated::Wait(format!(
                            "re-hash of source {} failed: {e}",
                            escape_control(rel)
                        ));
                    }
                }
            }
        }
        evidence.source_rehashed = self.cfg.cleanup.rehash_source;
        Gated::Ready(Box::new(Plan {
            source,
            dest,
            files,
            bytes,
            evidence,
        }))
    }

    /// Delete one run that passed every gate.
    ///
    /// Implements: SRS-DEL-003, SRS-DEL-004
    #[allow(clippy::too_many_lines)]
    fn delete_run(
        &self,
        j: &mut Journal,
        plan: &Plan,
        already: &BTreeSet<String>,
        rr: &mut RunReport,
    ) -> std::result::Result<(), String> {
        let state_dir = &self.cfg.state_dir.resolved;
        if j.deleting.is_none() {
            j.append(
                self.fs,
                Record::Deleting {
                    ts: journal::now_rfc3339(),
                },
            )
            .map_err(|e| format!("journal: {e}"))?;
        }
        // Files in sub-folders first, files at the top of the run (where
        // the run markers are) last.
        let mut order: Vec<&(String, Committed)> = plan.files.iter().collect();
        order.sort_by_key(|(rel, _)| (!rel.contains('/'), rel.clone()));
        let mut dirs = BTreeSet::new();
        for (rel, c) in order {
            if self.stopping() {
                return Err("stopping; the next pass resumes this run".into());
            }
            let src = safe_join(&plan.source, Path::new(rel)).map_err(|e| e.to_string())?;
            let mut parent = src.parent();
            while let Some(p) = parent {
                if !p.starts_with(&plan.source) || p == plan.source {
                    break;
                }
                dirs.insert(p.to_path_buf());
                parent = p.parent();
            }
            let shown = escape_control(rel);
            match self.fs.symlink_metadata(&src) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && already.contains(rel) => {
                    continue;
                }
                _ => stat_matches(self.fs, &src, c)
                    .map_err(|why| format!("source {shown}: {why}"))?,
            }
            self.guard
                .check(self.fs)
                .map_err(|e| format!("destination guard: {e}"))?;
            let target = safe_join(&plan.dest, Path::new(rel)).map_err(|e| e.to_string())?;
            match self.fs.metadata(&target) {
                Ok(st) if st.len == c.size => {}
                Ok(st) => {
                    return Err(format!(
                        "{shown} has {} bytes on the share, {} were committed",
                        st.len, c.size
                    ));
                }
                Err(e) => return Err(format!("{shown} on the share: {e}")),
            }
            let rec = DeletionRecord {
                ts: journal::now_rfc3339(),
                run_id: j.id.clone(),
                path: src.to_string_lossy().into_owned(),
                rel: rel.clone(),
                size: c.size,
                mtime_ns: c.mtime_ns,
                sha256: c.sha256.clone(),
                destination: target.to_string_lossy().into_owned(),
                evidence: plan.evidence.clone(),
            };
            let mut line = serde_json::to_vec(&rec).map_err(|e| e.to_string())?;
            line.push(b'\n');
            self.fs
                .append_sync(&state_dir.join(DELETIONS), &line)
                .map_err(|e| format!("{DELETIONS}: {e}"))?;
            self.fs
                .remove_file(&src)
                .map_err(|e| format!("removing source {shown}: {e}"))?;
            rr.deleted_files += 1;
        }
        // Empty folders last, deepest first; a folder with anything left in
        // it stays (remove_dir refuses).
        let mut dirs: Vec<PathBuf> = dirs.into_iter().collect();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in dirs.iter().chain(std::iter::once(&plan.source)) {
            if let Err(e) = self.fs.remove_dir(d) {
                log::debug!(
                    "cleanup: keeping {}: {e}",
                    escape_control(&d.to_string_lossy())
                );
            }
        }
        rr.kept = leftovers(self.fs, &plan.source);
        let removed = self.fs.symlink_metadata(&plan.source).is_err();
        if let Some(parent) = plan.source.parent() {
            let _ = self.fs.sync_dir(parent);
        }
        self.guard
            .check(self.fs)
            .map_err(|e| format!("destination guard before {DELETED}: {e}"))?;
        let record = serde_json::json!({
            "tool": "ont-offload",
            "version": env!("CARGO_PKG_VERSION"),
            "deleted_at": journal::now_rfc3339(),
            "host": journal::host_name(self.fs),
            "run_id": j.id,
            "source": plan.source.to_string_lossy(),
            "files": plan.files.len(),
            "bytes": plan.bytes,
            "evidence": plan.evidence,
            "source_folder_removed": removed,
            "kept": rr.kept,
            "deletions_log": state_dir.join(DELETIONS).to_string_lossy(),
        });
        let text = serde_json::to_string_pretty(&record).unwrap_or_default() + "\n";
        write_atomic(self.fs, &plan.dest, DELETED, text.as_bytes())
            .map_err(|e| format!("writing {DELETED}: {e}"))?;
        j.append(
            self.fs,
            Record::Deleted {
                ts: journal::now_rfc3339(),
            },
        )
        .map_err(|e| format!("journal: {e}"))?;
        rr.reason = if removed {
            "deleted; source folder removed".into()
        } else {
            format!(
                "deleted; {} other entries kept in the source folder",
                rr.kept.len()
            )
        };
        log::info!(
            "cleanup: deleted {} ({} files, {}){}",
            rr.source,
            rr.deleted_files,
            Bytes(plan.bytes),
            if removed { "" } else { "; folder kept" }
        );
        Ok(())
    }
}

/// Entries left below a source run folder after deletion (relative paths,
/// files and folders that are not empty; at most 100).
fn leftovers(fs: &dyn Fs, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((d, prefix)) = stack.pop() {
        let Ok(entries) = fs.read_dir(&d) else {
            continue;
        };
        for e in entries {
            let rel = format!("{prefix}{}", e.name.to_string_lossy());
            if e.kind == FileKind::Dir {
                stack.push((d.join(&e.name), format!("{rel}/")));
            } else if out.len() < 100 {
                out.push(escape_control(&rel));
            }
        }
    }
    out.sort();
    out
}

/// Files already deleted, by run id, from [`DELETIONS`].
///
/// # Errors
/// I/O errors other than a missing log.
pub fn read_deletions(fs: &dyn Fs, state_dir: &Path) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let bytes = match fs.read(&state_dir.join(DELETIONS)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in String::from_utf8_lossy(&bytes).lines() {
        if let Ok(r) = serde_json::from_str::<DeletionRecord>(line) {
            out.entry(r.run_id).or_default().insert(r.rel);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
