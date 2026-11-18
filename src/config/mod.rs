// SPDX-License-Identifier: AGPL-3.0-or-later
//! Configuration loading and validation (SWI-CONFIG).
//!
//! The TOML file is parsed strictly (unknown keys are errors) into a raw form
//! where sizes, rates, durations and paths are still text. [`validate`] then
//! checks every value and every path, collects *all* problems instead of
//! stopping at the first, and produces the typed [`Config`].
//!
//! Path validation is a risk control (HAZ-001, HAZ-008): a path that is empty,
//! relative, contains control characters (the BEL byte that once arrived
//! through a symlink), or that resolves into the other side of the transfer is
//! refused before anything is read or written.

pub mod units;

use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::cli::LogFormat;
use crate::error::{Error, Result};
use crate::fsio::{FileKind, Fs};
use crate::logging::escape_control;
use units::{Bits, Bytes, Rate, parse_duration, parse_rate, parse_size};

// ---------------------------------------------------------------------------
// Raw (as written) configuration
// ---------------------------------------------------------------------------

/// The configuration file as written, before validation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    /// Destination root on the mounted share.
    pub dest_root: String,
    /// Expected content of `dest_root/.ont-offload-target`.
    pub target_id: String,
    /// Which MinKNOW level becomes the top folder under `dest_root`.
    #[serde(default)]
    pub dest_starts_at: DestLevel,
    /// Journal and state directory.
    #[serde(default = "d_state_dir")]
    pub state_dir: String,
    /// `[source]`
    #[serde(default)]
    pub source: RawSource,
    /// `[selection]`
    #[serde(default)]
    pub selection: RawSelection,
    /// `[bandwidth]`
    #[serde(default)]
    pub bandwidth: RawBandwidth,
    /// `[transfer]`
    #[serde(default)]
    pub transfer: RawTransfer,
    /// `[outage]`
    #[serde(default)]
    pub outage: RawOutage,
    /// `[cleanup]`
    #[serde(default)]
    pub cleanup: RawCleanup,
    /// `[guard]`
    #[serde(default)]
    pub guard: RawGuard,
    /// `[logging]`
    #[serde(default)]
    pub logging: RawLogging,
    /// `[state]`
    #[serde(default)]
    pub state: RawState,
}

/// `[state]` as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawState {
    /// How often `state.json` is rewritten.
    pub interval: String,
    /// Size at which `events.jsonl` is rotated to `events.jsonl.1`.
    pub events_max_size: String,
}

impl Default for RawState {
    fn default() -> Self {
        Self {
            interval: "2s".into(),
            events_max_size: "64MB".into(),
        }
    }
}

fn d_state_dir() -> String {
    "/var/lib/ont-offload".into()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// Level of the MinKNOW structure that becomes the top folder under `dest_root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DestLevel {
    /// `dest_root/<experiment>/<sample>/<run>/` (default; with `any-depth`, the whole path below the root).
    #[default]
    Experiment,
    /// `dest_root/<sample>/<run>/`
    Sample,
    /// `dest_root/<run>/`
    Run,
}

impl DestLevel {
    /// Setting value as written in the configuration.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Experiment => "experiment",
            Self::Sample => "sample",
            Self::Run => "run",
        }
    }
}

/// Where runs are found and how they are recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    /// `<root>/<experiment>/<sample>/<run>/` (MinKNOW default).
    Minknow,
    /// Any folder below the root that contains a run marker.
    AnyDepth,
}

/// `[source]` as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawSource {
    /// Base directory that holds experiment folders.
    pub root: String,
    /// Folder layout below the root.
    pub layout: Layout,
    /// File name patterns that mark a folder as a run.
    pub run_markers: Vec<String>,
    /// File name patterns that mark a run as complete.
    pub complete_markers: Vec<String>,
    /// Patterns (relative to the root) never treated as runs.
    pub ignore: Vec<String>,
    /// Full rescan interval; between full rescans only changed folders are listed.
    pub full_rescan_every: String,
}

impl Default for RawSource {
    fn default() -> Self {
        Self {
            root: "/data".into(),
            layout: Layout::Minknow,
            run_markers: strings(&["sample_sheet_*.csv"]),
            complete_markers: strings(&["final_summary_*.txt"]),
            ignore: strings(&["lost+found", "**/.*", "ont-runsim"]),
            full_rescan_every: "10m".into(),
        }
    }
}

/// `[selection]` as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawSelection {
    /// Files to copy, relative to the run folder.
    pub include: Vec<String>,
    /// Files never copied.
    pub exclude: Vec<String>,
    /// Files copied only after the run is complete.
    pub defer_until_complete: Vec<String>,
}

impl Default for RawSelection {
    fn default() -> Self {
        Self {
            include: strings(&["**"]),
            exclude: strings(&["**/.*", "**/*.tmp"]),
            defer_until_complete: strings(&["sequencing_summary_*.txt", "other_reports/**"]),
        }
    }
}

/// `[bandwidth]` as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawBandwidth {
    /// Files copied at the same time.
    pub workers: u32,
    /// Total rate for all workers together.
    pub rate_limit: String,
    /// Optional network connection speed, used only to check `rate_limit`.
    pub network_speed: Option<String>,
}

impl Default for RawBandwidth {
    fn default() -> Self {
        Self {
            workers: 4,
            rate_limit: "unlimited".into(),
            network_speed: None,
        }
    }
}

/// How copies are verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerifyMode {
    /// SHA-256 while copying, size check after commit (default).
    Inflight,
    /// Additionally read the committed copy back over the network.
    Readback,
}

/// What to do when a destination file already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnExisting {
    /// Stop with an error for that file.
    Fail,
    /// Hash the existing copy and accept it if identical.
    Hash,
}

/// `[transfer]` as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawTransfer {
    /// Read and write chunk size.
    pub chunk_size: String,
    /// Verification mode.
    pub verify: VerifyMode,
    /// Existing destination file policy.
    pub on_existing: OnExisting,
    /// Quiet time before a live file is copied.
    pub settle: String,
    /// Quiet time after the complete marker before the run is finished.
    pub settle_after_complete: String,
    /// No progress for this long marks a worker as stalled.
    pub stall_timeout: String,
    /// Attempts per file for non-outage errors.
    pub max_attempts: u32,
    /// Free space to keep on the destination.
    pub min_free: String,
    /// Copy modification times.
    pub preserve_mtime: bool,
    /// Octal permissions of copied files.
    pub file_mode: String,
    /// How long a completed run is still watched for changes (`"0s"`: not at all).
    pub watch_completed_for: String,
}

impl Default for RawTransfer {
    fn default() -> Self {
        Self {
            chunk_size: "8MiB".into(),
            verify: VerifyMode::Inflight,
            on_existing: OnExisting::Fail,
            settle: "30s".into(),
            settle_after_complete: "5m".into(),
            stall_timeout: "120s".into(),
            max_attempts: 3,
            min_free: "500GiB".into(),
            preserve_mtime: true,
            file_mode: "0640".into(),
            watch_completed_for: "24h".into(),
        }
    }
}

/// `[outage]` as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawOutage {
    /// Longest wait between checks during an outage.
    pub check_every_max: String,
    /// Log a warning after this long.
    pub warn_after: String,
    /// Raise an alert after this long.
    pub alert_after: String,
    /// Repeat the alert this often.
    pub remind_every: String,
    /// Log a summary line this often.
    pub log_every: String,
    /// The mount must be healthy this long before resuming.
    pub stable_before_resume: String,
    /// Alert when source free space falls below this during an outage.
    pub source_free_alert: String,
    /// Command run on alerts (empty: none).
    pub notify_command: String,
}

impl Default for RawOutage {
    fn default() -> Self {
        Self {
            check_every_max: "5m".into(),
            warn_after: "10m".into(),
            alert_after: "1h".into(),
            remind_every: "1h".into(),
            log_every: "15m".into(),
            stable_before_resume: "60s".into(),
            source_free_alert: "200GB".into(),
            notify_command: String::new(),
        }
    }
}

/// Source deletion mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeleteMode {
    /// Never delete (default).
    Off,
    /// Log what would be deleted.
    DryRun,
    /// Delete runs that pass the gate.
    On,
}

/// Evidence required before a source run may be deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Gate {
    /// A storage-side `VERIFIED.json` receipt.
    StorageVerify,
    /// A read-back verification by ont-offload.
    Readback,
    /// Both.
    Both,
}

/// `[cleanup]` as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawCleanup {
    /// Deletion mode.
    pub delete: DeleteMode,
    /// Deletion gate.
    pub gate: Gate,
    /// Minimum age of a verified run before deletion.
    pub delete_after: String,
    /// Re-hash the source against the manifest before deleting.
    pub rehash_source: bool,
    /// Only delete when source free space is below this (`"always"`: no condition).
    pub only_when_free_below: String,
}

impl Default for RawCleanup {
    fn default() -> Self {
        Self {
            delete: DeleteMode::Off,
            gate: Gate::StorageVerify,
            delete_after: "7d".into(),
            rehash_source: true,
            only_when_free_below: "always".into(),
        }
    }
}

/// `[guard]` as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawGuard {
    /// Filesystem types accepted for the destination mount.
    pub allowed_fstypes: Vec<String>,
}

impl Default for RawGuard {
    fn default() -> Self {
        Self {
            allowed_fstypes: strings(&["nfs", "nfs4", "cifs", "smb3"]),
        }
    }
}

/// `[logging]` as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLogging {
    /// Log file (optional).
    #[serde(default)]
    pub file: Option<String>,
    /// Line format.
    #[serde(default = "d_log_format")]
    pub format: LogFormat,
}

fn d_log_format() -> LogFormat {
    LogFormat::Text
}

impl Default for RawLogging {
    fn default() -> Self {
        Self {
            file: None,
            format: LogFormat::Text,
        }
    }
}

// ---------------------------------------------------------------------------
// Validated configuration
// ---------------------------------------------------------------------------

/// A validated path: as configured, and with symlinks resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedPath {
    /// The path as written in the configuration.
    pub configured: PathBuf,
    /// The path with every symlink resolved.
    pub resolved: PathBuf,
    /// Device id of the resolved path.
    pub dev: u64,
}

/// `[source]`, validated.
#[derive(Debug, Clone)]
pub struct Source {
    /// Base directory.
    pub root: CheckedPath,
    /// Folder layout.
    pub layout: Layout,
    /// Run marker patterns.
    pub run_markers: Vec<String>,
    /// Complete marker patterns.
    pub complete_markers: Vec<String>,
    /// Ignored patterns.
    pub ignore: Vec<String>,
    /// Full rescan interval.
    pub full_rescan_every: Duration,
}

/// `[state]`, validated.
#[derive(Debug, Clone, Copy)]
pub struct StateCfg {
    /// `state.json` rewrite interval.
    pub interval: Duration,
    /// `events.jsonl` rotation size in bytes.
    pub events_max_size: u64,
}

/// `[selection]`, validated.
#[derive(Debug, Clone)]
pub struct Selection {
    /// Include patterns.
    pub include: Vec<String>,
    /// Exclude patterns.
    pub exclude: Vec<String>,
    /// Patterns copied only after completion.
    pub defer_until_complete: Vec<String>,
}

/// `[bandwidth]`, validated.
#[derive(Debug, Clone, Copy)]
pub struct Bandwidth {
    /// Concurrent file copies.
    pub workers: u32,
    /// Total rate for all workers.
    pub rate_limit: Rate,
    /// Network connection speed, if given.
    pub network_speed: Option<Rate>,
}

/// `[transfer]`, validated.
#[derive(Debug, Clone, Copy)]
pub struct Transfer {
    /// Chunk size in bytes.
    pub chunk_size: u64,
    /// Verification mode.
    pub verify: VerifyMode,
    /// Existing file policy.
    pub on_existing: OnExisting,
    /// Live-file settle time.
    pub settle: Duration,
    /// Settle time after the complete marker.
    pub settle_after_complete: Duration,
    /// Stall timeout.
    pub stall_timeout: Duration,
    /// Attempts per file.
    pub max_attempts: u32,
    /// Destination free-space floor in bytes.
    pub min_free: u64,
    /// Preserve modification times.
    pub preserve_mtime: bool,
    /// File permissions.
    pub file_mode: u32,
    /// How long completed runs stay watched.
    pub watch_completed_for: Duration,
}

/// `[outage]`, validated.
#[derive(Debug, Clone)]
pub struct Outage {
    /// Longest check interval.
    pub check_every_max: Duration,
    /// Warning threshold.
    pub warn_after: Duration,
    /// Alert threshold.
    pub alert_after: Duration,
    /// Alert repeat interval.
    pub remind_every: Duration,
    /// Summary log interval.
    pub log_every: Duration,
    /// Stability window before resuming.
    pub stable_before_resume: Duration,
    /// Source free-space alert threshold in bytes.
    pub source_free_alert: u64,
    /// Alert command.
    pub notify_command: Option<String>,
}

/// `[cleanup]`, validated.
#[derive(Debug, Clone, Copy)]
pub struct Cleanup {
    /// Deletion mode.
    pub delete: DeleteMode,
    /// Deletion gate.
    pub gate: Gate,
    /// Minimum age.
    pub delete_after: Duration,
    /// Re-hash before deleting.
    pub rehash_source: bool,
    /// Free-space condition in bytes (`None`: always).
    pub only_when_free_below: Option<u64>,
}

/// The validated configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Destination root.
    pub dest_root: CheckedPath,
    /// Destination target id.
    pub target_id: String,
    /// Top destination level.
    pub dest_starts_at: DestLevel,
    /// State directory.
    pub state_dir: CheckedPath,
    /// Source settings.
    pub source: Source,
    /// Selection settings.
    pub selection: Selection,
    /// Bandwidth settings.
    pub bandwidth: Bandwidth,
    /// Transfer settings.
    pub transfer: Transfer,
    /// Outage settings.
    pub outage: Outage,
    /// Cleanup settings.
    pub cleanup: Cleanup,
    /// Allowed destination filesystem types.
    pub allowed_fstypes: Vec<String>,
    /// Log file.
    pub log_file: Option<PathBuf>,
    /// Log format.
    pub log_format: LogFormat,
    /// Live state settings.
    pub state: StateCfg,
}

/// A validated configuration plus the warnings found on the way.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The configuration.
    pub config: Config,
    /// Non-fatal findings, each naming its key.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Loading and validation
// ---------------------------------------------------------------------------

/// Parse TOML text strictly.
///
/// Implements: SRS-CFG-003
///
/// # Errors
/// [`Error::Config`] with the parser's message (key, line and column).
pub fn parse(text: &str) -> Result<RawConfig> {
    toml::from_str(text).map_err(|e| Error::Config(e.to_string().trim_end().to_owned()))
}

/// Read, parse and validate the configuration file.
///
/// Implements: SRS-CFG-001, SRS-CFG-002, SRS-CFG-003, SRS-CFG-004
///
/// # Errors
/// [`Error::Config`] listing every problem found.
pub fn load(path: &Path, fs: &dyn Fs) -> Result<Loaded> {
    let shown = escape_control(&path.to_string_lossy());
    let bytes = fs
        .read(path)
        .map_err(|e| Error::Config(format!("cannot read {shown}: {e}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Config(format!("{shown} is not valid UTF-8 text")))?;
    let raw = parse(&text).map_err(|e| match e {
        Error::Config(m) => Error::Config(format!("{shown}: {m}")),
        other => other,
    })?;
    validate(&raw, fs).map_err(|problems| {
        let mut msg = format!("{} problem(s) in {shown}:", problems.len());
        for p in &problems {
            let _ = write!(msg, "\n  - {p}");
        }
        Error::Config(msg)
    })
}

/// Collects problems and warnings while validating.
#[derive(Default)]
struct Check {
    problems: Vec<String>,
    warnings: Vec<String>,
}

impl Check {
    fn problem(&mut self, key: &str, msg: impl std::fmt::Display) {
        self.problems.push(format!("{key}: {msg}"));
    }

    fn warn(&mut self, key: &str, msg: impl std::fmt::Display) {
        self.warnings.push(format!("{key}: {msg}"));
    }

    fn size(&mut self, key: &str, v: &str) -> u64 {
        parse_size(v).unwrap_or_else(|e| {
            self.problem(key, e);
            0
        })
    }

    fn duration(&mut self, key: &str, v: &str) -> Duration {
        parse_duration(v).unwrap_or_else(|e| {
            self.problem(key, e);
            Duration::ZERO
        })
    }

    fn rate(&mut self, key: &str, v: &str) -> Option<Rate> {
        parse_rate(v).map_err(|e| self.problem(key, e)).ok()
    }

    /// Text checks shared by every configured path.
    fn path_text(&mut self, key: &str, v: &str) -> bool {
        let shown = escape_control(v);
        if v.is_empty() {
            self.problem(key, "is empty");
            return false;
        }
        let mut ok = true;
        if v.chars().any(char::is_control) {
            self.problem(key, format!("{shown:?} contains control characters"));
            ok = false;
        }
        if v.trim() != v {
            self.problem(key, format!("{shown:?} has leading or trailing whitespace"));
            ok = false;
        }
        let p = Path::new(v);
        if !p.is_absolute() {
            self.problem(key, format!("{shown:?} must be an absolute path"));
            ok = false;
        }
        if p.components().any(|c| c == Component::ParentDir) {
            self.problem(key, format!("{shown:?} must not contain \"..\""));
            ok = false;
        }
        ok
    }

    /// Full check of a directory that must exist: text, existence, kind and
    /// the symlink-resolved form.
    fn dir(&mut self, key: &str, v: &str, fs: &dyn Fs) -> Option<CheckedPath> {
        if !self.path_text(key, v) {
            return None;
        }
        let shown = escape_control(v);
        let p = Path::new(v);
        let st = match fs.metadata(p) {
            Ok(st) => st,
            Err(e) => {
                self.problem(
                    key,
                    format!("{shown} does not exist or is not reachable: {e}"),
                );
                return None;
            }
        };
        if st.kind != FileKind::Dir {
            self.problem(key, format!("{shown} is not a directory"));
            return None;
        }
        let resolved = match fs.canonicalize(p) {
            Ok(r) => r,
            Err(e) => {
                self.problem(key, format!("{shown} cannot be resolved: {e}"));
                return None;
            }
        };
        let Some(rs) = resolved.to_str() else {
            self.problem(
                key,
                format!(
                    "{shown} resolves to {:?}, which is not valid UTF-8",
                    escape_control(&resolved.to_string_lossy())
                ),
            );
            return None;
        };
        if rs.chars().any(char::is_control) {
            self.problem(
                key,
                format!(
                    "{shown} resolves through a symlink to {:?}, which contains control characters",
                    escape_control(rs)
                ),
            );
            return None;
        }
        if resolved != p {
            self.warn(key, format!("{shown} resolves to {rs} (symlink)"));
        }
        let dev = fs.metadata(&resolved).map_or(st.dev, |s| s.dev);
        Some(CheckedPath {
            configured: p.to_owned(),
            resolved,
            dev,
        })
    }

    fn globs(&mut self, key: &str, patterns: &[String], file_names_only: bool) {
        for (i, pat) in patterns.iter().enumerate() {
            let k = format!("{key}[{i}]");
            if pat.trim().is_empty() || pat.chars().any(char::is_control) {
                self.problem(
                    &k,
                    format!(
                        "{:?} is empty or contains control characters",
                        escape_control(pat)
                    ),
                );
                continue;
            }
            if file_names_only && pat.trim_end_matches('/').contains('/') {
                self.problem(
                    &k,
                    format!("{pat:?} must be a file name pattern without \"/\""),
                );
                continue;
            }
            if let Err(e) = globset::Glob::new(pat.trim_end_matches('/')) {
                self.problem(&k, format!("invalid pattern {pat:?}: {e}"));
            }
        }
    }
}

fn contains(outer: &Path, inner: &Path) -> bool {
    inner.starts_with(outer)
}

/// Validate a parsed configuration against the filesystem.
///
/// Implements: SRS-CFG-001, SRS-CFG-002, SRS-CFG-003, SRS-RATE-002
///
/// # Errors
/// Every problem found, each naming its key.
#[allow(clippy::too_many_lines)]
pub fn validate(raw: &RawConfig, fs: &dyn Fs) -> std::result::Result<Loaded, Vec<String>> {
    let mut c = Check::default();

    // Paths (SRS-CFG-001).
    let dest = c.dir("dest_root", &raw.dest_root, fs);
    let src = c.dir("source.root", &raw.source.root, fs);
    let state = c.dir("state_dir", &raw.state_dir, fs);
    let log_file = raw.logging.file.as_deref().and_then(|f| {
        if !c.path_text("logging.file", f) {
            return None;
        }
        let p = PathBuf::from(f);
        match p.parent() {
            Some(parent) if fs.metadata(parent).is_ok_and(|s| s.kind == FileKind::Dir) => Some(p),
            _ => {
                c.problem(
                    "logging.file",
                    format!("the folder of {} does not exist", escape_control(f)),
                );
                None
            }
        }
    });

    // Containment and device (SRS-CFG-002).
    if let (Some(d), Some(s)) = (&dest, &src) {
        if contains(&d.resolved, &s.resolved) || contains(&s.resolved, &d.resolved) {
            c.problem(
                "dest_root",
                format!(
                    "{} and source.root {} contain each other (after resolving symlinks)",
                    d.resolved.display(),
                    s.resolved.display()
                ),
            );
        } else if d.dev == s.dev {
            c.problem(
                "dest_root",
                format!(
                    "{} is on the same device as source.root {}; is the network share mounted?",
                    d.resolved.display(),
                    s.resolved.display()
                ),
            );
        }
    }
    if let Some(st) = &state {
        for (key, other) in [("dest_root", &dest), ("source.root", &src)] {
            if let Some(o) = other {
                if contains(&o.resolved, &st.resolved) || contains(&st.resolved, &o.resolved) {
                    c.problem(
                        "state_dir",
                        format!(
                            "{} must be outside {key} {}",
                            st.resolved.display(),
                            o.resolved.display()
                        ),
                    );
                }
            }
        }
    }

    // Target id.
    let id = raw.target_id.as_str();
    if id.is_empty() || id == "CHANGE-ME" {
        c.problem(
            "target_id",
            "set it to the id written in dest_root/.ont-offload-target",
        );
    } else if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        c.problem(
            "target_id",
            format!(
                "{:?} may only contain letters, digits, '-', '_' and '.'",
                escape_control(id)
            ),
        );
    }

    // Patterns.
    c.globs("source.run_markers", &raw.source.run_markers, true);
    c.globs(
        "source.complete_markers",
        &raw.source.complete_markers,
        true,
    );
    c.globs("source.ignore", &raw.source.ignore, false);
    c.globs("selection.include", &raw.selection.include, false);
    c.globs("selection.exclude", &raw.selection.exclude, false);
    c.globs(
        "selection.defer_until_complete",
        &raw.selection.defer_until_complete,
        false,
    );
    if raw.source.run_markers.is_empty() {
        c.problem("source.run_markers", "needs at least one pattern");
    }
    if raw.source.complete_markers.is_empty() {
        c.problem("source.complete_markers", "needs at least one pattern");
    }
    if raw.selection.include.is_empty() {
        c.problem(
            "selection.include",
            "needs at least one pattern (\"**\" copies everything)",
        );
    }

    // Bandwidth (SRS-CFG-003, SRS-RATE-002).
    let workers = raw.bandwidth.workers;
    if !(1..=32).contains(&workers) {
        c.problem("bandwidth.workers", format!("{workers} is outside 1 to 32"));
    }
    let rate_limit = c.rate("bandwidth.rate_limit", &raw.bandwidth.rate_limit);
    let network_speed = raw
        .bandwidth
        .network_speed
        .as_deref()
        .and_then(|v| c.rate("bandwidth.network_speed", v));
    match (rate_limit, network_speed) {
        (_, Some(Rate::Unlimited)) => {
            c.problem(
                "bandwidth.network_speed",
                "must be a speed such as \"1Gbit/s\", not unlimited",
            );
        }
        (Some(Rate::BytesPerSec(limit)), Some(Rate::BytesPerSec(link))) => {
            if limit > link {
                c.problem(
                    "bandwidth.rate_limit",
                    format!(
                        "{} ({}) is more than network_speed {}; lower rate_limit (80% of the link is {})",
                        Bytes(limit),
                        Bits(limit),
                        Bits(link),
                        Rate::BytesPerSec(link / 10 * 8)
                    ),
                );
            } else if limit.saturating_mul(10) > link.saturating_mul(9) {
                c.warn(
                    "bandwidth.rate_limit",
                    format!(
                        "{} is over 90% of network_speed {}; other traffic on this link will be squeezed",
                        Rate::BytesPerSec(limit),
                        Bits(link)
                    ),
                );
            }
        }
        (Some(Rate::Unlimited), Some(Rate::BytesPerSec(link))) => c.warn(
            "bandwidth.rate_limit",
            format!("unlimited: transfers can use the whole {} link", Bits(link)),
        ),
        (Some(Rate::Unlimited), None) => c.warn(
            "bandwidth.rate_limit",
            "unlimited: transfers can use the whole network connection; set e.g. \"100MB/s\"",
        ),
        _ => {}
    }

    // Transfer.
    let t = &raw.transfer;
    let chunk_size = c.size("transfer.chunk_size", &t.chunk_size);
    if chunk_size != 0 && !(64 << 10..=256 << 20).contains(&chunk_size) {
        c.problem("transfer.chunk_size", "must be between 64KiB and 256MiB");
    }
    let settle = c.duration("transfer.settle", &t.settle);
    let settle_after_complete =
        c.duration("transfer.settle_after_complete", &t.settle_after_complete);
    let stall_timeout = c.duration("transfer.stall_timeout", &t.stall_timeout);
    if stall_timeout > Duration::ZERO && stall_timeout < Duration::from_secs(10) {
        c.problem("transfer.stall_timeout", "must be at least 10s");
    }
    if !(1..=100).contains(&t.max_attempts) {
        c.problem(
            "transfer.max_attempts",
            format!("{} is outside 1 to 100", t.max_attempts),
        );
    }
    let min_free = c.size("transfer.min_free", &t.min_free);
    let watch_completed_for = c.duration("transfer.watch_completed_for", &t.watch_completed_for);
    let full_rescan_every = c.duration("source.full_rescan_every", &raw.source.full_rescan_every);
    if full_rescan_every < Duration::from_secs(10)
        && !c
            .problems
            .iter()
            .any(|p| p.starts_with("source.full_rescan_every"))
    {
        c.problem("source.full_rescan_every", "must be at least 10s");
    }
    let state_interval = c.duration("state.interval", &raw.state.interval);
    if state_interval < Duration::from_secs(1)
        && !c.problems.iter().any(|p| p.starts_with("state.interval"))
    {
        c.problem("state.interval", "must be at least 1s");
    }
    let events_max_size = c.size("state.events_max_size", &raw.state.events_max_size);
    if events_max_size < 1 << 20
        && !c
            .problems
            .iter()
            .any(|p| p.starts_with("state.events_max_size"))
    {
        c.problem("state.events_max_size", "must be at least 1MiB");
    }
    let file_mode = match u32::from_str_radix(t.file_mode.trim_start_matches("0o"), 8) {
        Ok(m) if m <= 0o777 && m & 0o600 == 0o600 => m,
        Ok(m) if m <= 0o777 => {
            c.problem(
                "transfer.file_mode",
                format!("{m:o} must let the owner read and write (e.g. \"0640\")"),
            );
            0
        }
        _ => {
            c.problem(
                "transfer.file_mode",
                format!("{:?} is not an octal mode such as \"0640\"", t.file_mode),
            );
            0
        }
    };

    // Outage.
    let o = &raw.outage;
    let check_every_max = c.duration("outage.check_every_max", &o.check_every_max);
    let warn_after = c.duration("outage.warn_after", &o.warn_after);
    let alert_after = c.duration("outage.alert_after", &o.alert_after);
    let remind_every = c.duration("outage.remind_every", &o.remind_every);
    let log_every = c.duration("outage.log_every", &o.log_every);
    let stable_before_resume = c.duration("outage.stable_before_resume", &o.stable_before_resume);
    let source_free_alert = c.size("outage.source_free_alert", &o.source_free_alert);
    if warn_after > alert_after {
        c.problem("outage.warn_after", "must not be longer than alert_after");
    }
    for (key, d) in [
        ("outage.check_every_max", check_every_max),
        ("outage.remind_every", remind_every),
        ("outage.log_every", log_every),
    ] {
        if d.is_zero() && !c.problems.iter().any(|p| p.starts_with(key)) {
            c.problem(key, "must be more than zero");
        }
    }
    let notify_command = if o.notify_command.trim().is_empty() {
        None
    } else {
        let program = o
            .notify_command
            .split_whitespace()
            .next()
            .unwrap_or_default();
        if o.notify_command.chars().any(char::is_control) {
            c.problem("outage.notify_command", "contains control characters");
        } else if !Path::new(program).is_absolute() {
            c.problem(
                "outage.notify_command",
                format!("{program:?} must be an absolute path"),
            );
        } else if !fs
            .metadata(Path::new(program))
            .is_ok_and(|s| s.kind == FileKind::File)
        {
            c.problem("outage.notify_command", format!("{program} does not exist"));
        }
        Some(o.notify_command.clone())
    };

    // Cleanup.
    let cl = &raw.cleanup;
    let delete_after = c.duration("cleanup.delete_after", &cl.delete_after);
    let only_when_free_below = if cl.only_when_free_below.trim() == "always" {
        None
    } else {
        Some(c.size("cleanup.only_when_free_below", &cl.only_when_free_below))
    };
    if matches!(cl.gate, Gate::Readback | Gate::Both) && t.verify != VerifyMode::Readback {
        c.problem(
            "cleanup.gate",
            "\"readback\" and \"both\" need transfer.verify = \"readback\"",
        );
    }
    if cl.delete != DeleteMode::Off {
        {
            let (after, watched) = (delete_after, watch_completed_for);
            if after < watched {
                c.warn(
                    "cleanup.delete_after",
                    format!(
                        "is shorter than transfer.watch_completed_for ({}); runs are deleted only once no longer watched",
                        humantime::format_duration(watched)
                    ),
                );
            }
        }
    }
    match cl.delete {
        DeleteMode::On => c.warn(
            "cleanup.delete",
            "deleting offloaded source runs is ENABLED",
        ),
        DeleteMode::DryRun => c.warn("cleanup.delete", "dry-run: deletions are only logged"),
        DeleteMode::Off => {}
    }

    // Guard.
    if raw.guard.allowed_fstypes.is_empty() {
        c.problem(
            "guard.allowed_fstypes",
            "needs at least one filesystem type, e.g. \"nfs4\"",
        );
    }
    for (i, t) in raw.guard.allowed_fstypes.iter().enumerate() {
        if t.is_empty()
            || !t
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.')
        {
            c.problem(
                &format!("guard.allowed_fstypes[{i}]"),
                format!("{:?} is not a filesystem type", escape_control(t)),
            );
        }
    }

    if !c.problems.is_empty() {
        return Err(c.problems);
    }
    let (Some(dest_root), Some(root), Some(state_dir), Some(rate_limit)) =
        (dest, src, state, rate_limit)
    else {
        return Err(vec!["internal: validation incomplete".into()]);
    };
    Ok(Loaded {
        config: Config {
            dest_root,
            target_id: raw.target_id.clone(),
            dest_starts_at: raw.dest_starts_at,
            state_dir,
            source: Source {
                root,
                layout: raw.source.layout,
                run_markers: raw.source.run_markers.clone(),
                complete_markers: raw.source.complete_markers.clone(),
                ignore: raw.source.ignore.clone(),
                full_rescan_every,
            },
            selection: Selection {
                include: raw.selection.include.clone(),
                exclude: raw.selection.exclude.clone(),
                defer_until_complete: raw.selection.defer_until_complete.clone(),
            },
            bandwidth: Bandwidth {
                workers,
                rate_limit,
                network_speed,
            },
            transfer: Transfer {
                chunk_size,
                verify: t.verify,
                on_existing: t.on_existing,
                settle,
                settle_after_complete,
                stall_timeout,
                max_attempts: t.max_attempts,
                min_free,
                preserve_mtime: t.preserve_mtime,
                file_mode,
                watch_completed_for,
            },
            outage: Outage {
                check_every_max,
                warn_after,
                alert_after,
                remind_every,
                log_every,
                stable_before_resume,
                source_free_alert,
                notify_command,
            },
            cleanup: Cleanup {
                delete: cl.delete,
                gate: cl.gate,
                delete_after,
                rehash_source: cl.rehash_source,
                only_when_free_below,
            },
            allowed_fstypes: raw.guard.allowed_fstypes.clone(),
            log_file,
            log_format: raw.logging.format,
            state: StateCfg {
                interval: state_interval,
                events_max_size,
            },
        },
        warnings: c.warnings,
    })
}

impl Config {
    /// Human-readable summary of the effective settings, one per line.
    ///
    /// Implements: SRS-CFG-004
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn summary(&self) -> String {
        let mut s = String::new();
        let path = |p: &CheckedPath| {
            if p.configured == p.resolved {
                p.configured.display().to_string()
            } else {
                format!("{} -> {}", p.configured.display(), p.resolved.display())
            }
        };
        let b = &self.bandwidth;
        let per_worker = match b.rate_limit {
            Rate::Unlimited => String::new(),
            Rate::BytesPerSec(r) => format!(
                " (about {} per worker)",
                Rate::BytesPerSec(r / u64::from(b.workers.max(1)))
            ),
        };
        let link = match (b.rate_limit, b.network_speed) {
            (Rate::BytesPerSec(r), Some(Rate::BytesPerSec(l))) => {
                format!(
                    ", {}% of the {} link",
                    r.saturating_mul(100) / l.max(1),
                    Bits(l)
                )
            }
            _ => String::new(),
        };
        let t = &self.transfer;
        let lines = [
            (
                "source.root",
                format!(
                    "{} (device {}, layout {:?})",
                    path(&self.source.root),
                    self.source.root.dev,
                    self.source.layout
                ),
            ),
            (
                "dest_root",
                format!(
                    "{} (device {}, target_id {})",
                    path(&self.dest_root),
                    self.dest_root.dev,
                    self.target_id
                ),
            ),
            (
                "dest_starts_at",
                format!(
                    "{} (runs go to dest_root/{})",
                    self.dest_starts_at.as_str(),
                    match self.dest_starts_at {
                        DestLevel::Experiment => "<experiment>/<sample>/<run>/",
                        DestLevel::Sample => "<sample>/<run>/",
                        DestLevel::Run => "<run>/",
                    }
                ),
            ),
            ("state_dir", path(&self.state_dir)),
            ("run markers", self.source.run_markers.join(", ")),
            ("complete markers", self.source.complete_markers.join(", ")),
            ("include", self.selection.include.join(", ")),
            ("exclude", self.selection.exclude.join(", ")),
            ("deferred", self.selection.defer_until_complete.join(", ")),
            (
                "bandwidth",
                format!(
                    "{} workers, total {}{per_worker}{link}",
                    b.workers, b.rate_limit
                ),
            ),
            (
                "verify",
                format!(
                    "{:?}, on_existing {:?}, {} attempts",
                    t.verify, t.on_existing, t.max_attempts
                ),
            ),
            (
                "timing",
                format!(
                    "settle {}, after complete {}, stall {}",
                    humantime::format_duration(t.settle),
                    humantime::format_duration(t.settle_after_complete),
                    humantime::format_duration(t.stall_timeout)
                ),
            ),
            (
                "free space",
                format!("keep {} free on the destination", Bytes(t.min_free)),
            ),
            (
                "outage",
                format!(
                    "warn after {}, alert after {}",
                    humantime::format_duration(self.outage.warn_after),
                    humantime::format_duration(self.outage.alert_after)
                ),
            ),
            (
                "cleanup",
                format!(
                    "delete {:?}, gate {:?}",
                    self.cleanup.delete, self.cleanup.gate
                ),
            ),
            ("guard fstypes", self.allowed_fstypes.join(", ")),
        ];
        for (k, v) in lines {
            let _ = writeln!(s, "  {k:<17} {}", escape_control(&v));
        }
        s
    }
}

#[cfg(test)]
mod tests;
