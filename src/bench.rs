// SPDX-License-Identifier: AGPL-3.0-or-later
//! `bench`: measure write throughput to the share with 1, 2, 4, 8 and 16
//! parallel streams, then a few chunk sizes, and suggest `workers`,
//! `rate_limit` and `chunk_size` for the configuration file (part of
//! SWI-CLI).
//!
//! The test files go to a hidden folder below `dest_root`, after the guard
//! has passed, and are removed afterwards. The results are written to
//! `state_dir/bench.json` so the settings chosen for a site can be traced
//! back to a measurement.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::units::Bytes;
use crate::error::{Error, Result};
use crate::fsio::Fs;
use crate::guard::Guard;
use crate::journal::{self, write_atomic};
use crate::logging::escape_control;

/// Result file in `state_dir`.
pub const BENCH_FILE: &str = "bench.json";

/// Chunk sizes tried after the stream counts.
pub const CHUNK_SIZES: [u64; 3] = [1 << 20, 4 << 20, 16 << 20];

/// Settings for one bench.
#[derive(Debug, Clone, Copy)]
pub struct BenchOptions {
    /// Bytes written per step, split over the streams.
    pub size: u64,
    /// Largest stream count tried (1, 2, 4, ... up to this).
    pub max_streams: usize,
    /// Chunk size for the stream steps.
    pub chunk_size: u64,
    /// Also try the chunk sizes in [`CHUNK_SIZES`].
    pub chunks: bool,
}

/// One measurement.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Step {
    /// Parallel streams.
    pub streams: usize,
    /// Chunk size in bytes.
    pub chunk_size: u64,
    /// Bytes written (and flushed).
    pub bytes: u64,
    /// Seconds from the first write to the last flush.
    pub seconds: f64,
    /// Bytes per second.
    pub bytes_per_s: f64,
}

/// Suggested settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Suggestion {
    /// Fewest streams that reach 90% of the best throughput.
    pub workers: usize,
    /// 80% of the best throughput, rounded down to two significant figures,
    /// as written in the configuration file (`"80MB/s"`).
    pub rate_limit: String,
    /// Smallest chunk size within 5% of the best, as written in the
    /// configuration file (`"4MiB"`); `null` when chunk sizes were not tried.
    pub chunk_size: Option<String>,
}

/// `bench.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Report {
    /// ont-offload version.
    pub tool_version: String,
    /// Host name.
    pub host: String,
    /// When (RFC 3339).
    pub measured_at: String,
    /// Destination root.
    pub dest_root: String,
    /// File system type and mount source.
    pub mount: String,
    /// Stream-count steps.
    pub streams: Vec<Step>,
    /// Chunk-size steps (at the suggested worker count).
    pub chunks: Vec<Step>,
    /// What to put in the configuration file.
    pub suggestion: Suggestion,
}

fn mib(n: u64) -> String {
    format!("{}MiB", n >> 20)
}

/// Round down to two significant figures, in MB/s.
fn rate_text(bytes_per_s: f64) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mbs = (bytes_per_s / 1e6).floor().max(1.0) as u64;
    let mut scale = 1;
    while mbs / scale >= 100 {
        scale *= 10;
    }
    format!("{}MB/s", (mbs / scale) * scale)
}

/// Settings from measurements.
///
/// Implements: SRS-PAR-002
#[must_use]
pub fn suggest(streams: &[Step], chunks: &[Step]) -> Suggestion {
    let best = streams.iter().map(|s| s.bytes_per_s).fold(0.0, f64::max);
    let workers = streams
        .iter()
        .filter(|s| s.bytes_per_s >= 0.9 * best)
        .map(|s| s.streams)
        .min()
        .unwrap_or(1);
    let best_chunk = chunks.iter().map(|s| s.bytes_per_s).fold(0.0, f64::max);
    let chunk_size = chunks
        .iter()
        .filter(|s| s.bytes_per_s >= 0.95 * best_chunk)
        .map(|s| s.chunk_size)
        .min()
        .map(mib);
    Suggestion {
        workers,
        rate_limit: rate_text(0.8 * best.max(best_chunk)),
        chunk_size,
    }
}

#[allow(clippy::cast_possible_truncation)]
/// Fill `buf` with bytes that do not compress or deduplicate.
fn fill(buf: &mut [u8], mut seed: u64) {
    for b in buf.iter_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *b = (seed >> 24) as u8;
    }
}

fn write_one(fs: &dyn Fs, path: &Path, bytes: u64, chunk: &[u8]) -> std::io::Result<()> {
    let mut f = fs.create_new(path)?;
    let mut left = bytes;
    while left > 0 {
        let n = usize::try_from(left.min(chunk.len() as u64)).unwrap_or(chunk.len());
        f.write_all(&chunk[..n])?;
        left -= n as u64;
    }
    f.sync_data()?;
    Ok(())
}

/// One step: `streams` threads each write `size / streams` bytes.
fn step(
    fs: &dyn Fs,
    dir: &Path,
    n: usize,
    streams: usize,
    size: u64,
    chunk_size: u64,
) -> Result<Step> {
    let per = (size / streams as u64).max(1);
    let mut chunk = vec![0u8; usize::try_from(chunk_size).unwrap_or(1 << 20)];
    fill(&mut chunk, 0x9E37_79B9_7F4A_7C15 ^ n as u64);
    let t0 = Instant::now();
    let results: Vec<std::io::Result<()>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..streams)
            .map(|i| {
                let path = dir.join(format!("step{n}-{i}.bin"));
                let chunk = &chunk;
                s.spawn(move || write_one(fs, &path, per, chunk))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(std::io::Error::other("writer thread panicked")))
            })
            .collect()
    });
    let seconds = t0.elapsed().as_secs_f64().max(1e-6);
    for i in 0..streams {
        let _ = fs.remove_file(&dir.join(format!("step{n}-{i}.bin")));
    }
    for r in results {
        r?;
    }
    let bytes = per * streams as u64;
    #[allow(clippy::cast_precision_loss)]
    let bytes_per_s = bytes as f64 / seconds;
    Ok(Step {
        streams,
        chunk_size,
        bytes,
        seconds,
        bytes_per_s,
    })
}

/// Run the bench against the destination and write `state_dir/bench.json`.
///
/// Implements: SRS-PAR-002, SRS-GRD-001
///
/// # Errors
/// [`Error::Guard`] (nothing is written), [`Error::Config`] when the share
/// lacks the space for the test, I/O errors (the test folder is removed).
pub fn run(
    fs: &dyn Fs,
    guard: &Guard,
    state_dir: &Path,
    opts: &BenchOptions,
    min_free: u64,
    out: &mut dyn std::io::Write,
) -> Result<Report> {
    let mount = guard.check(fs)?;
    let free = fs.free_space(guard.dest())?;
    if free < opts.size.saturating_add(min_free) {
        return Err(Error::Config(format!(
            "the share has {} free; the bench needs {} plus min_free {}",
            Bytes(free),
            Bytes(opts.size),
            Bytes(min_free)
        )));
    }
    let dir: PathBuf = guard
        .dest()
        .join(format!(".ont-offload-bench-{}", std::process::id()));
    fs.create_dir_all(&dir)?;
    let result = measure(fs, guard, &dir, opts, out);
    let _ = std::fs::remove_dir(&dir);
    let (streams, chunks) = result?;
    let suggestion = suggest(&streams, &chunks);
    let report = Report {
        tool_version: env!("CARGO_PKG_VERSION").into(),
        host: journal::host_name(fs),
        measured_at: journal::now_rfc3339(),
        dest_root: guard.dest().to_string_lossy().into_owned(),
        mount: format!("{} {}", mount.fstype, mount.source),
        streams,
        chunks,
        suggestion,
    };
    let text = serde_json::to_string_pretty(&report).unwrap_or_default() + "\n";
    fs.create_dir_all(state_dir)?;
    write_atomic(fs, state_dir, BENCH_FILE, text.as_bytes())?;
    writeln!(
        out,
        "suggested settings (recorded in {}):",
        escape_control(&state_dir.join(BENCH_FILE).to_string_lossy())
    )?;
    writeln!(out, "  [bandwidth]")?;
    writeln!(out, "  workers = {}", report.suggestion.workers)?;
    writeln!(
        out,
        "  rate_limit = {:?}   # 80% of the best measured",
        report.suggestion.rate_limit
    )?;
    if let Some(c) = &report.suggestion.chunk_size {
        writeln!(out, "  [transfer]")?;
        writeln!(out, "  chunk_size = {c:?}")?;
    }
    Ok(report)
}

type Measured = (Vec<Step>, Vec<Step>);

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn measure(
    fs: &dyn Fs,
    guard: &Guard,
    dir: &Path,
    opts: &BenchOptions,
    out: &mut dyn std::io::Write,
) -> Result<Measured> {
    writeln!(
        out,
        "bench: {} per step to {}",
        Bytes(opts.size),
        escape_control(&guard.dest().to_string_lossy())
    )?;
    let mut streams = Vec::new();
    let mut n = 0;
    let mut k = 1;
    while k <= opts.max_streams.max(1) {
        guard.check(fs)?;
        let s = step(fs, dir, n, k, opts.size, opts.chunk_size)?;
        writeln!(out, "  {k:>2} stream(s): {}/s", Bytes(s.bytes_per_s as u64))?;
        streams.push(s);
        n += 1;
        k *= 2;
    }
    let mut chunks = Vec::new();
    if opts.chunks {
        let workers = suggest(&streams, &[]).workers;
        for c in CHUNK_SIZES {
            guard.check(fs)?;
            let s = step(fs, dir, n, workers, opts.size, c)?;
            writeln!(
                out,
                "  chunk {:>6} x {workers}: {}/s",
                mib(c),
                Bytes(s.bytes_per_s as u64)
            )?;
            chunks.push(s);
            n += 1;
        }
    }
    Ok((streams, chunks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(streams: usize, chunk: u64, mbs: f64) -> Step {
        Step {
            streams,
            chunk_size: chunk,
            bytes: 1,
            seconds: 1.0,
            bytes_per_s: mbs * 1e6,
        }
    }

    /// Verifies: SRS-PAR-002
    #[test]
    fn suggests_fewest_streams_near_the_best_and_80_percent_of_it() {
        let streams = [
            st(1, 8 << 20, 40.0),
            st(2, 8 << 20, 80.0),
            st(4, 8 << 20, 112.0),
            st(8, 8 << 20, 115.0),
            st(16, 8 << 20, 109.0),
        ];
        let chunks = [
            st(4, 1 << 20, 100.0),
            st(4, 4 << 20, 113.0),
            st(4, 16 << 20, 115.0),
        ];
        let s = suggest(&streams, &chunks);
        assert_eq!(s.workers, 4);
        assert_eq!(s.rate_limit, "92MB/s");
        assert_eq!(s.chunk_size.as_deref(), Some("4MiB"));
        // 10 GbE: three significant figures are cut to two.
        let s = suggest(&[st(8, 8 << 20, 1180.0)], &[]);
        assert_eq!(s.rate_limit, "940MB/s");
        assert_eq!(s.chunk_size, None);
        let s = suggest(&[st(8, 8 << 20, 12_400.0)], &[]);
        assert_eq!(s.rate_limit, "9900MB/s");
    }

    /// Verifies: SRS-PAR-002
    #[test]
    fn suggested_values_are_valid_configuration() {
        let s = suggest(&[st(1, 8 << 20, 0.2)], &[st(1, 1 << 20, 0.2)]);
        assert_eq!(s.rate_limit, "1MB/s");
        assert!(crate::config::units::parse_rate(&s.rate_limit).is_ok());
        assert!(crate::config::units::parse_size(s.chunk_size.as_deref().unwrap()).is_ok());
    }
}
