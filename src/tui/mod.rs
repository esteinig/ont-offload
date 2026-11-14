// SPDX-License-Identifier: AGPL-3.0-or-later
//! `ont-offload tui`: a read-only dashboard of the daemon's state.
//!
//! The dashboard reads the files `watch` writes (`state.json`,
//! `events.jsonl`, `cleanup-reports.jsonl` and, when given, the log file)
//! and draws them. It never writes, locks or creates a file, and it starts
//! when the daemon, the share or the configuration file is missing, since
//! that is when it is needed.
//!
//! The reading, alert and theme logic ([`source`], [`model`], [`theme`],
//! [`launch`]) is always built and tested; drawing and the terminal loop
//! need the `tui` build feature.

pub mod launch;
pub mod model;
pub mod source;
pub mod theme;

#[cfg(feature = "tui")]
pub mod app;
#[cfg(feature = "tui")]
pub mod view;

#[cfg(test)]
pub(crate) mod fixtures;
