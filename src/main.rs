// SPDX-License-Identifier: AGPL-3.0-or-later
//! ont-offload binary: parse arguments, set up logging, run, map errors to exit codes.

use std::process::ExitCode;

use clap::Parser;
use ont_offload::cli::Cli;
use ont_offload::error::exit;

/// Implements: SRS-CLI-003, SRS-CLI-004, SRS-LOG-002
fn main() -> ExitCode {
    let cli = Cli::parse(); // usage errors exit with code 2 (clap)

    // The dashboard owns the screen: only errors reach the console.
    let level: log::LevelFilter = cli.global.log_level.into();
    let level = if cli.command.is_fullscreen() {
        level.min(log::LevelFilter::Error)
    } else {
        level
    };
    let from_config = if cli.command.uses_config_log() {
        ont_offload::config::logging_settings(&cli.global.config)
    } else {
        None
    };
    let (log_file, log_format) = ont_offload::logging::choose(
        cli.global.log_file.as_deref(),
        cli.global.log_format,
        from_config,
    );
    if let Err(e) = ont_offload::logging::init(level, log_file.as_deref(), log_format) {
        eprintln!("ont-offload: {e}");
        return ExitCode::from(e.exit_code());
    }

    log::info!(
        "ont-offload {} starting: {}",
        env!("CARGO_PKG_VERSION"),
        cli.command.name()
    );
    match ont_offload::execute(&cli) {
        Ok(()) => ExitCode::from(exit::OK),
        Err(e) => {
            log::error!("{e}");
            ExitCode::from(e.exit_code())
        }
    }
}
