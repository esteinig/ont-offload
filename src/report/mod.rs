// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reports for clinical operations (SWI-REPORT): a status report over a
//! period, built from the event archive and the log, and a verification
//! report of an execution against an independent reference. Every report
//! is a JSON record first; the PDF is a rendering of it. Reports only
//! read: nothing in the state folder or on the share is changed.

pub mod history;
pub mod render;
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
#[allow(clippy::struct_excessive_bools)]
pub struct Formats {
    /// The JSON record.
    pub json: bool,
    /// The PDF.
    pub pdf: bool,
    /// The typst sources with the record, to compile elsewhere.
    pub typ: bool,
}

/// Formats when `--format` is not given: the PDF and its record, or only
/// the record in a build without PDF support.
pub const DEFAULT_FORMATS: &str = if cfg!(feature = "report") {
    "pdf,json"
} else {
    "json"
};

impl Formats {
    /// Parse `pdf`, `json` and `typ` (comma-separated). The PDF always
    /// comes with its record: it names the record's digest.
    ///
    /// # Errors
    /// [`Error::Config`] for an unknown or empty list.
    pub fn parse(s: &str) -> Result<Self> {
        let mut f = Self::default();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part {
                "json" => f.json = true,
                "pdf" => {
                    f.pdf = true;
                    f.json = true;
                }
                "typ" => f.typ = true,
                other => {
                    return Err(Error::Config(format!(
                        "--format: unknown format `{other}` (pdf, json, typ)"
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
    /// `--format` (else [`DEFAULT_FORMATS`]).
    pub format: Option<String>,
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

/// Write the outputs of a report into `dir`: each file atomically (names
/// may hold one folder level), then a `sha256sum` file of them all.
/// Returns the names written at the top of `dir`.
fn write_outputs(
    fs: &dyn Fs,
    dir: &Path,
    stem: &str,
    files: &[(String, Vec<u8>)],
) -> Result<Vec<String>> {
    use std::fmt::Write as _;
    fs.create_dir_all(dir)?;
    let mut sums = String::new();
    let mut names = Vec::new();
    for (name, data) in files {
        let path = dir.join(name);
        let parent = path.parent().unwrap_or(dir);
        fs.create_dir_all(parent)?;
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        write_atomic(fs, parent, &file, data)?;
        let _ = writeln!(sums, "{}  {name}", sha256_hex(data));
        let top = name.split('/').next().unwrap_or(name).to_owned();
        if !names.contains(&top) {
            names.push(top);
        }
    }
    let name = format!("{stem}.sha256");
    write_atomic(fs, dir, &name, sums.as_bytes())?;
    names.push(name);
    Ok(names)
}

/// The record, the PDF and the typst sources as asked for.
fn render_outputs(
    t: render::Template,
    record: &[u8],
    formats: Formats,
    stem: &str,
    mut extras: render::Extras,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut json = record.to_vec();
    json.push(b'\n');
    extras.digest = sha256_hex(&json);
    let mut files = Vec::new();
    if formats.pdf {
        files.push((format!("{stem}.pdf"), render::pdf(t, &json, &extras)?));
    }
    if formats.json {
        files.push((format!("{stem}.json"), json.clone()));
    }
    if formats.typ {
        for (name, data) in render::bundle(t, &json, &extras) {
            files.push((format!("{stem}.typst/{name}"), data));
        }
    }
    Ok(files)
}

/// The logo named in `[report]`, read for the template.
fn read_logo(fs: &dyn Fs, logo: Option<&Path>) -> Result<Option<(String, Vec<u8>)>> {
    let Some(path) = logo else { return Ok(None) };
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let bytes = fs.read(path).map_err(|e| {
        Error::Config(format!(
            "report.logo {}: {e}",
            escape_control(&path.to_string_lossy())
        ))
    })?;
    Ok(Some((format!("logo.{ext}"), bytes)))
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
    let formats = Formats::parse(args.format.as_deref().unwrap_or(DEFAULT_FORMATS))?;
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
    let extras = render::Extras {
        appendix: args.appendix,
        logo: read_logo(fs, report_cfg.as_ref().and_then(|r| r.logo.as_deref()))?,
        when: now,
        ..render::Extras::default()
    };
    let files = render_outputs(render::Template::Status, &json, formats, &stem, extras)?;
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
