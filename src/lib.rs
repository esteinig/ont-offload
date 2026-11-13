// SPDX-License-Identifier: AGPL-3.0-or-later
//! ont-offload: verified offload of ONT sequencing runs to mounted network storage.
//!
//! Module boundaries follow the design plan and the software items in
//! `compliance/architecture/`; behaviour is added release by release (see the
//! staged implementation plan in the docs). v0.1 provides the filesystem
//! layer ([`fsio`]) and configuration validation ([`config`], `check-config`).

pub mod cleanup;
pub mod cli;
pub mod config;
pub mod copy;
pub mod error;
pub mod fsio;
pub mod guard;
pub mod journal;
pub mod logging;
pub mod run;
pub mod sched;
pub mod state;

pub use error::{Error, Result};

use std::io::Write;
use std::path::Path;

use cli::{Cli, Command};
use fsio::{Fs, RealFs};

/// Dispatch a parsed command.
///
/// Implements: SRS-CLI-001
///
/// # Errors
/// Returns the command's error; `main` maps it to an exit code.
pub fn execute(cli: &Cli) -> Result<()> {
    log::debug!("command: {:?}", cli.command);
    match &cli.command {
        Command::CheckConfig => check_config(&cli.global.config, &RealFs, &mut std::io::stdout()),
        other => Err(Error::NotImplemented {
            command: other.name(),
            planned: other.planned(),
        }),
    }
}

/// `check-config`: validate the configuration and print the effective settings.
///
/// Implements: SRS-CFG-004
///
/// # Errors
/// [`Error::Config`] listing every problem; I/O errors writing the report.
pub fn check_config(path: &Path, fs: &dyn Fs, out: &mut dyn Write) -> Result<()> {
    let loaded = config::load(path, fs)?;
    for w in &loaded.warnings {
        log::warn!("{w}");
    }
    writeln!(
        out,
        "configuration: {}",
        logging::escape_control(&path.to_string_lossy())
    )?;
    write!(out, "{}", loaded.config.summary())?;
    writeln!(
        out,
        "configuration OK ({} warning(s)); destination guard and run detection checks follow in v0.2",
        loaded.warnings.len()
    )?;
    Ok(())
}
