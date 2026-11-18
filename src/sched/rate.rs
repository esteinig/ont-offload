// SPDX-License-Identifier: AGPL-3.0-or-later
//! One token bucket shared by all workers, so `rate_limit` is the total.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::config::units::Rate;

use super::Clock;

/// Total-rate limiter shared by all workers.
///
/// Bytes are reserved before they are sent; the bucket may go into debt and
/// the reserving worker waits until the debt is paid. Over any interval of
/// length `T` from the start, at most `burst + rate * T` bytes are released.
///
/// Implements: SRS-RATE-001
#[derive(Debug)]
pub struct RateLimiter {
    rate: Option<f64>,
    burst: f64,
    clock: Arc<dyn Clock>,
    state: Mutex<(f64, Duration)>,
}

impl RateLimiter {
    /// A limiter for `rate` with a bucket of `burst` bytes, starting full.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn new(rate: Rate, burst: u64, clock: Arc<dyn Clock>) -> Self {
        let now = clock.now();
        Self {
            rate: rate.bytes_per_sec().map(|r| r as f64),
            burst: burst as f64,
            clock,
            state: Mutex::new((burst as f64, now)),
        }
    }

    /// Reserve `n` bytes and return how long to wait before sending them.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn reserve(&self, n: u64) -> Duration {
        let Some(rate) = self.rate else {
            return Duration::ZERO;
        };
        let now = self.clock.now();
        let mut g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (tokens, last) = *g;
        let refill = now.saturating_sub(last).as_secs_f64() * rate;
        let tokens = (tokens + refill).min(self.burst) - n as f64;
        *g = (tokens, now.max(last));
        if tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-tokens / rate)
        }
    }

    /// Reserve `n` bytes and wait until they may be sent.
    pub fn acquire(&self, n: u64) {
        let wait = self.reserve(n);
        if !wait.is_zero() {
            self.clock.sleep(wait);
        }
    }
}
