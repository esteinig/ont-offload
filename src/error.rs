// SPDX-License-Identifier: AGPL-3.0-or-later
//! Error type and process exit codes.

use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong in ont-offload.
///
/// Each variant maps to one documented process exit code (see [`Error::exit_code`]).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Configuration is invalid or the destination guard refused to start.
    #[error("configuration error: {0}")]
    Config(String),

    /// The log file could not be opened or the logger could not be installed.
    #[error("logging setup failed for {path}: {source}")]
    Logging {
        /// Log file path, or `<console>`.
        path: PathBuf,
        /// Underlying cause.
        #[source]
        source: std::io::Error,
    },

    /// The logger was already installed.
    #[error("logger already initialised")]
    LoggerInstalled,

    /// One or more files or runs failed.
    #[error("{0} file(s) or run(s) failed")]
    Failed(usize),

    /// An integrity mismatch could not be resolved.
    #[error("integrity mismatch: {0}")]
    Integrity(String),

    /// Workers are still stuck after an outage; a restart is needed.
    #[error("restart needed: {0}")]
    RestartNeeded(String),

    /// The command exists but is not implemented in this version.
    #[error("`{command}` is not implemented yet (planned for {planned})")]
    NotImplemented {
        /// Subcommand name.
        command: &'static str,
        /// Planned release, e.g. `v0.2`.
        planned: &'static str,
    },

    /// Any other I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Process exit codes, as documented in the user guide.
pub mod exit {
    /// All done and verified.
    pub const OK: u8 = 0;
    /// Internal error.
    pub const INTERNAL: u8 = 1;
    /// Configuration or guard refused (also clap usage errors).
    pub const CONFIG: u8 = 2;
    /// One or more files or runs failed.
    pub const FAILED: u8 = 3;
    /// Integrity mismatch not resolved.
    pub const INTEGRITY: u8 = 4;
    /// Restart needed (workers stuck after an outage); journal is consistent.
    pub const RESTART: u8 = 75;
    /// Interrupted by a signal; journal is consistent.
    pub const INTERRUPTED: u8 = 130;
}

impl Error {
    /// The process exit code for this error.
    ///
    /// Implements: SRS-CLI-004
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) => exit::CONFIG,
            Self::Failed(_) => exit::FAILED,
            Self::Integrity(_) => exit::INTEGRITY,
            Self::RestartNeeded(_) => exit::RESTART,
            Self::Logging { .. }
            | Self::LoggerInstalled
            | Self::NotImplemented { .. }
            | Self::Io(_) => exit::INTERNAL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies: SRS-CLI-004
    #[test]
    fn exit_codes_match_documentation() {
        assert_eq!(Error::Config("x".into()).exit_code(), 2);
        assert_eq!(Error::Failed(1).exit_code(), 3);
        assert_eq!(Error::Integrity("x".into()).exit_code(), 4);
        assert_eq!(Error::RestartNeeded("x".into()).exit_code(), 75);
        assert_eq!(
            Error::NotImplemented {
                command: "watch",
                planned: "v0.3"
            }
            .exit_code(),
            1
        );
    }
}
