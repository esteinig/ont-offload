// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sizes (`"500MB"`, `"8MiB"`) and durations (`"10m"`, `"2h"`).

use std::time::Duration;

/// Parse a size with an explicit unit (B, KB, MB, GB, TB, KiB, MiB, GiB, TiB).
///
/// # Errors
/// A message naming the accepted forms.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let s = text.trim();
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = (&s[..end], s[end..].trim());
    let mult: u64 = match unit.to_ascii_lowercase().as_str() {
        "b" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        "" if num == "0" => 1,
        _ => return Err(format!("invalid size {text:?}: use e.g. 500MB, 8MiB, 2TB")),
    };
    let value: f64 = num
        .parse()
        .map_err(|_| format!("invalid size {text:?}: use e.g. 500MB, 8MiB, 2TB"))?;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let bytes = (value * mult as f64).round() as u64;
    Ok(bytes)
}

/// Parse a duration such as `"30s"`, `"10m"`, `"2h"`.
///
/// # Errors
/// A message naming the accepted forms.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    humantime::parse_duration(text.trim())
        .map_err(|e| format!("invalid duration {text:?} ({e}): use e.g. 30s, 10m, 2h"))
}

/// Format seconds as `HH:MM:SS`.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn hms(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Format bytes with a decimal unit.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i + 1 < UNITS.len() {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_durations() {
        assert_eq!(parse_size("500MB"), Ok(500_000_000));
        assert_eq!(parse_size("8MiB"), Ok(8 << 20));
        assert_eq!(parse_size("1.5 GB"), Ok(1_500_000_000));
        assert!(parse_size("500").is_err());
        assert!(parse_size("5 megs").is_err());
        assert_eq!(parse_duration("10m").unwrap().as_secs(), 600);
        assert_eq!(hms(3725.0), "01:02:05");
        assert_eq!(bytes(1_500_000), "1.5 MB");
    }
}
