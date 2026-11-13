// SPDX-License-Identifier: AGPL-3.0-or-later
//! Compare a copied run folder with the simulator's manifest.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{Error, Result, io_err};

/// Files ont-offload itself adds at the top of a destination run folder.
pub const OFFLOAD_FILES: &[&str] = &["manifest.sha256", "run.json", "COMPLETE", "VERIFIED.json"];

/// Result of a comparison.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    /// Files in the manifest.
    pub expected: usize,
    /// Files present with the right content.
    pub matched: usize,
    /// Files in the manifest but not in the copy.
    pub missing: Vec<String>,
    /// Files whose content differs.
    pub changed: Vec<String>,
    /// Files in the copy but not in the manifest.
    pub extra: Vec<String>,
}

impl Report {
    /// True when the copy matches the manifest exactly.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty() && self.extra.is_empty()
    }
}

/// Read a `sha256sum` manifest into path -> hex digest.
///
/// # Errors
/// I/O or format errors.
pub fn read_manifest(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path).map_err(io_err("read", path))?;
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let Some((digest, file)) = line.split_once("  ") else {
            return Err(Error::Mismatch(format!(
                "{}:{}: not a sha256sum line",
                path.display(),
                n + 1
            )));
        };
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Mismatch(format!(
                "{}:{}: bad digest",
                path.display(),
                n + 1
            )));
        }
        out.insert(file.to_owned(), digest.to_ascii_lowercase());
    }
    Ok(out)
}

/// SHA-256 of a file as lowercase hex.
///
/// # Errors
/// I/O errors.
pub fn sha256_file(path: &Path) -> Result<String> {
    use std::fmt::Write as _;
    let mut f = File::open(path).map_err(io_err("open", path))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(io_err("read", path))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

fn walk(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(io_err("read folder", &dir))? {
            let path = entry.map_err(io_err("read folder", &dir))?.path();
            let meta = std::fs::symlink_metadata(&path).map_err(io_err("stat", &path))?;
            if meta.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(root) {
                files.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Compare `dest` (a copied run folder) with `manifest`.
///
/// `ignore` lists further relative paths to leave out of the "extra" list;
/// ont-offload's own files ([`OFFLOAD_FILES`]) are always ignored.
///
/// # Errors
/// I/O or manifest errors (a mismatch is reported in the [`Report`]).
pub fn check(dest: &Path, manifest: &Path, ignore: &[String]) -> Result<Report> {
    let expected = read_manifest(manifest)?;
    let mut report = Report {
        expected: expected.len(),
        ..Report::default()
    };
    for (rel, digest) in &expected {
        let p = dest.join(rel);
        if !p.is_file() {
            report.missing.push(rel.clone());
        } else if sha256_file(&p)? == *digest {
            report.matched += 1;
        } else {
            report.changed.push(rel.clone());
        }
    }
    for rel in walk(dest)? {
        if !expected.contains_key(&rel)
            && !OFFLOAD_FILES.contains(&rel.as_str())
            && !ignore.iter().any(|i| *i == rel)
        {
            report.extra.push(rel);
        }
    }
    Ok(report)
}
