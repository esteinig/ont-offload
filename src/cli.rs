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

    /// Also write logs to this file.
    #[arg(long, global = true)]
    pub log_file: Option<PathBuf>,

    /// Log line format.
    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Text)]
    pub log_format: LogFormat,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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
    Watch,
    /// Offload one run in the foreground until it is complete or failed.
    Run {
        /// Run folder.
        path: PathBuf,
        /// The run has already finished; start at reconciliation.
        #[arg(long)]
        completed: bool,
    },
    /// Re-hash a run on the destination against its manifest.
    Verify {
        /// Run folder on the destination.
        run: PathBuf,
        /// Also re-hash the source copy.
        #[arg(long)]
        source: bool,
    },
    /// Print the live-state snapshot.
    Status {
        /// Print raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Validate configuration, paths, run detection and the destination guard without copying.
    CheckConfig,
    /// Measure the share and suggest workers and `rate_limit`.
    Bench,
    /// Delete source runs that pass every deletion gate.
    Cleanup {
        /// Report only; delete nothing.
        #[arg(long)]
        dry_run: bool,
        /// Limit to one run folder.
        run: Option<PathBuf>,
    },
}

impl Command {
    /// Subcommand name as typed on the command line.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Watch => "watch",
            Self::Run { .. } => "run",
            Self::Verify { .. } => "verify",
            Self::Status { .. } => "status",
            Self::CheckConfig => "check-config",
            Self::Bench => "bench",
            Self::Cleanup { .. } => "cleanup",
        }
    }

    /// Release in which this command is planned (see the staged implementation plan).
    #[must_use]
    pub fn planned(&self) -> &'static str {
        match self {
            Self::Run { .. } | Self::Verify { .. } | Self::CheckConfig => "v0.2",
            Self::Watch => "v0.3",
            Self::Status { .. } | Self::Bench => "v0.4",
            Self::Cleanup { .. } => "v0.5",
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
            vec!["ont-offload", "run", "/data/x", "--completed"],
            vec!["ont-offload", "verify", "/mnt/x", "--source"],
            vec!["ont-offload", "status", "--json"],
            vec!["ont-offload", "check-config"],
            vec!["ont-offload", "bench"],
            vec!["ont-offload", "cleanup", "--dry-run"],
        ] {
            assert!(Cli::try_parse_from(&args).is_ok(), "{args:?}");
        }
    }
}
