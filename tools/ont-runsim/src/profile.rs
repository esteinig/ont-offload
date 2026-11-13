// SPDX-License-Identifier: AGPL-3.0-or-later
//! Run timelines ("profiles"): which files a run writes, how big, and when.
//!
//! Paths are templates relative to the run folder. Placeholders:
//! `{flow_cell}`, `{run_hash}`, `{acq}` (acquisition id), `{date}`
//! (`YYYYMMDD_HHMM`) and `{position}`.

use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::rng::SplitMix64;
use crate::scenario::{Extend, SyntheticParams};
use crate::{Error, Result, io_err};

/// Current profile format version.
pub const PROFILE_VERSION: u32 = 1;

/// Role of a file in the run's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Written when the run starts (sample sheet, pore scan).
    Start,
    /// Output batch, written over one batch interval (pod5, fastq, bam).
    Batch,
    /// Written for the whole run.
    Growing,
    /// Written when the run ends (summaries, reports, final summary).
    End,
}

/// One file of a timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileSpec {
    /// Path template relative to the run folder.
    pub path: String,
    /// Role.
    pub kind: Kind,
    /// Final size in bytes.
    pub size: u64,
    /// Seconds after run start when the file appears.
    pub created_s: f64,
    /// Seconds after run start when the file is complete.
    pub closed_s: f64,
    /// Original path in the recorded run (for replayed content).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// A run timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Format version.
    pub profile_version: u32,
    /// Where it came from (folder name of a recorded run, or `synthetic`).
    pub source_run: String,
    /// Run length in seconds (until the last file closes).
    pub duration_seconds: f64,
    /// Files, sorted by `created_s`.
    pub files: Vec<FileSpec>,
}

impl Profile {
    /// Load profile JSON.
    ///
    /// # Errors
    /// I/O or format errors.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(io_err("read", path))?;
        let p: Self = serde_json::from_str(&text)
            .map_err(|e| Error::Profile(format!("{}: {e}", path.display())))?;
        if p.profile_version != PROFILE_VERSION {
            return Err(Error::Profile(format!(
                "{}: profile_version {} is not supported (expected {PROFILE_VERSION})",
                path.display(),
                p.profile_version
            )));
        }
        for f in &p.files {
            if f.path.starts_with('/') || f.path.split('/').any(|c| c == ".." || c.is_empty()) {
                return Err(Error::Profile(format!(
                    "unsafe path {:?} in profile",
                    f.path
                )));
            }
        }
        Ok(p)
    }

    /// Save as pretty JSON.
    ///
    /// # Errors
    /// I/O errors.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(|e| Error::Profile(e.to_string()))?;
        std::fs::write(path, text + "\n").map_err(io_err("write", path))
    }

    fn sort(&mut self) {
        self.files.sort_by(|a, b| {
            a.created_s
                .total_cmp(&b.created_s)
                .then_with(|| a.path.cmp(&b.path))
        });
        self.duration_seconds = self.files.iter().map(|f| f.closed_s).fold(0.0, f64::max);
    }
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

/// Identity parsed from a MinKNOW run folder name
/// `<YYYYMMDD>_<HHMM>_<position>_<flow_cell>_<run_hash>`.
struct FolderId {
    date: String,
    position: String,
    flow_cell: String,
    run_hash: String,
}

fn parse_folder(name: &str) -> Option<FolderId> {
    let parts: Vec<&str> = name.split('_').collect();
    if parts.len() != 5 || parts[0].len() != 8 || parts[1].len() != 4 {
        return None;
    }
    Some(FolderId {
        date: format!("{}_{}", parts[0], parts[1]),
        position: parts[2].to_owned(),
        flow_cell: parts[3].to_owned(),
        run_hash: parts[4].to_owned(),
    })
}

fn template(rel: &str, id: Option<&FolderId>) -> String {
    let Some(id) = id else { return rel.to_owned() };
    let mut t = rel
        .replace(&id.flow_cell, "{flow_cell}")
        .replace(&id.run_hash, "{run_hash}")
        .replace(&id.date, "{date}");
    // An 8-hex acquisition id follows "{run_hash}_" in MinKNOW file names.
    let key = "{run_hash}_";
    if let Some(pos) = t.find(key) {
        let start = pos + key.len();
        if t.len() >= start + 8 && t[start..start + 8].bytes().all(|b| b.is_ascii_hexdigit()) {
            t.replace_range(start..start + 8, "{acq}");
        }
    }
    if id.position.len() > 2 {
        t = t.replace(&format!("_{}_", id.position), "_{position}_");
    }
    t
}

fn classify(rel: &str, created: f64, closed: f64, duration: f64) -> Kind {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let top = rel.split('/').next().unwrap_or("");
    if rel.contains('/')
        && (top.starts_with("pod5")
            || top.starts_with("fastq")
            || top.starts_with("bam")
            || top.starts_with("fast5"))
    {
        Kind::Batch
    } else if name.starts_with("sample_sheet") {
        Kind::Start
    } else if [
        "final_summary",
        "sequencing_summary",
        "report_",
        "pore_activity",
        "throughput",
        "barcode_alignment",
    ]
    .iter()
    .any(|p| name.starts_with(p))
    {
        Kind::End
    } else if created < 0.05 * duration && closed > 0.9 * duration {
        Kind::Growing
    } else if closed > 0.9 * duration {
        Kind::End
    } else {
        Kind::Start
    }
}

fn secs(t: SystemTime) -> f64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Record the timeline of a finished run folder from file metadata only.
///
/// Creation times come from the file birth time where the filesystem has
/// one, otherwise each file is assumed to be written just before it was
/// last modified.
///
/// # Errors
/// I/O errors, or a folder with no files.
pub fn record(run: &Path) -> Result<Profile> {
    let name = run
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let id = parse_folder(&name);
    let mut raw = Vec::new();
    let mut stack = vec![run.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(io_err("read folder", &dir))? {
            let entry = entry.map_err(io_err("read folder", &dir))?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).map_err(io_err("stat", &path))?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                let Ok(rel) = path.strip_prefix(run) else {
                    continue;
                };
                let Some(rel) = rel.to_str() else {
                    return Err(Error::Profile(format!(
                        "non-UTF-8 file name under {}",
                        run.display()
                    )));
                };
                let modified = meta.modified().map_or(0.0, secs);
                let created = meta.created().map_or(modified, secs).min(modified);
                raw.push((rel.replace('\\', "/"), meta.len(), created, modified));
            }
        }
    }
    if raw.is_empty() {
        return Err(Error::Profile(format!("no files in {}", run.display())));
    }
    let t0 = raw.iter().map(|r| r.2).fold(f64::INFINITY, f64::min);
    let duration = raw.iter().map(|r| r.3 - t0).fold(0.0, f64::max).max(1.0);
    let mut profile = Profile {
        profile_version: PROFILE_VERSION,
        source_run: name,
        duration_seconds: duration,
        files: raw
            .into_iter()
            .map(|(rel, size, c, m)| {
                let (created_s, closed_s) = (c - t0, m - t0);
                FileSpec {
                    path: template(&rel, id.as_ref()),
                    kind: classify(&rel, created_s, closed_s, duration),
                    size,
                    created_s,
                    closed_s,
                    source: Some(rel),
                }
            })
            .collect(),
    };
    profile.sort();
    Ok(profile)
}

// ---------------------------------------------------------------------------
// Synthetic timelines
// ---------------------------------------------------------------------------

fn spec(path: String, kind: Kind, size: u64, created_s: f64, closed_s: f64) -> FileSpec {
    FileSpec {
        path,
        kind,
        size,
        created_s,
        closed_s,
        source: None,
    }
}

/// A MinKNOW-like timeline of `hours`: start files, batch output every
/// `batch_every`, growing reports and end-of-run summaries, with the final
/// summary last.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]
pub fn synthetic(p: &SyntheticParams, hours: f64, rng: &mut SplitMix64) -> Profile {
    let end = hours * 3600.0;
    let every = p.batch_every_s;
    let batches = ((end / every).floor() as u64).max(1);
    let stem = "{flow_cell}_{run_hash}_{acq}";
    let mut files = vec![
        spec(
            "sample_sheet_{flow_cell}_{date}_{run_hash}.csv".into(),
            Kind::Start,
            0,
            0.0,
            1.0,
        ),
        spec(
            format!("other_reports/pore_scan_data_{stem}.csv"),
            Kind::Start,
            rng.jitter(2_000_000, 0.1),
            30.0,
            300.0_f64.min(end),
        ),
        spec(
            format!("other_reports/temperature_{stem}.csv"),
            Kind::Growing,
            rng.jitter(1_000_000, 0.1),
            0.0,
            end,
        ),
    ];
    let barcodes: Vec<Option<u32>> = if p.barcodes == 0 {
        vec![None]
    } else {
        (1..=p.barcodes).map(Some).chain([None]).collect()
    };
    for n in 0..batches {
        let c = n as f64 * every;
        let d = ((n + 1) as f64 * every).min(end);
        if p.pod5 > 0 {
            files.push(spec(
                format!("pod5/{stem}_{n}.pod5"),
                Kind::Batch,
                rng.jitter(p.pod5, 0.1),
                c,
                d,
            ));
        }
        for (dir, tag, size, ext) in [
            ("fastq_pass", "pass", p.fastq_pass, "fastq.gz"),
            ("fastq_fail", "fail", p.fastq_fail, "fastq.gz"),
            ("bam_pass", "pass", p.bam_pass, "bam"),
            ("bam_fail", "fail", p.bam_fail, "bam"),
        ] {
            if size == 0 {
                continue;
            }
            let share = size / barcodes.len() as u64;
            for bc in &barcodes {
                let (sub, label) = match bc {
                    Some(b) => (format!("barcode{b:02}/"), format!("barcode{b:02}")),
                    None if p.barcodes > 0 => {
                        ("unclassified/".to_owned(), "unclassified".to_owned())
                    }
                    None => (String::new(), String::new()),
                };
                let name = if label.is_empty() {
                    format!("{dir}/{{flow_cell}}_{tag}_{{run_hash}}_{{acq}}_{n}.{ext}")
                } else {
                    format!("{dir}/{sub}{{flow_cell}}_{tag}_{label}_{{run_hash}}_{{acq}}_{n}.{ext}")
                };
                files.push(spec(name, Kind::Batch, rng.jitter(share.max(1), 0.2), c, d));
            }
        }
    }
    let summary = (hours * 50_000_000.0) as u64;
    let mut end_files = vec![
        (
            format!("sequencing_summary_{stem}.txt"),
            rng.jitter(summary.max(1000), 0.1),
        ),
        (
            format!("pore_activity_{stem}.csv"),
            rng.jitter(200_000, 0.1),
        ),
        (format!("throughput_{stem}.csv"), rng.jitter(100_000, 0.1)),
        (
            "report_{flow_cell}_{date}_{run_hash}.json".to_owned(),
            rng.jitter(3_000_000, 0.1),
        ),
        (
            "report_{flow_cell}_{date}_{run_hash}.html".to_owned(),
            rng.jitter(2_000_000, 0.1),
        ),
        (
            "report_{flow_cell}_{date}_{run_hash}.md".to_owned(),
            rng.jitter(10_000, 0.1),
        ),
    ];
    if p.barcodes > 0 {
        end_files.push((
            format!("barcode_alignment_{stem}.tsv"),
            rng.jitter(5_000, 0.1),
        ));
    }
    for (i, (path, size)) in end_files.into_iter().enumerate() {
        let t = end + 5.0 + i as f64;
        files.push(spec(path, Kind::End, size, end, t));
    }
    // The final summary is the run's complete marker: always last.
    files.push(spec(
        format!("final_summary_{stem}.txt"),
        Kind::End,
        0,
        end + 29.0,
        end + 30.0,
    ));
    let mut profile = Profile {
        profile_version: PROFILE_VERSION,
        source_run: "synthetic".into(),
        duration_seconds: 0.0,
        files,
    };
    profile.sort();
    profile
}

// ---------------------------------------------------------------------------
// Fitting to a run length
// ---------------------------------------------------------------------------

/// Insert `_r<k>` before the file extension(s) of the last path component.
fn repeat_name(path: &str, k: u64) -> String {
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
    let (stem, ext) = name.split_once('.').map_or((name, ""), |(s, e)| (s, e));
    let renamed = if ext.is_empty() {
        format!("{stem}_r{k}")
    } else {
        format!("{stem}_r{k}.{ext}")
    };
    if dir.is_empty() {
        renamed
    } else {
        format!("{dir}/{renamed}")
    }
}

/// Fit a profile to `hours` of sequencing.
///
/// `RepeatBatches` keeps the batch rhythm: batches are repeated (renamed
/// with `_r1`, `_r2`, ...) or cut to fill the new length, growing files
/// close at the new end and end files move with it. `Stretch` scales every
/// time by the same factor.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn fit(profile: &Profile, hours: f64, extend: Extend) -> Profile {
    let target = hours * 3600.0;
    let batch: Vec<&FileSpec> = profile
        .files
        .iter()
        .filter(|f| f.kind == Kind::Batch)
        .collect();
    let b0 = batch
        .iter()
        .map(|f| f.created_s)
        .fold(f64::INFINITY, f64::min);
    let b1 = batch.iter().map(|f| f.closed_s).fold(0.0, f64::max);
    // Length of sequencing (batch output) in the original.
    let seq_len = if batch.is_empty() {
        profile.duration_seconds
    } else {
        b1
    };
    let mut out = Profile {
        files: Vec::with_capacity(profile.files.len()),
        source_run: profile.source_run.clone(),
        ..profile.clone()
    };
    match extend {
        Extend::Stretch => {
            let f = target / seq_len.max(1.0);
            for s in &profile.files {
                out.files.push(FileSpec {
                    created_s: s.created_s * f,
                    closed_s: s.closed_s * f,
                    ..s.clone()
                });
            }
        }
        Extend::RepeatBatches => {
            let shift = target - seq_len;
            // A recorded run whose batches all carry nearly the same time (for
            // example a copy that lost its timestamps) would repeat thousands of
            // times; one repeat cycle is at least a minute.
            let span = (b1 - b0).max(60.0);
            let new_b1 = b1 + shift;
            for s in &profile.files {
                match s.kind {
                    Kind::Start => out.files.push(s.clone()),
                    Kind::Growing => out.files.push(FileSpec {
                        closed_s: (s.closed_s + shift).max(s.created_s),
                        ..s.clone()
                    }),
                    Kind::End => out.files.push(FileSpec {
                        created_s: (s.created_s + shift).max(0.0),
                        closed_s: (s.closed_s + shift).max(0.0),
                        ..s.clone()
                    }),
                    Kind::Batch => {
                        let cycles = ((new_b1 - b0) / span).ceil().max(1.0) as u64;
                        for k in 0..cycles {
                            let off = k as f64 * span;
                            let created = s.created_s + off;
                            if created >= new_b1 {
                                break;
                            }
                            out.files.push(FileSpec {
                                path: if k == 0 {
                                    s.path.clone()
                                } else {
                                    repeat_name(&s.path, k)
                                },
                                created_s: created,
                                closed_s: (s.closed_s + off).min(new_b1).max(created),
                                ..s.clone()
                            });
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> SyntheticParams {
        SyntheticParams {
            batch_every_s: 600.0,
            pod5: 1000,
            fastq_pass: 100,
            fastq_fail: 10,
            bam_pass: 50,
            bam_fail: 5,
            barcodes: 0,
        }
    }

    #[test]
    fn synthetic_shape() {
        let p = synthetic(&params(), 2.0, &mut SplitMix64::new(1));
        let pod5 = p
            .files
            .iter()
            .filter(|f| f.path.starts_with("pod5/"))
            .count();
        assert_eq!(pod5, 12);
        let last = p
            .files
            .iter()
            .max_by(|a, b| a.closed_s.total_cmp(&b.closed_s))
            .unwrap();
        assert!(last.path.starts_with("final_summary_"));
        assert!((p.duration_seconds - 7230.0).abs() < 1e-6);
        assert!(p.files.iter().any(|f| f.path.starts_with("sample_sheet_")));
        let bc = synthetic(
            &SyntheticParams {
                barcodes: 3,
                ..params()
            },
            1.0,
            &mut SplitMix64::new(1),
        );
        assert!(
            bc.files
                .iter()
                .any(|f| f.path.starts_with("fastq_pass/barcode03/"))
        );
        assert!(
            bc.files
                .iter()
                .any(|f| f.path.starts_with("fastq_fail/unclassified/"))
        );
        assert!(
            bc.files
                .iter()
                .any(|f| f.path.starts_with("bam_pass/barcode02/") && f.path.contains(".bam"))
        );
    }

    #[test]
    fn repeat_batches_extends_and_cuts() {
        let p = synthetic(&params(), 2.0, &mut SplitMix64::new(1));
        let long = fit(&p, 6.0, Extend::RepeatBatches);
        let pod5 = long
            .files
            .iter()
            .filter(|f| f.path.starts_with("pod5/"))
            .count();
        assert_eq!(pod5, 36);
        assert!(long.files.iter().any(|f| f.path.contains("_r2.pod5")));
        assert!((long.duration_seconds - (6.0 * 3600.0 + 30.0)).abs() < 1e-6);
        let mut names: Vec<_> = long.files.iter().map(|f| &f.path).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), long.files.len(), "unique paths");

        let short = fit(&p, 1.0, Extend::RepeatBatches);
        assert_eq!(
            short
                .files
                .iter()
                .filter(|f| f.path.starts_with("pod5/"))
                .count(),
            6
        );
        assert!(short.files.iter().all(|f| f.closed_s >= f.created_s));
    }

    #[test]
    fn stretch_scales_times() {
        let p = synthetic(&params(), 2.0, &mut SplitMix64::new(1));
        let s = fit(&p, 4.0, Extend::Stretch);
        assert_eq!(s.files.len(), p.files.len());
        assert!((s.duration_seconds - 2.0 * p.duration_seconds).abs() < 1e-6);
    }

    #[test]
    fn templates_from_folder_name() {
        let id = parse_folder("20240101_1200_1A_PAW12345_abcdef12").unwrap();
        assert_eq!(
            template("pod5/PAW12345_abcdef12_0a1b2c3d_7.pod5", Some(&id)),
            "pod5/{flow_cell}_{run_hash}_{acq}_7.pod5"
        );
        assert_eq!(
            template("report_PAW12345_20240101_1200_abcdef12.html", Some(&id)),
            "report_{flow_cell}_{date}_{run_hash}.html"
        );
        assert_eq!(repeat_name("pod5/a_1.pod5", 2), "pod5/a_1_r2.pod5");
        assert_eq!(repeat_name("x.fastq.gz", 1), "x_r1.fastq.gz");
    }
}
