// SPDX-License-Identifier: AGPL-3.0-or-later
//! Command-line interface (clap).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Verified offload of ONT sequencing runs to mounted network storage.
///
/// Implements: SRS-CLI-001, SRS-CLI-002
#[derive(Debug, Parser)]
#[command(name = "ont-offload", version, about, long_about = None, propagate_version = true)]
pub struct Cli {
    /// Global options.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Options shared by every command.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Configuration file.
    #[arg(
        long,
        short = 'c',
        global = true,
        env = "ONT_OFFLOAD_CONFIG",
        default_value = "/etc/ont-offload/config.toml"
    )]
    pub config: PathBuf,

    /// Console log level.
    #[arg(long, global = true, value_enum, default_value_t = LogLevel::Info, env = "ONT_OFFLOAD_LOG")]
    pub log_level: LogLevel,

    /// Also write logs to this file (`watch` defaults to `logging.file`
    /// of the configuration).
    #[arg(long, global = true)]
    pub log_file: Option<PathBuf>,

    /// Log line format [default: text, or `logging.format` for `watch`].
    #[arg(long, global = true, value_enum)]
    pub log_format: Option<LogFormat>,
}

/// Log levels accepted on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogLevel {
    /// Errors only.
    Error,
    /// Warnings and errors.
    Warn,
    /// Normal operation.
    Info,
    /// Diagnostic detail.
    Debug,
    /// Everything.
    Trace,
}

impl From<LogLevel> for log::LevelFilter {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Error => Self::Error,
            LogLevel::Warn => Self::Warn,
            LogLevel::Info => Self::Info,
            LogLevel::Debug => Self::Debug,
            LogLevel::Trace => Self::Trace,
        }
    }
}

/// Log line formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable text.
    Text,
    /// One JSON object per line.
    Json,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Daemon: discover runs under the source root and offload live and completed runs.
    Watch {
        /// Exit once every detected run is complete or failed (for scripts and tests).
        #[arg(long)]
        until_idle: bool,
    },
    /// Offload one run in the foreground until it is complete or failed.
    Run {
        /// Run folder.
        path: PathBuf,
        /// Treat the run as finished even without a complete marker (`final_summary`).
        #[arg(long)]
        completed: bool,
    },
    /// Re-hash a run on the destination against its manifest.
    Verify {
        /// Run folder on the destination.
        run: PathBuf,
        /// Also re-hash the source run recorded in run.json.
        #[arg(long, conflicts_with = "against")]
        source: bool,
        /// Instead, check another copy of the run (for example the legacy
        /// service's) against this run's manifest; writes nothing.
        #[arg(long, value_name = "FOLDER")]
        against: Option<PathBuf>,
    },
    /// Print the live-state snapshot.
    Status {
        /// Print raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Write a configuration from a preset, create its folders, prepare and
    /// probe the share, then print what to do next.
    ///
    /// Presets: `local` (laptop test bench with a tmpfs share), `test`
    /// (rehearsal against a real share from a user's home folder) and
    /// `production` (the service on a sequencer). Every value can be changed
    /// with a flag, and any other setting with `--set KEY=VALUE`.
    Init(Box<InitArgs>),
    /// Validate configuration, paths, run detection and the destination guard without copying.
    CheckConfig,
    /// Measure the share and suggest workers, `rate_limit` and `chunk_size`.
    Bench {
        /// Data written per step, split over the streams.
        #[arg(long, default_value = "2GB")]
        size: String,
        /// Largest number of parallel streams to try (1, 2, 4, ... up to this).
        #[arg(long, default_value_t = 16)]
        max_streams: usize,
        /// Skip the chunk-size steps.
        #[arg(long)]
        no_chunks: bool,
    },
    /// Delete source runs that pass every deletion gate.
    Cleanup {
        /// Report only; delete nothing.
        #[arg(long)]
        dry_run: bool,
        /// Limit to one run folder.
        run: Option<PathBuf>,
    },
    /// Read-only dashboard of the daemon's state, events and alerts.
    ///
    /// Each setting comes from its flag, else its environment variable,
    /// else the `[tui]` section of the configuration file, else the
    /// default; `--check` prints what was chosen and from where.
    Tui {
        /// State folder of the daemon [env: `ONT_OFFLOAD_STATE_DIR`].
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The daemon's log file, shown in the log pane [env: `ONT_OFFLOAD_TUI_LOG`].
        #[arg(long)]
        log: Option<PathBuf>,
        /// Theme: plain, matrix, phosphor or synthwave [env: `ONT_OFFLOAD_TUI_THEME`].
        #[arg(long)]
        theme: Option<String>,
        /// Refresh interval, 200ms to 60s [env: `ONT_OFFLOAD_TUI_REFRESH`].
        #[arg(long)]
        refresh: Option<String>,
        /// Ring the terminal bell on a new alert [env: `ONT_OFFLOAD_TUI_BELL`].
        #[arg(long, value_enum)]
        bell: Option<Switch>,
        /// Print the settings, where each came from and whether each file
        /// can be read, then exit.
        #[arg(long)]
        check: bool,
    },
    /// Clinical operations reports (PDF and JSON).
    Report {
        /// Which report.
        #[command(subcommand)]
        kind: ReportCmd,
    },
}

/// Reports.
#[derive(Debug, Subcommand)]
pub enum ReportCmd {
    /// What ont-offload did over a period, from its event, log and journal
    /// records: runs, data, incidents, sessions, cleanup and record
    /// completeness, with a verdict.
    Status(StatusReportArgs),
    /// Whether an execution was offloaded completely and correctly: a
    /// simulation (ont-runsim) or a MinKNOW mock run, checked against a
    /// reference ont-offload did not produce, under a test protocol. Exits
    /// 0 on PASS, 3 on FAIL and 5 when a required check cannot be decided.
    Verify(VerifyReportArgs),
}

/// `report verify`.
#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("reference").required(true).args(["sim", "minknow"])))]
pub struct VerifyReportArgs {
    /// Output folder of ont-runsim (its manifests are the reference).
    #[arg(long, value_name = "OUT")]
    pub sim: Option<PathBuf>,
    /// MinKNOW run folders at the source (re-hashed as the reference; they
    /// must not have been deleted yet).
    #[arg(long, value_name = "RUN", num_args = 1..)]
    pub minknow: Vec<PathBuf>,
    /// Test protocol: simulation, p2i-acceptance, minknow-mock, or a
    /// protocol file [default: simulation with --sim, minknow-mock with
    /// --minknow].
    #[arg(long)]
    pub protocol: Option<String>,
    /// Start of the execution, RFC 3339 or a date [default: when the
    /// simulation started, else the first event about a reference run].
    #[arg(long)]
    pub from: Option<String>,
    /// End of the execution [default: now].
    #[arg(long)]
    pub to: Option<String>,
    /// Who ran the execution, printed on the report.
    #[arg(long)]
    pub operator: Option<String>,
    /// A deviation from the protocol, printed on the report (repeatable).
    #[arg(long)]
    pub deviation: Vec<String>,
    /// State folder of the daemon [default: `state_dir` of the configuration].
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    /// The daemon's log file [default: `logging.file` of the configuration].
    #[arg(long)]
    pub log: Option<PathBuf>,
    /// Folder for the report files.
    #[arg(long, short, default_value = ".")]
    pub out: PathBuf,
    /// Comma-separated output formats: pdf, json, typ [default: pdf,json,
    /// or json in a build without PDF support].
    #[arg(long)]
    pub format: Option<String>,
}

/// `report status`.
#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("when").args(["month", "last", "from"])))]
#[command(group(clap::ArgGroup::new("until").args(["month", "last", "to"])))]
pub struct StatusReportArgs {
    /// First day (2026-10-01) or time (RFC 3339) of the period, UTC
    /// [default: 30 days before --to].
    #[arg(long)]
    pub from: Option<String>,
    /// Last day (inclusive) or end time of the period, UTC [default: now].
    #[arg(long)]
    pub to: Option<String>,
    /// A calendar month, e.g. 2026-10.
    #[arg(long)]
    pub month: Option<String>,
    /// The time up to now, e.g. 7d or 24h.
    #[arg(long)]
    pub last: Option<String>,
    /// State folder of the daemon [default: `state_dir` of the configuration].
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    /// The daemon's log file; its rotations are read too [default:
    /// `logging.file` of the configuration].
    #[arg(long)]
    pub log: Option<PathBuf>,
    /// Folder for the report files.
    #[arg(long, short, default_value = ".")]
    pub out: PathBuf,
    /// Comma-separated output formats: pdf (always with its json record),
    /// json, typ (the typst sources and record, to compile elsewhere)
    /// [default: pdf,json, or json in a build without PDF support].
    #[arg(long)]
    pub format: Option<String>,
    /// Also read each completed run's VERIFIED.json on the share.
    #[arg(long)]
    pub check_destination: bool,
    /// Append every event of the period (except per-file commits) and every
    /// warning and error log line.
    #[arg(long)]
    pub appendix: bool,
}

/// Options of `init`.
#[derive(Debug, Clone, Default, Args)]
#[allow(clippy::struct_excessive_bools)] // independent command-line switches
pub struct InitArgs {
    /// Starting point for every setting.
    #[arg(long, short = 'p', value_enum, default_value_t = Preset::Local)]
    pub preset: Preset,
    /// Destination folder on the share (`dest_root`) [default: local
    /// /dev/shm/ont-share; required for test and production].
    #[arg(long, short = 's', value_name = "DIR")]
    pub share: Option<PathBuf>,
    /// Id written to the share's sentinel (`target_id`) [default: local
    /// `local-test`, test `<host>-test`, production `<host>`].
    #[arg(long, value_name = "ID")]
    pub target_id: Option<String>,
    /// Folder for the configuration, state, logs and notify script
    /// [default: local ~/ont-local, test ~/ont-offload-test; production
    /// uses /etc, /var/lib and /var/log unless given].
    #[arg(long, short = 'w', value_name = "DIR")]
    pub work_dir: Option<PathBuf>,
    /// Folder holding the MinKNOW runs; `transfer/` and `simulation/` go
    /// below it [default: local <work-dir>/data, otherwise /data].
    #[arg(long, short = 'd', value_name = "DIR")]
    pub data: Option<PathBuf>,
    /// Root of the `runs` source (MinKNOW output) [default: <data>].
    #[arg(long, value_name = "DIR", conflicts_with = "no_runs")]
    pub runs: Option<PathBuf>,
    /// Root of the `transfer` source (user datasets marked READY)
    /// [default: <data>/transfer].
    #[arg(long, value_name = "DIR", conflicts_with = "no_transfer")]
    pub transfer: Option<PathBuf>,
    /// Root of the `simulation` source (ont-runsim test runs)
    /// [default: <data>/simulation].
    #[arg(long, value_name = "DIR", conflicts_with = "no_simulation")]
    pub simulation: Option<PathBuf>,
    /// Leave out the `runs` source.
    #[arg(long)]
    pub no_runs: bool,
    /// Leave out the `transfer` source.
    #[arg(long)]
    pub no_transfer: bool,
    /// Leave out the `simulation` source.
    #[arg(long)]
    pub no_simulation: bool,
    /// Journal and state folder (`state_dir`) [default: <work-dir>/state].
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Log file of the daemon (`logging.file`)
    /// [default: <work-dir>/logs/ont-offload.log].
    #[arg(long, value_name = "FILE")]
    pub log: Option<PathBuf>,
    /// Where to write the configuration [default: <work-dir>/config.toml,
    /// production /etc/ont-offload/config.toml].
    #[arg(long, short = 'o', value_name = "FILE")]
    pub output: Option<PathBuf>,
    /// Filesystem type the share may have (`guard.allowed_fstypes`);
    /// repeat for several [default: local tmpfs, otherwise nfs, nfs4,
    /// cifs, smb3].
    #[arg(long = "fstype", value_name = "TYPE")]
    pub fstypes: Vec<String>,
    /// Files copied at the same time (`bandwidth.workers`).
    #[arg(long)]
    pub workers: Option<u32>,
    /// Total transfer rate (`bandwidth.rate_limit`), e.g. 100MB/s.
    #[arg(long, value_name = "RATE")]
    pub rate_limit: Option<String>,
    /// Free space to keep on the share (`transfer.min_free`), e.g. 500GiB.
    #[arg(long, value_name = "SIZE")]
    pub min_free: Option<String>,
    /// Deleting offloaded runs from the sequencer (`cleanup.delete`):
    /// off, dry-run or on.
    #[arg(long, value_name = "MODE", value_parser = ["off", "dry-run", "on"])]
    pub cleanup: Option<String>,
    /// Program run for each alert (`outage.notify_command`); "" for none
    /// [default: local and test a notify.sh in the work folder that
    /// appends to notify.log].
    #[arg(long, value_name = "PROGRAM")]
    pub notify_command: Option<String>,
    /// Any other setting, as `key`, `section.key` or `source.<name>.key`
    /// with a TOML value (a bare word is taken as text); repeatable.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub set: Vec<String>,
    /// Write nothing to the share: no folder, sentinel or probe (for a share
    /// that is not mounted yet).
    #[arg(long)]
    pub no_probe: bool,
    /// Print the configuration and change nothing.
    #[arg(long)]
    pub print: bool,
    /// Replace an existing configuration file (the old one is kept as
    /// `<file>.bak`).
    #[arg(long)]
    pub force: bool,
}

/// Presets of `init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Preset {
    /// Laptop test bench: tmpfs share in /dev/shm, everything in
    /// ~/ont-local, short timings, cleanup as a dry run.
    #[default]
    Local,
    /// Rehearsal against the real share as a normal user: own folder on
    /// the share, everything else in ~/ont-offload-test, short timings,
    /// cleanup as a dry run.
    Test,
    /// The service on a sequencer: /etc, /var/lib and /var/log, production
    /// timings, cleanup off.
    Production,
}

impl Preset {
    /// Name as typed on the command line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Test => "test",
            Self::Production => "production",
        }
    }
}

/// On or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Switch {
    /// On.
    On,
    /// Off.
    Off,
}

impl Command {
    /// The command is the daemon, which writes the log file named in the
    /// configuration (`logging.file`); other commands, often run by an
    /// operator without write access to it, log to the console only unless
    /// given `--log-file`.
    #[must_use]
    pub fn uses_config_log(&self) -> bool {
        matches!(self, Self::Watch { .. })
    }

    /// The command takes over the terminal (console logging would draw
    /// over it).
    #[must_use]
    pub fn is_fullscreen(&self) -> bool {
        matches!(self, Self::Tui { check: false, .. })
    }
}

impl Command {
    /// Subcommand name as typed on the command line.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Watch { .. } => "watch",
            Self::Run { .. } => "run",
            Self::Verify { .. } => "verify",
            Self::Status { .. } => "status",
            Self::Init(_) => "init",
            Self::CheckConfig => "check-config",
            Self::Bench { .. } => "bench",
            Self::Cleanup { .. } => "cleanup",
            Self::Tui { .. } => "tui",
            Self::Report { .. } => "report",
        }
    }

    /// Release in which this command is planned (see the staged implementation plan).
    #[must_use]
    pub fn planned(&self) -> &'static str {
        match self {
            Self::CheckConfig => "v0.1",
            Self::Run { .. } | Self::Verify { .. } => "v0.2",
            Self::Watch { .. } => "v0.3",
            Self::Status { .. } | Self::Bench { .. } => "v0.4",
            Self::Cleanup { .. } => "v0.5",
            Self::Tui { .. } => "v0.6",
            Self::Report { .. } | Self::Init(_) => "v0.9",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Verifies: SRS-CLI-002
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// Verifies: SRS-CLI-001
    #[test]
    fn parses_every_subcommand() {
        for args in [
            vec!["ont-offload", "watch"],
            vec!["ont-offload", "watch", "--until-idle"],
            vec!["ont-offload", "run", "/data/x", "--completed"],
            vec!["ont-offload", "verify", "/mnt/x", "--source"],
            vec![
                "ont-offload",
                "verify",
                "/mnt/x",
                "--against",
                "/mnt/legacy/x",
            ],
            vec!["ont-offload", "status", "--json"],
            vec!["ont-offload", "check-config"],
            vec!["ont-offload", "init"],
            vec![
                "ont-offload",
                "init",
                "--preset",
                "production",
                "--share",
                "/mnt/nas/ont",
                "--target-id",
                "lab-nas-01",
                "--data",
                "/data/data-offload",
                "--no-simulation",
                "--fstype",
                "nfs4",
                "--set",
                "transfer.settle=\"10s\"",
                "--print",
            ],
            vec!["ont-offload", "bench"],
            vec![
                "ont-offload",
                "bench",
                "--size",
                "1GB",
                "--max-streams",
                "8",
                "--no-chunks",
            ],
            vec!["ont-offload", "cleanup", "--dry-run"],
            vec!["ont-offload", "tui"],
            vec!["ont-offload", "report", "status", "--month", "2026-10"],
            vec![
                "ont-offload",
                "report",
                "verify",
                "--sim",
                "/data/ont-runsim",
            ],
            vec![
                "ont-offload",
                "report",
                "verify",
                "--minknow",
                "/data/a",
                "/data/b",
                "--protocol",
                "minknow-mock",
                "--operator",
                "E. S.",
                "--deviation",
                "share swapped",
            ],
            vec![
                "ont-offload",
                "report",
                "status",
                "--from",
                "2026-10-01",
                "--to",
                "2026-10-31",
                "--state-dir",
                "/var/lib/x",
                "--out",
                "/tmp",
                "--check-destination",
                "--appendix",
            ],
            vec![
                "ont-offload",
                "tui",
                "--state-dir",
                "/var/lib/x",
                "--log",
                "/var/log/x.log",
                "--theme",
                "matrix",
                "--refresh",
                "2s",
                "--bell",
                "off",
                "--check",
            ],
        ] {
            assert!(Cli::try_parse_from(&args).is_ok(), "{args:?}");
        }
    }
}
