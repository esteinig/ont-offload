// SPDX-License-Identifier: AGPL-3.0-or-later
//! UTC times for reports: RFC 3339 parsing, calendar days and the period
//! chosen on the command line.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch of an RFC 3339 UTC time (fractions
/// dropped); `None` when it does not parse.
#[must_use]
pub fn parse(ts: &str) -> Option<u64> {
    humantime::parse_rfc3339_weak(ts.trim())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// RFC 3339 UTC, seconds precision.
#[must_use]
pub fn rfc3339(secs: u64) -> String {
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_secs(secs)).to_string()
}

/// Seconds since the epoch, now.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
#[must_use]
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::similar_names
)]
pub fn civil(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Days since 1970-01-01 of a calendar date; `None` before 1970.
#[must_use]
#[allow(clippy::cast_sign_loss, clippy::similar_names)]
pub fn days_from_civil(y: i64, m: u32, d: u32) -> Option<u64> {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days >= 0).then_some(days as u64)
}

/// `YYYY-MM-DD` of a time.
#[must_use]
pub fn day(secs: u64) -> String {
    let (y, m, d) = civil(secs / 86_400);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYYMMDD` of a time (file names and report ids).
#[must_use]
pub fn compact_day(secs: u64) -> String {
    day(secs).replace('-', "")
}

/// A reporting period: `[from, to)` in seconds since the epoch, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    /// First second in the period.
    pub from: u64,
    /// First second after the period.
    pub to: u64,
}

impl Period {
    /// Whether `t` lies in the period.
    #[must_use]
    pub fn contains(&self, t: u64) -> bool {
        t >= self.from && t < self.to
    }

    /// Seconds of `[a, b)` inside the period.
    #[must_use]
    pub fn overlap(&self, a: u64, b: u64) -> u64 {
        b.min(self.to).saturating_sub(a.max(self.from))
    }

    /// Length in seconds.
    #[must_use]
    pub fn secs(&self) -> u64 {
        self.to - self.from
    }

    /// Calendar days touched by the period (a partial day counts).
    #[must_use]
    pub fn days(&self) -> u64 {
        (self.to - 1) / 86_400 - self.from / 86_400 + 1
    }
}

fn date(s: &str) -> Option<u64> {
    let mut it = s.splitn(3, '-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || s.len() != 10 {
        return None;
    }
    let days = days_from_civil(y, m, d)?;
    // 2026-02-31 would roll over into March: refuse it.
    (civil(days) == (y, m, d)).then_some(days * 86_400)
}

/// The start of a bound: a date (midnight UTC) or an RFC 3339 time.
fn bound(s: &str, end_of_day: bool) -> Result<u64, String> {
    if let Some(t) = date(s) {
        return Ok(if end_of_day { t + 86_400 } else { t });
    }
    parse(s).ok_or_else(|| format!("`{s}` is neither a date (2026-10-31) nor an RFC 3339 time"))
}

/// The period from the command line: `--month YYYY-MM`, `--last 7d`, or
/// `--from`/`--to` (a date in `--to` includes that whole day). Without any,
/// the last 30 days up to `now`.
///
/// # Errors
/// A value that does not parse, or an empty period.
pub fn period(
    from: Option<&str>,
    to: Option<&str>,
    month: Option<&str>,
    last: Option<&str>,
    now: u64,
) -> Result<Period, String> {
    let p = if let Some(m) = month {
        let start =
            date(&format!("{m}-01")).ok_or_else(|| format!("`{m}` is not a month (2026-10)"))?;
        let (y, mo, _) = civil(start / 86_400);
        let (ny, nm) = if mo == 12 { (y + 1, 1) } else { (y, mo + 1) };
        let end = days_from_civil(ny, nm, 1).ok_or("month out of range")? * 86_400;
        Period {
            from: start,
            to: end,
        }
    } else if let Some(l) = last {
        let d = humantime::parse_duration(l).map_err(|e| format!("--last {l}: {e}"))?;
        Period {
            from: now.saturating_sub(d.as_secs()),
            to: now,
        }
    } else {
        let to = to.map_or(Ok(now), |t| bound(t, true))?;
        let from = from.map_or(Ok(to.saturating_sub(30 * 86_400)), |f| bound(f, false))?;
        Period { from, to }
    };
    if p.to <= p.from {
        return Err("the period is empty: --from must be before --to".into());
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies: SRS-RPT-001
    #[test]
    fn calendar_days_round_trip_and_periods_parse() {
        for days in [0, 59, 60, 365, 10_957, 20_407, 20_819, 30_000] {
            let (y, m, d) = civil(days);
            assert_eq!(days_from_civil(y, m, d), Some(days));
        }
        assert_eq!(day(parse("2026-11-17T08:15:00Z").unwrap()), "2026-11-17");
        assert_eq!(
            parse("2026-11-17T08:15:00.123Z"),
            parse("2026-11-17T08:15:00Z")
        );
        let oct = period(None, None, Some("2026-10"), None, 0).unwrap();
        assert_eq!(rfc3339(oct.from), "2026-10-01T00:00:00Z");
        assert_eq!(rfc3339(oct.to), "2026-11-01T00:00:00Z");
        assert_eq!(oct.days(), 31);
        let dec = period(None, None, Some("2026-12"), None, 0).unwrap();
        assert_eq!(rfc3339(dec.to), "2027-01-01T00:00:00Z");
        let p = period(Some("2026-10-01"), Some("2026-10-31"), None, None, 0).unwrap();
        assert_eq!(p, oct, "a date in --to includes the whole day");
        let now = parse("2026-11-17T12:00:00Z").unwrap();
        let w = period(None, None, None, Some("7d"), now).unwrap();
        assert_eq!(w.secs(), 7 * 86_400);
        assert!(period(Some("2026-10-05"), Some("2026-10-01"), None, None, 0).is_err());
        assert!(period(Some("2026-02-31"), None, None, None, now).is_err());
        assert!(period(None, None, Some("2026-13"), None, 0).is_err());
        assert_eq!(oct.overlap(oct.from - 10, oct.from + 10), 10);
        assert!(oct.contains(oct.from) && !oct.contains(oct.to));
    }
}
