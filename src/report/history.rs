// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reading the records a report is built from: every event segment
//! (`events/`, `events.jsonl.1`, `events.jsonl`) and every log file
//! (current, rotated by date or number, plain or gzip). Files are streamed
//! line by line, so months of history do not have to fit in memory.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsio::Fs;
use crate::state::{Event, event_files};

use super::time;

/// What one kind of record contributed to a report, and what is wrong
/// with it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSummary {
    /// What the record is, e.g. `events`.
    pub record: String,
    /// Where it was read from (a folder or a pattern).
    pub path: String,
    /// Files read.
    pub files: usize,
    /// Lines read.
    pub lines: u64,
    /// Time of the first entry (RFC 3339).
    pub first: Option<String>,
    /// Time of the last entry (RFC 3339).
    pub last: Option<String>,
    /// Problems that mean the report cannot vouch for part of the history
    /// (unreadable files, lines that do not parse, missing events).
    pub gaps: Vec<String>,
}

impl InputSummary {
    fn new(record: &str, path: &Path) -> Self {
        Self {
            record: record.into(),
            path: path.to_string_lossy().into_owned(),
            ..Self::default()
        }
    }

    fn saw(&mut self, t: u64) {
        let ts = time::rfc3339(t);
        if self
            .first
            .as_ref()
            .is_none_or(|f| time::parse(f).is_some_and(|f| t < f))
        {
            self.first = Some(ts.clone());
        }
        if self
            .last
            .as_ref()
            .is_none_or(|l| time::parse(l).is_some_and(|l| t > l))
        {
            self.last = Some(ts);
        }
    }
}

fn lines_of(fs: &dyn Fs, path: &Path) -> std::io::Result<Box<dyn BufRead>> {
    let raw = fs.open_read(path)?;
    let gz = path.extension().is_some_and(|e| e == "gz");
    let reader: Box<dyn Read> = if gz {
        Box::new(flate2::read::MultiGzDecoder::new(raw))
    } else {
        Box::new(raw)
    };
    Ok(Box::new(BufReader::with_capacity(1 << 16, reader)))
}

/// Call `f` for each line of `path`, lossily decoded and without its line
/// ending. Returns the number of lines.
fn each_line(fs: &dyn Fs, path: &Path, mut f: impl FnMut(&str)) -> std::io::Result<u64> {
    let mut r = lines_of(fs, path)?;
    let mut buf = Vec::new();
    let mut n = 0;
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf)? == 0 {
            return Ok(n);
        }
        n += 1;
        let s = String::from_utf8_lossy(&buf);
        f(s.trim_end_matches(['\n', '\r']));
    }
}

/// Stream every event in `state_dir`, oldest first, to `f` with its time.
/// Missing sequence numbers within one start of the daemon and lines that
/// do not parse are recorded as gaps (a torn last line of the live stream
/// is not: the daemon may be writing it).
///
/// Implements: SRS-RPT-002
#[must_use]
pub fn read_events(fs: &dyn Fs, state_dir: &Path, mut f: impl FnMut(&Event, u64)) -> InputSummary {
    let mut sum = InputSummary::new("events", state_dir);
    let files = event_files(fs, state_dir);
    let mut prev: Option<(u64, u64)> = None; // (seq, ts)
    for (i, path) in files.iter().enumerate() {
        let live = i + 1 == files.len() && path.ends_with(crate::state::EVENTS_FILE);
        let mut bad: Vec<u64> = Vec::new();
        let mut n_lines = 0u64;
        let res = each_line(fs, path, |line| {
            n_lines += 1;
            if line.trim().is_empty() {
                return;
            }
            let Ok(ev) = serde_json::from_str::<Event>(line) else {
                bad.push(n_lines);
                return;
            };
            let Some(t) = time::parse(&ev.ts) else {
                bad.push(n_lines);
                return;
            };
            if let Some((seq, pt)) = prev {
                let restart = ev.seq == 1;
                if !restart && ev.seq != seq + 1 {
                    sum.gaps.push(format!(
                        "events {} to {} missing between {} and {}",
                        seq + 1,
                        ev.seq.saturating_sub(1),
                        time::rfc3339(pt),
                        ev.ts
                    ));
                }
            }
            prev = Some((ev.seq, t));
            sum.saw(t);
            f(&ev, t);
        });
        match res {
            Ok(n) => {
                sum.files += 1;
                sum.lines += n;
                // The live stream's last line may still be being written.
                if live && bad.last() == Some(&n) {
                    bad.pop();
                }
                if !bad.is_empty() {
                    sum.gaps.push(format!(
                        "{}: {} line(s) do not parse (first: line {})",
                        path.display(),
                        bad.len(),
                        bad[0]
                    ));
                }
            }
            Err(e) => sum
                .gaps
                .push(format!("{}: cannot read: {e}", path.display())),
        }
    }
    if files.is_empty() {
        sum.gaps.push("no event files found".into());
    }
    sum
}

/// One line of the daemon's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    /// Time (RFC 3339, seconds).
    pub ts: String,
    /// `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE`.
    pub level: String,
    /// Module that logged it.
    pub target: String,
    /// Message.
    pub message: String,
}

/// Parse a text (`<ts> <LEVEL> <target>: <message>`) or JSON log line.
#[must_use]
pub fn parse_log_line(line: &str) -> Option<(u64, LogLine)> {
    if line.starts_with('{') {
        #[derive(Deserialize)]
        struct J {
            ts: String,
            level: String,
            target: String,
            msg: String,
        }
        let j: J = serde_json::from_str(line).ok()?;
        let t = time::parse(&j.ts)?;
        return Some((
            t,
            LogLine {
                ts: time::rfc3339(t),
                level: j.level.to_uppercase(),
                target: j.target,
                message: j.msg,
            },
        ));
    }
    let (ts, rest) = line.split_once(' ')?;
    let t = time::parse(ts)?;
    let rest = rest.trim_start();
    let (level, rest) = rest.split_once(' ')?;
    let (target, message) = rest.trim_start().split_once(": ")?;
    Some((
        t,
        LogLine {
            ts: time::rfc3339(t),
            level: level.to_uppercase(),
            target: target.into(),
            message: message.into(),
        },
    ))
}

/// The log file and its rotations (`<name>-<date>[...]`, `<name>.<n>`,
/// with or without `.gz`), oldest first by modification time.
#[must_use]
pub fn log_files(fs: &dyn Fs, log: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (log.parent(), log.file_name()) else {
        return Vec::new();
    };
    let name = name.to_string_lossy().into_owned();
    let mut out: Vec<(u64, String, PathBuf)> = fs
        .read_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| {
            let n = e.name.to_string_lossy().into_owned();
            let rotated = n
                .strip_prefix(&name)
                .is_some_and(|rest| rest.starts_with('-') || rest.starts_with('.'));
            (n == name || rotated).then(|| {
                let p = dir.join(&e.name);
                let mtime = fs
                    .metadata(&p)
                    .ok()
                    .map_or(0, |m| crate::journal::nanos(m.modified));
                (mtime, n, p)
            })
        })
        .collect();
    // The live file last, the others by age (name breaks ties).
    out.sort_by(|a, b| (a.1 == name, a.0, &a.1).cmp(&(b.1 == name, b.0, &b.1)));
    out.into_iter().map(|(_, _, p)| p).collect()
}

/// Stream every line of the log and its rotations to `f`.
///
/// Implements: SRS-RPT-002
#[must_use]
pub fn read_log(fs: &dyn Fs, log: &Path, mut f: impl FnMut(u64, &LogLine)) -> InputSummary {
    let mut sum = InputSummary::new("log", log);
    let files = log_files(fs, log);
    if files.is_empty() {
        sum.gaps
            .push(format!("{}: no log files found", log.display()));
    }
    for path in files {
        let mut unparsed = 0u64;
        let res = each_line(fs, &path, |line| match parse_log_line(line) {
            Some((t, l)) => {
                sum.saw(t);
                f(t, &l);
            }
            // Continuation lines and foreign lines are not errors.
            None => unparsed += 1,
        });
        match res {
            Ok(n) => {
                sum.files += 1;
                sum.lines += n;
            }
            Err(e) => sum
                .gaps
                .push(format!("{}: cannot read: {e}", path.display())),
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsio::RealFs;
    use std::io::Write as _;

    /// Verifies: SRS-RPT-002
    #[test]
    fn text_json_rotated_and_gzip_logs_are_all_read() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("ont-offload.log");
        std::fs::write(
            dir.path().join("ont-offload.log-20261001-1"),
            "2026-10-01T00:00:01.000Z WARN  ont_offload::sched: outage\n",
        )
        .unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(
            b"2026-09-01T00:00:00.000Z INFO  ont_offload: ont-offload 0.8.0 starting: watch\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ont-offload.log.2.gz"),
            gz.finish().unwrap(),
        )
        .unwrap();
        std::fs::write(
            &log,
            "{\"ts\":\"2026-11-01T00:00:00.000Z\",\"level\":\"ERROR\",\"target\":\"x\",\"msg\":\"a: b\"}\nnot a log line\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("other.log"),
            "2026-01-01T00:00:00Z INFO x: y\n",
        )
        .unwrap();
        let mut seen = Vec::new();
        let sum = read_log(&RealFs, &log, |_, l| {
            seen.push((l.level.clone(), l.message.clone()));
        });
        assert_eq!(sum.files, 3);
        assert_eq!(sum.lines, 4);
        assert!(sum.gaps.is_empty(), "{:?}", sum.gaps);
        assert_eq!(sum.first.as_deref(), Some("2026-09-01T00:00:00Z"));
        assert_eq!(sum.last.as_deref(), Some("2026-11-01T00:00:00Z"));
        assert!(seen.contains(&("ERROR".into(), "a: b".into())));
        assert!(seen.contains(&("INFO".into(), "ont-offload 0.8.0 starting: watch".into())));
        assert!(seen.contains(&("WARN".into(), "outage".into())));
    }
}
