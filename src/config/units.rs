// SPDX-License-Identifier: AGPL-3.0-or-later
//! Human-friendly sizes and rates: `"8MiB"`, `"500GB"`, `"100MB/s"`, `"800Mbit/s"`.
//!
//! Units are explicit on purpose. Decimal units (`KB`, `MB`, `GB`, `TB`) are
//! powers of 1000, binary units (`KiB`, `MiB`, `GiB`, `TiB`) powers of 1024.
//! Bit rates (`kbit/s`, `Mbit/s`, `Gbit/s`, also `Mbps`, `Gbps`) are converted
//! to bytes per second. A bare `K`, `M` or `G` is rejected as ambiguous.

use std::fmt;

const UNITS_HINT: &str = "use a number and a unit, e.g. 8MiB, 500GB, 2TB";
const RATE_HINT: &str = "use e.g. \"100MB/s\", \"800Mbit/s\", \"1Gbit/s\" or \"unlimited\"";

fn byte_unit(unit: &str) -> Option<u64> {
    Some(match unit.to_ascii_lowercase().as_str() {
        "b" | "" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return None,
    })
}

/// Bits per second for a bit-rate unit (`Mbit/s`, `Mbps`, ...).
fn bit_rate_unit(unit: &str) -> Option<u64> {
    Some(match unit.to_ascii_lowercase().as_str() {
        "bit/s" | "bps" => 1,
        "kbit/s" | "kbps" => 1_000,
        "mbit/s" | "mbps" => 1_000_000,
        "gbit/s" | "gbps" => 1_000_000_000,
        _ => return None,
    })
}

/// Split `"12.5 MB"` into the number and unit text.
fn split(s: &str) -> (&str, &str) {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    (&s[..end], s[end..].trim())
}

/// `number * multiplier` exactly for a decimal number such as `"12.5"`.
fn scaled(number: &str, multiplier: u64) -> Option<u64> {
    let (int, frac) = number.split_once('.').unwrap_or((number, ""));
    if int.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let int: u128 = int.parse().ok()?;
    let frac_value: u128 = if frac.is_empty() {
        0
    } else {
        frac.parse().ok()?
    };
    let denom = 10u128.pow(u32::try_from(frac.len()).ok()?);
    let total = int.checked_mul(u128::from(multiplier))?
        + frac_value.checked_mul(u128::from(multiplier))? / denom;
    u64::try_from(total).ok()
}

/// Parse a size such as `"8MiB"` or `"500 GB"` into bytes.
///
/// Implements: SRS-CFG-003
///
/// # Errors
/// Returns a message naming the accepted forms.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let s = text.trim();
    let (num, unit) = split(s);
    let bad = || format!("invalid size {text:?}: {UNITS_HINT}");
    if num.is_empty() {
        return Err(bad());
    }
    if unit.is_empty() && num != "0" {
        return Err(format!(
            "invalid size {text:?}: the unit is missing; {UNITS_HINT}"
        ));
    }
    let mult = byte_unit(unit).ok_or_else(bad)?;
    scaled(num, mult).ok_or_else(bad)
}

/// A total transfer rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rate {
    /// No limit.
    Unlimited,
    /// Bytes per second (always at least 1).
    BytesPerSec(u64),
}

impl Rate {
    /// Bytes per second, or `None` for unlimited.
    #[must_use]
    pub fn bytes_per_sec(self) -> Option<u64> {
        match self {
            Self::Unlimited => None,
            Self::BytesPerSec(b) => Some(b),
        }
    }
}

/// Parse a rate such as `"100MB/s"`, `"800Mbit/s"`, `"1Gbps"` or `"unlimited"`.
///
/// Implements: SRS-CFG-003
///
/// # Errors
/// Returns a message naming the accepted forms.
pub fn parse_rate(text: &str) -> Result<Rate, String> {
    let s = text.trim();
    if s.eq_ignore_ascii_case("unlimited") {
        return Ok(Rate::Unlimited);
    }
    let bad = || format!("invalid rate {text:?}: {RATE_HINT}");
    let (num, unit) = split(s);
    if num.is_empty() {
        return Err(bad());
    }
    let bytes = if let Some(bits) = bit_rate_unit(unit) {
        scaled(num, bits).map(|b| b / 8)
    } else if let Some(size_unit) = unit.strip_suffix("/s").map(str::trim) {
        if size_unit.is_empty() {
            return Err(bad());
        }
        byte_unit(size_unit).and_then(|m| scaled(num, m))
    } else {
        return Err(if byte_unit(unit).is_some() {
            format!("invalid rate {text:?}: add \"/s\", e.g. \"{num}{unit}/s\"")
        } else {
            bad()
        });
    };
    match bytes {
        Some(0) => Err(format!(
            "invalid rate {text:?}: must be more than zero (or \"unlimited\")"
        )),
        Some(b) => Ok(Rate::BytesPerSec(b)),
        None => Err(bad()),
    }
}

/// Parse a duration such as `"30s"`, `"5m"`, `"1h"` or `"7d"`.
///
/// Implements: SRS-CFG-003
///
/// # Errors
/// Returns a message naming the accepted forms.
pub fn parse_duration(text: &str) -> Result<std::time::Duration, String> {
    humantime::parse_duration(text.trim()).map_err(|e| {
        format!("invalid duration {text:?} ({e}): use e.g. \"30s\", \"5m\", \"1h\", \"7d\"")
    })
}

/// Display bytes with a decimal unit, e.g. `"1.5 GB"`.
#[derive(Debug, Clone, Copy)]
pub struct Bytes(pub u64);

impl fmt::Display for Bytes {
    #[allow(clippy::cast_precision_loss)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
        let mut v = self.0 as f64;
        let mut i = 0;
        while v >= 1000.0 && i + 1 < UNITS.len() {
            v /= 1000.0;
            i += 1;
        }
        if i == 0 {
            write!(f, "{} B", self.0)
        } else if (v - v.round()).abs() < 0.005 {
            write!(f, "{v:.0} {}", UNITS[i])
        } else {
            write!(f, "{v:.2} {}", UNITS[i])
        }
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unlimited => f.write_str("unlimited"),
            Self::BytesPerSec(b) => write!(f, "{}/s", Bytes(*b)),
        }
    }
}

/// Display bytes per second as bits per second, e.g. `"800 Mbit/s"`.
#[derive(Debug, Clone, Copy)]
pub struct Bits(pub u64);

impl fmt::Display for Bits {
    #[allow(clippy::cast_precision_loss)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 4] = ["bit/s", "kbit/s", "Mbit/s", "Gbit/s"];
        let mut v = self.0 as f64 * 8.0;
        let mut i = 0;
        while v >= 1000.0 && i + 1 < UNITS.len() {
            v /= 1000.0;
            i += 1;
        }
        if (v - v.round()).abs() < 0.005 {
            write!(f, "{v:.0} {}", UNITS[i])
        } else {
            write!(f, "{v:.2} {}", UNITS[i])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies: SRS-CFG-003
    #[test]
    fn sizes() {
        for (text, want) in [
            ("8MiB", 8 << 20),
            ("500GiB", 500 << 30),
            ("200GB", 200_000_000_000),
            ("1.5 GB", 1_500_000_000),
            ("64KiB", 65_536),
            ("0", 0),
            ("10b", 10),
            ("2tb", 2_000_000_000_000),
        ] {
            assert_eq!(parse_size(text), Ok(want), "{text}");
        }
        for text in [
            "",
            "8",
            "8M",
            "8 megs",
            "-1GB",
            "1.2.3GB",
            "GB",
            "99999999999TB",
            "8MB/s",
        ] {
            assert!(parse_size(text).is_err(), "{text} should fail");
        }
        assert!(parse_size("8").unwrap_err().contains("unit is missing"));
    }

    /// Verifies: SRS-CFG-003
    #[test]
    fn rates() {
        for (text, want) in [
            ("100MB/s", Rate::BytesPerSec(100_000_000)),
            ("100 MB/s", Rate::BytesPerSec(100_000_000)),
            ("25MiB/s", Rate::BytesPerSec(25 << 20)),
            ("800Mbit/s", Rate::BytesPerSec(100_000_000)),
            ("1Gbit/s", Rate::BytesPerSec(125_000_000)),
            ("10Gbps", Rate::BytesPerSec(1_250_000_000)),
            ("100mbps", Rate::BytesPerSec(12_500_000)),
            ("unlimited", Rate::Unlimited),
            ("Unlimited", Rate::Unlimited),
        ] {
            assert_eq!(parse_rate(text), Ok(want), "{text}");
        }
        for text in ["", "100", "100MB", "0MB/s", "fast", "100M/s", "/s", "1Gbit"] {
            assert!(parse_rate(text).is_err(), "{text} should fail");
        }
        assert!(parse_rate("100MB").unwrap_err().contains("add \"/s\""));
    }

    /// Verifies: SRS-CFG-003
    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap().as_secs(), 30);
        assert_eq!(parse_duration("5m").unwrap().as_secs(), 300);
        assert_eq!(parse_duration("7d").unwrap().as_secs(), 7 * 86_400);
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("").is_err());
    }

    /// Verifies: SRS-CFG-004
    #[test]
    fn display() {
        assert_eq!(Bytes(100_000_000).to_string(), "100 MB");
        assert_eq!(Bytes(1_500_000_000).to_string(), "1.50 GB");
        assert_eq!(Bytes(12).to_string(), "12 B");
        assert_eq!(Rate::BytesPerSec(25_000_000).to_string(), "25 MB/s");
        assert_eq!(Bits(125_000_000).to_string(), "1 Gbit/s");
        assert_eq!(Bits(100_000_000).to_string(), "800 Mbit/s");
    }
}
