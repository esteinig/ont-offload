// SPDX-License-Identifier: AGPL-3.0-or-later
//! Console and file logging (fern).
//!
//! Console output goes to stderr so stdout stays free for machine-readable output.
//! Timestamps are RFC 3339 UTC. Control characters in messages are escaped, so a
//! BEL byte in a path prints as `\u{7}` instead of nothing.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

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
        let handle: Box<dyn Write + Send> = Box::new(
            FollowedFile::open(path, REOPEN_CHECK).map_err(|source| Error::Logging {
                path: path.to_path_buf(),
                source,
            })?,
        );
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

/// How often the log file's name is checked for a rotation.
pub const REOPEN_CHECK: Duration = Duration::from_secs(1);

/// A log file that follows its name: when the file is renamed or removed
/// (logrotate without `copytruncate`), the next write after the check
/// interval opens a new file under the name. No signal is needed, and a
/// rotated file stops growing. New files are created with mode 0640.
///
/// Implements: SRS-LOG-005
#[derive(Debug)]
pub struct FollowedFile {
    path: PathBuf,
    file: File,
    id: Option<(u64, u64)>,
    every: Duration,
    checked: Instant,
}

fn open_append(path: &Path) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o640);
    }
    o.open(path)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // None on other platforms
fn file_id(m: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_id(_m: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

impl FollowedFile {
    /// Open `path` for appending, checking its name at most every `every`.
    ///
    /// # Errors
    /// The file cannot be opened or created.
    pub fn open(path: &Path, every: Duration) -> std::io::Result<Self> {
        let file = open_append(path)?;
        let id = file.metadata().ok().as_ref().and_then(file_id);
        Ok(Self {
            path: path.to_path_buf(),
            file,
            id,
            every,
            checked: Instant::now(),
        })
    }

    /// Reopen when the name no longer refers to the open file. A file that
    /// cannot be created (folder gone) keeps the old one.
    fn follow(&mut self) {
        if self.checked.elapsed() < self.every {
            return;
        }
        self.checked = Instant::now();
        let current = std::fs::metadata(&self.path)
            .ok()
            .as_ref()
            .and_then(file_id);
        if current.is_some() && current == self.id {
            return;
        }
        if let Ok(f) = open_append(&self.path) {
            self.id = f.metadata().ok().as_ref().and_then(file_id);
            self.file = f;
        }
    }
}

impl Write for FollowedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.follow();
        self.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
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

    /// Verifies: SRS-LOG-005
    #[test]
    fn log_file_follows_rotation_by_rename_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ont-offload.log");
        let mut f = FollowedFile::open(&p, Duration::ZERO).unwrap();
        f.write_all(b"one\n").unwrap();
        // logrotate: rename, then the next line goes to a new file.
        std::fs::rename(&p, dir.path().join("ont-offload.log.1")).unwrap();
        f.write_all(b"two\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ont-offload.log.1")).unwrap(),
            "one\n"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two\n");
        // Removed: recreated.
        std::fs::remove_file(&p).unwrap();
        f.write_all(b"three\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "three\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode & 0o007, 0, "not world-readable: {mode:o}");
        }
        // The folder is gone: the old handle keeps working, nothing fails.
        let gone = tempfile::tempdir().unwrap();
        let q = gone.path().join("x.log");
        let mut g = FollowedFile::open(&q, Duration::ZERO).unwrap();
        drop(gone);
        g.write_all(b"still\n").unwrap();
        // Checked at most every interval: a slow check keeps the old file.
        let mut slow = FollowedFile::open(&p, Duration::from_secs(3600)).unwrap();
        std::fs::rename(&p, dir.path().join("old.log")).unwrap();
        slow.write_all(b"four\n").unwrap();
        assert!(
            std::fs::read_to_string(dir.path().join("old.log"))
                .unwrap()
                .ends_with("four\n")
        );
    }

    /// Verifies: SRS-LOG-001
    #[test]
    fn json_lines_escape_quotes_and_controls() {
        assert_eq!(json_escape("a\"b\\c\u{1}"), "a\\\"b\\\\c\\u0001");
    }
}
