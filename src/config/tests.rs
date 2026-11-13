// SPDX-License-Identifier: AGPL-3.0-or-later
//! Unit tests of configuration loading and validation.

use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::fsio::RealFs;
use crate::fsio::faulty::{Fault, FaultyFs, Op, Rule};

/// Quote a string as a TOML basic string.
fn q(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A source, destination and state folder in a temp dir. The destination is
/// reported on its own device, as a mounted share would be.
struct Env {
    dir: tempfile::TempDir,
    fs: FaultyFs,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["data", "mnt/share", "state"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        let fs = FaultyFs::new(Arc::new(RealFs));
        fs.add(Rule::always(
            Op::Metadata,
            "mnt/share",
            Fault::Device(9_999_999),
        ));
        Self { dir, fs }
    }

    fn p(&self, rel: &str) -> String {
        self.dir.path().join(rel).to_str().unwrap().to_owned()
    }

    /// A minimal valid configuration, with `extra` appended.
    fn toml(&self, extra: &str) -> String {
        format!(
            "dest_root = {}\ntarget_id = \"lab-nas-01\"\nstate_dir = {}\n{extra}\n[source]\nroot = {}\n",
            q(&self.p("mnt/share")),
            q(&self.p("state")),
            q(&self.p("data")),
        )
    }

    fn check(&self, text: &str) -> std::result::Result<Loaded, Vec<String>> {
        let raw = parse(text).map_err(|e| vec![e.to_string()])?;
        validate(&raw, &self.fs)
    }

    fn problems(&self, text: &str) -> String {
        match self.check(text) {
            Ok(_) => panic!("expected problems for:\n{text}"),
            Err(p) => p.join("\n"),
        }
    }
}

/// Verifies: SRS-CFG-003
#[test]
fn example_config_parses_strictly() {
    let text = include_str!("../../packaging/ont-offload.example.toml");
    let raw = parse(text).unwrap();
    assert_eq!(raw.bandwidth.workers, 4);
    assert_eq!(raw.bandwidth.rate_limit, "100MB/s");
    assert_eq!(raw.source.root, "/data");
}

/// Verifies: SRS-CFG-001, SRS-CFG-002, SRS-CFG-004
#[test]
fn minimal_config_is_valid_with_defaults() {
    let env = Env::new();
    let loaded = env.check(&env.toml("")).unwrap();
    let c = &loaded.config;
    assert_eq!(c.bandwidth.workers, 4);
    assert_eq!(c.bandwidth.rate_limit, Rate::Unlimited);
    assert_eq!(c.transfer.chunk_size, 8 << 20);
    assert_eq!(c.transfer.verify, VerifyMode::Inflight);
    assert_eq!(c.transfer.file_mode, 0o640);
    assert_eq!(c.cleanup.delete, DeleteMode::Off);
    assert_eq!(c.source.run_markers, vec!["sample_sheet_*.csv"]);
    assert!(loaded.warnings.iter().any(|w| w.contains("unlimited")));
    let summary = c.summary();
    assert!(summary.contains("4 workers, total unlimited"), "{summary}");
}

/// Verifies: SRS-CFG-003
#[test]
fn unknown_keys_are_rejected() {
    let env = Env::new();
    for extra in [
        "bwlimit = \"1MB/s\"",
        "[bandwidth]\nworkers = 2\nrate = \"10MB/s\"",
        "[transfer]\nverfy = \"inflight\"",
    ] {
        let msg = env.problems(&env.toml(extra));
        assert!(msg.contains("unknown field"), "{extra}: {msg}");
    }
}

/// Verifies: SRS-CFG-003
#[test]
fn invalid_values_are_all_reported() {
    let env = Env::new();
    let text = env.toml(
        "[bandwidth]\nworkers = 0\nrate_limit = \"fast\"\n\
         [transfer]\nchunk_size = \"8M\"\nsettle = \"soon\"\nmin_free = \"-5GB\"\nfile_mode = \"0400\"\nmax_attempts = 0\n\
         [outage]\nwarn_after = \"2h\"\nalert_after = \"1h\"\n\
         [guard]\nallowed_fstypes = [\"NFS!\"]",
    );
    let msg = env.problems(&text);
    for key in [
        "bandwidth.workers",
        "bandwidth.rate_limit",
        "transfer.chunk_size",
        "transfer.settle",
        "transfer.min_free",
        "transfer.file_mode",
        "transfer.max_attempts",
        "outage.warn_after",
        "guard.allowed_fstypes[0]",
    ] {
        assert!(msg.contains(key), "missing {key} in:\n{msg}");
    }
}

/// Verifies: SRS-CFG-003
#[test]
fn invalid_enum_value_is_rejected() {
    let env = Env::new();
    let msg = env.problems(&env.toml("[transfer]\nverify = \"maybe\""));
    assert!(msg.contains("unknown variant"), "{msg}");
}

/// Verifies: SRS-CFG-001
#[test]
fn bad_path_values_are_refused_with_key_and_escaped_value() {
    let env = Env::new();
    let good_dest = env.p("mnt/share");
    let cases: Vec<(String, &str)> = vec![
        (String::new(), "is empty"),
        ("relative/path".into(), "absolute"),
        (format!("{good_dest}\u{7}"), "control characters"),
        (format!(" {good_dest}"), "whitespace"),
        (format!("{good_dest}/../share"), "\"..\""),
        (env.p("does-not-exist"), "does not exist"),
    ];
    for (value, want) in cases {
        let text = format!(
            "dest_root = {}\ntarget_id = \"x\"\nstate_dir = {}\n[source]\nroot = {}\n",
            q(&value),
            q(&env.p("state")),
            q(&env.p("data"))
        );
        let msg = env.problems(&text);
        assert!(
            msg.contains("dest_root") && msg.contains(want),
            "{value:?}: {msg}"
        );
        assert!(!msg.contains('\u{7}'), "BEL must be escaped: {msg}");
    }
}

/// Verifies: SRS-CFG-001
#[test]
fn file_instead_of_directory_is_refused() {
    let env = Env::new();
    std::fs::write(env.dir.path().join("afile"), b"x").unwrap();
    let text = env.toml("").replace(&env.p("state"), &env.p("afile"));
    assert!(env.problems(&text).contains("not a directory"));
}

/// Verifies: SRS-CFG-001
#[cfg(unix)]
#[test]
fn symlink_resolving_to_control_characters_is_refused() {
    // P2I-00569: a symlink to the mount introduced a BEL byte into the path.
    let env = Env::new();
    let bad_target = env.dir.path().join("mnt/share\u{7}");
    std::fs::create_dir_all(&bad_target).unwrap();
    let link = env.dir.path().join("nas");
    std::os::unix::fs::symlink(&bad_target, &link).unwrap();
    let text = env
        .toml("")
        .replace(&env.p("mnt/share"), link.to_str().unwrap());
    let msg = env.problems(&text);
    assert!(
        msg.contains("resolves through a symlink") && msg.contains("\\u{7}"),
        "{msg}"
    );
}

/// Verifies: SRS-CFG-001
#[cfg(unix)]
#[test]
fn clean_symlink_is_accepted_with_a_warning() {
    let env = Env::new();
    let link = env.dir.path().join("nas");
    std::os::unix::fs::symlink(env.dir.path().join("mnt/share"), &link).unwrap();
    let text = env
        .toml("")
        .replace(&env.p("mnt/share"), link.to_str().unwrap());
    let loaded = env.check(&text).unwrap();
    assert!(loaded.config.dest_root.resolved.ends_with("mnt/share"));
    assert!(loaded.warnings.iter().any(|w| w.contains("(symlink)")));
}

/// Verifies: SRS-CFG-002
#[test]
fn destination_inside_source_is_refused() {
    let env = Env::new();
    std::fs::create_dir_all(env.dir.path().join("data/mnt/share")).unwrap();
    let text = env
        .toml("")
        .replace(&env.p("mnt/share"), &env.p("data/mnt/share"));
    assert!(env.problems(&text).contains("contain each other"));
}

/// Verifies: SRS-CFG-002
#[cfg(unix)]
#[test]
fn symlink_into_source_is_refused() {
    let env = Env::new();
    let link = env.dir.path().join("mnt/share/loop");
    std::os::unix::fs::symlink(env.dir.path().join("data"), &link).unwrap();
    let text = env
        .toml("")
        .replace(&env.p("mnt/share"), link.to_str().unwrap());
    assert!(env.problems(&text).contains("contain each other"));
}

/// Verifies: SRS-CFG-002
#[test]
fn unmounted_destination_on_same_device_is_refused() {
    let env = Env::new();
    env.fs.clear();
    assert!(env.problems(&env.toml("")).contains("same device"));
}

/// Verifies: SRS-CFG-002
#[test]
fn state_dir_inside_destination_is_refused() {
    let env = Env::new();
    std::fs::create_dir_all(env.dir.path().join("mnt/share/state")).unwrap();
    let text = env
        .toml("")
        .replace(&env.p("state"), &env.p("mnt/share/state"));
    assert!(env.problems(&text).contains("state_dir"));
}

/// Verifies: SRS-RATE-002
#[test]
fn rate_limit_is_checked_against_network_speed() {
    let env = Env::new();
    let ok = env
        .check(&env.toml(
            "[bandwidth]\nworkers = 4\nrate_limit = \"100MB/s\"\nnetwork_speed = \"1Gbit/s\"",
        ))
        .unwrap();
    assert!(ok.warnings.is_empty(), "{:?}", ok.warnings);
    assert!(ok.config.summary().contains("80% of the 1 Gbit/s link"));

    let near = env
        .check(&env.toml("[bandwidth]\nrate_limit = \"120MB/s\"\nnetwork_speed = \"1Gbit/s\""))
        .unwrap();
    assert!(
        near.warnings.iter().any(|w| w.contains("over 90%")),
        "{:?}",
        near.warnings
    );

    let msg = env.problems(
        &env.toml("[bandwidth]\nrate_limit = \"1Gbit/s\"\nnetwork_speed = \"800Mbit/s\""),
    );
    assert!(msg.contains("more than network_speed"), "{msg}");
}

/// Verifies: SRS-CFG-003
#[test]
fn readback_gate_requires_readback_verification() {
    let env = Env::new();
    let msg = env.problems(&env.toml("[cleanup]\ngate = \"readback\""));
    assert!(msg.contains("cleanup.gate"));
    let ok = env.check(&env.toml(
        "[transfer]\nverify = \"readback\"\n[cleanup]\ngate = \"both\"\ndelete = \"dry-run\"",
    ));
    assert!(ok.unwrap().warnings.iter().any(|w| w.contains("dry-run")));
}

/// Verifies: SRS-CFG-003
#[test]
fn patterns_are_validated() {
    let env = Env::new();
    let msg = env.problems(&env.toml("[selection]\ninclude = [\"pod5/[\"]"));
    assert!(msg.contains("selection.include[0]"), "{msg}");
    let env = Env::new();
    let text = env.toml("").replace(
        "[source]\n",
        "[source]\nrun_markers = [\"run/sample_sheet_*.csv\"]\n",
    );
    assert!(env.problems(&text).contains("source.run_markers[0]"));
}

/// Verifies: SRS-CFG-001
#[test]
fn target_id_must_be_set() {
    let env = Env::new();
    let text = env.toml("").replace("lab-nas-01", "CHANGE-ME");
    assert!(env.problems(&text).contains("target_id"));
    let text = env.toml("").replace("lab-nas-01", "nas\\u0007");
    assert!(env.problems(&text).contains("target_id"));
}

/// Verifies: SRS-CFG-004
#[test]
fn load_reports_file_and_count() {
    let env = Env::new();
    let path = env.dir.path().join("config.toml");
    std::fs::write(
        &path,
        env.toml("[bandwidth]\nworkers = 99\nrate_limit = \"x\""),
    )
    .unwrap();
    let err = load(&path, &env.fs).unwrap_err().to_string();
    assert!(
        err.contains("2 problem(s)") && err.contains("config.toml"),
        "{err}"
    );
    std::fs::write(&path, env.toml("")).unwrap();
    assert!(load(&path, &env.fs).is_ok());
    assert!(
        load(Path::new("/nonexistent/config.toml"), &env.fs)
            .unwrap_err()
            .to_string()
            .contains("cannot read")
    );
}
