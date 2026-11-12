// SPDX-License-Identifier: AGPL-3.0-or-later
//! ont-runsim CLI.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Simulate MinKNOW sequencing runs on disk (test tool).
#[derive(Debug, Parser)]
#[command(name = "ont-runsim", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Record the timeline of an existing run (metadata only).
    Profile {
        /// Finished run folder.
        run: PathBuf,
        /// Output profile JSON.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Generate runs over time.
    Run {
        /// Scenario file.
        #[arg(long, conflicts_with = "preset")]
        scenario: Option<PathBuf>,
        /// Built-in preset (quick-2h, promethion-24h, outage-drill).
        #[arg(long)]
        preset: Option<String>,
        /// Output folder.
        #[arg(long, default_value = "/data/ont-runsim")]
        out: PathBuf,
        /// Simulated run length in hours.
        #[arg(long)]
        hours: Option<f64>,
        /// Time compression against the wall clock.
        #[arg(long)]
        speed: Option<f64>,
        /// Random seed.
        #[arg(long)]
        seed: Option<u64>,
        /// Minimum free space to keep on the disk.
        #[arg(long, default_value = "500GB")]
        keep_free: String,
    },
    /// Print progress of a running simulation.
    Status,
    /// Compare an offloaded copy with the simulator's manifest.
    Check {
        /// Run folder on the destination.
        #[arg(long)]
        dest: PathBuf,
    },
    /// Delete everything the simulator created, and nothing else.
    Clean {
        /// Output folder.
        #[arg(long, default_value = "/data/ont-runsim")]
        out: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let name = match cli.command {
        Cmd::Profile { .. } => "profile",
        Cmd::Run { .. } => "run",
        Cmd::Status => "status",
        Cmd::Check { .. } => "check",
        Cmd::Clean { .. } => "clean",
    };
    eprintln!("ont-runsim: {}", ont_runsim::Error::NotImplemented(name));
    ExitCode::from(1)
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
