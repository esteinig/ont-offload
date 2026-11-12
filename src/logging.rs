// SPDX-License-Identifier: AGPL-3.0-or-later
//! Console and file logging (fern).
//!
//! Console output goes to stderr so stdout stays free for machine-readable output.
//! Timestamps are RFC 3339 UTC. Control characters in messages are escaped, so a
//! BEL byte in a path prints as `\u{7}` instead of nothing.

use std::path::Path;
use std::time::SystemTime;

use crate::cli::LogFormat;
use crate::error::{Error, Result};

/// Install the global logger.
///
/// Implements: SRS-LOG-001, SRS-LOG-002
///
/// # Errors
/// Returns an error if the log file cannot be opened or a logger is already installed.
pub fn init(level: log::LevelFilter, file: Option<&Path>, format: LogFormat) -> Result<()> {
    let console = fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "{}",
                render(format, record, &message.to_string())
            ));
        })
        .chain(std::io::stderr());

    let mut root = fern::Dispatch::new().level(level).chain(console);

    if let Some(path) = file {
        let handle = fern::log_file(path).map_err(|source| Error::Logging {
            path: path.to_path_buf(),
            source,
        })?;
        let file_dispatch = fern::Dispatch::new()
            .format(move |out, message, record| {
                out.finish(format_args!(
                    "{}",
                    render(format, record, &message.to_string())
                ));
            })
            .chain(handle);
        root = root.chain(file_dispatch);
    }

    root.apply().map_err(|_| Error::LoggerInstalled)
}

fn render(format: LogFormat, record: &log::Record<'_>, message: &str) -> String {
    let ts = humantime::format_rfc3339_millis(SystemTime::now());
    let msg = escape_control(message);
    match format {
        LogFormat::Text => format!("{ts} {:<5} {}: {msg}", record.level(), record.target()),
        LogFormat::Json => format!(
            "{{\"ts\":\"{ts}\",\"level\":\"{}\",\"target\":\"{}\",\"msg\":\"{}\"}}",
            record.level(),
            json_escape(record.target()),
            json_escape(&msg)
        ),
    }
}

/// Replace control characters with a visible `\u{..}` form.
///
/// Implements: SRS-LOG-003
#[must_use]
pub fn escape_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.push_str(&c.escape_unicode().to_string());
        } else {
            out.push(c);
        }
    }
    out
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies: SRS-LOG-003
    #[test]
    fn bel_byte_is_visible() {
        assert_eq!(escape_control("dest\u{7}"), "dest\\u{7}");
        assert_eq!(escape_control("plain/path"), "plain/path");
    }

    /// Verifies: SRS-LOG-001
    #[test]
    fn json_lines_escape_quotes_and_controls() {
        assert_eq!(json_escape("a\"b\\c\u{1}"), "a\\\"b\\\\c\\u0001");
    }
}
