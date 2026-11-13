// SPDX-License-Identifier: AGPL-3.0-or-later
//! Scheduling (SWI-SCHED): the clock, the shared rate limiter, outage
//! tracking with backoff, the worker pool with stall detection, systemd
//! notification, and the `watch` controller.
//!
//! One controller thread owns every decision and the journals; workers only
//! copy files and write the end-of-run outputs. The controller never touches
//! the destination itself: guard probes run on their own thread with a
//! timeout, so a hung share cannot block it.

pub mod lock;
pub mod notify;
pub mod outage;
pub mod pool;
pub mod rate;
pub mod watch;

#[cfg(test)]
mod tests;

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A monotonic clock that can be faked in tests.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Time since the clock started.
    fn now(&self) -> Duration;
    /// Wait `d` (a fake clock advances instead).
    fn sleep(&self, d: Duration);
}

/// The real monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct RealClock(Instant);

impl RealClock {
    /// A clock starting now.
    #[must_use]
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A clock that only moves when told to (or when something sleeps on it).
#[derive(Debug, Default)]
pub struct FakeClock(Mutex<Duration>);

impl FakeClock {
    /// A fake clock at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Move the clock forward.
    pub fn advance(&self, d: Duration) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) += d;
    }

    /// Set the clock (never moves it backwards).
    pub fn set(&self, t: Duration) {
        let mut g = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if t > *g {
            *g = t;
        }
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
}

/// `1h 2m 3s` style duration for log lines.
#[must_use]
pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, (s / 60) % 60, s % 60);
    match (h, m) {
        (0, 0) => format!("{sec}s"),
        (0, _) => format!("{m}m {sec}s"),
        _ => format!("{h}h {m}m"),
    }
}
