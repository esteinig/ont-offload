// SPDX-License-Identifier: AGPL-3.0-or-later
//! Built-in traceability check, standard library only.
//!
//! Does what `medtrace check` and `medtrace trace matrix` do for this
//! repository, so traceability is enforced on every `cargo test` even where
//! the medtrace binary is not installed:
//!
//! - every `#[test]` in `src/` and `tests/` carries a `Verifies:` tag;
//! - every `Implements:` and `Verifies:` tag names a requirement in
//!   `compliance/requirements/`;
//! - every requirement is allocated to an existing software item, and every
//!   software item's paths exist;
//! - every requirement due in or before [`MILESTONE`] is implemented and
//!   verified (and, when test results are given, its tests passed);
//! - hazards and requirements link to each other both ways.
//!
//! Used by `cargo xtask trace` (when medtrace is absent) and by
//! `xtask/tests/traceability.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The release under development. Requirements with this release or an
/// earlier one must be implemented and verified.
pub const MILESTONE: &str = "v0.2";

/// Folders scanned for tags (as in `medtrace.toml`).
pub const SCAN: &[&str] = &["src", "tests"];

/// One register item, flattened to `key` / `section.key` -> values.
#[derive(Debug, Clone, Default)]
pub struct Item {
    /// Item id.
    pub id: String,
    /// The file it came from.
    pub file: PathBuf,
    /// Keys to values (a scalar is a one-element list).
    pub fields: BTreeMap<String, Vec<String>>,
}

impl Item {
    /// First value of a key, or "".
    #[must_use]
    pub fn get(&self, key: &str) -> &str {
        self.fields
            .get(key)
            .and_then(|v| v.first())
            .map_or("", String::as_str)
    }

    /// All values of a key.
    #[must_use]
    pub fn list(&self, key: &str) -> &[String] {
        self.fields.get(key).map_or(&[], Vec::as_slice)
    }
}

/// Values in a TOML right-hand side: a quoted string or an inline array of
/// quoted strings. Other values are kept as they are written.
fn values(rhs: &str) -> Vec<String> {
    let rhs = rhs.trim();
    if !rhs.starts_with('"') && !rhs.starts_with('[') {
        return vec![rhs.to_owned()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut escaped = false;
    for c in rhs.chars() {
        if in_str {
            if escaped {
                cur.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                out.push(std::mem::take(&mut cur));
                in_str = false;
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_str = true;
        } else if c == '#' {
            break;
        }
    }
    out
}

/// Parse the subset of TOML the register uses: `key = "..."`,
/// `key = ["...", ...]` on one line, `[section]` headers, and `"""` blocks
/// (skipped).
#[must_use]
pub fn parse_item(text: &str, file: &Path) -> Item {
    let mut item = Item {
        file: file.to_owned(),
        ..Item::default()
    };
    let mut section = String::new();
    let mut in_block = false;
    for line in text.lines() {
        let t = line.trim();
        if in_block {
            if t.contains("\"\"\"") {
                in_block = false;
            }
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with('[') && !t.contains('=') {
            t.trim_matches(|c| c == '[' || c == ']')
                .trim()
                .clone_into(&mut section);
            continue;
        }
        let Some((k, v)) = t.split_once('=') else {
            continue;
        };
        let v = v.trim();
        if v.starts_with("\"\"\"") {
            in_block = v.matches("\"\"\"").count() == 1;
            continue;
        }
        let key = if section.is_empty() {
            k.trim().to_owned()
        } else {
            format!("{section}.{}", k.trim())
        };
        item.fields.insert(key, values(v));
    }
    let id = item.get("id").to_owned();
    item.id = id;
    item
}

fn read_items(dir: &Path, errors: &mut Vec<String>) -> BTreeMap<String, Item> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        errors.push(format!(
            "{}: cannot read the register folder",
            dir.display()
        ));
        return out;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    for f in files {
        match std::fs::read_to_string(&f) {
            Ok(text) => {
                let item = parse_item(&text, &f);
                let stem = f.file_stem().map(|s| s.to_string_lossy().into_owned());
                if item.id.is_empty() {
                    errors.push(format!("{}: no id", f.display()));
                } else if stem.as_deref() != Some(item.id.as_str()) {
                    errors.push(format!(
                        "{}: id {} does not match the file name",
                        f.display(),
                        item.id
                    ));
                }
                if out.insert(item.id.clone(), item).is_some() {
                    errors.push(format!("{}: duplicate id", f.display()));
                }
            }
            Err(e) => errors.push(format!("{}: {e}", f.display())),
        }
    }
    out
}

/// A tag found in the source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tag {
    /// Requirement id.
    pub id: String,
    /// File, relative to the root, with `/` separators.
    pub file: String,
    /// 1-based line.
    pub line: usize,
    /// Test function name for `Verifies:` on a test.
    pub test: Option<String>,
}

/// Tags and tests found in the source.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    /// `Implements:` tags.
    pub implements: Vec<Tag>,
    /// `Verifies:` tags on tests.
    pub verifies: Vec<Tag>,
    /// Tests without a `Verifies:` tag, as `file:line name`.
    pub untagged: Vec<String>,
    /// Number of tests.
    pub tests: usize,
}

fn ids_after(line: &str, tag: &str) -> Option<Vec<String>> {
    let rest = &line[line.find(tag)? + tag.len()..];
    Some(
        rest.split(|c: char| c == ',' || c.is_whitespace())
            .map(|s| s.trim_end_matches(['.', ';']))
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

fn is_doc(t: &str) -> bool {
    t.starts_with("///") || t.starts_with("//!")
}

/// Scan one Rust file.
pub fn scan_file(text: &str, file: &str, scan: &mut Scan) {
    let lines: Vec<&str> = text.lines().collect();
    for (i, raw) in lines.iter().enumerate() {
        let t = raw.trim();
        if is_doc(t) {
            if let Some(ids) = ids_after(t, "Implements:") {
                for id in ids {
                    scan.implements.push(Tag {
                        id,
                        file: file.to_owned(),
                        line: i + 1,
                        test: None,
                    });
                }
            }
        }
        if t != "#[test]" && !t.starts_with("#[test]") {
            continue;
        }
        scan.tests += 1;
        // The test's name: the first `fn` after the attribute.
        let name = lines[i + 1..]
            .iter()
            .find_map(|l| {
                let l = l.trim();
                let at = l.find("fn ")?;
                let rest = &l[at + 3..];
                Some(
                    rest[..rest.find(['(', '<']).unwrap_or(rest.len())]
                        .trim()
                        .to_owned(),
                )
            })
            .unwrap_or_default();
        // Tags in the doc comments and attributes directly above.
        let mut ids = Vec::new();
        let mut tag_line = i + 1;
        for (j, l) in lines[..i].iter().enumerate().rev() {
            let l = l.trim();
            if !(l.starts_with("///") || l.starts_with("#[") || l.starts_with("//")) {
                break;
            }
            if let Some(found) = ids_after(l, "Verifies:") {
                ids.extend(found);
                tag_line = j + 1;
            }
        }
        if ids.is_empty() {
            scan.untagged.push(format!("{file}:{} {name}", i + 1));
        }
        for id in ids {
            scan.verifies.push(Tag {
                id,
                file: file.to_owned(),
                line: tag_line,
                test: Some(name.clone()),
            });
        }
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Scan the [`SCAN`] folders below `root`.
#[must_use]
pub fn scan(root: &Path) -> Scan {
    let mut files = Vec::new();
    for d in SCAN {
        rust_files(&root.join(d), &mut files);
    }
    files.sort();
    let mut s = Scan::default();
    for f in files {
        let rel = f
            .strip_prefix(root)
            .unwrap_or(&f)
            .to_string_lossy()
            .replace('\\', "/");
        if let Ok(text) = std::fs::read_to_string(&f) {
            scan_file(&text, &rel, &mut s);
        }
    }
    s
}

/// Test outcomes from `cargo test` output: last path segment of the test
/// name -> passed (false if any test of that name failed).
#[must_use]
pub fn parse_test_output(text: &str) -> BTreeMap<String, bool> {
    let mut out: BTreeMap<String, bool> = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name, result)) = rest.rsplit_once(" ... ") else {
            continue;
        };
        let passed = match result.trim() {
            "ok" => true,
            "FAILED" => false,
            _ => continue,
        };
        let short = name.rsplit("::").next().unwrap_or(name).trim().to_owned();
        let e = out.entry(short).or_insert(true);
        *e = *e && passed;
    }
    out
}

/// `vX.Y[.Z]` as numbers; unknown releases sort last.
#[must_use]
pub fn release_key(r: &str) -> (u32, u32, u32) {
    let mut it = r
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse::<u32>().unwrap_or(u32::MAX));
    (
        it.next().unwrap_or(u32::MAX),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Result of [`check`].
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Problems that fail the check.
    pub errors: Vec<String>,
    /// Notes that do not.
    pub warnings: Vec<String>,
    /// Markdown traceability matrix.
    pub matrix: String,
    /// Requirements, software items, hazards, tests, tags.
    pub counts: (usize, usize, usize, usize, usize),
}

impl Report {
    /// No errors.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Run every check below `root`. With `results`, requirements due by the
/// milestone also need at least one passing test and no failing ones.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn check(root: &Path, milestone: &str, results: Option<&BTreeMap<String, bool>>) -> Report {
    let mut rep = Report::default();
    let register = root.join("compliance");
    let reqs = read_items(&register.join("requirements"), &mut rep.errors);
    let swis = read_items(&register.join("architecture"), &mut rep.errors);
    let hazs = read_items(&register.join("risk"), &mut rep.errors);
    let s = scan(root);
    rep.counts = (
        reqs.len(),
        swis.len(),
        hazs.len(),
        s.tests,
        s.implements.len() + s.verifies.len(),
    );

    for u in &s.untagged {
        rep.errors.push(format!("{u}: test has no `Verifies:` tag"));
    }
    for t in s.implements.iter().chain(&s.verifies) {
        if !reqs.contains_key(&t.id) {
            rep.errors.push(format!(
                "{}:{}: unknown requirement {}",
                t.file, t.line, t.id
            ));
        }
    }
    for w in &swis {
        for p in w.1.list("paths") {
            if !root.join(p).exists()
                && release_key(milestone) >= release_key(&first_release(w.0, &reqs))
            {
                rep.errors.push(format!("{}: path {p} does not exist", w.0));
            }
        }
    }

    let due = release_key(milestone);
    let mut impl_by: BTreeMap<&str, Vec<&Tag>> = BTreeMap::new();
    for t in &s.implements {
        impl_by.entry(t.id.as_str()).or_default().push(t);
    }
    let mut ver_by: BTreeMap<&str, Vec<&Tag>> = BTreeMap::new();
    for t in &s.verifies {
        ver_by.entry(t.id.as_str()).or_default().push(t);
    }

    let mut m = String::new();
    let _ = writeln!(
        m,
        "# Traceability matrix\n\nMilestone {milestone}. Generated by `cargo xtask trace` (built-in check).\n\n\
         | Requirement | Release | Software items | Hazards | Implemented in | Verified by | Result |\n\
         |---|---|---|---|---|---|---|"
    );
    for (id, r) in &reqs {
        let release = r.get("release");
        let is_due = release_key(release) <= due;
        let alloc = r.list("links.allocated_to");
        if alloc.is_empty() {
            rep.errors.push(format!(
                "{}: {id} is not allocated to a software item",
                r.file.display()
            ));
        }
        for a in alloc {
            if !swis.contains_key(a) {
                rep.errors
                    .push(format!("{id}: allocated to unknown software item {a}"));
            }
        }
        for h in r.list("links.mitigates") {
            match hazs.get(h) {
                None => rep
                    .errors
                    .push(format!("{id}: mitigates unknown hazard {h}")),
                Some(hz) if !hz.list("links.controlled_by").contains(id) => rep.warnings.push(
                    format!("{id} mitigates {h}, but {h} does not list it in controlled_by"),
                ),
                Some(_) => {}
            }
        }
        let imps = impl_by.get(id.as_str()).map_or(&[][..], Vec::as_slice);
        let vers = ver_by.get(id.as_str()).map_or(&[][..], Vec::as_slice);
        // Implementation outside the allocated items is worth a look.
        for t in imps {
            let inside = alloc.iter().filter_map(|a| swis.get(a)).any(|w| {
                w.list("paths")
                    .iter()
                    .any(|p| t.file == *p || (p.ends_with('/') && t.file.starts_with(p.as_str())))
            });
            if !inside {
                rep.warnings.push(format!(
                    "{id}: implemented in {}:{} outside its software items ({})",
                    t.file,
                    t.line,
                    alloc.join(", ")
                ));
            }
        }
        let mut result = "not run".to_owned();
        if let Some(res) = results {
            let outcomes: Vec<Option<bool>> = vers
                .iter()
                .map(|t| t.test.as_ref().and_then(|n| res.get(n).copied()))
                .collect();
            let passed = outcomes.iter().filter(|o| **o == Some(true)).count();
            let failed = outcomes.iter().filter(|o| **o == Some(false)).count();
            result = if failed > 0 {
                format!("{failed} FAILED")
            } else if passed > 0 {
                format!("{passed} passed")
            } else {
                "no result".to_owned()
            };
            if is_due && (failed > 0 || passed == 0) {
                rep.errors.push(format!("{id}: verifying tests {result}"));
            }
        }
        if is_due {
            if imps.is_empty() {
                rep.errors
                    .push(format!("{id} ({release}): no `Implements:` tag"));
            }
            if vers.is_empty() {
                rep.errors
                    .push(format!("{id} ({release}): no test `Verifies:` it"));
            }
        }
        let list = |tags: &[&Tag], with_test: bool| -> String {
            let v: BTreeSet<String> = tags
                .iter()
                .map(|t| match (&t.test, with_test) {
                    (Some(n), true) => format!("`{n}`"),
                    _ => format!("{}:{}", t.file, t.line),
                })
                .collect();
            if v.is_empty() {
                "-".to_owned()
            } else {
                v.into_iter().collect::<Vec<_>>().join("<br>")
            }
        };
        let _ = writeln!(
            m,
            "| {id} {} | {release}{} | {} | {} | {} | {} | {result} |",
            r.get("title"),
            if is_due { "" } else { " (later)" },
            alloc.join(", "),
            r.list("links.mitigates").join(", "),
            list(imps, false),
            list(vers, true),
        );
    }

    let _ = writeln!(
        m,
        "\n## Hazards\n\n| Hazard | Severity | Before | After | Controlled by |\n|---|---|---|---|---|"
    );
    for (id, h) in &hazs {
        let ctl = h.list("links.controlled_by");
        if ctl.is_empty() {
            rep.errors
                .push(format!("{id}: no risk control (controlled_by is empty)"));
        }
        for c in ctl {
            match reqs.get(c) {
                None => rep
                    .errors
                    .push(format!("{id}: controlled by unknown requirement {c}")),
                Some(r) if !r.list("links.mitigates").contains(id) => rep.warnings.push(format!(
                    "{id} is controlled by {c}, but {c} does not list it in mitigates"
                )),
                Some(_) => {}
            }
        }
        let _ = writeln!(
            m,
            "| {id} {} | {} | {} | {} | {} |",
            h.get("title"),
            h.get("severity"),
            h.get("probability_before"),
            if h.get("probability_after").is_empty() {
                "-"
            } else {
                h.get("probability_after")
            },
            ctl.join(", ")
        );
    }
    rep.matrix = m;
    rep
}

/// The earliest release of a requirement allocated to software item `swi`
/// (a software item's paths must exist once its first requirement is due).
fn first_release(swi: &str, reqs: &BTreeMap<String, Item>) -> String {
    reqs.values()
        .filter(|r| r.list("links.allocated_to").iter().any(|a| a == swi))
        .map(|r| r.get("release").to_owned())
        .min_by_key(|r| release_key(r))
        .unwrap_or_else(|| "v999".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_register_toml() {
        let item = parse_item(
            "id = \"SRS-X-001\"\nrelease = \"v0.2\" # note\ntext = \"\"\"\nid = \"nope\"\n\"\"\"\n\
             [links]\nallocated_to = [\"SWI-A\", \"SWI-B\"]\n",
            Path::new("x.toml"),
        );
        assert_eq!(item.id, "SRS-X-001");
        assert_eq!(item.get("release"), "v0.2");
        assert_eq!(item.list("links.allocated_to"), ["SWI-A", "SWI-B"]);
    }

    #[test]
    fn finds_tests_and_tags() {
        let src = "/// Implements: SRS-A-001, SRS-A-002\nfn f() {}\n\n\
                   /// Verifies: SRS-A-001\n#[test]\nfn t1() {}\n\n#[test]\nfn t2() {}\n\n\
                   /// Verifies: SRS-A-002\n///\n/// More text.\n#[cfg(unix)]\n#[test]\nfn t3() {}\n";
        let mut s = Scan::default();
        scan_file(src, "src/a.rs", &mut s);
        assert_eq!(s.tests, 3);
        assert_eq!(s.implements.len(), 2);
        assert_eq!(s.untagged, ["src/a.rs:8 t2"]);
        let v: Vec<_> = s
            .verifies
            .iter()
            .map(|t| (t.id.as_str(), t.test.as_deref()))
            .collect();
        assert_eq!(v, [("SRS-A-001", Some("t1")), ("SRS-A-002", Some("t3"))]);
    }

    #[test]
    fn parses_cargo_test_output() {
        let r = parse_test_output(
            "test run::tests::a ... ok\ntest b ... FAILED\ntest x::b ... ok\ntest c ... ignored\n",
        );
        assert_eq!(r.get("a"), Some(&true));
        assert_eq!(r.get("b"), Some(&false));
        assert!(!r.contains_key("c"));
    }

    #[test]
    fn orders_releases() {
        assert!(release_key("v0.2") < release_key("v0.10"));
        assert!(release_key("v1.0") > release_key("v0.9"));
        assert!(release_key("") > release_key("v9.9"));
    }
}
