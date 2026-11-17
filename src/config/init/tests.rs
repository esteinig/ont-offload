// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of `init`: plans, overrides, the share checks and the next steps.

use super::*;
use crate::fsio::RealFs;

fn host(home: &Path) -> Host {
    Host {
        home: Some(home.to_path_buf()),
        hostname: Some("PC24B001.lab.example".into()),
        cwd: home.to_path_buf(),
        mountinfo: PathBuf::from(guard::MOUNTINFO),
    }
}

fn args(preset: Preset) -> InitArgs {
    InitArgs {
        preset,
        ..InitArgs::default()
    }
}

/// A share folder on tmpfs (on another device than the temp dir), or
/// `None` where there is no usable `/dev/shm`.
fn tmpfs_share() -> Option<tempfile::TempDir> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let shm = tempfile::tempdir_in("/dev/shm").ok()?;
        let tmp = tempfile::tempdir().ok()?;
        let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
        (dev(shm.path()) != dev(tmp.path())).then_some(shm)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn run_init(a: &InitArgs, h: &Host) -> (Result<()>, String) {
    let mut out = Vec::new();
    let r = run(a, &RealFs, h, &mut out);
    (r, String::from_utf8_lossy(&out).into_owned())
}

/// Verifies: SRS-CFG-005
#[test]
fn local_preset_puts_everything_in_the_work_folder() {
    let p = plan(&args(Preset::Local), &host(Path::new("/home/u"))).unwrap();
    assert_eq!(p.output, Path::new("/home/u/ont-local/config.toml"));
    assert_eq!(p.share, Path::new("/dev/shm/ont-share"));
    assert_eq!(p.target_id, "local-test");
    assert_eq!(p.state_dir, Path::new("/home/u/ont-local/state"));
    assert_eq!(
        p.log_file,
        Path::new("/home/u/ont-local/logs/ont-offload.log")
    );
    assert_eq!(
        p.notify_script.as_deref(),
        Some(Path::new("/home/u/ont-local/notify.sh"))
    );
    let roots: Vec<_> = p
        .sources
        .iter()
        .map(|s| {
            (
                s.name.as_str(),
                s.root.to_string_lossy().into_owned(),
                s.layout,
            )
        })
        .collect();
    assert_eq!(
        roots,
        [
            ("runs", "/home/u/ont-local/data".into(), Layout::Minknow),
            (
                "transfer",
                "/home/u/ont-local/data/transfer".into(),
                Layout::Ready
            ),
            (
                "simulation",
                "/home/u/ont-local/data/simulation".into(),
                Layout::AnyDepth
            ),
        ]
    );
    assert_eq!(p.allowed_fstypes, ["tmpfs"]);
    assert_eq!(p.cleanup, "dry-run");
    let raw = config::parse(&p.text).unwrap();
    assert_eq!(raw.source[1].dest, "transfer");
    assert_eq!(raw.source[1].cleanup, Some(false));
    assert_eq!(raw.source[2].dest, "simulation");
    assert_eq!(raw.source[2].cleanup, Some(true));
    assert_eq!(raw.transfer.settle, "5s");
}

/// Verifies: SRS-CFG-005
#[test]
fn test_and_production_presets_need_a_share_and_use_the_host_name() {
    let h = host(Path::new("/home/prom"));
    for preset in [Preset::Test, Preset::Production] {
        let e = plan(&args(preset), &h).unwrap_err().to_string();
        assert!(e.contains("needs --share"), "{e}");
    }
    let mut a = args(Preset::Test);
    a.share = Some("/mnt/HPC/ont-offload-test".into());
    let t = plan(&a, &h).unwrap();
    assert_eq!(t.target_id, "PC24B001-test");
    assert_eq!(
        t.output,
        Path::new("/home/prom/ont-offload-test/config.toml")
    );
    assert_eq!(t.source("runs").unwrap().root, Path::new("/data"));
    assert_eq!(t.cleanup, "dry-run");
    assert_eq!(t.allowed_fstypes, ["nfs", "nfs4", "cifs", "smb3"]);

    let mut a = args(Preset::Production);
    a.share = Some("/mnt/HPC/ont-offload".into());
    let p = plan(&a, &h).unwrap();
    assert_eq!(p.target_id, "PC24B001");
    assert_eq!(p.work_dir, None);
    assert_eq!(p.output, Path::new(SYSTEM_CONFIG));
    assert_eq!(p.state_dir, Path::new("/var/lib/ont-offload"));
    assert_eq!(
        p.log_file,
        Path::new("/var/log/ont-offload/ont-offload.log")
    );
    assert_eq!(p.notify_script, None);
    assert_eq!(p.cleanup, "off");
    assert!(p.text.contains("REVIEW"), "{}", p.text);
    let raw = config::parse(&p.text).unwrap();
    assert_eq!(raw.transfer.settle, "30s");
    assert_eq!(raw.outage.notify_command, "");
}

/// Verifies: SRS-CFG-005
#[test]
fn flags_override_the_preset() {
    let mut a = args(Preset::Production);
    a.share = Some("share".into()); // relative: resolved against the cwd
    a.target_id = Some("lab-nas-01".into());
    a.work_dir = Some("/srv/oo".into());
    a.data = Some("/data/data-offload".into());
    a.transfer = Some("/data/drop".into());
    a.no_simulation = true;
    a.log = Some("/var/log/oo.log".into());
    a.fstypes = vec!["nfs4".into()];
    a.workers = Some(8);
    a.rate_limit = Some("400MB/s".into());
    a.min_free = Some("1TB".into());
    a.cleanup = Some("dry-run".into());
    a.notify_command = Some("/usr/local/bin/notify".into());
    let p = plan(&a, &host(Path::new("/home/u"))).unwrap();
    assert_eq!(p.share, Path::new("/home/u/share"));
    assert_eq!(p.target_id, "lab-nas-01");
    assert_eq!(p.output, Path::new("/srv/oo/config.toml"));
    assert_eq!(p.state_dir, Path::new("/srv/oo/state"));
    assert_eq!(p.log_file, Path::new("/var/log/oo.log"));
    assert_eq!(p.sources.len(), 2);
    assert_eq!(
        p.source("runs").unwrap().root,
        Path::new("/data/data-offload")
    );
    assert_eq!(p.source("transfer").unwrap().root, Path::new("/data/drop"));
    assert!(p.source("simulation").is_none());
    assert_eq!(p.allowed_fstypes, ["nfs4"]);
    assert_eq!(p.min_free, 1_000_000_000_000);
    assert_eq!(p.cleanup, "dry-run");
    let raw = config::parse(&p.text).unwrap();
    assert_eq!(raw.bandwidth.workers, 8);
    assert_eq!(raw.bandwidth.rate_limit, "400MB/s");
    assert_eq!(raw.outage.notify_command, "/usr/local/bin/notify");

    let mut none = args(Preset::Local);
    none.no_runs = true;
    none.no_transfer = true;
    none.no_simulation = true;
    let e = plan(&none, &host(Path::new("/home/u"))).unwrap_err();
    assert!(e.to_string().contains("no source"), "{e}");
}

/// Verifies: SRS-CFG-005
#[test]
fn set_changes_any_setting_and_is_checked_by_the_parser() {
    let mut a = args(Preset::Local);
    a.set = vec![
        "dest_starts_at=run".into(),
        "transfer.settle = \"12s\"".into(),
        "transfer.max_attempts=5".into(),
        "source.runs.dest_starts_at=sample".into(),
        "source.transfer.cleanup=true".into(),
        "tui.theme=phosphor".into(),
        "transfer.min_free=2GB".into(),
        "cleanup.delete=on".into(),
    ];
    let p = plan(&a, &host(Path::new("/home/u"))).unwrap();
    let raw = config::parse(&p.text).unwrap();
    assert_eq!(raw.dest_starts_at, config::DestLevel::Run);
    assert_eq!(raw.transfer.settle, "12s");
    assert_eq!(raw.transfer.max_attempts, 5);
    assert_eq!(
        raw.source[0].dest_starts_at,
        Some(config::DestLevel::Sample)
    );
    assert_eq!(raw.source[1].cleanup, Some(true));
    assert_eq!(raw.tui.theme, config::TuiTheme::Phosphor);
    // The plan follows the overrides.
    assert_eq!(p.min_free, 2_000_000_000);
    assert_eq!(p.cleanup, "on");
    assert_eq!(
        p.text.matches("settle = ").count(),
        1,
        "replaced, not added"
    );

    for (bad, why) in [
        ("transfer.settle", "KEY=VALUE"),
        ("source.nope.root=/x", "no source named"),
        ("source.root=/x", "source.<name>.<key>"),
        ("transfer.bogus=1", "bogus"),
        ("a.b.c=1", "expected key"),
        ("transfer.workers=2", "workers"),
    ] {
        let mut a = args(Preset::Local);
        a.set = vec![bad.into()];
        let e = plan(&a, &host(Path::new("/home/u")))
            .unwrap_err()
            .to_string();
        assert!(e.contains(why), "{bad}: {e}");
    }
}

/// Verifies: SRS-CFG-005
#[test]
fn shell_quoting_and_host_ids() {
    assert_eq!(sh("/data/x-1"), "/data/x-1");
    assert_eq!(sh("/my data"), "'/my data'");
    assert_eq!(sh("it's"), "'it'\\''s'");
    assert_eq!(sh(""), "''");
    assert_eq!(host_id(Some("PC24B001.lab")), Some("PC24B001".into()));
    assert_eq!(host_id(Some("odd host")), Some("odd-host".into()));
    assert_eq!(host_id(Some("")), None);
    assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    assert_eq!(literal("10s"), "\"10s\"");
    assert_eq!(literal("[\"nfs4\"]"), "[\"nfs4\"]");
    assert_eq!(literal("3"), "3");
}

/// Verifies: SRS-CFG-005, SRS-GRD-004
#[test]
fn local_init_sets_up_folders_share_and_a_valid_configuration() {
    let Some(shm) = tmpfs_share() else { return };
    let home = tempfile::tempdir().unwrap();
    let h = host(home.path());
    let share = shm.path().join("share");
    let mut a = args(Preset::Local);
    a.share = Some(share.clone());
    let (r, out) = run_init(&a, &h);
    r.unwrap();
    let work = home.path().join("ont-local");
    for d in ["state", "logs", "data", "data/transfer", "data/simulation"] {
        assert!(work.join(d).is_dir(), "{d}\n{out}");
    }
    assert!(work.join("notify.sh").is_file());
    assert_eq!(
        std::fs::read_to_string(share.join(SENTINEL)).unwrap(),
        "local-test\n"
    );
    // Only the sentinel is left on the share (the probe removed itself).
    assert_eq!(std::fs::read_dir(&share).unwrap().count(), 1);
    let cfg = work.join("config.toml");
    let loaded = config::load(&cfg, &RealFs).unwrap();
    assert_eq!(loaded.config.sources.len(), 3);
    for want in [
        "created  ",
        "write probe",
        "configuration valid and destination guard OK",
        "Next steps",
        "export ONT_OFFLOAD_CONFIG=",
        "ont-offload check-config",
        "ont-offload bench",
        "ont-runsim run --preset quick-2h",
        "READY",
        "rm -r ",
    ] {
        assert!(out.contains(want), "missing {want:?} in\n{out}");
    }

    // A second run keeps the file unless --force, which keeps a backup.
    let (r, _) = run_init(&a, &h);
    assert!(r.unwrap_err().to_string().contains("--force"));
    std::fs::write(&cfg, "# edited by hand\n").unwrap();
    a.force = true;
    let (r, out) = run_init(&a, &h);
    r.unwrap();
    assert!(out.contains("exists   "), "{out}");
    assert!(out.contains("kept     "), "{out}");
    assert_eq!(
        std::fs::read_to_string(work.join("config.toml.bak")).unwrap(),
        "# edited by hand\n"
    );
}

/// Verifies: SRS-CFG-005
#[test]
fn a_share_on_a_filesystem_the_guard_refuses_gets_nothing_written() {
    let home = tempfile::tempdir().unwrap();
    let h = host(home.path());
    let mut a = args(Preset::Test);
    // The temp dir is not an NFS mount.
    a.share = Some(home.path().join("share"));
    let (r, _) = run_init(&a, &h);
    let e = r.unwrap_err();
    assert!(matches!(e, Error::Guard(_)), "{e}");
    assert!(e.to_string().contains("allowed: nfs"), "{e}");
    assert!(!home.path().join("share").exists());
    assert!(!home.path().join("ont-offload-test").exists());
}

/// Verifies: SRS-CFG-005
#[test]
fn an_nfs_share_from_the_mount_table_is_prepared() {
    let Some(shm) = tmpfs_share() else { return };
    let home = tempfile::tempdir().unwrap();
    let mountinfo = home.path().join("mountinfo");
    std::fs::write(
        &mountinfo,
        format!(
            "1 0 0:1 / / rw - ext4 /dev/sda1 rw\n\
             50 1 0:99 / {} rw - nfs4 nas:/export/ont rw\n",
            shm.path().display()
        ),
    )
    .unwrap();
    let mut h = host(home.path());
    h.mountinfo = mountinfo;
    let mut a = args(Preset::Test);
    a.share = Some(shm.path().join("ont-offload-test"));
    a.data = Some(home.path().join("data"));
    a.min_free = Some("1MB".into());
    let (r, out) = run_init(&a, &h);
    r.unwrap();
    assert!(out.contains("nfs4 mounted from nas:/export/ont"), "{out}");
    assert_eq!(
        std::fs::read_to_string(shm.path().join("ont-offload-test").join(SENTINEL)).unwrap(),
        "PC24B001-test\n"
    );
}

/// Verifies: SRS-CFG-005
#[test]
fn a_sentinel_of_another_target_is_never_replaced() {
    let Some(shm) = tmpfs_share() else { return };
    let home = tempfile::tempdir().unwrap();
    std::fs::write(shm.path().join(SENTINEL), "lab-nas-01\n").unwrap();
    let mut a = args(Preset::Local);
    a.share = Some(shm.path().to_path_buf());
    let (r, _) = run_init(&a, &host(home.path()));
    let e = r.unwrap_err().to_string();
    assert!(e.contains("names target \"lab-nas-01\""), "{e}");
    assert!(e.contains("--target-id lab-nas-01"), "{e}");
    assert_eq!(
        std::fs::read_to_string(shm.path().join(SENTINEL)).unwrap(),
        "lab-nas-01\n"
    );
    assert!(!home.path().join("ont-local").exists(), "nothing created");

    // With the matching id the sentinel is kept as it is.
    a.target_id = Some("lab-nas-01".into());
    let (r, out) = run_init(&a, &host(home.path()));
    r.unwrap();
    assert!(out.contains("kept"), "{out}");
}

/// Verifies: SRS-CFG-005
#[test]
fn no_probe_leaves_the_share_alone_and_says_how_to_prepare_it() {
    let home = tempfile::tempdir().unwrap();
    let mut a = args(Preset::Production);
    let share = home.path().join("not-mounted/ont-offload");
    a.share = Some(share.clone());
    a.work_dir = Some(home.path().join("oo"));
    a.data = Some(home.path().join("data"));
    a.no_probe = true;
    let (r, out) = run_init(&a, &host(home.path()));
    r.unwrap();
    assert!(!share.exists());
    assert!(home.path().join("oo/config.toml").is_file());
    assert!(out.contains("skipped"), "{out}");
    assert!(out.contains("findmnt -T"), "{out}");
    assert!(out.contains("echo PC24B001 > "), "{out}");
    assert!(out.contains("systemctl enable --now ont-offload"), "{out}");
    assert!(out.contains("chown -R ont-offload:"), "{out}");
}

/// Verifies: SRS-CFG-005
#[test]
fn print_shows_the_file_and_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let mut a = args(Preset::Local);
    a.print = true;
    let (r, out) = run_init(&a, &host(home.path()));
    r.unwrap();
    assert!(
        out.starts_with("# ont-offload configuration written by"),
        "{out}"
    );
    config::parse(&out).unwrap();
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

/// Verifies: SRS-CFG-005
#[test]
fn next_steps_follow_the_plan() {
    let h = host(Path::new("/home/u"));
    let mut a = args(Preset::Local);
    a.no_simulation = true;
    a.no_transfer = true;
    let p = plan(&a, &h).unwrap();
    let s = next_steps(&p, true, true);
    assert!(!s.contains("ont-runsim"), "{s}");
    assert!(!s.contains("READY"), "{s}");
    assert!(!s.contains("Fix what FAILED"), "{s}");
    assert!(s.contains("ont-offload watch"), "{s}");
    let failed = next_steps(&p, true, false);
    assert!(failed.contains("Fix what FAILED"), "{failed}");

    let mut a = args(Preset::Production);
    a.share = Some("/mnt/HPC/ont-offload".into());
    let p = plan(&a, &h).unwrap();
    let s = next_steps(&p, true, true);
    assert!(
        !s.contains("export ONT_OFFLOAD_CONFIG"),
        "default path: {s}"
    );
    assert!(s.contains("REVIEW"), "{s}");
    assert!(s.contains("notify_command"), "{s}");
    assert!(!s.contains("rm -r"), "{s}");
}
