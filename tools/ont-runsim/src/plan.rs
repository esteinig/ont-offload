// SPDX-License-Identifier: AGPL-3.0-or-later
//! Turn a scenario into concrete run plans: names, files, times, content and faults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::profile::{self, FileSpec, Kind, Profile};
use crate::rng::{SplitMix64, hash_str};
use crate::scenario::{ContentMode, Fill, Scenario, TimelineMode};
use crate::units::parse_duration;
use crate::{Error, LABEL_FILE, Result, io_err};

/// Upper bound on files per run, against runaway timelines.
pub const MAX_FILES_PER_RUN: usize = 200_000;

/// Identity of one simulated run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunId {
    /// Experiment folder.
    pub experiment: String,
    /// Sample folder.
    pub sample: String,
    /// Flow cell position.
    pub position: String,
    /// Flow cell id.
    pub flow_cell: String,
    /// Eight hex characters, as in MinKNOW folder names.
    pub run_hash: String,
    /// Acquisition id (eight hex characters).
    pub acq: String,
    /// Start time as `YYYYMMDD_HHMM` (UTC).
    pub date: String,
}

impl RunId {
    /// `<date>_<position>_<flow_cell>_<run_hash>`
    #[must_use]
    pub fn folder_name(&self) -> String {
        format!(
            "{}_{}_{}_{}",
            self.date, self.position, self.flow_cell, self.run_hash
        )
    }

    /// `<experiment>/<sample>/<folder>`
    #[must_use]
    pub fn rel_dir(&self) -> PathBuf {
        Path::new(&self.experiment)
            .join(&self.sample)
            .join(self.folder_name())
    }

    /// Fill the placeholders of a profile path template.
    #[must_use]
    pub fn expand(&self, template: &str) -> String {
        template
            .replace("{flow_cell}", &self.flow_cell)
            .replace("{run_hash}", &self.run_hash)
            .replace("{acq}", &self.acq)
            .replace("{date}", &self.date)
            .replace("{position}", &self.position)
    }
}

/// Where a file's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// Pseudo-random bytes; `printable` gives text lines.
    Random {
        /// Generator seed.
        seed: u64,
        /// Hex text with line breaks instead of raw bytes.
        printable: bool,
    },
    /// Zero bytes.
    Zero,
    /// Exact bytes (sample sheet, final summary, label).
    Text(Arc<Vec<u8>>),
    /// Bytes of an existing file, repeated as needed.
    Replay(PathBuf),
}

/// One file to write.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Path relative to the run folder.
    pub rel: String,
    /// Role.
    pub kind: Kind,
    /// Final size.
    pub size: u64,
    /// Simulated second (since simulation start) the file appears.
    pub created_s: f64,
    /// Simulated second the file is complete.
    pub closed_s: f64,
    /// Content source.
    pub content: Content,
}

/// A planned rewrite of a closed file (fault).
#[derive(Debug, Clone, Copy)]
pub struct Rewrite {
    /// Index into [`RunPlan::files`].
    pub file: usize,
    /// Simulated second of the rewrite.
    pub at_s: f64,
    /// Seed of the new content.
    pub seed: u64,
}

/// Everything about one run.
#[derive(Debug, Clone)]
pub struct RunPlan {
    /// Identity and folder.
    pub id: RunId,
    /// Simulated second the run starts.
    pub start_s: f64,
    /// Files sorted by `created_s`.
    pub files: Vec<PlannedFile>,
    /// Simulated second after which nothing more is written (abort fault).
    pub abort_s: Option<f64>,
    /// Rewrites of closed files (fault).
    pub rewrites: Vec<Rewrite>,
}

impl RunPlan {
    /// Simulated second the last write of this run happens.
    #[must_use]
    pub fn end_s(&self) -> f64 {
        let last = self
            .files
            .iter()
            .map(|f| f.closed_s)
            .chain(self.rewrites.iter().map(|r| r.at_s))
            .fold(self.start_s, f64::max);
        self.abort_s.map_or(last, |a| a.min(last))
    }

    /// Total planned bytes.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// `YYYYMMDD_HHMM` in UTC for a time.
#[must_use]
pub fn date_stamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    let rem = secs % 86_400;
    format!("{y:04}{m:02}{d:02}_{:02}{:02}", rem / 3600, rem / 60 % 60)
}

/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's algorithm.
#[allow(clippy::similar_names)]
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Files of a replay source run grouped by extension (`pod5`, `fastq.gz`, ...).
fn replay_pool(run: &Path) -> Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut pool: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut stack = vec![run.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(io_err("read folder", &dir))? {
            let path = entry.map_err(io_err("read folder", &dir))?.path();
            let meta = std::fs::symlink_metadata(&path).map_err(io_err("stat", &path))?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && meta.len() > 0 {
                pool.entry(ext_of(&path.to_string_lossy()))
                    .or_default()
                    .push(path);
            }
        }
    }
    for v in pool.values_mut() {
        v.sort();
    }
    Ok(pool)
}

fn ext_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.split_once('.')
        .map(|(_, e)| e.to_owned())
        .unwrap_or_default()
}

fn is_text(rel: &str) -> bool {
    ["csv", "txt", "tsv", "json", "md", "html"].contains(&ext_of(rel).as_str())
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn scale(size: u64, f: f64) -> u64 {
    if size == 0 {
        0
    } else {
        ((size as f64 * f).round() as u64).max(1)
    }
}

/// Build the run plans for a scenario, with run folders dated `now`.
///
/// # Errors
/// Profile or replay folder problems.
#[allow(clippy::too_many_lines)]
pub fn build(sc: &Scenario, now: SystemTime) -> Result<Vec<RunPlan>> {
    let mut rng = SplitMix64::new(sc.seed);
    let recorded = match sc.timeline {
        TimelineMode::Profile => {
            let path = sc
                .profile
                .as_deref()
                .ok_or_else(|| Error::Scenario("profile missing".into()))?;
            Some(Profile::load(path)?)
        }
        TimelineMode::Synthetic => None,
    };
    let pool = match (&sc.content, &sc.replay) {
        (ContentMode::Replay, Some(r)) => {
            let pool = replay_pool(&r.run)?;
            if pool.is_empty() {
                return Err(Error::Scenario(format!(
                    "replay run {} has no files",
                    r.run.display()
                )));
            }
            Some((r.run.clone(), pool))
        }
        _ => None,
    };
    let abort_after = sc
        .faults
        .abort_after
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(Error::Scenario)?;

    let mut plans = Vec::new();
    for (i, spec) in sc.runs.iter().enumerate() {
        let start_s = parse_duration(&spec.start_after)
            .map_err(Error::Scenario)?
            .as_secs_f64();
        let id = RunId {
            experiment: spec.experiment.clone(),
            sample: spec.sample.clone(),
            position: spec.position.clone().unwrap_or_else(|| {
                let row = char::from(b'A' + u8::try_from(i % 8).unwrap_or(0));
                format!("{}{row}", i / 8 + 1)
            }),
            flow_cell: spec
                .flow_cell
                .clone()
                .unwrap_or_else(|| format!("PAW{:05}", rng.below(100_000))),
            run_hash: rng.hex8(),
            acq: rng.hex8(),
            date: date_stamp(now + std::time::Duration::from_secs_f64(start_s / sc.speed)),
        };
        let timeline = match &recorded {
            Some(p) => profile::fit(p, sc.hours, sc.extend),
            None => profile::synthetic(&sc.synthetic_params()?, sc.hours, &mut rng),
        };
        if timeline.files.len() > MAX_FILES_PER_RUN {
            return Err(Error::Scenario(format!(
                "run {} would have {} files (limit {MAX_FILES_PER_RUN}); use fewer hours, a longer batch_every or extend = \"stretch\"",
                i,
                timeline.files.len()
            )));
        }
        let mut files: Vec<PlannedFile> = Vec::with_capacity(timeline.files.len() + 2);
        // The label is added below; a recorded simulator run already has one.
        let mut seen = std::collections::BTreeSet::from([LABEL_FILE.to_owned()]);
        for f in &timeline.files {
            let rel = id.expand(&f.path);
            if !seen.insert(rel.clone()) {
                continue;
            }
            files.push(plan_file(f, rel, start_s, sc, &mut rng, pool.as_ref()));
        }
        // Label: always first, so the folder is never mistaken for real data.
        let label = format!(
            "This folder was generated by ont-runsim {} (test tool).\nIt contains no sequencing data.\nseed={} content={:?} timeline={:?} hours={}\n",
            env!("CARGO_PKG_VERSION"),
            sc.seed,
            sc.content,
            sc.timeline,
            sc.hours
        );
        files.push(text_file(LABEL_FILE, Kind::Start, start_s, label));

        // Generated files with real content: sample sheet and final summary.
        let pod5 = files.iter().filter(|f| ext_of(&f.rel) == "pod5").count();
        let fastq = files.iter().filter(|f| f.rel.contains(".fastq")).count();
        let bam = files.iter().filter(|f| ext_of(&f.rel) == "bam").count();
        for f in &mut files {
            let name = f.rel.rsplit('/').next().unwrap_or(&f.rel);
            if name.starts_with("sample_sheet_") && ext_of(name) == "csv" {
                f.content = text(format!(
                    "flow_cell_id,kit,sample_id,experiment_id,position_id\n{},SQK-LSK114,{},{},{}\n",
                    id.flow_cell, id.sample, id.experiment, id.position
                ));
            } else if name.starts_with("final_summary_") && ext_of(name) == "txt" {
                f.content = text(format!(
                    "instrument=ont-runsim\nposition={}\nflow_cell_id={}\nsample_id={}\nprotocol_group_id={}\nprotocol_run_id={}\nacquisition_run_id={}\npod5_files_in_final_dest={pod5}\nfastq_files_in_final_dest={fastq}\nbam_files_in_final_dest={bam}\nsimulated=true\n",
                    id.position, id.flow_cell, id.sample, id.experiment, id.run_hash, id.acq
                ));
            }
            if let Content::Text(t) = &f.content {
                f.size = t.len() as u64;
            }
        }

        // Faults.
        let complete_s = files
            .iter()
            .filter(|f| f.rel.starts_with("final_summary_"))
            .map(|f| f.closed_s)
            .fold(start_s, f64::max);
        for n in 0..sc.faults.late_files {
            let at = complete_s + 120.0 + f64::from(n) * 10.0;
            files.push(text_file(
                &format!("other_reports/late_file_{n}.txt"),
                Kind::End,
                at,
                format!(
                    "written {}s after the final summary (fault: late_files)\n",
                    120 + n * 10
                ),
            ));
        }
        files.sort_by(|a, b| {
            a.created_s
                .total_cmp(&b.created_s)
                .then_with(|| a.rel.cmp(&b.rel))
        });
        let mut rewrites = Vec::new();
        let batch: Vec<usize> = files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.kind == Kind::Batch)
            .map(|(i, _)| i)
            .collect();
        if !batch.is_empty() {
            for _ in 0..sc
                .faults
                .rewrite_closed
                .min(u32::try_from(batch.len()).unwrap_or(u32::MAX))
            {
                let pick = batch[usize::try_from(rng.below(batch.len() as u64)).unwrap_or(0)];
                if rewrites.iter().any(|r: &Rewrite| r.file == pick) {
                    continue;
                }
                rewrites.push(Rewrite {
                    file: pick,
                    at_s: files[pick].closed_s + 60.0,
                    seed: rng.next_u64(),
                });
            }
        }
        plans.push(RunPlan {
            id,
            start_s,
            files,
            abort_s: abort_after.map(|a| start_s + a.as_secs_f64()),
            rewrites,
        });
    }
    Ok(plans)
}

fn text(s: String) -> Content {
    Content::Text(Arc::new(s.into_bytes()))
}

fn text_file(rel: &str, kind: Kind, at: f64, body: String) -> PlannedFile {
    PlannedFile {
        rel: rel.to_owned(),
        kind,
        size: body.len() as u64,
        created_s: at,
        closed_s: at,
        content: text(body),
    }
}

fn plan_file(
    f: &FileSpec,
    rel: String,
    start_s: f64,
    sc: &Scenario,
    rng: &mut SplitMix64,
    pool: Option<&(PathBuf, BTreeMap<String, Vec<PathBuf>>)>,
) -> PlannedFile {
    let seed = sc.seed ^ hash_str(&rel);
    let fake = match sc.fake.fill {
        Fill::Zero if !is_text(&rel) => Content::Zero,
        _ => Content::Random {
            seed,
            printable: is_text(&rel),
        },
    };
    let content = match pool {
        Some((run, pool)) => f
            .source
            .as_ref()
            .map(|s| run.join(s))
            .filter(|p| p.is_file())
            .or_else(|| {
                pool.get(&ext_of(&rel)).and_then(|v| {
                    usize::try_from(rng.below(v.len() as u64))
                        .ok()
                        .and_then(|i| v.get(i))
                        .cloned()
                })
            })
            .map_or(fake, Content::Replay),
        None => fake,
    };
    PlannedFile {
        rel,
        kind: f.kind,
        size: scale(f.size, sc.fake.size_scale),
        created_s: start_s + f.created_s,
        closed_s: start_s + f.closed_s,
        content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date_stamp(SystemTime::UNIX_EPOCH), "19700101_0000");
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_709_210_096);
        assert_eq!(date_stamp(t), "20240229_1234");
    }

    #[test]
    fn build_quick_preset() {
        let sc = Scenario::preset("quick-2h").unwrap();
        let plans = build(&sc, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(plans.len(), 1);
        let p = &plans[0];
        let dir = p.id.rel_dir();
        assert!(
            dir.to_string_lossy()
                .starts_with("runsim_quick/sample01/19700101_0000_1A_PAW"),
            "{}",
            dir.display()
        );
        assert!(p.files.iter().any(|f| f.rel == LABEL_FILE));
        let fs = p
            .files
            .iter()
            .find(|f| f.rel.starts_with("final_summary_"))
            .unwrap();
        let Content::Text(t) = &fs.content else {
            panic!("final summary is text")
        };
        assert!(String::from_utf8_lossy(t).contains("pod5_files_in_final_dest=12"));
        assert!(
            p.files.iter().all(|f| !f.rel.contains('{')),
            "placeholders filled"
        );
        // Same seed, same plan.
        let again = build(&sc, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(again[0].id, p.id);
    }

    #[test]
    fn promethion_72h_is_full_scale_with_bam() {
        let sc = Scenario::preset("promethion-72h").unwrap();
        let p = &build(&sc, SystemTime::UNIX_EPOCH).unwrap()[0];
        let count = |ext: &str| p.files.iter().filter(|f| ext_of(&f.rel) == ext).count();
        assert_eq!(count("pod5"), 864);
        assert_eq!(count("bam"), 2 * 864);
        assert!(p.files.iter().any(|f| f.rel.starts_with("bam_fail/")));
        let total = p.total_bytes();
        assert!(
            (1_200_000_000_000..1_400_000_000_000).contains(&total),
            "{total}"
        );
        let fs = p
            .files
            .iter()
            .find(|f| f.rel.starts_with("final_summary_"))
            .unwrap();
        let Content::Text(t) = &fs.content else {
            panic!("final summary is text")
        };
        assert!(String::from_utf8_lossy(t).contains("bam_files_in_final_dest=1728"));

        let bc = Scenario::preset("promethion-72h-barcoded").unwrap();
        let p = &build(&bc, SystemTime::UNIX_EPOCH).unwrap()[0];
        assert!(
            p.files
                .iter()
                .any(|f| f.rel.starts_with("bam_pass/barcode24/"))
        );
        assert!(
            p.files
                .iter()
                .any(|f| f.rel.starts_with("fastq_pass/unclassified/"))
        );
    }

    #[test]
    fn faults_are_planned() {
        let mut sc = Scenario::preset("quick-2h").unwrap();
        sc.faults.late_files = 2;
        sc.faults.rewrite_closed = 3;
        sc.faults.abort_after = Some("1h".into());
        let p = &build(&sc, SystemTime::UNIX_EPOCH).unwrap()[0];
        assert_eq!(
            p.files
                .iter()
                .filter(|f| f.rel.contains("late_file_"))
                .count(),
            2
        );
        assert!(!p.rewrites.is_empty());
        assert_eq!(p.abort_s, Some(3600.0));
        assert!((p.end_s() - 3600.0).abs() < 1e-9);
    }
}
