// SPDX-License-Identifier: AGPL-3.0-or-later
//! ont-offload: verified offload of ONT sequencing runs to mounted network storage.
//!
//! This is the v0.1 scaffold. Module boundaries follow the design plan and the
//! software items in `compliance/architecture/`; behaviour is added release by
//! release (see the staged implementation plan in the docs).

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

use cli::Command;

/// Dispatch a parsed command.
///
/// Implements: SRS-CLI-001
///
/// # Errors
/// Returns the command's error; `main` maps it to an exit code.
pub fn execute(command: &Command) -> Result<()> {
    log::debug!("command: {command:?}");
    Err(Error::NotImplemented {
        command: command.name(),
        planned: command.planned(),
    })
}
