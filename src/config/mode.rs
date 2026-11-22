// SPDX-License-Identifier: AGPL-3.0-or-later
//! Permission modes of what ont-offload writes on the destination
//! (`transfer.file_mode`, `transfer.dir_mode`, `transfer.record_mode`):
//! octal such as `"0640"`, or chmod symbols such as `"u=rw,g=r,o="` or
//! `"a+rX"`, applied as `chmod` would to the mode a new file or folder has.

use std::fmt;
use std::path::Path;

use crate::fsio::Fs;

/// One `chmod` action: who, operator and permissions (`u+rw`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Action {
    /// Bits of the classes affected (`0o700` for `u`, `0o777` for `a`).
    who: u32,
    /// `=`, `+` or `-`.
    op: u8,
    /// `rwx` as `0o4 | 0o2 | 0o1`.
    perm: u32,
    /// `X`: execute only for folders and files that already have one.
    cond_x: bool,
}

/// A permission mode: an octal base, `chmod` actions applied after it, or
/// both (`file_mode` then `world_readable`'s `a+r`). Without a base the
/// actions start from the mode the file or folder was created with (the
/// process umask).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mode {
    base: Option<u32>,
    actions: Vec<Action>,
    text: Vec<String>,
}

impl Mode {
    /// An octal mode.
    #[must_use]
    pub fn octal(mode: u32) -> Self {
        Self {
            base: Some(mode & 0o777),
            actions: Vec::new(),
            text: Vec::new(),
        }
    }

    /// Parse `"0640"`, `"640"`, `"0o640"`, or `chmod` symbols: clauses
    /// separated by commas, each `[ugoa]*` followed by one or more of
    /// `=`, `+` or `-` and `[rwxX]*` (no who means `a`). setuid, setgid and
    /// sticky bits are not accepted.
    ///
    /// Implements: SRS-CPY-003
    ///
    /// # Errors
    /// A message naming what is wrong.
    pub fn parse(text: &str) -> Result<Self, String> {
        let s = text.trim();
        let digits = s.strip_prefix("0o").unwrap_or(s);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return match u32::from_str_radix(digits, 8) {
                Ok(m) if m <= 0o777 => Ok(Self::octal(m)),
                Ok(_) => Err(format!(
                    "{text:?}: setuid, setgid and sticky bits are not supported"
                )),
                Err(_) => Err(format!("{text:?} is not an octal mode such as \"0640\"")),
            };
        }
        let bad = || {
            format!(
                "{text:?} is not a mode: use octal such as \"0640\" or chmod symbols such as \
                 \"u=rw,g=r,o=\" or \"a+rX\""
            )
        };
        let mut out = Self::default();
        for clause in s.split(',') {
            let c = clause.trim();
            let mut chars = c.chars().peekable();
            let mut who = 0;
            while let Some(&ch) = chars.peek() {
                who |= match ch {
                    'u' => 0o700,
                    'g' => 0o070,
                    'o' => 0o007,
                    'a' => 0o777,
                    _ => break,
                };
                chars.next();
            }
            if who == 0 {
                who = 0o777;
            }
            let mut any = false;
            while let Some(op) = chars.next() {
                if !matches!(op, '=' | '+' | '-') {
                    return Err(bad());
                }
                let (mut perm, mut cond_x) = (0, false);
                while let Some(&ch) = chars.peek() {
                    match ch {
                        'r' => perm |= 0o4,
                        'w' => perm |= 0o2,
                        'x' => perm |= 0o1,
                        'X' => cond_x = true,
                        '=' | '+' | '-' => break,
                        _ => return Err(bad()),
                    }
                    chars.next();
                }
                out.actions.push(Action {
                    who,
                    op: op as u8,
                    perm,
                    cond_x,
                });
                any = true;
            }
            if !any {
                return Err(bad());
            }
            out.text.push(c.to_owned());
        }
        Ok(out)
    }

    /// This mode followed by `then`'s actions (an octal `then` replaces it).
    #[must_use]
    pub fn then(mut self, then: &Self) -> Self {
        if then.base.is_some() {
            return then.clone();
        }
        self.actions.extend_from_slice(&then.actions);
        self.text.extend(then.text.iter().cloned());
        self
    }

    /// The mode bits for a file or folder whose mode is now `current`.
    /// A folder keeps its setgid bit (inherited from its parent on many
    /// shares); nothing else outside `0o777` is ever set.
    #[must_use]
    pub fn apply(&self, current: u32, is_dir: bool) -> u32 {
        let mut m = self.base.unwrap_or(current & 0o777);
        for a in &self.actions {
            let mut perm = a.perm;
            if a.cond_x && (is_dir || m & 0o111 != 0) {
                perm |= 0o1;
            }
            let bits = (perm * 0o111) & a.who;
            m = match a.op {
                b'=' => (m & !a.who) | bits,
                b'+' => m | bits,
                _ => m & !bits,
            };
        }
        if is_dir {
            m |= current & 0o2000;
        }
        m
    }

    /// Whether the mode depends on the mode the file was created with.
    #[must_use]
    pub fn is_relative(&self) -> bool {
        self.base.is_none()
    }

    /// Set this mode on `path`. Relative modes read the current mode first.
    ///
    /// Implements: SRS-CPY-003
    ///
    /// # Errors
    /// I/O errors.
    pub fn set(&self, fs: &dyn Fs, path: &Path, is_dir: bool) -> std::io::Result<()> {
        let current = if self.is_relative() || is_dir {
            fs.symlink_metadata(path)?.mode
        } else {
            0
        };
        fs.set_mode(path, self.apply(current, is_dir))
    }

    /// For `check-config`: the octal result where it does not depend on
    /// the created mode, otherwise `umask then <symbols>`.
    #[must_use]
    pub fn describe(&self, is_dir: bool) -> String {
        let low = self.apply(0, is_dir);
        if self.base.is_some() || low == self.apply(0o777, is_dir) {
            format!("{low:04o}")
        } else {
            format!("umask then {}", self.text.join(","))
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.base, self.text.is_empty()) {
            (Some(b), true) => write!(f, "{b:04o}"),
            (Some(b), false) => write!(f, "{b:04o} then {}", self.text.join(",")),
            (None, _) => f.write_str(&self.text.join(",")),
        }
    }
}

/// The modes of everything written on the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputModes {
    /// Copied files (`transfer.file_mode`).
    pub file: Mode,
    /// Folders ont-offload creates (`transfer.dir_mode`; `None`: the umask).
    pub dir: Option<Mode>,
    /// Run records: manifest, `run.json`, `COMPLETE`, the share claim,
    /// `DELETED.json` (`transfer.record_mode`; `None`: the umask).
    pub record: Option<Mode>,
}

impl Default for OutputModes {
    fn default() -> Self {
        Self {
            file: Mode::octal(0o640),
            dir: None,
            record: None,
        }
    }
}

impl OutputModes {
    /// With `transfer.world_readable`: `a+r` after the file and record
    /// modes and `a+rx` after the folder mode.
    #[must_use]
    pub fn world_readable(self) -> Self {
        let r = Mode::parse("a+r").unwrap_or_default();
        let rx = Mode::parse("a+rx").unwrap_or_default();
        Self {
            file: self.file.then(&r),
            dir: Some(self.dir.unwrap_or_default().then(&rx)),
            record: Some(self.record.unwrap_or_default().then(&r)),
        }
    }

    /// The `permissions` line of `check-config`.
    #[must_use]
    pub fn summary(&self) -> String {
        let opt = |m: &Option<Mode>, dir: bool| {
            m.as_ref()
                .map_or_else(|| "by umask".to_owned(), |m| m.describe(dir))
        };
        format!(
            "files {}, folders {}, run records {}",
            self.file.describe(false),
            opt(&self.dir, true),
            opt(&self.record, false)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Mode {
        Mode::parse(s).unwrap()
    }

    /// Verifies: SRS-CPY-003
    #[test]
    fn octal_and_symbolic_modes_apply_as_chmod_does() {
        assert_eq!(p("0640").apply(0o777, false), 0o640);
        assert_eq!(p("640").apply(0, false), 0o640);
        assert_eq!(p("0o755").apply(0, true), 0o755);
        assert_eq!(p("u=rw,g=r,o=").apply(0o777, false), 0o640);
        assert_eq!(p("a+r").apply(0o640, false), 0o644);
        assert_eq!(p("go-rwx").apply(0o755, true), 0o700);
        assert_eq!(p("+r").apply(0o600, false), 0o644);
        assert_eq!(p("u=rwx,g=rx,o=rx").apply(0o700, true), 0o755);
        assert_eq!(p("ug=rw,o=r").apply(0, false), 0o664);
        assert_eq!(p("u+r-w").apply(0o200, false), 0o400);
        assert_eq!(p("g=").apply(0o770, true), 0o700);
        // X: execute for folders and for files that already have one.
        assert_eq!(p("a+rX").apply(0o750, true), 0o755);
        assert_eq!(p("a+rX").apply(0o640, false), 0o644);
        assert_eq!(p("a+rX").apply(0o740, false), 0o755);
        // A folder keeps an inherited setgid bit; files never get one.
        assert_eq!(p("0755").apply(0o2750, true), 0o2755);
        assert_eq!(p("0644").apply(0o2640, false), 0o644);
        // Composition, as world_readable does.
        let m = p("0640").then(&p("a+r"));
        assert_eq!(m.apply(0o600, false), 0o644);
        assert_eq!(m.to_string(), "0640 then a+r");
        assert_eq!(p("0700").then(&p("0644")), p("0644"));
    }

    /// Verifies: SRS-CPY-003
    #[test]
    fn invalid_modes_are_refused() {
        for bad in [
            "", "rw", "u", "u=rwz", "q+r", "0888", "4755", "01777", "u=s", "u+r,,g+r", "0x1ff",
        ] {
            assert!(Mode::parse(bad).is_err(), "{bad:?} accepted");
        }
    }

    /// Verifies: SRS-CPY-003
    #[test]
    fn world_readable_adds_read_to_files_and_records_and_rx_to_folders() {
        let m = OutputModes::default();
        assert_eq!(
            m.summary(),
            "files 0640, folders by umask, run records by umask"
        );
        let w = m.world_readable();
        assert_eq!(w.file.apply(0, false), 0o644);
        assert_eq!(w.dir.as_ref().unwrap().apply(0o750, true), 0o755);
        assert_eq!(w.dir.as_ref().unwrap().apply(0o700, true), 0o755);
        assert_eq!(w.record.as_ref().unwrap().apply(0o640, false), 0o644);
        assert_eq!(
            w.summary(),
            "files 0644, folders umask then a+rx, run records umask then a+r"
        );
        let fixed = OutputModes {
            file: p("0600"),
            dir: Some(p("0700")),
            record: Some(p("u=rw")),
        }
        .world_readable();
        assert_eq!(
            fixed.summary(),
            "files 0644, folders 0755, run records umask then u=rw,a+r"
        );
    }
}
