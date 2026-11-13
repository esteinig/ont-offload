// SPDX-License-Identifier: AGPL-3.0-or-later
//! Lifecycle of a live run: which files are stable enough to copy, and when
//! the run may be declared complete. Pure logic on a monotonic clock
//! reading, so it is tested on a fake clock.
//!
//! A run is *live* until a complete marker appears, *settling* until nothing
//! has changed for `settle_after_complete`, *reconciling* until every
//! selected file is committed with its current size and modification time,
//! and then *finalising* (manifest, `run.json`, `COMPLETE`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::journal::{Committed, nanos};

use super::Selected;

/// What is known about one source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Track {
    /// Size at the last scan.
    pub size: u64,
    /// Modification time at the last scan (ns since the epoch).
    pub mtime_ns: u64,
    /// Clock reading since which size and mtime have not changed.
    pub since: Duration,
}

/// Run phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// No complete marker yet.
    Live,
    /// Complete marker seen; waiting for `settle_after_complete` without changes.
    Settling,
    /// Settled; waiting for the last files to be committed.
    Reconciling,
    /// Ready for the manifest, `run.json` and `COMPLETE`.
    Finalising,
}

impl Phase {
    /// Lowercase name for logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Settling => "settling",
            Self::Reconciling => "reconciling",
            Self::Finalising => "finalising",
        }
    }
}

/// Stability and completion tracking for one run.
#[derive(Debug, Clone, Default)]
pub struct Lifecycle {
    /// Selected files by relative path.
    pub files: BTreeMap<String, Track>,
    /// Files that wait for the complete marker (`defer_until_complete`).
    pub deferred: BTreeSet<String>,
    /// When the complete marker was first seen.
    pub complete_since: Option<Duration>,
    /// Last time any file appeared, changed or disappeared.
    pub last_change: Duration,
}

/// Whether the journal records `rel` with exactly this size and mtime.
#[must_use]
pub fn committed_as(committed: &BTreeMap<String, Committed>, rel: &str, t: &Track) -> bool {
    committed
        .get(rel)
        .is_some_and(|c| c.size == t.size && c.mtime_ns == t.mtime_ns)
}

impl Lifecycle {
    /// A run first seen at `now`.
    #[must_use]
    pub fn new(now: Duration) -> Self {
        Self {
            last_change: now,
            ..Self::default()
        }
    }

    /// Update from a fresh scan. Returns how many files appeared, changed or
    /// disappeared.
    ///
    /// Implements: SRS-LIVE-001, SRS-LIVE-003
    pub fn observe(&mut self, now: Duration, sel: &Selected, complete_marker: bool) -> usize {
        let mut changes = 0;
        let mut seen = BTreeSet::new();
        for f in &sel.files {
            seen.insert(f.rel.clone());
            let mtime_ns = nanos(f.modified);
            match self.files.get_mut(&f.rel) {
                Some(t) if t.size == f.size && t.mtime_ns == mtime_ns => {}
                Some(t) => {
                    *t = Track {
                        size: f.size,
                        mtime_ns,
                        since: now,
                    };
                    changes += 1;
                }
                None => {
                    self.files.insert(
                        f.rel.clone(),
                        Track {
                            size: f.size,
                            mtime_ns,
                            since: now,
                        },
                    );
                    changes += 1;
                }
            }
            if f.deferred {
                self.deferred.insert(f.rel.clone());
            } else {
                self.deferred.remove(&f.rel);
            }
        }
        let before = self.files.len();
        self.files.retain(|k, _| seen.contains(k));
        self.deferred.retain(|k| seen.contains(k));
        changes += before - self.files.len();
        if changes > 0 {
            self.last_change = now;
        }
        match (complete_marker, self.complete_since) {
            (true, None) => self.complete_since = Some(now),
            (false, Some(_)) => self.complete_since = None,
            _ => {}
        }
        changes
    }

    /// Files to copy now: unchanged for `settle`, not deferred unless the
    /// complete marker is there, and not already committed as they are.
    ///
    /// Implements: SRS-LIVE-001
    #[must_use]
    pub fn ready(
        &self,
        now: Duration,
        settle: Duration,
        committed: &BTreeMap<String, Committed>,
    ) -> Vec<String> {
        self.files
            .iter()
            .filter(|(rel, t)| {
                now.saturating_sub(t.since) >= settle
                    && (self.complete_since.is_some() || !self.deferred.contains(*rel))
                    && !committed_as(committed, rel, t)
            })
            .map(|(rel, _)| rel.clone())
            .collect()
    }

    /// Files not yet committed as they are now.
    #[must_use]
    pub fn outstanding(&self, committed: &BTreeMap<String, Committed>) -> usize {
        self.files
            .iter()
            .filter(|(rel, t)| !committed_as(committed, rel, t))
            .count()
    }

    /// The run's phase.
    ///
    /// Implements: SRS-LIVE-003
    #[must_use]
    pub fn phase(
        &self,
        now: Duration,
        settle_after_complete: Duration,
        committed: &BTreeMap<String, Committed>,
    ) -> Phase {
        let Some(since) = self.complete_since else {
            return Phase::Live;
        };
        if now.saturating_sub(since) < settle_after_complete
            || now.saturating_sub(self.last_change) < settle_after_complete
        {
            return Phase::Settling;
        }
        if self.outstanding(committed) > 0 {
            Phase::Reconciling
        } else {
            Phase::Finalising
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::run::SelectedFile;

    fn sel(files: &[(&str, u64, u64, bool)]) -> Selected {
        Selected {
            files: files
                .iter()
                .map(|(rel, size, mtime, deferred)| SelectedFile {
                    rel: (*rel).to_owned(),
                    size: *size,
                    modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(*mtime)),
                    deferred: *deferred,
                })
                .collect(),
            ..Selected::default()
        }
    }

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn commit(c: &mut BTreeMap<String, Committed>, l: &Lifecycle, rel: &str) {
        let t = l.files[rel];
        c.insert(
            rel.to_owned(),
            Committed {
                size: t.size,
                mtime_ns: t.mtime_ns,
                sha256: String::new(),
            },
        );
    }

    /// Verifies: SRS-LIVE-001
    #[test]
    fn growing_file_is_copied_only_after_settle_without_change() {
        let mut l = Lifecycle::new(s(0));
        let none = BTreeMap::new();
        l.observe(s(0), &sel(&[("a.pod5", 10, 1, false)]), false);
        assert!(l.ready(s(29), s(30), &none).is_empty());
        // Grows at 20 s: the settle window starts again.
        l.observe(s(20), &sel(&[("a.pod5", 20, 2, false)]), false);
        assert!(l.ready(s(40), s(30), &none).is_empty());
        assert_eq!(l.ready(s(50), s(30), &none), ["a.pod5"]);
        // Same size, new mtime (rewritten in place) also restarts the window.
        l.observe(s(55), &sel(&[("a.pod5", 20, 3, false)]), false);
        assert!(l.ready(s(60), s(30), &none).is_empty());
    }

    /// Verifies: SRS-LIVE-001, SRS-SEL-001
    #[test]
    fn deferred_files_wait_for_the_complete_marker_and_committed_files_are_skipped() {
        let mut l = Lifecycle::new(s(0));
        let mut c = BTreeMap::new();
        let files = [
            ("a.pod5", 1, 1, false),
            ("sequencing_summary_x.txt", 5, 1, true),
        ];
        l.observe(s(0), &sel(&files), false);
        assert_eq!(l.ready(s(60), s(30), &c), ["a.pod5"]);
        commit(&mut c, &l, "a.pod5");
        assert!(l.ready(s(60), s(30), &c).is_empty());
        l.observe(s(70), &sel(&files), true);
        assert_eq!(l.ready(s(70), s(30), &c), ["sequencing_summary_x.txt"]);
        // A committed file that changes is copied again.
        l.observe(s(80), &sel(&[("a.pod5", 2, 2, false), files[1]]), true);
        assert_eq!(
            l.ready(s(120), s(30), &c),
            ["a.pod5", "sequencing_summary_x.txt"]
        );
    }

    /// Verifies: SRS-LIVE-003
    #[test]
    fn completion_needs_marker_quiet_settle_and_every_file_committed() {
        let sac = s(300);
        let mut l = Lifecycle::new(s(0));
        let mut c = BTreeMap::new();
        let base = [
            ("a.pod5", 1, 1, false),
            ("final_summary_x.txt", 1, 1, false),
        ];
        l.observe(s(0), &sel(&base[..1]), false);
        assert_eq!(l.phase(s(1000), sac, &c), Phase::Live);
        l.observe(s(100), &sel(&base), true);
        assert_eq!(l.phase(s(300), sac, &c), Phase::Settling);
        // A late file 120 s after the marker restarts the settle window.
        let late = [base[0], base[1], ("other_reports/late.txt", 1, 1, true)];
        l.observe(s(220), &sel(&late), true);
        assert_eq!(l.phase(s(450), sac, &c), Phase::Settling);
        assert_eq!(l.phase(s(520), sac, &c), Phase::Reconciling);
        for rel in ["a.pod5", "final_summary_x.txt"] {
            commit(&mut c, &l, rel);
        }
        assert_eq!(l.phase(s(520), sac, &c), Phase::Reconciling);
        commit(&mut c, &l, "other_reports/late.txt");
        assert_eq!(l.phase(s(520), sac, &c), Phase::Finalising);
        // The marker disappearing (run restarted) makes the run live again.
        l.observe(s(600), &sel(&late[..1]), false);
        assert_eq!(l.phase(s(2000), sac, &c), Phase::Live);
    }
}
