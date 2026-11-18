// SPDX-License-Identifier: AGPL-3.0-or-later
//! What a stopped simulation needs to be resumed.
//!
//! A plan is a pure function of the scenario and the time it was planned at,
//! so `<out>/.ont-runsim/resume.json` keeps the scenario text (with the
//! command-line overrides), that time, the tool version and a fingerprint of
//! the profile and replay files the scenario reads. `run --resume` rebuilds
//! the same plan from it and refuses when any of these no longer match.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::scenario::{ContentMode, Scenario};
use crate::status::{self, resume_path};
use crate::{Error, Result, io_err, write_atomic};

/// Command-line values that override the scenario.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Overrides {
    /// `--hours`
    pub hours: Option<f64>,
    /// `--speed`
    pub speed: Option<f64>,
    /// `--seed`
    pub seed: Option<u64>,
    /// `--size-scale`
    pub size_scale: Option<f64>,
}

/// Contents of `resume.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Always `ont-runsim`.
    pub tool: String,
    /// Version that planned the simulation; resume needs the same version.
    pub version: String,
    /// `preset <name>` or the scenario file, for messages.
    pub source: String,
    /// Scenario TOML as read at the start.
    pub toml: String,
    /// Absolute folder that relative profile and replay paths are resolved against.
    pub base: Option<PathBuf>,
    /// Command-line overrides.
    pub overrides: Overrides,
    /// Unix second the plan was built for (names the run folders).
    pub planned_at: u64,
    /// SHA-256 over the profile file and the replay files' names, sizes and times.
    pub inputs: String,
}

impl Session {
    fn new(source: String, toml: String, base: Option<PathBuf>, overrides: Overrides) -> Self {
        Self {
            tool: "ont-runsim".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            source,
            toml,
            base,
            overrides,
            planned_at: 0,
            inputs: String::new(),
        }
    }

    /// A built-in preset.
    ///
    /// # Errors
    /// [`Error::Scenario`] for an unknown name.
    pub fn preset(name: &str, overrides: Overrides) -> Result<Self> {
        let toml = Scenario::preset_text(name)?;
        Ok(Self::new(
            format!("preset {name}"),
            toml.to_owned(),
            None,
            overrides,
        ))
    }

    /// A scenario file; its folder is made absolute so a resume from another
    /// working folder finds the same profile and replay files.
    ///
    /// # Errors
    /// I/O errors.
    pub fn file(path: &Path, overrides: Overrides) -> Result<Self> {
        let toml = std::fs::read_to_string(path).map_err(io_err("read", path))?;
        let abs = std::fs::canonicalize(path).map_err(io_err("resolve", path))?;
        Ok(Self::new(
            abs.display().to_string(),
            toml,
            abs.parent().map(Path::to_owned),
            overrides,
        ))
    }

    /// The scenario with the overrides applied, validated.
    ///
    /// # Errors
    /// [`Error::Scenario`].
    #[allow(clippy::similar_names)]
    pub fn scenario(&self) -> Result<Scenario> {
        let mut sc = Scenario::from_toml(&self.toml, self.base.as_deref())
            .map_err(|e| Error::Scenario(format!("{}: {e}", self.source)))?;
        let o = &self.overrides;
        if let Some(h) = o.hours {
            sc.hours = h;
        }
        if let Some(s) = o.speed {
            sc.speed = s;
        }
        if let Some(s) = o.seed {
            sc.seed = s;
        }
        if let Some(s) = o.size_scale {
            sc.fake.size_scale = s;
        }
        sc.validate()?;
        Ok(sc)
    }

    /// Fix the planning time (whole seconds, so it survives the round trip)
    /// and fingerprint the inputs; returns the planning time.
    ///
    /// # Errors
    /// I/O errors reading the inputs.
    pub fn stamp(&mut self, sc: &Scenario, now: SystemTime) -> Result<SystemTime> {
        self.planned_at = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.inputs = fingerprint(sc)?;
        Ok(self.plan_time())
    }

    /// The time the plan was built for.
    #[must_use]
    pub fn plan_time(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.planned_at)
    }

    /// Refuse to resume when the tool version or the inputs changed.
    ///
    /// # Errors
    /// [`Error::Refused`] naming what changed.
    pub fn check(&self, sc: &Scenario) -> Result<()> {
        let version = env!("CARGO_PKG_VERSION");
        if self.version != version {
            return Err(Error::Refused(format!(
                "the simulation was started by ont-runsim {}, this is {version}; resume it with that version",
                self.version
            )));
        }
        if fingerprint(sc)? != self.inputs {
            return Err(Error::Refused(
                "the profile or replay files of the scenario changed since the simulation started"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Write `resume.json` atomically.
    ///
    /// # Errors
    /// I/O errors.
    pub fn write(&self, out: &Path) -> Result<()> {
        let text = serde_json::to_vec_pretty(self).map_err(|e| Error::Scenario(e.to_string()))?;
        write_atomic(&resume_path(out), &text)
    }

    /// Read `resume.json`.
    ///
    /// # Errors
    /// [`Error::Refused`] when there is none (nothing was simulated there, or
    /// by a version without resume); format errors.
    pub fn read(out: &Path) -> Result<Self> {
        let p = resume_path(out);
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Refused(format!(
                    "nothing to resume in {}: no {} (no simulation there, or one started by a version without --resume)",
                    out.display(),
                    status::resume_path(Path::new("")).display()
                )));
            }
            Err(e) => return Err(io_err("read", &p)(e)),
        };
        serde_json::from_str(&text).map_err(|e| Error::Scenario(format!("{}: {e}", p.display())))
    }
}

/// SHA-256 over the files a scenario reads besides itself: the profile's
/// bytes, and the replay run's file names, sizes and modification times.
///
/// # Errors
/// I/O errors.
pub fn fingerprint(sc: &Scenario) -> Result<String> {
    let mut h = Sha256::new();
    if let Some(p) = &sc.profile {
        h.update(b"profile\0");
        h.update(std::fs::read(p).map_err(io_err("read", p))?);
    }
    if sc.content == ContentMode::Replay {
        if let Some(r) = &sc.replay {
            h.update(b"replay\0");
            let mut files = Vec::new();
            let mut stack = vec![r.run.clone()];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).map_err(io_err("read folder", &dir))? {
                    let path = entry.map_err(io_err("read folder", &dir))?.path();
                    let meta = std::fs::symlink_metadata(&path).map_err(io_err("stat", &path))?;
                    if meta.is_dir() {
                        stack.push(path);
                    } else if meta.is_file() {
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_nanos());
                        files.push(format!("{}\0{}\0{mtime}\n", path.display(), meta.len()));
                    }
                }
            }
            files.sort();
            for f in files {
                h.update(f.as_bytes());
            }
        }
    }
    Ok(h.finalize().iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn round_trip_rebuilds_the_same_scenario() {
        let dir = tempfile::tempdir().unwrap();
        let o = Overrides {
            hours: Some(3.0),
            speed: Some(10.0),
            seed: Some(5),
            size_scale: Some(0.01),
        };
        let mut s = Session::preset("quick-2h", o).unwrap();
        let sc = s.scenario().unwrap();
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_900);
        assert_eq!(
            s.stamp(&sc, t).unwrap(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
        std::fs::create_dir_all(status::state_dir(dir.path())).unwrap();
        s.write(dir.path()).unwrap();
        let back = Session::read(dir.path()).unwrap();
        let sc2 = back.scenario().unwrap();
        assert_eq!((sc2.hours, sc2.speed, sc2.seed), (3.0, 10.0, 5));
        assert!((sc2.fake.size_scale - 0.01).abs() < f64::EPSILON);
        assert_eq!(back.planned_at, 1_700_000_000);
        back.check(&sc2).unwrap();
    }

    #[test]
    fn changed_profile_or_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("p.json");
        std::fs::write(&profile, "{}").unwrap();
        let scenario = dir.path().join("s.toml");
        std::fs::write(&scenario, "").unwrap();
        let mut s = Session::file(&scenario, Overrides::default()).unwrap();
        assert!(s.base.as_ref().unwrap().is_absolute());
        let mut sc = s.scenario().unwrap();
        sc.profile = Some(profile.clone());
        s.stamp(&sc, SystemTime::now()).unwrap();
        s.check(&sc).unwrap();
        std::fs::write(&profile, "{ }").unwrap();
        assert!(matches!(s.check(&sc), Err(Error::Refused(_))));
        std::fs::write(&profile, "{}").unwrap();
        s.version = "0.0.0".into();
        assert!(matches!(s.check(&sc), Err(Error::Refused(_))));
    }

    #[test]
    fn missing_resume_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(Session::read(dir.path()), Err(Error::Refused(_))));
    }
}
