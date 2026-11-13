// SPDX-License-Identifier: AGPL-3.0-or-later
//! ont-runsim CLI.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgGroup, Parser, Subcommand};
use ont_runsim::engine::{self, Options};
use ont_runsim::scenario::Scenario;
use ont_runsim::status::{self, Status};
use ont_runsim::units::{bytes, hms, parse_size};
use ont_runsim::{Error, PRESETS, check, clean, plan, profile};

/// Simulate MinKNOW sequencing runs on disk (test tool).
///
/// Writes run folders below --out over time, as MinKNOW would, and keeps a
/// SHA-256 manifest of every file so an offloaded copy can be checked.
/// Every run folder carries a SIMULATED_RUN.txt label.
#[allow(clippy::doc_markdown)]
#[derive(Debug, Parser)]
#[command(name = "ont-runsim", version, about, long_about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Record the timeline of an existing run (file metadata only, no content).
    Profile {
        /// Finished run folder.
        run: PathBuf,
        /// Output profile JSON.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Generate runs over time.
    #[command(group(ArgGroup::new("source").required(true).args(["scenario", "preset"])))]
    Run {
        /// Scenario file (TOML).
        #[arg(long)]
        scenario: Option<PathBuf>,
        /// Built-in preset: quick-2h, promethion-24h, outage-drill.
        #[arg(long)]
        preset: Option<String>,
        /// Output folder (created; must be new, empty or a previous simulator folder).
        #[arg(long, default_value = "/data/ont-runsim")]
        out: PathBuf,
        /// Simulated run length in hours (overrides the scenario).
        #[arg(long)]
        hours: Option<f64>,
        /// Simulated seconds per wall-clock second, e.g. 60 (overrides the scenario).
        #[arg(long)]
        speed: Option<f64>,
        /// Random seed (overrides the scenario).
        #[arg(long)]
        seed: Option<u64>,
        /// Multiply every file size, e.g. 0.01 (overrides the scenario).
        #[arg(long)]
        size_scale: Option<f64>,
        /// Stop when free space on the output disk falls below this.
        #[arg(long, default_value = "500GB")]
        keep_free: String,
        /// Print the plan and exit without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// No progress lines.
        #[arg(long, short)]
        quiet: bool,
    },
    /// List the built-in presets.
    Presets,
    /// Print progress of a simulation.
    Status {
        /// Output folder.
        #[arg(long, default_value = "/data/ont-runsim")]
        out: PathBuf,
        /// Print the raw status JSON.
        #[arg(long)]
        json: bool,
    },
    /// Compare an offloaded copy of a run with the simulator's manifest.
    Check {
        /// Run folder on the destination.
        #[arg(long)]
        dest: PathBuf,
        /// Manifest file (default: <out>/.ont-runsim/manifests/<run folder name>.sha256).
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Simulator output folder, to find the manifest.
        #[arg(long, default_value = "/data/ont-runsim")]
        out: PathBuf,
        /// Further relative paths to leave out of the "extra files" list.
        #[arg(long)]
        ignore: Vec<String>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete everything the simulator created, and nothing else.
    Clean {
        /// Output folder.
        #[arg(long, default_value = "/data/ont-runsim")]
        out: PathBuf,
    },
}

fn exit_code(e: &Error) -> u8 {
    match e {
        Error::Io { .. } | Error::Mismatch(_) => 1,
        Error::Scenario(_) | Error::Profile(_) | Error::Refused(_) => 2,
        Error::DiskFull { .. } => 3,
    }
}

fn out_line(s: &str) {
    let _ = writeln!(std::io::stdout(), "{s}");
}

#[allow(clippy::similar_names)]
fn load_scenario(
    scenario: Option<PathBuf>,
    preset: Option<String>,
    hours: Option<f64>,
    speed: Option<f64>,
    seed: Option<u64>,
    size_scale: Option<f64>,
) -> Result<Scenario, Error> {
    let mut sc = match (scenario, preset) {
        (Some(p), _) => Scenario::load(&p)?,
        (None, Some(name)) => Scenario::preset(&name)?,
        (None, None) => return Err(Error::Scenario("choose --scenario or --preset".into())),
    };
    if let Some(h) = hours {
        sc.hours = h;
    }
    if let Some(s) = speed {
        sc.speed = s;
    }
    if let Some(s) = seed {
        sc.seed = s;
    }
    if let Some(s) = size_scale {
        sc.fake.size_scale = s;
    }
    sc.validate()?;
    Ok(sc)
}

#[allow(clippy::too_many_lines)]
fn run(cli: Cli) -> Result<(), Error> {
    match cli.command {
        Cmd::Profile { run, output } => {
            let p = profile::record(&run)?;
            p.save(&output)?;
            out_line(&format!(
                "profile of {}: {} files over {}, written to {}",
                p.source_run,
                p.files.len(),
                hms(p.duration_seconds),
                output.display()
            ));
        }
        Cmd::Presets => {
            for name in PRESETS {
                let sc = Scenario::preset(name)?;
                out_line(&format!(
                    "{name:<16} {} run(s), {} h at {}x speed (about {} wall clock), size scale {}",
                    sc.runs.len(),
                    sc.hours,
                    sc.speed,
                    hms(sc.hours * 3600.0 / sc.speed),
                    sc.fake.size_scale
                ));
            }
        }
        Cmd::Run {
            scenario,
            preset,
            out,
            hours,
            speed,
            seed,
            size_scale,
            keep_free,
            dry_run,
            quiet,
        } => {
            let sc = load_scenario(scenario, preset, hours, speed, seed, size_scale)?;
            let keep_free = parse_size(&keep_free).map_err(Error::Scenario)?;
            if dry_run {
                let plans = plan::build(&sc, std::time::SystemTime::now())?;
                let total: f64 = plans.iter().map(plan::RunPlan::end_s).fold(0.0, f64::max);
                for p in &plans {
                    out_line(&format!(
                        "{}  {} files, {}",
                        out.join(p.id.rel_dir()).display(),
                        p.files.len(),
                        bytes(p.total_bytes())
                    ));
                }
                out_line(&format!(
                    "{} simulated in about {} wall clock; {} in total",
                    hms(total),
                    hms(total / sc.speed),
                    bytes(plans.iter().map(plan::RunPlan::total_bytes).sum())
                ));
                return Ok(());
            }
            eprintln!(
                "ont-runsim: SIMULATED RUNS (test data) into {}; {} h at {}x speed",
                out.display(),
                sc.hours,
                sc.speed
            );
            let outcome = engine::simulate(
                &sc,
                &out,
                &Options {
                    keep_free,
                    progress_every: (!quiet).then_some(Duration::from_secs(10)),
                },
            )?;
            for r in &outcome.status.runs {
                out_line(&format!(
                    "{}  {}  {} files",
                    r.path, r.state, r.files_closed
                ));
            }
        }
        Cmd::Status { out, json } => {
            let s = Status::read(&out)?;
            if json {
                out_line(&std::fs::read_to_string(status::status_path(&out)).unwrap_or_default());
                return Ok(());
            }
            let state = if s.state == "running" && !s.is_live() {
                "stopped (process gone)".to_owned()
            } else {
                s.state.clone()
            };
            out_line(&format!(
                "{state}: simulated {} of {} ({}x), {} of {} files, {} written; updated {}",
                hms(s.sim_seconds),
                hms(s.total_seconds),
                s.speed,
                s.files_closed,
                s.files_planned,
                bytes(s.bytes_written),
                s.updated
            ));
            for r in &s.runs {
                out_line(&format!(
                    "  {}  {}  {}/{} files",
                    r.name, r.state, r.files_closed, r.files_planned
                ));
            }
            if let Some(m) = &s.message {
                out_line(&format!("  note: {m}"));
            }
        }
        Cmd::Check {
            dest,
            manifest,
            out,
            ignore,
            json,
        } => {
            let manifest = if let Some(m) = manifest {
                m
            } else {
                let name = dest.file_name().ok_or_else(|| {
                    Error::Scenario(format!("{} has no folder name", dest.display()))
                })?;
                status::manifests_dir(&out).join(format!("{}.sha256", name.to_string_lossy()))
            };
            let report = check::check(&dest, &manifest, &ignore)?;
            if json {
                out_line(&serde_json::to_string_pretty(&report).unwrap_or_default());
            } else {
                out_line(&format!(
                    "{}: {} of {} files match",
                    dest.display(),
                    report.matched,
                    report.expected
                ));
                for (label, list) in [
                    ("missing", &report.missing),
                    ("changed", &report.changed),
                    ("extra", &report.extra),
                ] {
                    for f in list {
                        out_line(&format!("  {label}: {f}"));
                    }
                }
            }
            if !report.ok() {
                return Err(Error::Mismatch(format!(
                    "copy differs: {} missing, {} changed, {} extra",
                    report.missing.len(),
                    report.changed.len(),
                    report.extra.len()
                )));
            }
        }
        Cmd::Clean { out } => {
            let c = clean::clean(&out)?;
            for d in &c.removed {
                out_line(&format!("removed {}", d.display()));
            }
            for (d, why) in &c.skipped {
                out_line(&format!("kept {} ({why})", d.display()));
            }
            if c.removed_out {
                out_line(&format!("removed {}", out.display()));
            }
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ont-runsim: {e}");
            ExitCode::from(exit_code(&e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
