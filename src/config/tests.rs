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
    assert_eq!(raw.source[0].root, "/data");
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
    assert_eq!(c.sources[0].run_markers, vec!["sample_sheet_*.csv"]);
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

/// Verifies: SRS-TUI-008
#[test]
fn tui_section_is_validated_and_read_without_path_checks() {
    let env = Env::new();
    let ok = env.toml("[tui]\ntheme = \"synthwave\"\nrefresh = \"2s\"\nbell = false");
    let p = env.dir.path().join("ok.toml");
    std::fs::write(&p, &ok).unwrap();
    let t = tui_settings(&p, &env.fs).unwrap();
    assert_eq!(t.theme, TuiTheme::Synthwave);
    assert_eq!(t.refresh, Duration::from_secs(2));
    assert!(!t.bell);
    assert!(
        env.problems(&env.toml("[tui]\nrefresh = \"50ms\""))
            .contains("tui.refresh")
    );
    assert!(parse("dest_root = \"/x\"\ntarget_id = \"t\"\nstate_dir = \"/s\"\n[source]\nroot = \"/d\"\n[tui]\ntheme = \"neon\"\n").is_err());
    for (text, ok) in [
        ("200ms", true),
        ("60s", true),
        ("199ms", false),
        ("61s", false),
        ("x", false),
    ] {
        assert_eq!(tui_refresh(text).is_ok(), ok, "{text}");
    }
    // The dashboard starts on a configuration whose paths are gone.
    let gone = "dest_root = \"/absent/share\"\ntarget_id = \"t\"\nstate_dir = \"/absent/state\"\n[source]\nroot = \"/absent/data\"\n";
    let p = env.dir.path().join("gone.toml");
    std::fs::write(&p, gone).unwrap();
    assert_eq!(
        tui_settings(&p, &env.fs).unwrap().state_dir,
        PathBuf::from("/absent/state")
    );
    assert!(
        load(&p, &env.fs).is_err(),
        "while the daemon would refuse it"
    );
}

impl Env {
    /// Two sources, MinKNOW runs in `data` and user datasets in `transfer`,
    /// with `extra` lines for the second `[[source]]`.
    fn two_sources(&self, first: &str, second: &str) -> String {
        std::fs::create_dir_all(self.dir.path().join("transfer")).unwrap();
        format!(
            "dest_root = {}\ntarget_id = \"lab-nas-01\"\nstate_dir = {}\n\
             [[source]]\nroot = {}\n{first}\n\
             [[source]]\nroot = {}\nlayout = \"ready\"\n{second}\n",
            q(&self.p("mnt/share")),
            q(&self.p("state")),
            q(&self.p("data")),
            q(&self.p("transfer")),
        )
    }
}

/// Verifies: SRS-SRC-001, SRS-SRC-002, SRS-SRC-004
#[test]
fn several_sources_load_with_their_own_settings_and_ready_defaults() {
    let env = Env::new();
    let c = env
        .check(&env.two_sources("dest_starts_at = \"sample\"", "dest = \"transfer\""))
        .unwrap()
        .config;
    assert_eq!(c.sources.len(), 2);
    assert!(c.several());
    let (runs, drops) = (&c.sources[0], &c.sources[1]);
    assert_eq!(runs.name, "data");
    assert_eq!(runs.layout, Layout::Minknow);
    assert_eq!(runs.run_markers, vec!["sample_sheet_*.csv"]);
    assert_eq!(runs.dest_starts_at, DestLevel::Sample);
    assert!(runs.cleanup);
    assert_eq!(drops.name, "transfer");
    assert_eq!(drops.run_markers, vec![READY_MARKER]);
    assert_eq!(drops.complete_markers, vec![READY_MARKER]);
    assert!(
        drops.nested_markers().is_empty(),
        "READY below a dataset is part of it"
    );
    assert_eq!(drops.dest, Path::new("transfer"));
    assert_eq!(drops.dest_starts_at, DestLevel::Experiment);
    assert!(!drops.cleanup, "user datasets are never deleted by default");
    let summary = c.summary();
    assert!(summary.contains("source[2] root"), "{summary}");
    assert!(
        summary.contains("dest_root/transfer/<path below the root>/"),
        "{summary}"
    );
    assert!(summary.contains("never deletes its runs"), "{summary}");

    // A single [source] is still one source, named after its folder.
    let one = env.check(&env.toml("")).unwrap().config;
    assert_eq!(one.sources.len(), 1);
    assert!(!one.several());

    // Unknown keys in a [[source]] name the entry.
    let msg = env.problems(&env.two_sources("", "dest = \"t\"\nmarker = \"READY\""));
    assert!(msg.contains("source[2]") && msg.contains("marker"), "{msg}");

    // Turning cleanup on for user datasets is allowed, with a warning.
    let w = env
        .check(&env.two_sources("", "dest = \"t\"\ncleanup = true"))
        .unwrap()
        .warnings;
    assert!(
        w.iter().any(|w| w.starts_with("source[2].cleanup")),
        "{w:?}"
    );
}

/// Verifies: SRS-SRC-003
#[test]
fn sources_must_not_share_names_roots_or_destination_folders() {
    let env = Env::new();
    // Both into dest_root itself.
    let msg = env.problems(&env.two_sources("", ""));
    assert!(
        msg.contains("source[2].dest") && msg.contains("overlaps"),
        "{msg}"
    );
    // Nested destination folders.
    let msg = env.problems(&env.two_sources("dest = \"runs\"", "dest = \"runs/users\""));
    assert!(msg.contains("source[2].dest"), "{msg}");
    // Not a plain relative folder.
    for bad in ["../elsewhere", "/abs", "a/../b"] {
        let msg = env.problems(&env.two_sources("", &format!("dest = {}", q(bad))));
        assert!(msg.contains("source[2].dest"), "{bad}: {msg}");
    }
    // The same name twice, and the same root twice.
    let msg = env.problems(&env.two_sources("name = \"x\"", "name = \"x\"\ndest = \"t\""));
    assert!(msg.contains("source[2].name"), "{msg}");
    let same = env
        .two_sources("", "dest = \"t\"")
        .replace(&q(&env.p("transfer")), &q(&env.p("data")));
    let msg = env.problems(&same.replace(
        "layout = \"ready\"\n",
        "name = \"again\"\nlayout = \"ready\"\n",
    ));
    assert!(
        msg.contains("source[2].root") && msg.contains("already the root"),
        "{msg}"
    );
    // A name that is not a plain word.
    let msg = env.problems(&env.two_sources("", "name = \"a b\"\ndest = \"t\""));
    assert!(msg.contains("source[2].name"), "{msg}");
}

/// Verifies: SRS-SRC-001, SRS-SRC-003
#[test]
fn a_source_inside_another_is_skipped_by_the_outer_one() {
    let env = Env::new();
    std::fs::create_dir_all(env.dir.path().join("data/transfer")).unwrap();
    let text = env
        .two_sources("", "dest = \"transfer\"")
        .replace(&q(&env.p("transfer")), &q(&env.p("data/transfer")));
    let loaded = env.check(&text).unwrap();
    let inner = std::fs::canonicalize(env.p("data/transfer")).unwrap();
    assert_eq!(loaded.config.sources[0].skip, vec![inner.clone()]);
    assert!(loaded.config.sources[1].skip.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|w| w.starts_with("source[1].root") && w.contains("left to source transfer")),
        "{:?}",
        loaded.warnings
    );
    assert_eq!(
        loaded.config.source_index_of(&inner.join("alice/set1")),
        Some(1),
        "the innermost root owns a path"
    );
    assert_eq!(
        loaded
            .config
            .source_index_of(&std::fs::canonicalize(env.p("data")).unwrap().join("e/s/r")),
        Some(0)
    );
}

/// Verifies: SRS-SRC-003
#[test]
fn a_run_that_would_land_in_another_sources_folder_is_refused() {
    let env = Env::new();
    let c = env
        .check(&env.two_sources("dest_starts_at = \"sample\"", "dest = \"transfer\""))
        .unwrap()
        .config;
    // A MinKNOW sample folder named like the other source's folder.
    let err = c.dest_of(0, Path::new("exp/transfer/run1")).unwrap_err();
    assert!(err.contains("source transfer"), "{err}");
    assert_eq!(
        c.dest_of(0, Path::new("exp/sample1/run1")).unwrap(),
        Path::new("sample1/run1")
    );
    assert_eq!(
        c.dest_of(1, Path::new("alice/set1")).unwrap(),
        Path::new("transfer/alice/set1")
    );
}
