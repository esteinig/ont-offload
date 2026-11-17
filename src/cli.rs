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
    /// Comma-separated output formats.
    #[arg(long, default_value = "json")]
    pub format: String,
    /// Also read each completed run's VERIFIED.json on the share.
    #[arg(long)]
    pub check_destination: bool,
    /// Append every event of the period (except per-file commits) and every
    /// warning and error log line.
    #[arg(long)]
    pub appendix: bool,
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
            Self::Report { .. } => "v0.9",
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
