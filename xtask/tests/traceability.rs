// SPDX-License-Identifier: AGPL-3.0-or-later
//! Traceability on every `cargo test`, without the medtrace binary: the
//! built-in check of `cargo xtask trace` run against this repository.
#![allow(clippy::expect_used, missing_docs, dead_code)]

#[path = "../src/trace.rs"]
mod trace;

use std::path::Path;

#[test]
fn repository_is_traceable_for_the_milestone() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let rep = trace::check(root, trace::MILESTONE, None);
    for w in &rep.warnings {
        eprintln!("warning: {w}");
    }
    assert!(
        rep.ok(),
        "{} traceability error(s):\n{}",
        rep.errors.len(),
        rep.errors.join("\n")
    );
}
