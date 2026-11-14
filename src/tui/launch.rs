// SPDX-License-Identifier: AGPL-3.0-or-later
//! Where the dashboard reads from and how it looks: each setting from a
//! command-line flag, else an environment variable, else the configuration
//! file, else the default, with its origin kept so that `tui --check` and
//! the sources screen can show it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cleanup::REPORTS;
use crate::config::{self, TuiSettings, TuiTheme};
use crate::error::{Error, Result};
use crate::fsio::Fs;
use crate::logging::escape_control;
use crate::state::{EVENTS_FILE, STATE_FILE};

/// Environment variable: the daemon's state folder.
pub const ENV_STATE_DIR: &str = "ONT_OFFLOAD_STATE_DIR";
/// Environment variable: the daemon's log file.
pub const ENV_LOG: &str = "ONT_OFFLOAD_TUI_LOG";
/// Environment variable: the theme.
pub const ENV_THEME: &str = "ONT_OFFLOAD_TUI_THEME";
/// Environment variable: the refresh interval.
pub const ENV_REFRESH: &str = "ONT_OFFLOAD_TUI_REFRESH";
/// Environment variable: the bell (`on` or `off`).
pub const ENV_BELL: &str = "ONT_OFFLOAD_TUI_BELL";
/// Environment variable (convention): no colours.
pub const ENV_NO_COLOR: &str = "NO_COLOR";

/// Every environment variable the dashboard reads, with what it sets.
pub const ENV_VARS: &[(&str, &str)] = &[
    (
        "ONT_OFFLOAD_CONFIG",
        "configuration file (default /etc/ont-offload/config.toml)",
    ),
    (
        ENV_STATE_DIR,
        "state folder of the daemon (state.json, events.jsonl)",
    ),
    (ENV_LOG, "the daemon's log file to show in the log pane"),
    (ENV_THEME, "plain, matrix, phosphor or synthwave"),
    (ENV_REFRESH, "refresh interval, 200ms to 60s"),
    (ENV_BELL, "on or off: ring the terminal bell on a new alert"),
    (ENV_NO_COLOR, "any value: terminal colours only"),
];

/// Where a setting came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A command-line flag.
    Flag,
    /// An environment variable.
    Env(&'static str),
    /// The configuration file.
    Config,
    /// The built-in default.
    Default,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flag => write!(f, "flag"),
            Self::Env(name) => write!(f, "${name}"),
            Self::Config => write!(f, "config file"),
            Self::Default => write!(f, "default"),
        }
    }
}

/// Values given on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flags {
    /// `--state-dir`.
    pub state_dir: Option<PathBuf>,
    /// `--log`.
    pub log: Option<PathBuf>,
    /// `--theme`.
    pub theme: Option<String>,
    /// `--refresh`.
    pub refresh: Option<String>,
    /// `--bell on|off`.
    pub bell: Option<bool>,
}

/// The resolved launch settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Configuration file and what happened reading it.
    pub config: (PathBuf, String),
    /// State folder.
    pub state_dir: (PathBuf, Origin),
    /// Log file.
    pub log: (Option<PathBuf>, Origin),
    /// Theme.
    pub theme: (TuiTheme, Origin),
    /// Refresh interval.
    pub refresh: (Duration, Origin),
    /// Bell.
    pub bell: (bool, Origin),
    /// No colours (`NO_COLOR`).
    pub no_color: bool,
}

fn pick<T>(
    flag: Option<T>,
    env: Option<(T, &'static str)>,
    cfg: Option<T>,
    default: T,
) -> (T, Origin) {
    if let Some(v) = flag {
        (v, Origin::Flag)
    } else if let Some((v, name)) = env {
        (v, Origin::Env(name))
    } else if let Some(v) = cfg {
        (v, Origin::Config)
    } else {
        (default, Origin::Default)
    }
}

fn parse_bell(text: &str) -> std::result::Result<bool, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        other => Err(format!("{:?} is not on or off", escape_control(other))),
    }
}

/// Resolve the settings: flag, else environment (`env` looks a variable
/// up), else the configuration file at `config_path`, else the default.
/// A missing configuration file is not an error: the dashboard runs on the
/// other sources.
///
/// Implements: SRS-TUI-009
///
/// # Errors
/// [`Error::Config`] for an unparsable configuration file or a bad value
/// in a flag or variable.
pub fn resolve(
    config_path: &Path,
    fs: &dyn Fs,
    flags: &Flags,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Launch> {
    let shown = escape_control(&config_path.to_string_lossy());
    let (cfg, config_note) = match fs.metadata(config_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (None, "not found; using the other sources".to_owned())
        }
        _ => (
            Some(config::tui_settings(config_path, fs)?),
            "read".to_owned(),
        ),
    };
    let var = |name: &'static str| {
        env(name)
            .filter(|v| !v.trim().is_empty())
            .map(|v| (v, name))
    };
    let bad = |what: &str, e: String| Error::Config(format!("tui {what}: {e}"));

    let theme_flag = flags
        .theme
        .as_deref()
        .map(TuiTheme::parse)
        .transpose()
        .map_err(|e| bad("--theme", e))?;
    let theme_env = var(ENV_THEME)
        .map(|(v, n)| TuiTheme::parse(&v).map(|t| (t, n)))
        .transpose()
        .map_err(|e| bad(ENV_THEME, e))?;
    let refresh_flag = flags
        .refresh
        .as_deref()
        .map(config::tui_refresh)
        .transpose()
        .map_err(|e| bad("--refresh", e))?;
    let refresh_env = var(ENV_REFRESH)
        .map(|(v, n)| config::tui_refresh(&v).map(|d| (d, n)))
        .transpose()
        .map_err(|e| bad(ENV_REFRESH, e))?;
    let bell_env = var(ENV_BELL)
        .map(|(v, n)| parse_bell(&v).map(|b| (b, n)))
        .transpose()
        .map_err(|e| bad(ENV_BELL, e))?;
    let d = TuiSettings::default();
    let c = cfg.as_ref();
    let state_dir = pick(
        flags.state_dir.clone(),
        var(ENV_STATE_DIR).map(|(v, n)| (PathBuf::from(v), n)),
        c.map(|c| c.state_dir.clone()),
        d.state_dir.clone(),
    );
    let log = pick(
        flags.log.clone().map(Some),
        var(ENV_LOG).map(|(v, n)| (Some(PathBuf::from(v)), n)),
        c.and_then(|c| c.log_file.clone()).map(Some),
        None,
    );
    Ok(Launch {
        config: (config_path.to_path_buf(), format!("{shown}: {config_note}")),
        state_dir,
        log,
        theme: pick(theme_flag, theme_env, c.map(|c| c.theme), d.theme),
        refresh: pick(refresh_flag, refresh_env, c.map(|c| c.refresh), d.refresh),
        bell: pick(flags.bell, bell_env, c.map(|c| c.bell), d.bell),
        no_color: env(ENV_NO_COLOR).is_some_and(|v| !v.is_empty()),
    })
}

fn readable(fs: &dyn Fs, p: &Path) -> String {
    match fs.metadata(p) {
        Ok(st) => format!("readable, {} bytes", st.len),
        Err(e) => format!("NOT READABLE: {e}"),
    }
}

/// The settings with their origins and whether each file can be read: the
/// output of `tui --check` and the sources screen.
///
/// Implements: SRS-TUI-009
#[must_use]
pub fn describe(l: &Launch, fs: &dyn Fs) -> String {
    let mut s = String::new();
    let dir = &l.state_dir.0;
    let _ = writeln!(s, "config      {}", l.config.1);
    let _ = writeln!(
        s,
        "state_dir   {}  ({})",
        escape_control(&dir.to_string_lossy()),
        l.state_dir.1
    );
    for name in [STATE_FILE, EVENTS_FILE, REPORTS] {
        let _ = writeln!(s, "  {name:<22}{}", readable(fs, &dir.join(name)));
    }
    match &l.log.0 {
        Some(p) => {
            let _ = writeln!(
                s,
                "log         {}  ({}): {}",
                escape_control(&p.to_string_lossy()),
                l.log.1,
                readable(fs, p)
            );
        }
        None => {
            let _ = writeln!(s, "log         none  ({})", l.log.1);
        }
    }
    let _ = writeln!(
        s,
        "theme       {}  ({}){}",
        l.theme.0.as_str(),
        l.theme.1,
        if l.no_color {
            "; NO_COLOR set: terminal colours only"
        } else {
            ""
        }
    );
    let _ = writeln!(
        s,
        "refresh     {}  ({})",
        humantime::format_duration(l.refresh.0),
        l.refresh.1
    );
    let _ = writeln!(
        s,
        "bell        {}  ({})",
        if l.bell.0 { "on" } else { "off" },
        l.bell.1
    );
    let _ = writeln!(s, "environment variables:");
    for (name, what) in ENV_VARS {
        let _ = writeln!(s, "  {name:<24}{what}");
    }
    s
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::fsio::RealFs;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn config(dir: &Path) -> PathBuf {
        let p = dir.join("config.toml");
        std::fs::write(
            &p,
            "dest_root = \"/mnt/absent\"\ntarget_id = \"t\"\nstate_dir = \"/srv/state\"\n\
             [source]\nroot = \"/absent\"\n\
             [logging]\nfile = \"/srv/log/ont.log\"\n\
             [tui]\ntheme = \"phosphor\"\nrefresh = \"5s\"\nbell = false\n",
        )
        .unwrap();
        p
    }

    /// Verifies: SRS-TUI-009
    #[test]
    fn flag_wins_over_environment_over_config_over_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let none = env_of(&[]);

        let l = resolve(&cfg, &RealFs, &Flags::default(), &none).unwrap();
        assert_eq!(l.state_dir, (PathBuf::from("/srv/state"), Origin::Config));
        assert_eq!(
            l.log,
            (Some(PathBuf::from("/srv/log/ont.log")), Origin::Config)
        );
        assert_eq!(l.theme, (TuiTheme::Phosphor, Origin::Config));
        assert_eq!(l.refresh, (Duration::from_secs(5), Origin::Config));
        assert_eq!(l.bell, (false, Origin::Config));

        let env = env_of(&[
            (ENV_STATE_DIR, "/env/state"),
            (ENV_LOG, "/env/ont.log"),
            (ENV_THEME, "matrix"),
            (ENV_REFRESH, "2s"),
            (ENV_BELL, "on"),
        ]);
        let l = resolve(&cfg, &RealFs, &Flags::default(), &env).unwrap();
        assert_eq!(
            l.state_dir,
            (PathBuf::from("/env/state"), Origin::Env(ENV_STATE_DIR))
        );
        assert_eq!(l.log.1, Origin::Env(ENV_LOG));
        assert_eq!(l.theme, (TuiTheme::Matrix, Origin::Env(ENV_THEME)));
        assert_eq!(l.refresh.0, Duration::from_secs(2));
        assert_eq!(l.bell, (true, Origin::Env(ENV_BELL)));

        let flags = Flags {
            state_dir: Some("/flag/state".into()),
            log: Some("/flag/ont.log".into()),
            theme: Some("synthwave".into()),
            refresh: Some("500ms".into()),
            bell: Some(false),
        };
        let l = resolve(&cfg, &RealFs, &flags, &env).unwrap();
        assert_eq!(l.state_dir, (PathBuf::from("/flag/state"), Origin::Flag));
        assert_eq!(l.log, (Some(PathBuf::from("/flag/ont.log")), Origin::Flag));
        assert_eq!(l.theme, (TuiTheme::Synthwave, Origin::Flag));
        assert_eq!(l.refresh, (Duration::from_millis(500), Origin::Flag));
        assert_eq!(l.bell, (false, Origin::Flag));
    }

    /// Verifies: SRS-TUI-009
    #[test]
    fn a_missing_config_file_uses_the_other_sources() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.toml");
        let l = resolve(&missing, &RealFs, &Flags::default(), &env_of(&[])).unwrap();
        assert!(l.config.1.contains("not found"), "{}", l.config.1);
        assert_eq!(
            l.state_dir,
            (PathBuf::from("/var/lib/ont-offload"), Origin::Default)
        );
        assert_eq!(l.log, (None, Origin::Default));
        assert_eq!(l.theme, (TuiTheme::Plain, Origin::Default));
        assert_eq!(l.refresh, (Duration::from_secs(1), Origin::Default));
        assert_eq!(l.bell, (true, Origin::Default));
        assert!(!l.no_color);
        // An empty variable counts as unset; NO_COLOR with any value sets it.
        let env = env_of(&[(ENV_THEME, " "), (ENV_NO_COLOR, "1")]);
        let l = resolve(&missing, &RealFs, &Flags::default(), &env).unwrap();
        assert_eq!(l.theme.1, Origin::Default);
        assert!(l.no_color);
    }

    /// Verifies: SRS-TUI-009
    #[test]
    fn bad_values_are_refused_with_their_source() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.toml");
        for (env, flags, needle) in [
            (vec![(ENV_THEME, "vaporwave")], Flags::default(), ENV_THEME),
            (vec![(ENV_REFRESH, "10ms")], Flags::default(), ENV_REFRESH),
            (vec![(ENV_BELL, "loud")], Flags::default(), ENV_BELL),
            (
                vec![],
                Flags {
                    theme: Some("x".into()),
                    ..Flags::default()
                },
                "--theme",
            ),
            (
                vec![],
                Flags {
                    refresh: Some("2m".into()),
                    ..Flags::default()
                },
                "--refresh",
            ),
        ] {
            let e = resolve(&missing, &RealFs, &flags, &env_of(&env)).unwrap_err();
            assert!(
                matches!(e, Error::Config(ref m) if m.contains(needle)),
                "{e}"
            );
        }
        // A configuration file that cannot be parsed is an error, not ignored.
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[tui]\ntheme = \"nope\"\n").unwrap();
        assert!(resolve(&bad, &RealFs, &Flags::default(), &env_of(&[])).is_err());
    }

    /// Verifies: SRS-TUI-009
    #[test]
    fn describe_shows_each_setting_its_origin_and_whether_files_can_be_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(STATE_FILE), "{}").unwrap();
        let env = env_of(&[
            (ENV_STATE_DIR, dir.path().to_str().unwrap()),
            (ENV_NO_COLOR, "1"),
        ]);
        let l = resolve(
            &dir.path().join("none.toml"),
            &RealFs,
            &Flags::default(),
            &env,
        )
        .unwrap();
        let s = describe(&l, &RealFs);
        assert!(s.contains(&format!("(${ENV_STATE_DIR})")), "{s}");
        assert!(s.contains("state.json            readable, 2 bytes"), "{s}");
        assert!(s.contains("events.jsonl          NOT READABLE"), "{s}");
        assert!(s.contains("log         none  (default)"), "{s}");
        assert!(s.contains("NO_COLOR set"), "{s}");
        for (name, _) in ENV_VARS {
            assert!(s.contains(name), "{name}");
        }
    }
}
