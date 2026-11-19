// SPDX-License-Identifier: AGPL-3.0-or-later
//! The verification record of an execution: did ont-offload copy every
//! run of a simulation (ont-runsim) or of a MinKNOW mock run completely and
//! correctly? The expected content comes from a reference that ont-offload
//! did not produce: the simulator's manifests, written as each file was
//! closed, or a fresh re-hash of the source runs with MinKNOW's own file
//! counts. Every check is decided here; a check without its input is
//! inconclusive, never passed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::copy::{hash_file, is_temp_name};
use crate::fsio::{FileKind, Fs};
use crate::journal::{
    COMPLETE, MANIFEST, RESERVED_NAMES, RUN_JSON, VERIFIED, parse_manifest, sha256_hex,
};
use crate::state::Kind;

use super::history::{self, InputSummary};
use super::time::{self, Period};

/// `schema` of a verification record.
pub const VERIFICATION_SCHEMA: &str = "ont-offload/verification-report/v1";

/// Check results.
pub mod result {
    /// The check passed.
    pub const PASS: &str = "pass";
    /// The check failed.
    pub const FAIL: &str = "fail";
    /// The check could not be decided (input missing, step not performed).
    pub const INCONCLUSIVE: &str = "inconclusive";
    /// Nothing to check (e.g. no aborted run in the execution).
    pub const NA: &str = "n/a";
}

/// A test protocol: which checks are required and the limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Protocol {
    /// Name.
    pub name: String,
    /// Version.
    pub version: String,
    /// What the protocol shows.
    pub description: String,
    /// Longest time from the end of a run at the source to `COMPLETE`.
    pub max_minutes_to_complete: u64,
    /// Checks that must pass.
    pub required: Vec<String>,
    /// Checks shown for information.
    pub informational: Vec<String>,
}

/// The built-in protocols by name.
pub const BUILTIN: [(&str, &str); 3] = [
    ("simulation", include_str!("protocols/simulation.toml")),
    (
        "p2i-acceptance",
        include_str!("protocols/p2i-acceptance.toml"),
    ),
    ("minknow-mock", include_str!("protocols/minknow-mock.toml")),
];

impl Protocol {
    /// A built-in protocol by name, or a protocol file; with its SHA-256.
    ///
    /// # Errors
    /// An unknown name, an unreadable or invalid file, or an unknown check.
    pub fn load(fs: &dyn Fs, name_or_path: &str) -> Result<(Self, String), String> {
        let text = if let Some((_, t)) = BUILTIN.iter().find(|(n, _)| *n == name_or_path) {
            (*t).to_owned()
        } else {
            let bytes = fs.read(Path::new(name_or_path)).map_err(|e| {
                format!(
                    "--protocol {name_or_path}: not a built-in protocol ({}) and not readable: {e}",
                    BUILTIN.map(|(n, _)| n).join(", ")
                )
            })?;
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let p: Self =
            toml::from_str(&text).map_err(|e| format!("--protocol {name_or_path}: {e}"))?;
        for id in p.required.iter().chain(&p.informational) {
            if !CHECKS.iter().any(|c| c.id == id) {
                return Err(format!("--protocol {name_or_path}: unknown check {id}"));
            }
        }
        Ok((p, sha256_hex(text.as_bytes())))
    }
}

/// A check in the catalogue.
#[derive(Debug, Clone, Copy)]
pub struct CheckDef {
    /// Id, e.g. `VC-03`.
    pub id: &'static str,
    /// What is checked.
    pub title: &'static str,
    /// What passes.
    pub expected: &'static str,
    /// Requirements it verifies.
    pub requirements: &'static [&'static str],
}

/// Every check, in report order.
pub const CHECKS: [CheckDef; 16] = [
    CheckDef {
        id: "VC-01",
        title: "Every run discovered",
        expected: "Each reference run has a run_discovered event",
        requirements: &["SRS-SEL-001", "SRS-LIVE-001"],
    },
    CheckDef {
        id: "VC-02",
        title: "Every finished run completed",
        expected: "COMPLETE, manifest.sha256 and run.json on the share for each run that finished at the source",
        requirements: &["SRS-VER-002", "SRS-LIVE-003"],
    },
    CheckDef {
        id: "VC-03",
        title: "Content identical to the reference",
        expected: "0 missing, 0 changed, 0 extra files; every file re-hashed on the share",
        requirements: &["SRS-CPY-001", "SRS-VER-001"],
    },
    CheckDef {
        id: "VC-04",
        title: "ont-offload's manifest agrees with the reference",
        expected: "manifest.sha256 lists the same files with the same digests",
        requirements: &["SRS-VER-001", "SRS-VER-002"],
    },
    CheckDef {
        id: "VC-05",
        title: "Files written after the final summary copied",
        expected: "Every file newer than the final summary is on the share and matches",
        requirements: &["SRS-LIVE-003"],
    },
    CheckDef {
        id: "VC-06",
        title: "Rewritten files copied with their final content",
        expected: "Every file the simulator rewrote after closing it matches its final content",
        requirements: &["SRS-LIVE-002"],
    },
    CheckDef {
        id: "VC-07",
        title: "Aborted runs copied but not completed",
        expected: "Files of a run without a final summary are copied; no COMPLETE",
        requirements: &["SRS-LIVE-003"],
    },
    CheckDef {
        id: "VC-08",
        title: "Outage detected and recovered",
        expected: "Each outage_started has its outage_ended; content still identical",
        requirements: &["SRS-RES-005", "SRS-LOG-004"],
    },
    CheckDef {
        id: "VC-09",
        title: "Restart resumes without copying again",
        expected: "After each restart no file is copied again with the same content",
        requirements: &["SRS-RES-004"],
    },
    CheckDef {
        id: "VC-10",
        title: "No unresolved failures",
        expected: "0 file_failed and 0 run_failed for the reference runs",
        requirements: &["SRS-RES-001", "SRS-CPY-002"],
    },
    CheckDef {
        id: "VC-11",
        title: "Receipts on the share",
        expected: "VERIFIED.json per completed run with the digest of its manifest",
        requirements: &["SRS-VER-003"],
    },
    CheckDef {
        id: "VC-12",
        title: "Time to COMPLETE",
        expected: "COMPLETE within the protocol's limit after the run ended at the source",
        requirements: &["SRS-LIVE-003"],
    },
    CheckDef {
        id: "VC-13",
        title: "Event stream complete",
        expected: "Every event of the execution parses; no sequence gaps",
        requirements: &["SRS-STATE-002", "SRS-RPT-002"],
    },
    CheckDef {
        id: "VC-14",
        title: "Records cover the execution",
        expected: "Events from before the first run to after the last completion, without gaps",
        requirements: &["SRS-LOG-006", "SRS-RPT-002"],
    },
    CheckDef {
        id: "VC-15",
        title: "Cleanup only deletes verified copies",
        expected: "No source run deleted whose copy failed a check",
        requirements: &["SRS-DEL-001"],
    },
    CheckDef {
        id: "VC-16",
        title: "File counts agree with MinKNOW",
        expected: "pod5, FASTQ and BAM files on the share as counted in final_summary",
        requirements: &["SRS-SEL-001"],
    },
];

/// The whole verification record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerificationRecord {
    /// Always [`VERIFICATION_SCHEMA`].
    pub schema: String,
    /// 1.
    pub schema_version: u32,
    /// Report id, e.g. `OOF-VR-20261117-c41d2e`.
    pub report_id: String,
    /// When the report was made (RFC 3339).
    pub generated_at: String,
    /// ont-offload version that made it.
    pub tool_version: String,
    /// Host it was made on.
    pub host: String,
    /// Site name from `[report]`.
    pub site: Option<String>,
    /// The protocol.
    pub protocol: ProtocolOut,
    /// `simulation` or `minknow`.
    pub mode: String,
    /// The data was simulated (every page is marked).
    pub simulated: bool,
    /// Where the expected content came from.
    pub reference: Reference,
    /// The execution window.
    pub window: Window,
    /// Who ran the execution (`--operator`).
    pub operator: Option<String>,
    /// Deviations from the protocol (`--deviation`).
    pub deviations: Vec<String>,
    /// The verdict.
    pub verdict: VerdictOut,
    /// The checks of the protocol.
    pub checks: Vec<CheckOut>,
    /// Each reference run.
    pub runs: Vec<RunOut>,
    /// What happened, in order.
    pub timeline: Vec<TimelineEntry>,
    /// Versions, settings and the share.
    pub environment: Vec<EnvEntry>,
    /// The records read.
    pub inputs: Vec<InputSummary>,
}

/// The protocol as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolOut {
    /// Name.
    pub name: String,
    /// Version.
    pub version: String,
    /// What it shows.
    pub description: String,
    /// SHA-256 of the protocol file.
    pub sha256: String,
    /// Limit for VC-12.
    pub max_minutes_to_complete: u64,
}

/// The reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    /// What it is.
    pub kind: String,
    /// Where.
    pub path: String,
    /// The simulator's version, scenario and state.
    pub detail: Option<String>,
}

/// The execution window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// Start (RFC 3339).
    pub from: String,
    /// End (RFC 3339).
    pub to: String,
}

/// The verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictOut {
    /// `pass`, `fail` or `inconclusive`.
    pub status: String,
    /// One sentence.
    pub headline: String,
    /// Required checks that passed.
    pub required_passed: u64,
    /// Required checks.
    pub required: u64,
}

/// One check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckOut {
    /// Id.
    pub id: String,
    /// What is checked.
    pub title: String,
    /// What passes.
    pub expected: String,
    /// Requirements it verifies.
    pub requirements: Vec<String>,
    /// Required by the protocol (else informational).
    pub required: bool,
    /// What was found.
    pub observed: String,
    /// `pass`, `fail`, `inconclusive` or `n/a`.
    pub result: String,
}

/// One reference run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOut {
    /// Run folder name.
    pub name: String,
    /// State at the source: `complete`, `aborted`, `sequencing`...
    pub source_state: String,
    /// Destination folder (from `run_complete`), if known.
    pub destination: Option<String>,
    /// Files in the reference.
    pub expected: u64,
    /// Files present with the reference content.
    pub matched: u64,
    /// Bytes re-hashed on the share.
    pub bytes: u64,
    /// Missing on the share.
    pub missing: Vec<String>,
    /// Different on the share.
    pub changed: Vec<String>,
    /// On the share but not in the reference.
    pub extra: Vec<String>,
    /// `COMPLETE` is present.
    pub complete: bool,
    /// ont-offload's manifest agrees with the reference (`null`: none).
    pub manifest_agrees: Option<bool>,
    /// `VERIFIED.json` names the manifest's digest (`null`: none).
    pub receipt: Option<bool>,
    /// Seconds from the end at the source to `COMPLETE`.
    pub latency_s: Option<u64>,
    /// `pass` or `fail`.
    pub result: String,
}

/// One entry of the timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    /// Time (RFC 3339).
    pub ts: String,
    /// `ont-offload` or `ont-runsim`.
    pub source: String,
    /// Event kind.
    pub kind: String,
    /// Detail.
    pub detail: String,
}

/// One line of the environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvEntry {
    /// What.
    pub key: String,
    /// Value.
    pub value: String,
}

/// Where the reference runs come from.
#[derive(Debug, Clone)]
pub enum Mode {
    /// An ont-runsim output folder.
    Simulation(PathBuf),
    /// MinKNOW run folders at the source.
    Minknow(Vec<PathBuf>),
}

/// Everything a verification record needs besides the records.
#[derive(Debug, Clone)]
pub struct VerifyInputs {
    /// Reference.
    pub mode: Mode,
    /// Protocol and its digest.
    pub protocol: (Protocol, String),
    /// State folder of the daemon.
    pub state_dir: PathBuf,
    /// Log file, if any.
    pub log: Option<PathBuf>,
    /// Window, if given (else from the reference and the events).
    pub window: Option<Period>,
    /// Report time.
    pub generated_at: u64,
    /// Host.
    pub host: String,
    /// Site.
    pub site: Option<String>,
    /// Id prefix.
    pub prefix: String,
    /// Operator.
    pub operator: Option<String>,
    /// Deviations.
    pub deviations: Vec<String>,
    /// Environment lines from the configuration.
    pub environment: Vec<EnvEntry>,
}

/// One reference run: its files and digests, and how it ended.
#[derive(Debug, Clone, Default)]
struct RefRun {
    name: String,
    source: Option<PathBuf>,
    state: String,
    files: BTreeMap<String, String>,
    ended_at: Option<u64>,
    rewritten: BTreeSet<String>,
    late: Option<BTreeSet<String>>,
    counts: BTreeMap<String, u64>,
    counted: BTreeMap<String, u64>,
    problem: Option<String>,
}

/// What the event stream says about one run.
#[derive(Debug, Clone, Default)]
struct Seen {
    discovered: Option<u64>,
    completed: Option<u64>,
    destination: Option<String>,
    failed: Vec<String>,
    files_failed: u64,
    deleted: bool,
}

fn leaf(run: &str) -> &str {
    run.rsplit('/').next().unwrap_or(run)
}

/// The reference runs of an ont-runsim output folder.
#[allow(clippy::too_many_lines)]
fn runsim_reference(
    fs: &dyn Fs,
    out: &Path,
    timeline: &mut Vec<TimelineEntry>,
) -> (Vec<RefRun>, Reference, Option<(u64, u64)>) {
    let state = out.join(".ont-runsim");
    let mut runs: Vec<RefRun> = Vec::new();
    let status: Option<Value> = fs
        .read(&state.join("status.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let mut detail = None;
    let mut span = None;
    if let Some(s) = &status {
        detail = Some(format!(
            "ont-runsim {}, simulation {}, speed {}x, {} of {} files",
            s.get("version").and_then(Value::as_str).unwrap_or("?"),
            s.get("state").and_then(Value::as_str).unwrap_or("?"),
            s.get("speed").and_then(Value::as_f64).unwrap_or(0.0),
            s.get("files_closed").and_then(Value::as_u64).unwrap_or(0),
            s.get("files_planned").and_then(Value::as_u64).unwrap_or(0),
        ));
        let started = s
            .get("started")
            .and_then(Value::as_str)
            .and_then(time::parse);
        let updated = s
            .get("updated")
            .and_then(Value::as_str)
            .and_then(time::parse);
        span = started.zip(updated);
        for r in s
            .get("runs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = r
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if name.is_empty() {
                continue;
            }
            let mut rr = RefRun {
                source: r.get("path").and_then(Value::as_str).map(PathBuf::from),
                state: r
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .into(),
                ..RefRun::default()
            };
            let manifest = state.join("manifests").join(format!("{name}.sha256"));
            match fs
                .read(&manifest)
                .map(|b| parse_manifest(&String::from_utf8_lossy(&b)))
            {
                Ok(Ok(m)) => rr.files = m,
                Ok(Err(e)) => rr.problem = Some(format!("{}: {e}", manifest.display())),
                Err(e) => rr.problem = Some(format!("{}: {e}", manifest.display())),
            }
            rr.name = name;
            runs.push(rr);
        }
    }
    // Without a status snapshot (a simulation driven in-process), the
    // list of run folders names the runs.
    if runs.is_empty() {
        if let Ok(b) = fs.read(&state.join("runs.txt")) {
            for rel in String::from_utf8_lossy(&b)
                .lines()
                .filter(|l| !l.trim().is_empty())
            {
                let rel = rel.trim();
                let mut rr = RefRun {
                    name: leaf(rel).to_owned(),
                    source: Some(out.join(rel)),
                    state: "sequencing".into(),
                    ..RefRun::default()
                };
                let manifest = state.join("manifests").join(format!("{}.sha256", rr.name));
                match fs
                    .read(&manifest)
                    .map(|b| parse_manifest(&String::from_utf8_lossy(&b)))
                {
                    Ok(Ok(m)) => rr.files = m,
                    Ok(Err(e)) => rr.problem = Some(format!("{}: {e}", manifest.display())),
                    Err(e) => rr.problem = Some(format!("{}: {e}", manifest.display())),
                }
                runs.push(rr);
            }
        }
    }
    // The simulator's own events: when each run ended, which files it rewrote.
    let events = state.join("events.jsonl");
    if let Ok(bytes) = fs.read(&events) {
        for line in String::from_utf8_lossy(&bytes).lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let ev = v.get("event").and_then(Value::as_str).unwrap_or_default();
            let ts = v.get("ts").and_then(Value::as_str).unwrap_or_default();
            let run = v.get("run").and_then(Value::as_str);
            let det = v.get("detail").and_then(Value::as_str).unwrap_or_default();
            let r = run.and_then(|n| runs.iter_mut().find(|r| r.name == n));
            match (ev, r) {
                ("run_complete" | "run_aborted", Some(r)) => {
                    r.ended_at = time::parse(ts);
                    r.state = ev.trim_start_matches("run_").into();
                }
                ("fault_rewrite", Some(r)) => {
                    r.rewritten.insert(det.to_owned());
                }
                _ => {}
            }
            if !matches!(ev, "fault_rewrite") {
                timeline.push(TimelineEntry {
                    ts: ts.to_owned(),
                    source: "ont-runsim".into(),
                    kind: ev.to_owned(),
                    detail: run.map_or_else(|| det.to_owned(), |r| format!("{r}: {det}")),
                });
            }
        }
    }
    (
        runs,
        Reference {
            kind: "manifests written by the run simulator as each file was closed".into(),
            path: state.join("manifests").to_string_lossy().into_owned(),
            detail,
        },
        span,
    )
}

/// Every regular file below `root` (relative paths), leaving out
/// ont-offload's own top-level files and temp files.
fn walk(fs: &dyn Fs, root: &Path) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((d, prefix)) = stack.pop() {
        for e in fs.read_dir(&d)? {
            let name = e.name.to_string_lossy().into_owned();
            let rel = format!("{prefix}{name}");
            match e.kind {
                FileKind::Dir => stack.push((d.join(&e.name), format!("{rel}/"))),
                FileKind::File => {
                    if !((prefix.is_empty() && RESERVED_NAMES.contains(&name.as_str()))
                        || is_temp_name(&name))
                    {
                        out.push(rel);
                    }
                }
                _ => {}
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A MinKNOW run at the source: re-hash every file, read the final summary.
fn minknow_reference(fs: &dyn Fs, run: &Path) -> RefRun {
    let name = run
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut rr = RefRun {
        name,
        source: Some(run.to_path_buf()),
        state: "unknown".into(),
        ..RefRun::default()
    };
    let files = match walk(fs, run) {
        Ok(f) => f,
        Err(e) => {
            rr.problem = Some(format!(
                "{}: cannot list the source run: {e}",
                run.display()
            ));
            return rr;
        }
    };
    let mut summary: Option<(String, u64)> = None;
    for rel in &files {
        match hash_file(fs, &run.join(rel), 8 << 20) {
            Ok((_, digest)) => {
                rr.files.insert(rel.clone(), digest);
            }
            Err(e) => {
                rr.problem = Some(format!("{rel}: cannot read at the source: {e}"));
            }
        }
        if !rel.contains('/')
            && rel.starts_with("final_summary_")
            && Path::new(rel).extension().is_some_and(|e| e == "txt")
        {
            let mtime = fs
                .metadata(&run.join(rel))
                .ok()
                .map(|m| crate::journal::nanos(m.modified) / 1_000_000_000);
            summary = Some((rel.clone(), mtime.unwrap_or(0)));
        }
    }
    if let Some((rel, mtime)) = summary {
        rr.state = "complete".into();
        rr.ended_at = Some(mtime);
        if let Ok(b) = fs.read(&run.join(&rel)) {
            for line in String::from_utf8_lossy(&b).lines() {
                if let Some((k, v)) = line.split_once('=') {
                    if k.ends_with("_files_in_final_dest") {
                        if let Ok(n) = v.trim().parse() {
                            rr.counts
                                .insert(k.trim_end_matches("_files_in_final_dest").to_owned(), n);
                        }
                    }
                }
            }
        }
        // Files modified after the final summary was written.
        let late: BTreeSet<String> = files
            .iter()
            .filter(|f| {
                **f != rel
                    && fs
                        .metadata(&run.join(f))
                        .ok()
                        .is_some_and(|m| crate::journal::nanos(m.modified) / 1_000_000_000 > mtime)
            })
            .cloned()
            .collect();
        rr.late = Some(late);
    } else {
        rr.state = "no final summary".into();
    }
    rr
}

/// Files of a MinKNOW output type on the share.
fn count_type(files: &[String], kind: &str) -> u64 {
    let exts: &[&str] = match kind {
        "pod5" => &[".pod5"],
        "fast5" => &[".fast5"],
        "fastq" => &[".fastq", ".fastq.gz", ".fq", ".fq.gz"],
        "bam" => &[".bam"],
        _ => return 0,
    };
    files
        .iter()
        .filter(|f| exts.iter().any(|e| f.ends_with(e)))
        .count() as u64
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A commit of a file: its digest, the restarts before it, adopted.
type Commit = (String, usize, bool);

/// Build the verification record.
///
/// Implements: SRS-RPT-003, SRS-RPT-005
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build(fs: &dyn Fs, inp: &VerifyInputs) -> VerificationRecord {
    let (protocol, protocol_sha) = &inp.protocol;
    let mut timeline = Vec::new();
    let (mut refs, reference, span, simulated, mode) = match &inp.mode {
        Mode::Simulation(out) => {
            let (r, reference, span) = runsim_reference(fs, out, &mut timeline);
            (r, reference, span, true, "simulation")
        }
        Mode::Minknow(runs) => (
            runs.iter().map(|r| minknow_reference(fs, r)).collect(),
            Reference {
                kind: "source runs re-hashed now, with the file counts of MinKNOW's final summary"
                    .into(),
                path: runs
                    .iter()
                    .map(|r| r.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", "),
                detail: None,
            },
            None,
            false,
            "minknow",
        ),
    };
    let names: BTreeSet<String> = refs.iter().map(|r| r.name.clone()).collect();

    // The events: everything about the reference runs and the daemon.
    let mut seen: BTreeMap<String, Seen> = BTreeMap::new();
    let mut starts: Vec<u64> = Vec::new();
    let mut outages_open = 0i64;
    let mut outages = 0u64;
    let mut outage_ended = 0u64;
    // (run, file) -> digests committed, with the restart count when committed.
    let mut commits: BTreeMap<(String, String), Vec<Commit>> = BTreeMap::new();
    let mut first_ref_event: Option<u64> = None;
    let mut last_ref_event: Option<u64> = None;
    let mut all: Vec<(u64, crate::state::Event)> = Vec::new();
    let ev_sum = history::read_events(fs, &inp.state_dir, |ev, ts| {
        all.push((ts, ev.clone()));
    });
    let window = inp.window.unwrap_or_else(|| {
        let from = span
            .map(|(a, _)| a.saturating_sub(60))
            .or_else(|| {
                all.iter()
                    .filter(|(_, e)| e.run.as_deref().is_some_and(|r| names.contains(leaf(r))))
                    .map(|(t, _)| t.saturating_sub(60))
                    .min()
            })
            .unwrap_or(0);
        // Half open: one second past now keeps the events of this second.
        Period {
            from,
            to: inp.generated_at.max(from) + 1,
        }
    });
    let mut ev_gaps = Vec::new();
    for (ts, ev) in &all {
        if !window.contains(*ts) {
            continue;
        }
        let run_name = ev.run.as_deref().map(leaf).filter(|r| names.contains(*r));
        if let Some(r) = run_name {
            first_ref_event = Some(first_ref_event.map_or(*ts, |f| f.min(*ts)));
            last_ref_event = Some(last_ref_event.map_or(*ts, |l| l.max(*ts)));
            let s = seen.entry(r.to_owned()).or_default();
            match ev.kind {
                Kind::RunDiscovered | Kind::RunReopened => {
                    s.discovered.get_or_insert(*ts);
                }
                Kind::RunComplete => {
                    s.completed = Some(*ts);
                    s.destination = ev
                        .data
                        .get("destination")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    s.failed.clear();
                }
                Kind::RunFailed => s.failed.push(ev.message.clone()),
                Kind::FileFailed => s.files_failed += 1,
                Kind::RunDeleted => s.deleted = true,
                Kind::FileCommitted => {
                    if let Some(f) = &ev.file {
                        commits.entry((r.to_owned(), f.clone())).or_default().push((
                            ev.data
                                .get("sha256")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            starts.len(),
                            ev.data
                                .get("adopted")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        ));
                    }
                }
                _ => {}
            }
        }
        match ev.kind {
            Kind::Started => starts.push(*ts),
            Kind::OutageStarted => {
                outages += 1;
                outages_open += 1;
            }
            Kind::OutageEnded => {
                outage_ended += 1;
                outages_open -= 1;
            }
            _ => {}
        }
        let interesting = !matches!(
            ev.kind,
            Kind::FileCommitted | Kind::RunStateChanged | Kind::OutageSummary
        ) && (run_name.is_some() || ev.run.is_none());
        if interesting {
            timeline.push(TimelineEntry {
                ts: ev.ts.clone(),
                source: "ont-offload".into(),
                kind: ev.kind.as_str(),
                detail: match &ev.run {
                    Some(r) => format!("{}: {}", leaf(r), ev.message),
                    None => ev.message.clone(),
                },
            });
        }
    }
    for g in &ev_sum.gaps {
        ev_gaps.push(g.clone());
    }
    timeline.sort_by(|a, b| a.ts.cmp(&b.ts));
    let mut inputs = vec![ev_sum];
    if let Some(log) = &inp.log {
        inputs.push(history::read_log(fs, log, |_, _| {}));
    }

    // Each run on the share against its reference.
    let mut runs = Vec::new();
    for rr in &mut refs {
        let s = seen.get(&rr.name).cloned().unwrap_or_default();
        let mut out = RunOut {
            name: rr.name.clone(),
            source_state: rr.state.clone(),
            destination: s.destination.clone(),
            expected: rr.files.len() as u64,
            matched: 0,
            bytes: 0,
            missing: Vec::new(),
            changed: Vec::new(),
            extra: Vec::new(),
            complete: false,
            manifest_agrees: None,
            receipt: None,
            latency_s: s
                .completed
                .zip(rr.ended_at)
                .map(|(c, e)| c.saturating_sub(e)),
            result: result::FAIL.into(),
        };
        // Without a destination from run_complete, look where the events
        // put it: nothing to compare against means everything is missing.
        let dest = s.destination.as_ref().map(PathBuf::from);
        if let Some(dest) = &dest {
            for (rel, want) in &rr.files {
                match hash_file(fs, &dest.join(rel), 8 << 20) {
                    Ok((len, got)) => {
                        out.bytes += len;
                        if got == *want {
                            out.matched += 1;
                        } else {
                            out.changed.push(rel.clone());
                        }
                    }
                    Err(_) => out.missing.push(rel.clone()),
                }
            }
            let present = walk(fs, dest).unwrap_or_default();
            out.extra = present
                .iter()
                .filter(|p| !rr.files.contains_key(*p))
                .cloned()
                .collect();
            for kind in rr.counts.keys() {
                rr.counted.insert(kind.clone(), count_type(&present, kind));
            }
            out.complete = fs.symlink_metadata(&dest.join(COMPLETE)).is_ok();
            if let Ok(b) = fs.read(&dest.join(MANIFEST)) {
                let digest = sha256_hex(&b);
                out.manifest_agrees =
                    Some(parse_manifest(&String::from_utf8_lossy(&b)).is_ok_and(|m| m == rr.files));
                let run_json_digest = fs
                    .read(&dest.join(RUN_JSON))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|v| {
                        v.get("manifest_sha256")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                out.receipt = Some(
                    fs.read(&dest.join(VERIFIED))
                        .ok()
                        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                        .is_some_and(|v| {
                            v.get("result").and_then(Value::as_str) == Some("ok")
                                && v.get("manifest_sha256").and_then(Value::as_str)
                                    == Some(digest.as_str())
                                && run_json_digest.as_deref().is_none_or(|r| r == digest)
                        }),
                );
            }
        } else {
            out.missing = rr.files.keys().cloned().collect();
        }
        let content_ok = rr.problem.is_none()
            && !rr.files.is_empty()
            && out.missing.is_empty()
            && out.changed.is_empty()
            && out.extra.is_empty();
        let complete_ok = match rr.state.as_str() {
            "complete" => out.complete,
            "aborted" => !out.complete,
            _ => true,
        };
        if content_ok && complete_ok {
            out.result = result::PASS.into();
        }
        runs.push(out);
    }

    // The checks.
    let req = |id: &str| protocol.required.iter().any(|r| r == id);
    let wanted = |id: &str| req(id) || protocol.informational.iter().any(|r| r == id);
    let n = refs.len() as u64;
    let no_refs = refs.is_empty();
    let mut checks = Vec::new();
    let mut push = |id: &str, result: &str, observed: String| {
        let def = CHECKS.iter().find(|c| c.id == id).copied();
        if let (Some(def), true) = (def, wanted(id)) {
            checks.push(CheckOut {
                id: id.into(),
                title: def.title.into(),
                expected: def.expected.into(),
                requirements: def.requirements.iter().map(|r| (*r).to_owned()).collect(),
                required: req(id),
                observed,
                result: result.into(),
            });
        }
    };
    let ref_problems: Vec<String> = refs
        .iter()
        .filter_map(|r| r.problem.as_ref().map(|p| format!("{}: {p}", r.name)))
        .collect();

    // VC-01
    if no_refs {
        push(
            "VC-01",
            result::INCONCLUSIVE,
            "no reference runs found".into(),
        );
    } else {
        let found = refs
            .iter()
            .filter(|r| seen.get(&r.name).is_some_and(|s| s.discovered.is_some()))
            .count() as u64;
        push(
            "VC-01",
            if found == n {
                result::PASS
            } else {
                result::FAIL
            },
            format!("{found} of {n} runs discovered"),
        );
    }
    // VC-02
    let finished: Vec<&RunOut> = runs
        .iter()
        .filter(|r| r.source_state == "complete")
        .collect();
    if finished.is_empty() {
        push(
            "VC-02",
            if no_refs {
                result::INCONCLUSIVE
            } else {
                result::NA
            },
            "no run finished at the source".into(),
        );
    } else {
        let done = finished
            .iter()
            .filter(|r| {
                r.complete
                    && r.destination.as_ref().is_some_and(|d| {
                        fs.metadata(&Path::new(d).join(MANIFEST)).is_ok()
                            && fs.metadata(&Path::new(d).join(RUN_JSON)).is_ok()
                    })
            })
            .count();
        push(
            "VC-02",
            if done == finished.len() {
                result::PASS
            } else {
                result::FAIL
            },
            format!(
                "{done} of {} finished runs complete on the share",
                finished.len()
            ),
        );
    }
    // VC-03
    let files: u64 = runs.iter().map(|r| r.expected).sum();
    let matched: u64 = runs.iter().map(|r| r.matched).sum();
    let missing: usize = runs.iter().map(|r| r.missing.len()).sum();
    let changed: usize = runs.iter().map(|r| r.changed.len()).sum();
    let extra: usize = runs.iter().map(|r| r.extra.len()).sum();
    let bytes: u64 = runs.iter().map(|r| r.bytes).sum();
    if no_refs || !ref_problems.is_empty() || files == 0 {
        push(
            "VC-03",
            result::INCONCLUSIVE,
            if ref_problems.is_empty() {
                "no reference files".into()
            } else {
                format!("reference unavailable: {}", ref_problems.join("; "))
            },
        );
    } else {
        push(
            "VC-03",
            if missing + changed + extra == 0 {
                result::PASS
            } else {
                result::FAIL
            },
            {
                let nowhere = runs.iter().filter(|r| r.destination.is_none()).count() as u64;
                let mut o = format!(
                    "{matched} of {files} files matched ({} re-hashed); {missing} missing, {changed} changed, {extra} extra",
                    crate::config::units::Bytes(bytes)
                );
                if nowhere > 0 {
                    o.push_str(&format!(
                        "; {} without a destination in the records",
                        plural(nowhere, "run", "runs")
                    ));
                }
                o
            },
        );
    }
    // VC-04
    let with_manifest: Vec<&RunOut> = runs
        .iter()
        .filter(|r| r.manifest_agrees.is_some())
        .collect();
    if with_manifest.is_empty() {
        push(
            "VC-04",
            if finished.is_empty() && !no_refs {
                result::NA
            } else {
                result::INCONCLUSIVE
            },
            "no manifest.sha256 on the share".into(),
        );
    } else {
        let agree = with_manifest
            .iter()
            .filter(|r| r.manifest_agrees == Some(true))
            .count();
        push(
            "VC-04",
            if agree == with_manifest.len() && with_manifest.len() == finished.len() {
                result::PASS
            } else {
                result::FAIL
            },
            format!(
                "{agree} of {} manifests agree",
                finished.len().max(with_manifest.len())
            ),
        );
    }
    // VC-05: late files (MinKNOW: newer than final summary; simulator: by name in its manifest after the summary is not known, so use the source mtimes when present).
    let mut late_total = 0u64;
    let mut late_bad = 0u64;
    let mut late_known = false;
    for (rr, out) in refs.iter_mut().zip(&runs) {
        if rr.late.is_none() {
            if let Some(src) = &rr.source {
                let fin = rr
                    .files
                    .keys()
                    .find(|f| !f.contains('/') && f.starts_with("final_summary_"))
                    .cloned();
                if let Some(fin) = fin {
                    if let Ok(m) = fs.metadata(&src.join(&fin)) {
                        let t = crate::journal::nanos(m.modified);
                        rr.late = Some(
                            rr.files
                                .keys()
                                .filter(|f| **f != fin)
                                .filter(|f| {
                                    fs.metadata(&src.join(f))
                                        .ok()
                                        .is_some_and(|m| crate::journal::nanos(m.modified) > t)
                                })
                                .cloned()
                                .collect(),
                        );
                    }
                }
            }
        }
        if let Some(late) = &rr.late {
            late_known = true;
            late_total += late.len() as u64;
            late_bad += late
                .iter()
                .filter(|f| out.missing.contains(f) || out.changed.contains(f))
                .count() as u64;
        }
    }
    if !late_known {
        push(
            "VC-05",
            result::INCONCLUSIVE,
            "the source runs are gone; file times unknown".into(),
        );
    } else if late_total == 0 {
        push(
            "VC-05",
            result::NA,
            "no file was written after a final summary".into(),
        );
    } else {
        push(
            "VC-05",
            if late_bad == 0 {
                result::PASS
            } else {
                result::FAIL
            },
            format!(
                "{} of {late_total} late files copied and matched",
                late_total - late_bad
            ),
        );
    }
    // VC-06
    let rewrites: u64 = refs.iter().map(|r| r.rewritten.len() as u64).sum();
    if rewrites == 0 {
        push(
            "VC-06",
            result::NA,
            "no file was rewritten after it was closed".into(),
        );
    } else {
        let bad = refs
            .iter()
            .zip(&runs)
            .map(|(r, o)| {
                r.rewritten
                    .iter()
                    .filter(|f| o.missing.contains(f) || o.changed.contains(f))
                    .count() as u64
            })
            .sum::<u64>();
        push(
            "VC-06",
            if bad == 0 { result::PASS } else { result::FAIL },
            format!(
                "{} of {rewrites} rewritten files match their final content",
                rewrites - bad
            ),
        );
    }
    // VC-07
    let aborted: Vec<&RunOut> = runs
        .iter()
        .filter(|r| r.source_state == "aborted")
        .collect();
    if aborted.is_empty() {
        push("VC-07", result::NA, "no run was aborted".into());
    } else {
        let ok = aborted
            .iter()
            .filter(|r| r.missing.is_empty() && r.changed.is_empty() && !r.complete)
            .count();
        push(
            "VC-07",
            if ok == aborted.len() {
                result::PASS
            } else {
                result::FAIL
            },
            format!(
                "{ok} of {} aborted runs copied and left without COMPLETE",
                aborted.len()
            ),
        );
    }
    // VC-08
    if outages == 0 {
        push(
            "VC-08",
            if req("VC-08") {
                result::INCONCLUSIVE
            } else {
                result::NA
            },
            "no outage during the execution (the drill was not performed)".into(),
        );
    } else {
        push(
            "VC-08",
            if outages_open <= 0 && outage_ended >= outages && missing + changed == 0 {
                result::PASS
            } else {
                result::FAIL
            },
            format!(
                "{} detected, {outage_ended} ended; content {}",
                plural(outages, "outage", "outages"),
                if missing + changed == 0 {
                    "identical"
                } else {
                    "differs"
                }
            ),
        );
    }
    // VC-09
    let restarts = starts.len().saturating_sub(1);
    if restarts == 0 {
        push(
            "VC-09",
            if req("VC-09") {
                result::INCONCLUSIVE
            } else {
                result::NA
            },
            "no restart during the execution (the step was not performed)".into(),
        );
    } else {
        let mut recopied = 0u64;
        let mut adopted = 0u64;
        for list in commits.values() {
            for (i, (sha, session, ad)) in list.iter().enumerate() {
                if *ad {
                    adopted += 1;
                    continue;
                }
                if list[..i]
                    .iter()
                    .any(|(s, sess, _)| s == sha && sess < session)
                {
                    recopied += 1;
                }
            }
        }
        push(
            "VC-09",
            if recopied == 0 {
                result::PASS
            } else {
                result::FAIL
            },
            format!(
                "{} ; {adopted} files adopted, {recopied} copied again with the same content",
                plural(restarts as u64, "restart", "restarts")
            ),
        );
    }
    // VC-10
    let failed_runs: u64 = seen.iter().filter(|(_, s)| !s.failed.is_empty()).count() as u64;
    let failed_files: u64 = seen.values().map(|s| s.files_failed).sum();
    push(
        "VC-10",
        if no_refs {
            result::INCONCLUSIVE
        } else if failed_runs + failed_files == 0 {
            result::PASS
        } else {
            result::FAIL
        },
        format!("{failed_files} files failed, {failed_runs} runs failed"),
    );
    // VC-11
    if finished.is_empty() {
        push(
            "VC-11",
            if no_refs {
                result::INCONCLUSIVE
            } else {
                result::NA
            },
            "no completed run".into(),
        );
    } else {
        let ok = finished.iter().filter(|r| r.receipt == Some(true)).count();
        push(
            "VC-11",
            if ok == finished.len() {
                result::PASS
            } else {
                result::FAIL
            },
            format!(
                "{ok} of {} completed runs have a matching VERIFIED.json",
                finished.len()
            ),
        );
    }
    // VC-12
    let limit = protocol.max_minutes_to_complete * 60;
    let lat: Vec<u64> = finished.iter().filter_map(|r| r.latency_s).collect();
    if finished.is_empty() {
        push(
            "VC-12",
            if no_refs {
                result::INCONCLUSIVE
            } else {
                result::NA
            },
            "no completed run".into(),
        );
    } else if lat.len() < finished.len() {
        push(
            "VC-12",
            result::INCONCLUSIVE,
            format!(
                "end time known for {} of {} runs",
                lat.len(),
                finished.len()
            ),
        );
    } else {
        let max = lat.iter().copied().max().unwrap_or(0);
        push(
            "VC-12",
            if max <= limit {
                result::PASS
            } else {
                result::FAIL
            },
            format!(
                "longest {} (limit {} min)",
                fmt_secs(max),
                protocol.max_minutes_to_complete
            ),
        );
    }
    // VC-13
    let in_window = all.iter().filter(|(t, _)| window.contains(*t)).count();
    push(
        "VC-13",
        if in_window == 0 {
            result::INCONCLUSIVE
        } else if ev_gaps.is_empty() {
            result::PASS
        } else {
            result::FAIL
        },
        if ev_gaps.is_empty() {
            format!(
                "{in_window} events, {}, no gaps",
                plural(starts.len() as u64, "start", "starts")
            )
        } else {
            ev_gaps.join("; ")
        },
    );
    // VC-14
    let rec_first = inputs[0].first.as_deref().and_then(time::parse);
    let rec_last = inputs[0].last.as_deref().and_then(time::parse);
    let covers = rec_first.zip(first_ref_event).is_some_and(|(f, r)| f <= r)
        && rec_last.zip(last_ref_event).is_some_and(|(l, r)| l >= r);
    let log_gaps: Vec<String> = inputs.iter().skip(1).flat_map(|i| i.gaps.clone()).collect();
    push(
        "VC-14",
        if first_ref_event.is_none() {
            result::INCONCLUSIVE
        } else if covers && log_gaps.is_empty() && ev_gaps.is_empty() {
            result::PASS
        } else {
            result::FAIL
        },
        format!(
            "events {} to {}{}",
            inputs[0]
                .first
                .as_deref()
                .map_or("—".into(), ToOwned::to_owned),
            inputs[0]
                .last
                .as_deref()
                .map_or("—".into(), ToOwned::to_owned),
            if log_gaps.is_empty() {
                String::new()
            } else {
                format!("; log: {}", log_gaps.join("; "))
            }
        ),
    );
    // VC-15
    let deleted: Vec<&RunOut> = runs
        .iter()
        .filter(|r| seen.get(&r.name).is_some_and(|s| s.deleted))
        .collect();
    if deleted.is_empty() {
        push(
            "VC-15",
            result::NA,
            "no source run deleted during the execution".into(),
        );
    } else {
        let bad = deleted.iter().filter(|r| r.result != result::PASS).count();
        push(
            "VC-15",
            if bad == 0 { result::PASS } else { result::FAIL },
            format!(
                "{} deleted, {bad} of them with a failed copy",
                plural(deleted.len() as u64, "source run", "source runs")
            ),
        );
    }
    // VC-16
    let with_counts: Vec<&RefRun> = refs.iter().filter(|r| !r.counts.is_empty()).collect();
    if with_counts.is_empty() {
        push(
            "VC-16",
            if matches!(inp.mode, Mode::Minknow(_)) {
                result::INCONCLUSIVE
            } else {
                result::NA
            },
            "no final summary with file counts".into(),
        );
    } else {
        let mut parts = Vec::new();
        let mut bad = 0;
        for r in &with_counts {
            for (k, want) in &r.counts {
                let Some(have) = r.counted.get(k).copied() else {
                    bad += 1;
                    parts.push(format!("{k} not on the share/{want}"));
                    continue;
                };
                if have != *want {
                    bad += 1;
                }
                parts.push(format!("{k} {have}/{want}"));
            }
        }
        push(
            "VC-16",
            if bad == 0 { result::PASS } else { result::FAIL },
            format!("on the share / in the final summary: {}", parts.join(", ")),
        );
    }

    // Verdict.
    let required: Vec<&CheckOut> = checks.iter().filter(|c| c.required).collect();
    let passed = required
        .iter()
        .filter(|c| c.result == result::PASS || c.result == result::NA)
        .count() as u64;
    let failed: Vec<&str> = required
        .iter()
        .filter(|c| c.result == result::FAIL)
        .map(|c| c.id.as_str())
        .collect();
    let open: Vec<&str> = required
        .iter()
        .filter(|c| c.result == result::INCONCLUSIVE)
        .map(|c| c.id.as_str())
        .collect();
    let summary = format!(
        "{} copied byte for byte out of {}.",
        plural(
            runs.iter().filter(|r| r.result == result::PASS).count() as u64,
            "run",
            "runs"
        ),
        refs.len()
    );
    let (status, headline) = if !failed.is_empty() {
        (
            result::FAIL,
            format!("Failed {}. {summary}", failed.join(", ")),
        )
    } else if !open.is_empty() {
        (
            result::INCONCLUSIVE,
            format!("Not decided: {}. {summary}", open.join(", ")),
        )
    } else {
        (
            result::PASS,
            format!(
                "{passed} of {} required checks passed. {summary}",
                required.len()
            ),
        )
    };

    let mut environment = vec![
        EnvEntry {
            key: "ont-offload".into(),
            value: format!("{} on {}", env!("CARGO_PKG_VERSION"), inp.host),
        },
        EnvEntry {
            key: "Reference".into(),
            value: reference
                .detail
                .clone()
                .unwrap_or_else(|| reference.kind.clone()),
        },
    ];
    let versions: BTreeSet<String> = all
        .iter()
        .filter(|(t, e)| window.contains(*t) && e.kind == Kind::Started)
        .filter_map(|(_, e)| {
            e.data
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    if !versions.is_empty() {
        environment.push(EnvEntry {
            key: "Daemon versions".into(),
            value: versions.into_iter().collect::<Vec<_>>().join(", "),
        });
    }
    environment.extend(inp.environment.iter().cloned());

    let id_seed = format!(
        "{}|{}|{}|{}",
        inp.host, window.from, window.to, inp.generated_at
    );
    VerificationRecord {
        schema: VERIFICATION_SCHEMA.into(),
        schema_version: 1,
        report_id: format!(
            "{}-VR-{}-{}",
            inp.prefix,
            time::compact_day(inp.generated_at),
            &sha256_hex(id_seed.as_bytes())[..6]
        ),
        generated_at: time::rfc3339(inp.generated_at),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        host: inp.host.clone(),
        site: inp.site.clone(),
        protocol: ProtocolOut {
            name: protocol.name.clone(),
            version: protocol.version.clone(),
            description: protocol.description.clone(),
            sha256: protocol_sha.clone(),
            max_minutes_to_complete: protocol.max_minutes_to_complete,
        },
        mode: mode.into(),
        simulated,
        reference,
        window: Window {
            from: time::rfc3339(window.from),
            to: time::rfc3339(window.to),
        },
        operator: inp.operator.clone(),
        deviations: inp.deviations.clone(),
        verdict: VerdictOut {
            status: status.into(),
            headline,
            required_passed: passed,
            required: required.len() as u64,
        },
        checks,
        runs,
        timeline,
        environment,
        inputs,
    }
}

fn fmt_secs(s: u64) -> String {
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min {} s", s / 60, s % 60)
    } else {
        format!("{} h {} min", s / 3600, s / 60 % 60)
    }
}

#[cfg(test)]
mod tests;
