// SPDX-License-Identifier: AGPL-3.0-or-later
//! Guided set-up (`ont-offload init`).
//!
//! A [`Plan`] is made from a preset, the flags and `--set` overrides, and
//! rendered as a commented configuration file, which is parsed strictly
//! before anything is written. [`run`] then creates the local folders,
//! prepares the share (folder and sentinel, only on a filesystem the guard
//! allows and never replacing a sentinel that names another target),
//! probes it, writes the file, validates it as `check-config` would, and
//! prints the next steps.

use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::{InitArgs, Preset};
use crate::config::{self, Layout, units};
use crate::error::{Error, Result};
use crate::fsio::{FileKind, Fs};
use crate::guard::{self, Guard, SENTINEL};
use crate::logging::escape_control;

/// What `init` needs to know about the machine (tests pass their own).
#[derive(Debug, Clone)]
pub struct Host {
    /// Home folder of the user, for the default work folder.
    pub home: Option<PathBuf>,
    /// Host name, for the default `target_id`.
    pub hostname: Option<String>,
    /// Folder that relative paths are resolved against.
    pub cwd: PathBuf,
    /// Mount table to read (`/proc/self/mountinfo`).
    pub mountinfo: PathBuf,
}

impl Host {
    /// The real machine.
    #[must_use]
    pub fn current() -> Self {
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .or_else(|_| std::fs::read_to_string("/etc/hostname"))
            .ok()
            .map(|h| h.trim().to_owned())
            .filter(|h| !h.is_empty());
        Self {
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute()),
            hostname,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            mountinfo: PathBuf::from(guard::MOUNTINFO),
        }
    }
}

/// Default configuration path of the service (and of `--config`).
pub const SYSTEM_CONFIG: &str = "/etc/ont-offload/config.toml";

/// One `key = value` line of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    key: String,
    /// TOML literal, already quoted.
    value: String,
    /// Comment after the value ("" for none).
    note: String,
}

/// A table of the file (or the top level, with an empty header).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Section {
    /// `""`, `[transfer]` or `[[source]]`.
    header: String,
    /// Comment lines above the header.
    intro: Vec<String>,
    entries: Vec<Entry>,
}

impl Section {
    fn new(header: &str, intro: &[&str]) -> Self {
        Self {
            header: header.to_owned(),
            intro: intro.iter().map(|s| (*s).to_owned()).collect(),
            entries: Vec::new(),
        }
    }

    fn set(&mut self, key: &str, value: String, note: &str) -> &mut Self {
        if let Some(e) = self.entries.iter_mut().find(|e| e.key == key) {
            e.value = value;
            if !note.is_empty() {
                note.clone_into(&mut e.note);
            }
        } else {
            self.entries.push(Entry {
                key: key.to_owned(),
                value,
                note: note.to_owned(),
            });
        }
        self
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.as_str())
    }
}

/// A source the configuration will have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePlan {
    /// `runs`, `transfer` or `simulation`.
    pub name: String,
    /// Folder watched.
    pub root: PathBuf,
    /// Layout.
    pub layout: Layout,
}

/// Everything `init` will write, before anything is written.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The preset used.
    pub preset: Preset,
    /// Configuration file to write.
    pub output: PathBuf,
    /// `dest_root`.
    pub share: PathBuf,
    /// `target_id`.
    pub target_id: String,
    /// Work folder, when there is one.
    pub work_dir: Option<PathBuf>,
    /// `state_dir`.
    pub state_dir: PathBuf,
    /// `logging.file`.
    pub log_file: PathBuf,
    /// Notify script to create, when the preset writes one.
    pub notify_script: Option<PathBuf>,
    /// Sources, in file order.
    pub sources: Vec<SourcePlan>,
    /// `guard.allowed_fstypes`.
    pub allowed_fstypes: Vec<String>,
    /// `transfer.min_free` in bytes.
    pub min_free: u64,
    /// `transfer.min_free` as written.
    pub min_free_text: String,
    /// `cleanup.delete` as written.
    pub cleanup: String,
    /// The file's text.
    pub text: String,
}

impl Plan {
    /// The source with this name.
    #[must_use]
    pub fn source(&self, name: &str) -> Option<&SourcePlan> {
        self.sources.iter().find(|s| s.name == name)
    }
}

/// TOML basic string.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn qpath(p: &Path) -> String {
    quote(&p.to_string_lossy())
}

fn qlist(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| quote(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A `--set` value: a TOML value as written, or else text.
fn literal(v: &str) -> String {
    let v = v.trim();
    if toml::from_str::<toml::Table>(&format!("v = {v}")).is_ok() {
        v.to_owned()
    } else {
        quote(v)
    }
}

/// Quote a path or word for a POSIX shell, only when needed.
#[must_use]
pub fn sh(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:=@%,".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn shp(p: &Path) -> String {
    sh(&p.to_string_lossy())
}

/// An id made of letters, digits, `-`, `_` and `.` from a host name.
fn host_id(host: Option<&str>) -> Option<String> {
    let h = host?.split('.').next()?.trim();
    let id: String = h
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    (!id.is_empty()).then_some(id)
}

fn absolute(host: &Host, p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        host.cwd.join(p)
    };
    // Drop "." components; ".." stays and is refused by validation.
    joined
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

/// Settings that differ between presets.
struct Defaults {
    description: &'static str,
    workers: u32,
    rate_limit: &'static str,
    min_free: &'static str,
    fstypes: &'static [&'static str],
    cleanup: &'static str,
    /// `[transfer]` and `[outage]` timings and `[cleanup]` extras.
    transfer: &'static [(&'static str, &'static str, &'static str)],
    outage: &'static [(&'static str, &'static str, &'static str)],
    cleanup_extra: &'static [(&'static str, &'static str, &'static str)],
}

const NFS: &[&str] = &["nfs", "nfs4", "cifs", "smb3"];

fn defaults(p: Preset) -> Defaults {
    match p {
        Preset::Local => Defaults {
            description: "laptop test bench: a tmpfs folder stands in for the share, short timings, cleanup as a dry run",
            workers: 2,
            rate_limit: "200MB/s",
            min_free: "100MB",
            fstypes: &["tmpfs"],
            cleanup: "dry-run",
            transfer: &[
                ("settle", "5s", "production: 30s"),
                ("settle_after_complete", "20s", "production: 5m"),
                ("watch_completed_for", "2h", "production: 24h"),
            ],
            outage: &[
                ("check_every_max", "20s", "production: 5m"),
                ("stable_before_resume", "15s", "production: 60s"),
                ("log_every", "30s", "production: 15m"),
                ("warn_after", "60s", "production: 10m"),
                ("alert_after", "5m", "production: 1h"),
                (
                    "source_free_alert",
                    "0B",
                    "a laptop never has 200GB free for the sequencer",
                ),
            ],
            cleanup_extra: &[("delete_after", "0s", "production: 7d")],
        },
        Preset::Test => Defaults {
            description: "rehearsal against the real share as a normal user: own folder and target_id on the share, short timings, cleanup as a dry run",
            workers: 4,
            rate_limit: "100MB/s",
            min_free: "500GiB",
            fstypes: NFS,
            cleanup: "dry-run",
            transfer: &[
                ("settle", "10s", "production: 30s"),
                ("settle_after_complete", "30s", "production: 5m"),
                ("watch_completed_for", "2h", "production: 24h"),
            ],
            outage: &[
                ("check_every_max", "20s", "production: 5m"),
                ("stable_before_resume", "15s", "production: 60s"),
                ("log_every", "30s", "production: 15m"),
                ("warn_after", "1m", "production: 10m"),
                ("alert_after", "5m", "production: 1h"),
                (
                    "source_free_alert",
                    "200GB",
                    "alert when a source disk has less free",
                ),
            ],
            cleanup_extra: &[("delete_after", "0s", "never \"on\" in a test")],
        },
        Preset::Production => Defaults {
            description: "the service on a sequencer: production timings, cleanup off",
            workers: 4,
            rate_limit: "100MB/s",
            min_free: "500GiB",
            fstypes: NFS,
            cleanup: "off",
            transfer: &[
                ("settle", "30s", ""),
                ("settle_after_complete", "5m", ""),
                ("watch_completed_for", "24h", ""),
            ],
            outage: &[
                ("warn_after", "10m", ""),
                ("alert_after", "1h", ""),
                (
                    "source_free_alert",
                    "200GB",
                    "alert when a source disk has less free",
                ),
            ],
            cleanup_extra: &[
                ("gate", "storage-verify", "or \"readback\" or \"both\""),
                ("delete_after", "7d", ""),
            ],
        },
    }
}

/// Make the plan: every path and value, and the file's text. Nothing is
/// read or written.
///
/// Implements: SRS-CFG-005
///
/// # Errors
/// [`Error::Config`] for a missing required value (`--share` outside the
/// `local` preset, a home folder), no source left, a bad `--set`, or a
/// file the configuration parser refuses.
#[allow(clippy::too_many_lines)]
pub fn plan(args: &InitArgs, host: &Host) -> Result<Plan> {
    let preset = args.preset;
    let d = defaults(preset);
    let abs = |p: &Path| absolute(host, p);
    let home = || {
        host.home.clone().ok_or_else(|| {
            Error::Config("HOME is not set; give the work folder with --work-dir".into())
        })
    };
    let work_dir = match (&args.work_dir, preset) {
        (Some(w), _) => Some(abs(w)),
        (None, Preset::Local) => Some(home()?.join("ont-local")),
        (None, Preset::Test) => Some(home()?.join("ont-offload-test")),
        (None, Preset::Production) => None,
    };
    let share = match (&args.share, preset) {
        (Some(s), _) => abs(s),
        (None, Preset::Local) => PathBuf::from("/dev/shm/ont-share"),
        (None, _) => {
            return Err(Error::Config(format!(
                "the {} preset needs --share: the folder on the mounted share that ont-offload \
                 writes into, e.g. --share /mnt/nas/ont-offload{}",
                preset.as_str(),
                if preset == Preset::Test { "-test" } else { "" }
            )));
        }
    };
    let target_id = match (&args.target_id, preset) {
        (Some(t), _) => t.clone(),
        (None, Preset::Local) => "local-test".into(),
        (None, Preset::Test) => host_id(host.hostname.as_deref())
            .map_or_else(|| "ont-offload-test".into(), |h| format!("{h}-test")),
        (None, Preset::Production) => host_id(host.hostname.as_deref())
            .ok_or_else(|| Error::Config("cannot read the host name; give --target-id".into()))?,
    };
    let data = match (&args.data, &work_dir, preset) {
        (Some(p), _, _) => abs(p),
        (None, Some(w), Preset::Local) => w.join("data"),
        _ => PathBuf::from("/data"),
    };
    let under_work = |name: &str, fhs: &str| -> PathBuf {
        work_dir
            .as_ref()
            .map_or_else(|| PathBuf::from(fhs), |w| w.join(name))
    };
    let state_dir = args
        .state_dir
        .as_deref()
        .map_or_else(|| under_work("state", "/var/lib/ont-offload"), abs);
    let log_file = args.log.as_deref().map_or_else(
        || {
            under_work(
                "logs/ont-offload.log",
                "/var/log/ont-offload/ont-offload.log",
            )
        },
        abs,
    );
    let output = args
        .output
        .as_deref()
        .map_or_else(|| under_work("config.toml", SYSTEM_CONFIG), abs);

    let mut sources = Vec::new();
    if !args.no_runs {
        sources.push(SourcePlan {
            name: "runs".into(),
            root: args.runs.as_deref().map_or_else(|| data.clone(), abs),
            layout: Layout::Minknow,
        });
    }
    if !args.no_transfer {
        sources.push(SourcePlan {
            name: "transfer".into(),
            root: args
                .transfer
                .as_deref()
                .map_or_else(|| data.join("transfer"), abs),
            layout: Layout::Ready,
        });
    }
    if !args.no_simulation {
        sources.push(SourcePlan {
            name: "simulation".into(),
            root: args
                .simulation
                .as_deref()
                .map_or_else(|| data.join("simulation"), abs),
            layout: Layout::AnyDepth,
        });
    }
    if sources.is_empty() {
        return Err(Error::Config(
            "--no-runs, --no-transfer and --no-simulation leave no source to watch".into(),
        ));
    }

    let notify_script = match (&args.notify_command, &work_dir, preset) {
        (None, Some(w), Preset::Local | Preset::Test) => Some(w.join("notify.sh")),
        _ => None,
    };
    let notify_command = match (&args.notify_command, &notify_script) {
        (Some(c), _) => c.clone(),
        (None, Some(s)) => s.to_string_lossy().into_owned(),
        (None, None) => String::new(),
    };
    let fstypes: Vec<String> = if args.fstypes.is_empty() {
        d.fstypes.iter().map(|s| (*s).to_owned()).collect()
    } else {
        args.fstypes.clone()
    };
    let cleanup = args.cleanup.clone().unwrap_or_else(|| d.cleanup.into());
    let min_free_text = args.min_free.clone().unwrap_or_else(|| d.min_free.into());
    let rate_limit = args
        .rate_limit
        .clone()
        .unwrap_or_else(|| d.rate_limit.into());

    // The file, section by section.
    let mut top = Section::new("", &[]);
    top.set("dest_root", qpath(&share), "the folder on the share")
        .set(
            "target_id",
            quote(&target_id),
            "must equal dest_root/.ont-offload-target",
        )
        .set(
            "state_dir",
            qpath(&state_dir),
            "journals, state.json, events",
        )
        .set(
            "dest_starts_at",
            quote("experiment"),
            "<experiment>/<sample>/<run>/ on the share; or \"sample\", \"run\"",
        );
    let mut sections = vec![top];
    for (i, s) in sources.iter().enumerate() {
        let n = i + 1;
        let intro = match s.layout {
            Layout::Minknow => format!(
                "# {n}. MinKNOW output: {}/<experiment>/<sample>/<run>/",
                s.root.display()
            ),
            Layout::Ready => format!(
                "# {n}. User transfers: a folder with a READY file, and everything below it, is one dataset"
            ),
            Layout::AnyDepth => {
                format!("# {n}. Simulated runs (ont-runsim), one campaign folder per test")
            }
        };
        let mut sec = Section {
            header: "[[source]]".into(),
            intro: vec![intro],
            entries: Vec::new(),
        };
        sec.set("name", quote(&s.name), "")
            .set("root", qpath(&s.root), "")
            .set(
                "layout",
                quote(s.layout.as_str()),
                match s.layout {
                    Layout::Minknow => "",
                    Layout::Ready => "marker READY",
                    Layout::AnyDepth => {
                        "ont-runsim adds a campaign folder above the MinKNOW layout"
                    }
                },
            );
        match s.layout {
            Layout::Minknow => {}
            Layout::Ready => {
                sec.set(
                    "dest",
                    quote("transfer"),
                    "-> dest_root/transfer/<same path>/",
                )
                .set(
                    "cleanup",
                    "false".into(),
                    "user datasets are never deleted from the sequencer",
                );
            }
            Layout::AnyDepth => {
                sec.set(
                    "dest",
                    quote("simulation"),
                    "-> dest_root/simulation/<campaign>/...",
                )
                .set(
                    "cleanup",
                    "true".into(),
                    "simulated runs may be deleted, to test cleanup safely",
                );
            }
        }
        sections.push(sec);
    }
    let mut bw = Section::new("[bandwidth]", &[]);
    bw.set(
        "workers",
        args.workers.unwrap_or(d.workers).to_string(),
        "files copied at the same time; see `ont-offload bench`",
    )
    .set(
        "rate_limit",
        quote(&rate_limit),
        "total for all workers; see `ont-offload bench`",
    );
    if preset == Preset::Production {
        bw.set(
            "network_speed",
            quote("1Gbit/s"),
            "REVIEW: the sequencer's real link; only used to check rate_limit",
        );
    }
    sections.push(bw);
    let mut tr = Section::new("[transfer]", &[]);
    tr.set(
        "min_free",
        quote(&min_free_text),
        "copying pauses (with an alert) rather than leave less free on the share",
    );
    for (k, v, note) in d.transfer {
        tr.set(k, quote(v), note);
    }
    tr.set(
        "file_mode",
        quote("0640"),
        "copied files; octal or chmod symbols, also dir_mode and record_mode",
    );
    tr.set(
        "world_readable",
        "false".to_owned(),
        "true: everyone can read the output (a+r, folders a+rx)",
    );
    sections.push(tr);
    let mut out_s = Section::new("[outage]", &[]);
    for (k, v, note) in d.outage {
        out_s.set(k, quote(v), note);
    }
    out_s.set(
        "notify_command",
        quote(&notify_command),
        if notify_script.is_some() {
            "appends each alert to notify.log next to it"
        } else if notify_command.is_empty() {
            "REVIEW: program run with each alert as JSON on stdin; \"\" for none"
        } else {
            "program run with each alert as JSON on stdin"
        },
    );
    sections.push(out_s);
    let mut cl = Section::new("[cleanup]", &[]);
    cl.set(
        "delete",
        quote(&cleanup),
        match cleanup.as_str() {
            "off" => "later: \"dry-run\" for two weeks, then \"on\"",
            "dry-run" => "logs what would be deleted; nothing is deleted",
            _ => "REVIEW: deletes verified runs from the sequencer",
        },
    );
    for (k, v, note) in d.cleanup_extra {
        cl.set(k, quote(v), note);
    }
    sections.push(cl);
    let mut gd = Section::new("[guard]", &[]);
    gd.set(
        "allowed_fstypes",
        qlist(&fstypes),
        "the share must be mounted with one of these",
    );
    sections.push(gd);
    let mut lg = Section::new("[logging]", &[]);
    lg.set("file", qpath(&log_file), "written by `ont-offload watch`");
    sections.push(lg);

    for s in &args.set {
        apply_set(&mut sections, s)?;
    }

    let mut text = String::new();
    let _ = writeln!(
        text,
        "# ont-offload configuration written by `ont-offload init --preset {}`.",
        preset.as_str()
    );
    let _ = writeln!(text, "# Preset {}: {}.", preset.as_str(), d.description);
    let mut body = String::new();
    for sec in &sections {
        render(&mut body, sec);
    }
    if body.contains("REVIEW") {
        let _ = writeln!(text, "# Lines marked REVIEW need a value for this site.");
    }
    let _ = writeln!(
        text,
        "# Every setting is in the Configuration page of the user guide; check with\n\
         # `ont-offload check-config`."
    );
    text.push_str(&body);

    let raw = config::parse(&text).map_err(|e| match e {
        Error::Config(m) => Error::Config(format!("the configuration init would write: {m}")),
        other => other,
    })?;
    // Read the values back, so that --set overrides are reflected.
    let min_free = units::parse_size(&raw.transfer.min_free)
        .map_err(|e| Error::Config(format!("transfer.min_free: {e}")))?;
    let cleanup = match raw.cleanup.delete {
        config::DeleteMode::Off => "off",
        config::DeleteMode::DryRun => "dry-run",
        config::DeleteMode::On => "on",
    }
    .to_owned();
    let sources = raw
        .source
        .iter()
        .map(|s| SourcePlan {
            name: s.name.clone().unwrap_or_default(),
            root: PathBuf::from(&s.root),
            layout: s.layout,
        })
        .collect();
    let notify_script = notify_script.filter(|s| raw.outage.notify_command == s.to_string_lossy());
    Ok(Plan {
        preset,
        output,
        share: PathBuf::from(&raw.dest_root),
        target_id: raw.target_id.clone(),
        work_dir,
        state_dir: PathBuf::from(&raw.state_dir),
        log_file: raw.logging.file.as_ref().map_or(log_file, PathBuf::from),
        notify_script,
        sources,
        allowed_fstypes: raw.guard.allowed_fstypes.clone(),
        min_free,
        min_free_text: raw.transfer.min_free.clone(),
        cleanup,
        text,
    })
}

/// Apply one `--set KEY=VALUE`.
fn apply_set(sections: &mut Vec<Section>, arg: &str) -> Result<()> {
    let bad = |why: &str| Error::Config(format!("--set {:?}: {why}", escape_control(arg)));
    let (key, value) = arg
        .split_once('=')
        .ok_or_else(|| bad("expected KEY=VALUE"))?;
    let parts: Vec<&str> = key.trim().split('.').map(str::trim).collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(bad("empty key"));
    }
    let value = literal(value);
    let note = "set with --set";
    match parts.as_slice() {
        [k] => {
            if let Some(top) = sections.first_mut() {
                top.set(k, value, note);
            }
        }
        ["source", name, k] => {
            let wanted = quote(name);
            let sec = sections
                .iter_mut()
                .find(|s| s.header == "[[source]]" && s.get("name") == Some(wanted.as_str()))
                .ok_or_else(|| bad(&format!("there is no source named {name:?}")))?;
            sec.set(k, value, note);
        }
        ["source", ..] => {
            return Err(bad("source settings are set as source.<name>.<key>"));
        }
        [section, k] => {
            let header = format!("[{section}]");
            if let Some(sec) = sections.iter_mut().find(|s| s.header == header) {
                sec.set(k, value, note);
            } else {
                let mut sec = Section::new(&header, &[]);
                sec.set(k, value, note);
                sections.push(sec);
            }
        }
        _ => return Err(bad("expected key, section.key or source.<name>.key")),
    }
    Ok(())
}

fn render(text: &mut String, sec: &Section) {
    text.push('\n');
    for l in &sec.intro {
        let _ = writeln!(text, "{l}");
    }
    if !sec.header.is_empty() {
        let _ = writeln!(text, "{}", sec.header);
    }
    let width = sec
        .entries
        .iter()
        .filter(|e| !e.note.is_empty())
        .map(|e| e.key.len() + 3 + e.value.len())
        .max()
        .unwrap_or(0)
        .min(48);
    for e in &sec.entries {
        let line = format!("{} = {}", e.key, e.value);
        if e.note.is_empty() {
            let _ = writeln!(text, "{line}");
        } else {
            let _ = writeln!(text, "{line:<width$}  # {}", e.note);
        }
    }
}

/// How one set-up step went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Created,
    Exists,
    Wrote,
    Kept,
    Ok,
    Warn,
    Skipped,
    Note,
    Failed,
}

impl Mark {
    fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Exists => "exists",
            Self::Wrote => "wrote",
            Self::Kept => "kept",
            Self::Ok => "ok",
            Self::Warn => "WARNING",
            Self::Skipped => "skipped",
            Self::Note => "note",
            Self::Failed => "FAILED",
        }
    }
}

fn step(out: &mut dyn Write, mark: Mark, what: &str, why: &str) -> Result<()> {
    if why.is_empty() {
        writeln!(out, "  {:<8} {}", mark.as_str(), escape_control(what))?;
    } else {
        writeln!(
            out,
            "  {:<8} {}  ({})",
            mark.as_str(),
            escape_control(what),
            escape_control(why)
        )?;
    }
    Ok(())
}

fn heading(out: &mut dyn Write, title: &str) -> Result<()> {
    writeln!(out)?;
    writeln!(out, "{title}")?;
    writeln!(out, "{}", "─".repeat(title.chars().count()))?;
    Ok(())
}

fn is_dir(fs: &dyn Fs, p: &Path) -> bool {
    fs.metadata(p).is_ok_and(|m| m.kind == FileKind::Dir)
}

fn mkdir(fs: &dyn Fs, out: &mut dyn Write, p: &Path, why: &str) -> Result<()> {
    if is_dir(fs, p) {
        return step(out, Mark::Exists, &p.to_string_lossy(), why);
    }
    fs.create_dir_all(p).map_err(|e| {
        Error::Config(format!(
            "cannot create {} ({why}): {e}{}",
            escape_control(&p.to_string_lossy()),
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                "; run with sudo, or choose folders you can write with --work-dir or the path flags"
            } else {
                ""
            }
        ))
    })?;
    step(out, Mark::Created, &p.to_string_lossy(), why)
}

/// The nearest folder at or above `p` that exists.
fn existing_ancestor(fs: &dyn Fs, p: &Path) -> Option<PathBuf> {
    p.ancestors().find(|a| is_dir(fs, a)).map(Path::to_path_buf)
}

/// Read-only checks of the share before anything is changed: the mount
/// under it has an allowed filesystem type, and a sentinel already there
/// names this target.
///
/// Implements: SRS-CFG-005
fn share_preflight(plan: &Plan, fs: &dyn Fs, host: &Host) -> Result<guard::Mount> {
    let shown = escape_control(&plan.share.to_string_lossy());
    let refuse = |why: String| Error::Guard(format!("share {shown} refused: {why}"));
    if fs.metadata(&plan.share).is_ok() && !is_dir(fs, &plan.share) {
        return Err(refuse("it exists and is not a folder".into()));
    }
    let anc = existing_ancestor(fs, &plan.share)
        .ok_or_else(|| refuse("no folder above it exists".into()))?;
    let real = fs
        .canonicalize(&anc)
        .map_err(|e| refuse(format!("{} cannot be resolved ({e})", anc.display())))?;
    let text = fs.read(&host.mountinfo).map_err(|e| {
        refuse(format!(
            "mount table {} cannot be read ({e}); the destination guard needs Linux",
            host.mountinfo.display()
        ))
    })?;
    let mounts = guard::parse_mountinfo(&String::from_utf8_lossy(&text));
    let mount = guard::mount_of(&mounts, &real)
        .cloned()
        .ok_or_else(|| refuse("no mount found for it".into()))?;
    if !plan.allowed_fstypes.contains(&mount.fstype) {
        let hint = if mount.fstype == "tmpfs" {
            "; for a laptop test use --preset local (or --fstype tmpfs)"
        } else if NFS.contains(&mount.fstype.as_str()) {
            "; for a mounted share use --preset test or production (or --fstype)"
        } else {
            "; is the share mounted? Check with `findmnt`, or allow this type with --fstype"
        };
        return Err(refuse(format!(
            "it would be on a {} filesystem mounted at {}; allowed: {}{hint}",
            mount.fstype,
            escape_control(&mount.point.to_string_lossy()),
            plan.allowed_fstypes.join(", ")
        )));
    }
    if let Ok(bytes) = fs.read(&plan.share.join(SENTINEL)) {
        let found = String::from_utf8_lossy(&bytes).trim().to_owned();
        if found != plan.target_id {
            return Err(refuse(format!(
                "its sentinel {SENTINEL} names target {:?}, not {:?}; it belongs to another \
                 configuration. Use --target-id {} only if this folder is meant for this \
                 configuration, or choose another folder with --share",
                escape_control(&found),
                plan.target_id,
                sh(&escape_control(&found))
            )));
        }
    }
    Ok(mount)
}

/// Write `text` to `path` through a temporary file and a rename.
fn write_file(fs: &dyn Fs, path: &Path, text: &[u8], mode: u32) -> Result<()> {
    let name = path
        .file_name()
        .map_or_else(|| "config".into(), |n| n.to_string_lossy().into_owned());
    let tmp = path.with_file_name(format!(".{name}.init-tmp"));
    let _ = fs.remove_file(&tmp);
    let fail = |what: &str, e: std::io::Error| {
        Error::Config(format!(
            "cannot {what} {}: {e}",
            escape_control(&path.to_string_lossy())
        ))
    };
    let mut f = fs.create_new(&tmp).map_err(|e| fail("write", e))?;
    let written = f.write_all(text).and_then(|()| f.sync_data());
    drop(f);
    let done = written
        .and_then(|()| fs.set_mode(&tmp, mode))
        .and_then(|()| fs.rename(&tmp, path));
    if let Err(e) = done {
        let _ = fs.remove_file(&tmp);
        return Err(fail("write", e));
    }
    Ok(())
}

const NOTIFY_SCRIPT: &str = "#!/bin/sh\n\
# Written by `ont-offload init`: appends every event that needs a person to\n\
# notify.log next to this script. Replace with e-mail or a chat hook.\n\
printf '%s %s %s %s\\n' \"$(date -Is)\" \"$ONT_OFFLOAD_LEVEL\" \"$ONT_OFFLOAD_EVENT\" \"$ONT_OFFLOAD_MESSAGE\" \\\n\
  >> \"$(dirname \"$0\")/notify.log\"\n";

/// `init`: plan, set up, validate, and print the next steps.
///
/// Implements: SRS-CFG-005
///
/// # Errors
/// [`Error::Config`] for a bad plan, an existing file without `--force`, a
/// folder that cannot be created or a configuration that does not
/// validate; [`Error::Guard`] when the share is refused (nothing has been
/// written to it then).
#[allow(clippy::too_many_lines)]
pub fn run(args: &InitArgs, fs: &dyn Fs, host: &Host, out: &mut dyn Write) -> Result<()> {
    let plan = plan(args, host)?;
    if args.print {
        out.write_all(plan.text.as_bytes())?;
        return Ok(());
    }
    let output_shown = escape_control(&plan.output.to_string_lossy());
    let exists = fs.metadata(&plan.output).is_ok();
    if exists && !args.force {
        return Err(Error::Config(format!(
            "{output_shown} already exists; use --force to replace it (the old file is kept as \
             {output_shown}.bak), --output for another file, or --print to see the new one"
        )));
    }
    writeln!(
        out,
        "ont-offload init, preset {}: {}",
        plan.preset.as_str(),
        defaults(plan.preset).description
    )?;
    let probe = !args.no_probe;
    let mount = if probe {
        Some(share_preflight(&plan, fs, host)?)
    } else {
        None
    };

    heading(out, "Folders")?;
    let mut folders: Vec<(PathBuf, String)> = Vec::new();
    if let Some(w) = &plan.work_dir {
        folders.push((w.clone(), "work folder".into()));
    }
    folders.push((plan.state_dir.clone(), "state_dir".into()));
    if let Some(parent) = plan.log_file.parent() {
        folders.push((parent.to_path_buf(), "log folder".into()));
    }
    if let Some(parent) = plan.output.parent() {
        folders.push((parent.to_path_buf(), "configuration folder".into()));
    }
    for s in &plan.sources {
        folders.push((
            s.root.clone(),
            format!("source {}, layout {}", s.name, s.layout.as_str()),
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (p, why) in folders {
        if seen.insert(p.clone()) {
            mkdir(fs, out, &p, &why)?;
        }
    }
    if let Some(script) = &plan.notify_script {
        if fs.metadata(script).is_ok() {
            step(out, Mark::Kept, &script.to_string_lossy(), "notify_command")?;
        } else {
            write_file(fs, script, NOTIFY_SCRIPT.as_bytes(), 0o755)?;
            step(
                out,
                Mark::Created,
                &script.to_string_lossy(),
                "notify_command, appends alerts to notify.log",
            )?;
        }
    }

    heading(out, "Share")?;
    let mut probe_failed = None;
    if let Some(mount) = &mount {
        step(
            out,
            Mark::Ok,
            &format!(
                "{} mounted from {} at {}",
                mount.fstype,
                mount.source,
                mount.point.to_string_lossy()
            ),
            &format!("allowed: {}", plan.allowed_fstypes.join(", ")),
        )?;
        let anc = existing_ancestor(fs, &plan.share).unwrap_or_else(|| plan.share.clone());
        let share_dev = fs.metadata(&anc).map(|m| m.dev).ok();
        for s in &plan.sources {
            if fs.metadata(&s.root).map(|m| m.dev).ok() == share_dev {
                return Err(Error::Guard(format!(
                    "share {} refused: it is on the same device as source {} ({}); is the \
                     share mounted?",
                    escape_control(&plan.share.to_string_lossy()),
                    s.name,
                    escape_control(&s.root.to_string_lossy())
                )));
            }
        }
        mkdir(fs, out, &plan.share, "dest_root")?;
        let sentinel = plan.share.join(SENTINEL);
        if fs.metadata(&sentinel).is_ok() {
            step(
                out,
                Mark::Kept,
                &sentinel.to_string_lossy(),
                &format!("sentinel, names {}", plan.target_id),
            )?;
        } else {
            write_file(
                fs,
                &sentinel,
                format!("{}\n", plan.target_id).as_bytes(),
                0o644,
            )
            .map_err(|e| Error::Guard(format!("{e}")))?;
            step(
                out,
                Mark::Created,
                &sentinel.to_string_lossy(),
                &format!("sentinel, names {}", plan.target_id),
            )?;
        }
        match guard::probe_write(&plan.share, &crate::config::mode::Mode::octal(0o640), fs) {
            Ok(p) => {
                step(
                    out,
                    Mark::Ok,
                    &format!(
                        "write probe: copies will be owned by uid {} gid {}, mode {:04o}",
                        p.uid, p.gid, p.mode
                    ),
                    &p.process
                        .map_or_else(String::new, |(u, g)| format!("written as uid {u} gid {g}")),
                )?;
                for w in p.warnings() {
                    step(out, Mark::Warn, &w, "")?;
                }
            }
            Err(e) => {
                step(out, Mark::Failed, &format!("write probe: {e}"), "")?;
                probe_failed = Some(e);
            }
        }
        match fs.free_space(&plan.share) {
            Ok(free) if free < plan.min_free => step(
                out,
                Mark::Warn,
                &format!(
                    "{} free on the share, below transfer.min_free {}: copying would pause",
                    units::Bytes(free),
                    units::Bytes(plan.min_free)
                ),
                "",
            )?,
            Ok(free) => step(
                out,
                Mark::Ok,
                &format!("{} free on the share", units::Bytes(free)),
                &format!("keeps {} free", units::Bytes(plan.min_free)),
            )?,
            Err(_) => {}
        }
    } else {
        step(
            out,
            Mark::Skipped,
            &plan.share.to_string_lossy(),
            "--no-probe: no folder, sentinel or write probe",
        )?;
    }

    heading(out, "Configuration")?;
    if exists {
        let bak = PathBuf::from(format!("{}.bak", plan.output.to_string_lossy()));
        let old = fs.read(&plan.output)?;
        write_file(fs, &bak, &old, 0o640)?;
        step(out, Mark::Kept, &bak.to_string_lossy(), "the previous file")?;
    }
    write_file(fs, &plan.output, plan.text.as_bytes(), 0o640)?;
    step(
        out,
        Mark::Wrote,
        &plan.output.to_string_lossy(),
        &format!("{} source(s)", plan.sources.len()),
    )?;
    let checked = if probe && probe_failed.is_none() {
        let loaded = config::load(&plan.output, fs);
        let result = loaded.and_then(|l| {
            Guard::new(&l.config)
                .with_mountinfo(&host.mountinfo)
                .check(fs)
                .map(|_| l.warnings)
        });
        match result {
            Ok(warnings) => {
                step(
                    out,
                    Mark::Ok,
                    "configuration valid and destination guard OK",
                    &format!("{} warning(s)", warnings.len()),
                )?;
                for w in &warnings {
                    step(out, Mark::Warn, w, "")?;
                }
                if !warnings.is_empty() {
                    step(
                        out,
                        Mark::Note,
                        "warnings about nested source roots, a dry-run cleanup or a short \
                         delete_after are expected with these presets",
                        "",
                    )?;
                }
                Ok(())
            }
            Err(e) => {
                step(out, Mark::Failed, &e.to_string(), "")?;
                Err(e)
            }
        }
    } else {
        Ok(())
    };
    let checked = match probe_failed {
        Some(e) => Err(e),
        None => checked,
    };

    write!(out, "{}", next_steps(&plan, probe, checked.is_ok()))?;
    checked
}

/// The numbered next steps for this plan.
///
/// Implements: SRS-CFG-005
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn next_steps(plan: &Plan, probed: bool, valid: bool) -> String {
    let mut s = String::new();
    let mut n = 0;
    let mut item = |s: &mut String, title: &str, lines: &[String]| {
        n += 1;
        let _ = writeln!(s, "\n {n:>2}. {title}");
        for l in lines {
            let _ = writeln!(s, "       {l}");
        }
    };
    let title = "Next steps";
    let _ = writeln!(s, "\n{title}\n{}", "─".repeat(title.len()));
    let cfg = shp(&plan.output);
    let default_path = plan.output == Path::new(SYSTEM_CONFIG);
    if !valid {
        item(
            &mut s,
            "Fix what FAILED above, then check again:",
            &[format!("ont-offload --config {cfg} check-config")],
        );
    }
    if !default_path {
        item(
            &mut s,
            "Use this configuration in every terminal (or pass --config each time):",
            &[format!("export ONT_OFFLOAD_CONFIG={cfg}")],
        );
    }
    let mut look: Vec<(String, String)> = vec![
        (
            format!("target_id = {}", plan.target_id),
            "names this configuration in the share's sentinel".into(),
        ),
        (
            "[bandwidth] workers, rate_limit".into(),
            "take them from the bench below".into(),
        ),
        (
            format!("[transfer] min_free = {}", plan.min_free_text),
            "always left free on the share".into(),
        ),
        (
            format!("[cleanup] delete = {}", plan.cleanup),
            "deleting offloaded runs from the sequencer".into(),
        ),
    ];
    if plan.preset == Preset::Production {
        look.push((
            "[bandwidth] network_speed".into(),
            "the sequencer's real link".into(),
        ));
        look.push((
            "[outage] notify_command".into(),
            "who hears about an outage".into(),
        ));
    }
    let w = look.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut review = vec![if plan.text.contains("REVIEW") {
        format!("In {cfg}, fill in each line marked REVIEW, and check:")
    } else {
        format!("In {cfg}, check:")
    }];
    review.extend(look.iter().map(|(k, v)| format!("  {k:<w$}  {v}")));
    review.push("Every setting is explained in the user guide, Configuration.".into());
    item(&mut s, "Review the configuration by hand:", &review);
    if !probed {
        let share = shp(&plan.share);
        item(
            &mut s,
            "Prepare the share once it is mounted (skipped with --no-probe):",
            &[
                format!(
                    "findmnt -T {share}        # must be one of: {}",
                    plan.allowed_fstypes.join(", ")
                ),
                format!("mkdir -p {share}"),
                format!(
                    "echo {} > {}",
                    sh(&plan.target_id),
                    shp(&plan.share.join(SENTINEL))
                ),
            ],
        );
    }
    item(
        &mut s,
        "Check the configuration, the share and the runs it finds:",
        &["ont-offload check-config".into()],
    );
    let bench = match plan.preset {
        Preset::Local => "ont-offload bench --size 256MB --max-streams 4",
        Preset::Test => "ont-offload bench --size 1GB",
        Preset::Production => "ont-offload bench",
    };
    item(
        &mut s,
        "Measure the share, then copy the suggested workers, rate_limit and chunk_size into the file:",
        &[bench.into()],
    );
    let sim = plan.source("simulation");
    let transfer = plan.source("transfer");
    if plan.preset == Preset::Production {
        let state = shp(&plan.state_dir);
        let logs = plan
            .log_file
            .parent()
            .map_or_else(String::new, |p| format!(" {}", shp(p)));
        let mut lines = vec![
            "Give the service user (ont-offload, or the account in run-as-existing-user.conf)"
                .into(),
            "the state and log folders, and let it read the configuration:".into(),
            format!("sudo chown -R ont-offload: {state}{logs}"),
            format!("sudo chgrp ont-offload {cfg} && sudo chmod 0640 {cfg}"),
        ];
        if !default_path {
            lines.push(format!(
                "The unit reads {SYSTEM_CONFIG}: set ONT_OFFLOAD_CONFIG={cfg} in a drop-in, or copy the file there."
            ));
        }
        lines.push("sudo systemctl enable --now ont-offload".into());
        lines.push(
            "journalctl -u ont-offload -f      # Ctrl-C stops following, not the service".into(),
        );
        item(
            &mut s,
            "Start the service (see the deployment guide):",
            &lines,
        );
    } else {
        item(
            &mut s,
            "Start the daemon in this terminal (Ctrl-C stops it; the next start resumes):",
            &["ont-offload watch".into()],
        );
    }
    item(
        &mut s,
        "Follow it from another terminal:",
        &[
            "ont-offload status                # one-shot summary".into(),
            "ont-offload tui                   # dashboard, q quits".into(),
        ],
    );
    if let Some(sim) = sim {
        let campaign = shp(&sim.root.join("test-01"));
        item(
            &mut s,
            "Offload a simulated run from a third terminal (needs ont-runsim; about 2 minutes):",
            &[
                format!("ont-runsim run --preset quick-2h --out {campaign} --keep-free 1GB -q"),
                format!(
                    "ont-runsim check --out {campaign} --dest {}/<experiment>/<sample>/<run>",
                    shp(&plan.share.join("simulation/test-01"))
                ),
            ],
        );
    }
    if let Some(t) = transfer {
        let set = t.root.join("me/set1");
        item(
            &mut s,
            "Send a dataset through the transfer folder: copy it in, then mark it READY last:",
            &[
                format!("mkdir -p {} && cp -r <files> {}/", shp(&set), shp(&set)),
                format!("touch {}", shp(&set.join("READY"))),
            ],
        );
    }
    item(
        &mut s,
        "See what cleanup would delete from the sequencer (deletes nothing):",
        &["ont-offload cleanup --dry-run".into()],
    );
    match (plan.preset, &plan.work_dir) {
        (Preset::Local, Some(w)) => item(
            &mut s,
            "Remove the test bench when done:",
            &[format!("rm -r {} {}", shp(w), shp(&plan.share))],
        ),
        (Preset::Test, Some(w)) => item(
            &mut s,
            "Remove the test when done (only its own folders):",
            &[format!("rm -r {} {}", shp(w), shp(&plan.share))],
        ),
        _ => {}
    }
    let guide = match plan.preset {
        Preset::Local => "Testing on a laptop",
        Preset::Test | Preset::Production => "Deploying on a sequencer",
    };
    let _ = writeln!(
        s,
        "\nMore in the user guide: \"Setting up with init\" and \"{guide}\"."
    );
    s
}

#[cfg(test)]
mod tests;
