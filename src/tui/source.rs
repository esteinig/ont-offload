// SPDX-License-Identifier: AGPL-3.0-or-later
//! Read-only access to what the daemon writes: the snapshot, and tails of
//! the append-only files (`events.jsonl` with its rotation, the log file and
//! `cleanup-reports.jsonl`). Nothing here writes, locks or creates a file.

use std::path::{Path, PathBuf};

use crate::fsio::Fs;
use crate::state::{EVENTS_FILE, Event, SCHEMA_VERSION, STATE_FILE, STATE_SCHEMA, Snapshot};

/// The most read from one file in one refresh (bounds the work per refresh
/// on the sequencer, HAZ-017).
pub const MAX_CHUNK: usize = 1 << 20;
/// How much of an existing file is shown at start.
pub const INITIAL_TAIL: u64 = 64 << 10;

/// The result of reading `state.json`.
#[derive(Debug, Clone, PartialEq)]
pub enum SnapshotRead {
    /// A snapshot of a known schema version.
    Ok(Box<Snapshot>),
    /// No snapshot file.
    Missing(String),
    /// The file cannot be read or parsed.
    Unreadable(String),
    /// Another schema or schema version.
    Unsupported(String),
}

/// Read `state_dir/state.json`, refusing other schema versions before
/// reading any field.
///
/// Implements: SRS-TUI-007
#[must_use]
pub fn read_snapshot(fs: &dyn Fs, state_dir: &Path) -> SnapshotRead {
    let path = state_dir.join(STATE_FILE);
    let bytes = match fs.read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return SnapshotRead::Missing(format!("{} does not exist", path.display()));
        }
        Err(e) => return SnapshotRead::Unreadable(format!("{}: {e}", path.display())),
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return SnapshotRead::Unreadable(format!("{}: {e}", path.display())),
    };
    let schema = value.get("schema").and_then(|v| v.as_str()).unwrap_or("");
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    if schema != STATE_SCHEMA || version != Some(u64::from(SCHEMA_VERSION)) {
        return SnapshotRead::Unsupported(format!(
            "{} is schema {schema:?} version {}; this dashboard reads {STATE_SCHEMA} version {SCHEMA_VERSION}. \
             Use the dashboard of the same ont-offload version as the daemon",
            path.display(),
            version.map_or_else(|| "?".to_owned(), |v| v.to_string())
        ));
    }
    match serde_json::from_value::<Snapshot>(value) {
        Ok(s) => SnapshotRead::Ok(Box::new(s)),
        Err(e) => SnapshotRead::Unreadable(format!("{}: {e}", path.display())),
    }
}

/// What one poll of a [`LineTail`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lines {
    /// Complete new lines, oldest first.
    pub lines: Vec<String>,
    /// The file was rotated or truncated since the last poll.
    pub rotated: bool,
    /// The file cannot be read (it may not exist yet).
    pub error: Option<String>,
}

/// Follows an append-only file by offset. A torn last line is held back
/// until its newline arrives. When the file is replaced (rotation) the rest
/// of the old file is read from `rotated_to`, when given, before the new
/// file from its start; when it shrinks (truncation) it is read again from
/// its start.
///
/// Implements: SRS-TUI-005
#[derive(Debug, Clone)]
pub struct LineTail {
    path: PathBuf,
    rotated_to: Option<PathBuf>,
    offset: u64,
    ino: Option<u64>,
    partial: Vec<u8>,
    started: bool,
}

impl LineTail {
    /// Follow `path`; with `rotated_to`, the name the file is rotated to.
    #[must_use]
    pub fn new(path: PathBuf, rotated_to: Option<PathBuf>) -> Self {
        Self {
            path,
            rotated_to,
            offset: 0,
            ino: None,
            partial: Vec::new(),
            started: false,
        }
    }

    /// The followed file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn split(&mut self, data: &[u8], out: &mut Vec<String>) {
        self.partial.extend_from_slice(data);
        while let Some(i) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if !text.trim().is_empty() {
                out.push(text);
            }
        }
    }

    /// Read what was appended since the last poll.
    pub fn poll(&mut self, fs: &dyn Fs) -> Lines {
        let mut out = Lines::default();
        let st = match fs.metadata(&self.path) {
            Ok(st) => st,
            Err(e) => {
                out.error = Some(format!("{}: {e}", self.path.display()));
                return out;
            }
        };
        if !self.started {
            // Start near the end, at a line boundary.
            self.started = true;
            self.ino = Some(st.ino);
            self.offset = st.len.saturating_sub(INITIAL_TAIL);
            if self.offset > 0 {
                if let Ok(head) = fs.read_at(&self.path, self.offset, MAX_CHUNK) {
                    match head.iter().position(|b| *b == b'\n') {
                        Some(i) => self.offset += i as u64 + 1,
                        None => self.offset = st.len,
                    }
                }
            }
        } else if self.ino != Some(st.ino) {
            // Replaced: finish the old file under its rotated name.
            out.rotated = true;
            if let Some(old) = &self.rotated_to {
                if let Ok(ost) = fs.metadata(old) {
                    if Some(ost.ino) == self.ino {
                        if let Ok(rest) = fs.read_at(old, self.offset, MAX_CHUNK) {
                            let mut lines = Vec::new();
                            self.split(&rest, &mut lines);
                            out.lines.extend(lines);
                        }
                    }
                }
            }
            self.partial.clear();
            self.ino = Some(st.ino);
            self.offset = 0;
        } else if st.len < self.offset {
            out.rotated = true;
            self.partial.clear();
            self.offset = 0;
        }
        match fs.read_at(&self.path, self.offset, MAX_CHUNK) {
            Ok(data) => {
                self.offset += data.len() as u64;
                let mut lines = Vec::new();
                self.split(&data, &mut lines);
                out.lines.extend(lines);
            }
            Err(e) => out.error = Some(format!("{}: {e}", self.path.display())),
        }
        out
    }
}

/// What one poll of an [`EventTail`] found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventsRead {
    /// New events, oldest first.
    pub events: Vec<Event>,
    /// Events skipped (gaps in `seq`, unreadable lines).
    pub missed: u64,
    /// The daemon restarted (`seq` began again at 1).
    pub restarted: bool,
    /// The stream cannot be read.
    pub error: Option<String>,
}

/// Follows `events.jsonl` (and its rotation to `events.jsonl.1`), checking
/// the sequence numbers so that a lost event is shown, not hidden.
///
/// Implements: SRS-TUI-005
#[derive(Debug, Clone)]
pub struct EventTail {
    tail: LineTail,
    last_seq: Option<u64>,
}

impl EventTail {
    /// Follow the events of `state_dir`.
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            tail: LineTail::new(
                state_dir.join(EVENTS_FILE),
                Some(state_dir.join(format!("{EVENTS_FILE}.1"))),
            ),
            last_seq: None,
        }
    }

    /// Read new events.
    pub fn poll(&mut self, fs: &dyn Fs) -> EventsRead {
        let lines = self.tail.poll(fs);
        let mut out = EventsRead {
            error: lines.error,
            ..EventsRead::default()
        };
        for line in lines.lines {
            match serde_json::from_str::<Event>(&line) {
                Ok(ev) => {
                    match self.last_seq {
                        Some(last) if ev.seq <= last => {
                            if ev.seq == 1 {
                                out.restarted = true;
                            } else {
                                // Read twice across a rotation: skip.
                                continue;
                            }
                        }
                        Some(last) if ev.seq > last + 1 => out.missed += ev.seq - last - 1,
                        _ => {}
                    }
                    self.last_seq = Some(ev.seq);
                    out.events.push(ev);
                }
                Err(_) => out.missed += 1,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;
