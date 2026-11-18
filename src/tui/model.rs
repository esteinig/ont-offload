// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the dashboard shows, built from the files on each refresh. Pure
//! logic on an injected time, so staleness and alerts are tested without a
//! terminal.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::cleanup::{REPORTS, Report};
use crate::fsio::Fs;
use crate::state::{Event, Kind, Snapshot, stale_after};

use super::source::{EventTail, LineTail, SnapshotRead, read_snapshot};

/// Events kept for the event pane.
pub const EVENTS_KEPT: usize = 500;
/// Log lines kept for the log pane.
pub const LOG_KEPT: usize = 300;
/// Speed samples kept for the sparkline.
pub const RATE_KEPT: usize = 240;
/// Cleared alerts kept as history.
pub const CLEARED_KEPT: usize = 20;

/// Whether what is on screen is current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// The snapshot is current.
    Live,
    /// The snapshot is older than three intervals: the daemon is not
    /// writing it.
    Stale {
        /// Age of the snapshot.
        age: Duration,
        /// When it was written.
        updated_at: String,
    },
    /// There is no readable snapshot.
    NoData {
        /// Why.
        why: String,
    },
    /// The snapshot is of another schema version.
    Unsupported {
        /// Why.
        why: String,
    },
}

impl Health {
    /// The values on screen are current.
    #[must_use]
    pub fn live(&self) -> bool {
        matches!(self, Self::Live)
    }
}

/// Where an alert came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// An open alert in `state.json`.
    Snapshot,
    /// An alerting event in `events.jsonl`.
    Event,
}

/// One alert on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertItem {
    /// Stable key (acknowledgement, de-duplication).
    pub key: String,
    /// Kind.
    pub kind: Kind,
    /// `warning` or `error` (`info` for recoveries).
    pub level: String,
    /// When it was raised (RFC 3339).
    pub since: String,
    /// Run, if any.
    pub run: Option<String>,
    /// Message.
    pub message: String,
    /// Source.
    pub origin: Origin,
}

/// Everything the dashboard shows.
#[derive(Debug, Default)]
pub struct Model {
    state_dir: PathBuf,
    events_tail: Option<EventTail>,
    log_tail: Option<LineTail>,
    reports_tail: Option<LineTail>,
    /// The last snapshot read (kept, dimmed, when it goes stale).
    pub snapshot: Option<Snapshot>,
    /// Whether it is current.
    pub health: Option<Health>,
    /// Recent events, oldest first.
    pub events: VecDeque<Event>,
    /// Events not shown (gaps in `seq`, unreadable lines).
    pub missed_events: u64,
    /// Daemon restarts seen in the event stream.
    pub restarts: u64,
    /// Why the event stream cannot be read, if so.
    pub events_error: Option<String>,
    /// Open alerts, newest first.
    pub alerts: Vec<AlertItem>,
    /// Alerts that cleared, newest first.
    pub cleared: VecDeque<AlertItem>,
    acknowledged: BTreeSet<String>,
    /// Alerts raised in the last refresh (the bell rings for these).
    pub new_alerts: usize,
    /// Speed samples (bytes per second), oldest first.
    pub rates: VecDeque<u64>,
    /// Recent log lines, oldest first.
    pub log: VecDeque<String>,
    /// Why the log cannot be read, if a log is followed.
    pub log_error: Option<String>,
    /// The last cleanup pass report.
    pub last_report: Option<Report>,
    /// When the model was last refreshed.
    pub refreshed_at: Option<SystemTime>,
    last_updated_at: Option<String>,
}

fn push_bounded<T>(q: &mut VecDeque<T>, item: T, cap: usize) {
    q.push_back(item);
    while q.len() > cap {
        q.pop_front();
    }
}

impl Model {
    /// A model for `state_dir`, following `log` when given.
    #[must_use]
    pub fn new(state_dir: &Path, log: Option<&Path>) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            events_tail: Some(EventTail::new(state_dir)),
            log_tail: log.map(|l| {
                LineTail::new(
                    l.to_path_buf(),
                    Some(PathBuf::from(format!("{}.1", l.display()))),
                )
            }),
            reports_tail: Some(LineTail::new(state_dir.join(REPORTS), None)),
            ..Self::default()
        }
    }

    /// Re-read everything at wall-clock time `now`.
    ///
    /// Implements: SRS-TUI-001, SRS-TUI-002, SRS-TUI-003, SRS-TUI-005
    pub fn refresh(&mut self, fs: &dyn Fs, now: SystemTime) {
        self.refreshed_at = Some(now);
        self.new_alerts = 0;
        let before: BTreeSet<String> = self
            .alerts
            .iter()
            .chain(&self.cleared)
            .map(|a| a.key.clone())
            .collect();
        self.read_state(fs, now);
        self.read_events(fs);
        self.reconcile_alerts();
        self.new_alerts = self
            .alerts
            .iter()
            .chain(&self.cleared)
            .filter(|a| !before.contains(&a.key) && a.level != "info")
            .count();
        self.read_tails(fs);
    }

    /// The snapshot and whether it is current.
    fn read_state(&mut self, fs: &dyn Fs, now: SystemTime) {
        match read_snapshot(fs, &self.state_dir) {
            SnapshotRead::Ok(snap) => {
                let updated = humantime::parse_rfc3339_weak(&snap.updated_at).ok();
                let age = updated
                    .and_then(|t| now.duration_since(t).ok())
                    .unwrap_or_default();
                self.health = Some(if updated.is_none() || age > stale_after(&snap) {
                    Health::Stale {
                        age,
                        updated_at: snap.updated_at.clone(),
                    }
                } else {
                    Health::Live
                });
                if self.last_updated_at.as_deref() != Some(snap.updated_at.as_str()) {
                    self.last_updated_at = Some(snap.updated_at.clone());
                    push_bounded(&mut self.rates, snap.rate.current_bytes_per_s, RATE_KEPT);
                }
                self.snapshot = Some(*snap);
            }
            SnapshotRead::Missing(why) | SnapshotRead::Unreadable(why) => {
                self.health = Some(Health::NoData { why });
            }
            SnapshotRead::Unsupported(why) => {
                self.snapshot = None;
                self.health = Some(Health::Unsupported { why });
            }
        }
    }

    /// New events; alerting ones become alerts.
    fn read_events(&mut self, fs: &dyn Fs) {
        if let Some(tail) = self.events_tail.as_mut() {
            let read = tail.poll(fs);
            self.events_error = read.error;
            self.missed_events += read.missed;
            self.restarts += u64::from(read.restarted);
            for ev in read.events {
                if ev.kind.alerts() {
                    let item = AlertItem {
                        key: format!("event:{}:{}", ev.ts, ev.seq),
                        kind: ev.kind,
                        level: ev.level.clone(),
                        since: ev.ts.clone(),
                        run: ev.run.clone(),
                        message: ev.message.clone(),
                        origin: Origin::Event,
                    };
                    if !self.alerts.iter().any(|a| a.key == item.key) {
                        self.alerts.insert(0, item);
                    }
                }
                push_bounded(&mut self.events, ev, EVENTS_KEPT);
            }
        }
    }

    /// Open alerts of a current snapshot, and alerts that cleared.
    fn reconcile_alerts(&mut self) {
        if let (Some(snap), Some(Health::Live)) = (&self.snapshot, &self.health) {
            for a in &snap.alerts {
                let key = format!(
                    "snapshot:{}:{}:{}",
                    a.kind.as_str(),
                    a.run.as_deref().unwrap_or(""),
                    a.since
                );
                if !self.alerts.iter().any(|x| x.key == key) {
                    self.alerts.insert(
                        0,
                        AlertItem {
                            key,
                            kind: a.kind,
                            level: a.level.clone(),
                            since: a.since.clone(),
                            run: a.run.clone(),
                            message: a.message.clone(),
                            origin: Origin::Snapshot,
                        },
                    );
                }
            }
            // An alert clears when a snapshot newer than it no longer
            // holds an alert of the same kind for the same run.
            let open = |item: &AlertItem| {
                snap.alerts
                    .iter()
                    .any(|a| a.kind == item.kind && a.run == item.run)
                    || item.since.as_str() >= snap.updated_at.as_str()
            };
            let (keep, gone): (Vec<_>, Vec<_>) =
                std::mem::take(&mut self.alerts).into_iter().partition(open);
            self.alerts = keep;
            for g in gone {
                self.cleared.push_front(g);
            }
            while self.cleared.len() > CLEARED_KEPT {
                self.cleared.pop_back();
            }
        }
        self.alerts.sort_by(|a, b| b.since.cmp(&a.since));
    }

    /// The log and the last cleanup report.
    fn read_tails(&mut self, fs: &dyn Fs) {
        if let Some(tail) = self.log_tail.as_mut() {
            let read = tail.poll(fs);
            self.log_error = read.error;
            for l in read.lines {
                push_bounded(&mut self.log, l, LOG_KEPT);
            }
        }
        if let Some(tail) = self.reports_tail.as_mut() {
            if let Some(last) = tail.poll(fs).lines.last() {
                if let Ok(r) = serde_json::from_str::<Report>(last) {
                    self.last_report = Some(r);
                }
            }
        }
    }

    /// Alerts not yet acknowledged on this screen, newest first: the
    /// banner. Cleared alerts stay here until acknowledged, so a short
    /// failure is not missed.
    ///
    /// Implements: SRS-TUI-003
    #[must_use]
    pub fn unacknowledged(&self) -> Vec<&AlertItem> {
        let mut v: Vec<&AlertItem> = self
            .alerts
            .iter()
            .chain(&self.cleared)
            .filter(|a| a.level != "info" && !self.acknowledged.contains(&a.key))
            .collect();
        v.sort_by(|a, b| b.since.cmp(&a.since));
        v
    }

    /// Acknowledge every alert on screen: hides the banner here only; the
    /// alerts stay in the alerts panel and in the daemon.
    pub fn acknowledge(&mut self) {
        let keys: Vec<String> = self
            .alerts
            .iter()
            .chain(&self.cleared)
            .map(|a| a.key.clone())
            .collect();
        self.acknowledged.extend(keys);
    }

    /// Age of the last refresh at `now` (shown ticking in the header).
    #[must_use]
    pub fn snapshot_age(&self, now: SystemTime) -> Option<Duration> {
        let snap = self.snapshot.as_ref()?;
        humantime::parse_rfc3339_weak(&snap.updated_at)
            .ok()
            .and_then(|t| now.duration_since(t).ok())
    }
}

#[cfg(test)]
mod tests;
