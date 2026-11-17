// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reports for clinical operations (SWI-REPORT): a status report over a
//! period, built from the event archive and the log, and a verification
//! report of an execution against an independent reference. Every report
//! is a JSON record first; the PDF is a rendering of it. Reports only
//! read: nothing in the state folder or on the share is changed.

pub mod history;
pub mod status;
pub mod time;

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::{Error, Result};
use crate::fsio::Fs;
use crate::journal::{sha256_hex, write_atomic};
use crate::logging::escape_control;

/// Output formats of a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Formats {
    /// The JSON record.
    pub json: bool,
}

impl Formats {
    /// Parse `json` (comma-separated).
    ///
    /// # Errors
    /// [`Error::Config`] for an unknown or empty list.
    pub fn parse(s: &str) -> Result<Self> {
        let mut f = Self::default();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part {
                "json" => f.json = true,
                other => {
                    return Err(Error::Config(format!(
                        "--format: unknown format `{other}` (json)"
                    )));
                }
            }
        }
        if f == Self::default() {
            return Err(Error::Config("--format: name at least one format".into()));
        }
        Ok(f)
    }
}

/// Settings of `report status` from the command line.
#[derive(Debug, Clone, Default)]
pub struct StatusArgs {
    /// `--from`.
    pub from: Option<String>,
    /// `--to`.
    pub to: Option<String>,
    /// `--month`.
    pub month: Option<String>,
    /// `--last`.
    pub last: Option<String>,
    /// `--state-dir` (else `state_dir` of the configuration).
    pub state_dir: Option<PathBuf>,
    /// `--log` (else `logging.file` of the configuration).
    pub log: Option<PathBuf>,
    /// `--out`.
    pub out: PathBuf,
    /// `--format`.
    pub format: String,
    /// `--check-destination`.
    pub check_destination: bool,
    /// `--appendix`.
    pub appendix: bool,
}

/// The configuration, or `None` when the flags name every path and the
/// configuration cannot be loaded (a report on a copy of the records).
fn load_config(
    config_path: &Path,
    fs: &dyn Fs,
    flags_suffice: bool,
) -> Result<Option<config::Config>> {
    match config::load(config_path, fs) {
        Ok(l) => Ok(Some(l.config)),
        Err(e) if flags_suffice => {
            log::warn!("configuration not used: {e}");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Write the outputs of a report into `dir`: each file atomically, then a
/// `sha256sum` file of them all. Returns the file names.
fn write_outputs(
    fs: &dyn Fs,
    dir: &Path,
    stem: &str,
    files: &[(String, Vec<u8>)],
) -> Result<Vec<String>> {
    fs.create_dir_all(dir)?;
    let mut sums = String::new();
    let mut names = Vec::new();
    for (ext, data) in files {
        let name = format!("{stem}.{ext}");
        write_atomic(fs, dir, &name, data)?;
        sums.push_str(&format!("{}  {name}\n", sha256_hex(data)));
        names.push(name);
    }
    let name = format!("{stem}.sha256");
    write_atomic(fs, dir, &name, sums.as_bytes())?;
    names.push(name);
    Ok(names)
}

/// A file-name-safe form of a host name.
fn safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `report status`: the status report of a period.
///
/// Implements: SRS-RPT-001, SRS-RPT-005
///
/// # Errors
/// [`Error::Config`] for a bad period, format or configuration; I/O errors
/// writing the outputs.
pub fn status_report(
    config_path: &Path,
    fs: &dyn Fs,
    args: &StatusArgs,
    now: u64,
    out: &mut dyn Write,
) -> Result<()> {
    let formats = Formats::parse(&args.format)?;
    let period = time::period(
        args.from.as_deref(),
        args.to.as_deref(),
        args.month.as_deref(),
        args.last.as_deref(),
        now,
    )
    .map_err(Error::Config)?;
    let cfg = load_config(config_path, fs, args.state_dir.is_some())?;
    let state_dir = args
        .state_dir
        .clone()
        .or_else(|| cfg.as_ref().map(|c| c.state_dir.resolved.clone()))
        .ok_or_else(|| Error::Config("no state folder: pass --state-dir".into()))?;
    let log = args
        .log
        .clone()
        .or_else(|| cfg.as_ref().and_then(|c| c.log_file.clone()));
    let report_cfg = cfg.as_ref().map(|c| c.report.clone());
    let prefix = report_cfg
        .as_ref()
        .map_or_else(|| "OOF".to_owned(), |r| r.prefix.clone());
    let inputs = status::StatusInputs {
        state_dir,
        log,
        period,
        check_destination: args.check_destination,
        appendix: args.appendix,
        generated_at: now,
        host: crate::journal::host_name(fs),
        site: report_cfg.as_ref().and_then(|r| r.site.clone()),
        prefix: prefix.clone(),
        target_id: cfg.as_ref().map(|c| c.target_id.clone()),
        destination: cfg
            .as_ref()
            .map(|c| c.dest_root.configured.to_string_lossy().into_owned()),
        sources: cfg
            .as_ref()
            .map(|c| {
                c.sources
                    .iter()
                    .map(|s| status::SourceOut {
                        name: s.name.clone(),
                        root: s.root.configured.to_string_lossy().into_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    };
    let rec = status::build(fs, &inputs);
    let json = serde_json::to_vec_pretty(&rec).map_err(|e| Error::Config(e.to_string()))?;
    let stem = format!(
        "{prefix}-SR-{}-{}-{}",
        safe(&rec.host),
        time::compact_day(period.from),
        time::compact_day(period.to - 1)
    );
    let mut files = Vec::new();
    if formats.json {
        let mut j = json.clone();
        j.push(b'\n');
        files.push(("json".to_owned(), j));
    }
    let names = write_outputs(fs, &args.out, &stem, &files)?;
    writeln!(
        out,
        "status report {}: {} ({} to {}): {} runs, {} committed; {}",
        rec.report_id,
        rec.verdict.status.replace('_', " ").to_uppercase(),
        rec.period.from,
        rec.period.last_day,
        rec.totals.runs_active,
        config::units::Bytes(rec.totals.bytes_committed),
        names
            .iter()
            .map(|n| escape_control(&args.out.join(n).to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    Ok(())
}
