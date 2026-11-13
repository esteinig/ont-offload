// SPDX-License-Identifier: AGPL-3.0-or-later
//! Outage tracking: when the destination fails, every write pauses; the
//! share is re-checked with exponential backoff capped at
//! `check_every_max`, and writing resumes once the guard has passed for
//! `stable_before_resume` without a failure in between. An outage is logged
//! as one start line, a summary every `log_every`, a warning, an alert with
//! reminders, and one end line; never one line per failed check.

use std::time::Duration;

use crate::config;
use crate::logging::escape_control;

use super::human;

/// First re-check interval after a failure.
pub const FIRST_RECHECK: Duration = Duration::from_secs(1);

/// Something to report about an outage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The destination became unavailable.
    Started {
        /// Why.
        reason: String,
        /// Files waiting.
        backlog: usize,
    },
    /// Periodic summary.
    Summary {
        /// Outage duration so far.
        down: Duration,
        /// Failed checks so far.
        failures: u64,
        /// Last error.
        reason: String,
        /// Files waiting.
        backlog: usize,
    },
    /// `warn_after` passed.
    Warn {
        /// Outage duration.
        down: Duration,
    },
    /// `alert_after` passed (and every `remind_every` after that).
    Alert {
        /// Outage duration.
        down: Duration,
        /// A repeat of an earlier alert.
        reminder: bool,
    },
    /// The destination passed the guard again; resuming after the stability window.
    Recovering,
    /// The outage ended.
    Ended {
        /// Outage duration.
        down: Duration,
        /// Failed checks.
        failures: u64,
        /// Files waiting.
        backlog: usize,
    },
}

impl Event {
    /// Log level and message.
    ///
    /// Implements: SRS-LOG-004
    #[must_use]
    pub fn message(&self) -> (log::Level, String) {
        match self {
            Self::Started { reason, backlog } => (
                log::Level::Warn,
                format!(
                    "destination unavailable: {}; all writes paused, {backlog} file(s) waiting; \
                     re-checking with backoff",
                    escape_control(reason)
                ),
            ),
            Self::Summary {
                down,
                failures,
                reason,
                backlog,
            } => (
                log::Level::Info,
                format!(
                    "outage continues: down {}, {failures} failed check(s), last error: {}; \
                     {backlog} file(s) waiting",
                    human(*down),
                    escape_control(reason)
                ),
            ),
            Self::Warn { down } => (
                log::Level::Warn,
                format!("destination has been unavailable for {}", human(*down)),
            ),
            Self::Alert { down, reminder } => (
                log::Level::Error,
                format!(
                    "ALERT: destination unavailable for {}{}; runs are waiting on the local disk",
                    human(*down),
                    if *reminder { " (reminder)" } else { "" }
                ),
            ),
            Self::Recovering => (
                log::Level::Info,
                "destination passes the guard again; resuming once it stays up".to_owned(),
            ),
            Self::Ended {
                down,
                failures,
                backlog,
            } => (
                log::Level::Info,
                format!(
                    "destination back after {} ({failures} failed check(s)); resuming {backlog} file(s)",
                    human(*down)
                ),
            ),
        }
    }
}

/// An ongoing outage as reported in the state snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// Time since it started.
    pub duration: Duration,
    /// Last error.
    pub reason: String,
    /// Failed checks.
    pub failures: u64,
    /// Time until the next check.
    pub next_check: Duration,
    /// The last check passed; waiting for stability.
    pub recovering: bool,
    /// `notice`, `warning` or `alert`.
    pub level: &'static str,
}

#[derive(Debug, Clone)]
struct Down {
    since: Duration,
    failures: u64,
    reason: String,
    backoff: Duration,
    next_check: Duration,
    ok_since: Option<Duration>,
    last_summary: Duration,
    warned: bool,
    alerts: u32,
}

/// Outage state machine on a monotonic clock reading.
///
/// Implements: SRS-RES-002, SRS-RES-005, SRS-LOG-004
#[derive(Debug, Clone)]
pub struct Tracker {
    cfg: config::Outage,
    down: Option<Down>,
}

impl Tracker {
    /// No outage.
    #[must_use]
    pub fn new(cfg: &config::Outage) -> Self {
        Self {
            cfg: cfg.clone(),
            down: None,
        }
    }

    /// Writes are paused.
    #[must_use]
    pub fn is_down(&self) -> bool {
        self.down.is_some()
    }

    /// Outage duration, if down.
    #[must_use]
    pub fn down_for(&self, now: Duration) -> Option<Duration> {
        self.down.as_ref().map(|d| now.saturating_sub(d.since))
    }

    /// A re-check of the destination is due.
    #[must_use]
    pub fn check_due(&self, now: Duration) -> bool {
        self.down.as_ref().is_some_and(|d| now >= d.next_check)
    }

    /// The ongoing outage, for the state snapshot.
    ///
    /// Implements: SRS-STATE-003
    #[must_use]
    pub fn info(&self, now: Duration) -> Option<Info> {
        self.down.as_ref().map(|d| Info {
            duration: now.saturating_sub(d.since),
            reason: d.reason.clone(),
            failures: d.failures,
            next_check: d.next_check.saturating_sub(now),
            recovering: d.ok_since.is_some(),
            level: if d.alerts > 0 {
                "alert"
            } else if d.warned {
                "warning"
            } else {
                "notice"
            },
        })
    }

    /// Current backoff (for tests and status).
    #[must_use]
    pub fn backoff(&self) -> Option<Duration> {
        self.down.as_ref().map(|d| d.backoff)
    }

    fn cap(&self) -> Duration {
        self.cfg.check_every_max.max(FIRST_RECHECK)
    }

    /// A write or a check failed.
    pub fn failure(&mut self, now: Duration, reason: &str, backlog: usize) -> Vec<Event> {
        let cap = self.cap();
        match &mut self.down {
            None => {
                self.down = Some(Down {
                    since: now,
                    failures: 1,
                    reason: reason.to_owned(),
                    backoff: FIRST_RECHECK,
                    next_check: now + FIRST_RECHECK,
                    ok_since: None,
                    last_summary: now,
                    warned: false,
                    alerts: 0,
                });
                vec![Event::Started {
                    reason: reason.to_owned(),
                    backlog,
                }]
            }
            Some(d) => {
                d.failures += 1;
                reason.clone_into(&mut d.reason);
                d.ok_since = None;
                d.backoff = (d.backoff * 2).min(cap);
                d.next_check = now + d.backoff;
                Vec::new()
            }
        }
    }

    /// A check passed. Ends the outage once checks have passed for
    /// `stable_before_resume`.
    pub fn success(&mut self, now: Duration, backlog: usize) -> Vec<Event> {
        let stable = self.cfg.stable_before_resume;
        let Some(d) = &mut self.down else {
            return Vec::new();
        };
        let mut ev = Vec::new();
        let ok_since = *d.ok_since.get_or_insert_with(|| {
            ev.push(Event::Recovering);
            now
        });
        if now.saturating_sub(ok_since) >= stable {
            ev.push(Event::Ended {
                down: now.saturating_sub(d.since),
                failures: d.failures,
                backlog,
            });
            self.down = None;
        } else {
            d.backoff = FIRST_RECHECK;
            d.next_check = now
                + (stable / 6).clamp(FIRST_RECHECK, self.cfg.check_every_max.max(FIRST_RECHECK));
        }
        ev
    }

    /// Periodic reporting while down.
    pub fn tick(&mut self, now: Duration, backlog: usize) -> Vec<Event> {
        let cfg = &self.cfg;
        let Some(d) = &mut self.down else {
            return Vec::new();
        };
        let down = now.saturating_sub(d.since);
        let mut ev = Vec::new();
        if !cfg.log_every.is_zero() && now.saturating_sub(d.last_summary) >= cfg.log_every {
            d.last_summary = now;
            ev.push(Event::Summary {
                down,
                failures: d.failures,
                reason: d.reason.clone(),
                backlog,
            });
        }
        if !d.warned && down >= cfg.warn_after {
            d.warned = true;
            ev.push(Event::Warn { down });
        }
        let next_alert = cfg.alert_after + cfg.remind_every.max(Duration::from_secs(60)) * d.alerts;
        if down >= next_alert && (d.alerts == 0 || !cfg.remind_every.is_zero()) {
            ev.push(Event::Alert {
                down,
                reminder: d.alerts > 0,
            });
            d.alerts += 1;
        }
        ev
    }
}
