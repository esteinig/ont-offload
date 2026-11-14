// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dashboard themes. A theme sets colours and decoration only: severity is
//! always also written as a word and a symbol, so no theme can hide it.

use crate::config::TuiTheme;

/// A colour: the terminal's own, or 24-bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    /// The terminal's default foreground or background.
    Default,
    /// One of the terminal's 16 named colours (plain theme).
    Named(Named),
    /// An exact colour.
    Rgb(u8, u8, u8),
}

/// Named terminal colours used by the plain theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    /// Red.
    Red,
    /// Yellow.
    Yellow,
    /// Green.
    Green,
    /// Cyan.
    Cyan,
    /// Dark grey.
    Gray,
}

/// Decoration drawn while nothing needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoration {
    /// None.
    None,
    /// Falling characters in the header.
    Rain,
    /// A blinking block cursor after the newest event.
    Cursor,
    /// A moving horizon line under the header.
    Horizon,
}

/// Severity of something shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Fine.
    Ok,
    /// Information.
    Info,
    /// Needs a look.
    Warn,
    /// Failed.
    Fail,
    /// Not current.
    Stale,
}

impl Severity {
    /// From an event or alert level.
    #[must_use]
    pub fn of_level(level: &str) -> Self {
        match level {
            "error" => Self::Fail,
            "warning" => Self::Warn,
            _ => Self::Info,
        }
    }

    /// The word and symbol that carry the severity in every theme.
    ///
    /// Implements: SRS-TUI-004
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "● OK",
            Self::Info => "· INFO",
            Self::Warn => "▲ WARN",
            Self::Fail => "✖ FAIL",
            Self::Stale => "◷ STALE",
        }
    }
}

/// Colours and decoration of one theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Which theme.
    pub kind: TuiTheme,
    /// Background.
    pub bg: Paint,
    /// Normal text.
    pub fg: Paint,
    /// Secondary text (and everything while stale).
    pub dim: Paint,
    /// Titles and highlights.
    pub accent: Paint,
    /// Frames.
    pub border: Paint,
    /// Fine.
    pub ok: Paint,
    /// Warnings.
    pub warn: Paint,
    /// Failures.
    pub fail: Paint,
    /// Failures are drawn in reverse video (themes where the failure colour
    /// is close to the text colour).
    pub fail_reverse: bool,
    /// Gauge fill character.
    pub gauge: char,
    /// Decoration.
    pub decoration: Decoration,
    /// Frame style: rounded, plain, double or thick.
    pub frame: Frame,
}

/// Frame style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// Single lines.
    Plain,
    /// Rounded corners.
    Rounded,
    /// Double lines.
    Double,
    /// Thick lines.
    Thick,
}

impl Theme {
    /// The theme, or the plain monochrome theme when `no_color` (the
    /// `NO_COLOR` convention, or a terminal with fewer than 16 colours).
    ///
    /// Implements: SRS-TUI-004, SRS-TUI-008
    #[must_use]
    pub fn new(kind: TuiTheme, no_color: bool) -> Self {
        if no_color {
            return Self::mono();
        }
        match kind {
            TuiTheme::Plain => Self {
                kind,
                bg: Paint::Default,
                fg: Paint::Default,
                dim: Paint::Named(Named::Gray),
                accent: Paint::Named(Named::Cyan),
                border: Paint::Default,
                ok: Paint::Named(Named::Green),
                warn: Paint::Named(Named::Yellow),
                fail: Paint::Named(Named::Red),
                fail_reverse: true,
                gauge: '█',
                decoration: Decoration::None,
                frame: Frame::Plain,
            },
            TuiTheme::Matrix => Self {
                kind,
                bg: Paint::Rgb(0, 0, 0),
                fg: Paint::Rgb(0, 255, 65),
                dim: Paint::Rgb(0, 160, 40),
                accent: Paint::Rgb(190, 255, 200),
                border: Paint::Rgb(0, 160, 40),
                ok: Paint::Rgb(0, 255, 65),
                warn: Paint::Rgb(220, 255, 60),
                fail: Paint::Rgb(255, 255, 255),
                fail_reverse: true,
                gauge: '▓',
                decoration: Decoration::Rain,
                frame: Frame::Plain,
            },
            TuiTheme::Phosphor => Self {
                kind,
                bg: Paint::Rgb(18, 10, 0),
                fg: Paint::Rgb(255, 176, 0),
                dim: Paint::Rgb(190, 125, 0),
                accent: Paint::Rgb(255, 214, 120),
                border: Paint::Rgb(190, 125, 0),
                ok: Paint::Rgb(255, 176, 0),
                warn: Paint::Rgb(255, 230, 160),
                fail: Paint::Rgb(255, 245, 225),
                fail_reverse: true,
                gauge: '█',
                decoration: Decoration::Cursor,
                frame: Frame::Double,
            },
            TuiTheme::Synthwave => Self {
                kind,
                bg: Paint::Rgb(34, 14, 56),
                fg: Paint::Rgb(242, 232, 255),
                dim: Paint::Rgb(180, 150, 215),
                accent: Paint::Rgb(255, 92, 205),
                border: Paint::Rgb(0, 229, 255),
                ok: Paint::Rgb(0, 229, 255),
                warn: Paint::Rgb(255, 204, 0),
                fail: Paint::Rgb(255, 92, 120),
                fail_reverse: false,
                gauge: '▰',
                decoration: Decoration::Horizon,
                frame: Frame::Thick,
            },
        }
    }

    /// Terminal colours only, failures in reverse video.
    #[must_use]
    pub fn mono() -> Self {
        Self {
            kind: TuiTheme::Plain,
            bg: Paint::Default,
            fg: Paint::Default,
            dim: Paint::Default,
            accent: Paint::Default,
            border: Paint::Default,
            ok: Paint::Default,
            warn: Paint::Default,
            fail: Paint::Default,
            fail_reverse: true,
            gauge: '█',
            decoration: Decoration::None,
            frame: Frame::Plain,
        }
    }

    /// The colour for a severity.
    #[must_use]
    pub fn paint(&self, s: Severity) -> Paint {
        match s {
            Severity::Ok => self.ok,
            Severity::Info => self.fg,
            Severity::Warn => self.warn,
            Severity::Fail => self.fail,
            Severity::Stale => self.dim,
        }
    }
}

fn linear(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.039_28 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// WCAG contrast ratio of two colours (1 to 21).
#[must_use]
pub fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Problems with a theme's contrast: text, warnings and failures need 4.5:1
/// against the background (WCAG AA), secondary text and frames 3:1. Themes
/// in terminal colours cannot be measured and pass.
///
/// Implements: SRS-TUI-004
#[must_use]
pub fn contrast_problems(t: &Theme) -> Vec<String> {
    let Paint::Rgb(br, bgc, bb) = t.bg else {
        return Vec::new();
    };
    let bg = (br, bgc, bb);
    let mut out = Vec::new();
    for (name, paint, min) in [
        ("text", t.fg, 4.5),
        ("ok", t.ok, 4.5),
        ("warning", t.warn, 4.5),
        ("failure", t.fail, 4.5),
        ("accent", t.accent, 4.5),
        ("secondary", t.dim, 3.0),
        ("frame", t.border, 3.0),
    ] {
        if let Paint::Rgb(r, g, b) = paint {
            let c = contrast((r, g, b), bg);
            if c < min {
                out.push(format!(
                    "{}: {name} has contrast {c:.1}:1, needs {min}:1",
                    t.kind.as_str()
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies: SRS-TUI-004
    #[test]
    fn every_theme_passes_the_contrast_check() {
        assert!((contrast((0, 0, 0), (255, 255, 255)) - 21.0).abs() < 0.01);
        assert!((contrast((7, 7, 7), (7, 7, 7)) - 1.0).abs() < 0.01);
        for kind in TuiTheme::ALL {
            for no_color in [false, true] {
                let t = Theme::new(kind, no_color);
                assert_eq!(contrast_problems(&t), Vec::<String>::new(), "{kind:?}");
            }
        }
        // And the check finds a bad palette.
        let mut bad = Theme::new(TuiTheme::Matrix, false);
        bad.warn = Paint::Rgb(0, 30, 0);
        assert!(contrast_problems(&bad)[0].contains("warning has contrast"));
    }

    /// Verifies: SRS-TUI-004
    #[test]
    fn severity_is_written_as_a_distinct_word_and_symbol() {
        let all = [
            Severity::Ok,
            Severity::Info,
            Severity::Warn,
            Severity::Fail,
            Severity::Stale,
        ];
        let labels: Vec<&str> = all.iter().map(|s| s.label()).collect();
        for (i, l) in labels.iter().enumerate() {
            let (sym, word) = l.split_once(' ').unwrap();
            assert_eq!(sym.chars().count(), 1, "{l}");
            assert!(word.chars().all(|c| c.is_ascii_uppercase()), "{l}");
            assert!(
                labels.iter().skip(i + 1).all(|o| !o.starts_with(sym)),
                "{l}"
            );
        }
        assert_eq!(Severity::of_level("error"), Severity::Fail);
        assert_eq!(Severity::of_level("warning"), Severity::Warn);
        assert_eq!(Severity::of_level("info"), Severity::Info);
    }

    /// Verifies: SRS-TUI-004, SRS-TUI-008
    #[test]
    fn no_color_gives_terminal_colours_and_reverse_failures() {
        for kind in TuiTheme::ALL {
            let t = Theme::new(kind, true);
            assert_eq!(t, Theme::mono());
            assert!(t.fail_reverse);
            assert_eq!(t.decoration, Decoration::None);
        }
        let kinds: Vec<_> = TuiTheme::ALL
            .iter()
            .map(|k| Theme::new(*k, false).decoration)
            .collect();
        assert_eq!(
            kinds,
            [
                Decoration::None,
                Decoration::Rain,
                Decoration::Cursor,
                Decoration::Horizon
            ]
        );
    }
}
