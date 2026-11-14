// SPDX-License-Identifier: AGPL-3.0-or-later
//! Render tests: each scenario drawn into a test terminal and compared with
//! a golden text file (`UPDATE_GOLDEN=1` rewrites them), plus checks that
//! severity is never carried by colour alone and that decoration pauses
//! while something needs attention.

use std::path::PathBuf;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

use super::*;
use crate::config::TuiTheme;
use crate::fsio::RealFs;
use crate::state::Kind;
use crate::tui::fixtures::{
    append_events, append_report, at, event, failed_run, normal, outage, refused, refused_report,
    stalled, t0, write_state,
};

const W: u16 = 132;
const H: u16 = 40;

fn draw(m: &Model, ui: &Ui) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(W, H)).unwrap();
    t.draw(|f| render(f, m, ui)).unwrap();
    t.backend().buffer().clone()
}

fn text(b: &Buffer) -> String {
    let mut s = String::new();
    for y in 0..b.area.height {
        let mut line = String::new();
        for x in 0..b.area.width {
            line.push_str(b[(x, y)].symbol());
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    s
}

fn ui(kind: TuiTheme) -> Ui {
    let mut u = Ui::new(
        Theme::new(kind, false),
        true,
        "sources".into(),
        t0() + Duration::from_secs(1),
    );
    u.tick = 3;
    u
}

/// A model of `scenario` at `t0 + 1 s`, with a few events.
fn model(scenario: &str) -> (tempfile::TempDir, Model) {
    let dir = tempfile::tempdir().unwrap();
    let now = at(0);
    let snap = match scenario {
        "normal" | "stale" => Some(normal(&now)),
        "outage" => Some(outage(&now)),
        "stalled" => Some(stalled(&now)),
        "failed" => Some(failed_run(&now)),
        "refused" => Some(refused(&now)),
        _ => None,
    };
    if let Some(s) = &snap {
        write_state(dir.path(), s);
    }
    if scenario == "refused" {
        append_report(dir.path(), &refused_report());
    }
    append_events(
        dir.path(),
        &[
            event(1, &at(-20), Kind::Started, "info", "ont-offload started"),
            event(
                2,
                &at(-10),
                Kind::FileCommitted,
                "info",
                "committed pod5/batch_0790.pod5",
            ),
        ],
    );
    // No data: a fixed path, so that the message (and its wrapping) is
    // the same on every machine.
    let state = if snap.is_some() {
        dir.path().to_path_buf()
    } else {
        PathBuf::from("/nonexistent/ont-offload-state")
    };
    let mut m = Model::new(&state, None);
    let when = if scenario == "stale" { 120 } else { 1 };
    m.refresh(&RealFs, t0() + Duration::from_secs(when));
    (dir, m)
}

fn golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/tui/view/golden")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; run with UPDATE_GOLDEN=1", path.display()));
    assert_eq!(actual, expected, "{name} differs from {}", path.display());
}

/// Verifies: SRS-TUI-002, SRS-TUI-003, SRS-TUI-006
#[test]
fn every_scenario_renders_as_its_golden_text() {
    for scenario in [
        "normal", "outage", "stalled", "failed", "refused", "stale", "nodata",
    ] {
        let (_d, m) = model(scenario);
        let mut u = ui(TuiTheme::Plain);
        if scenario == "stale" {
            u.now = t0() + Duration::from_secs(120);
        }
        golden(&format!("{scenario}-plain"), &text(&draw(&m, &u)));
    }
}

/// Verifies: SRS-TUI-004, SRS-TUI-008
#[test]
fn every_theme_renders_as_its_golden_text() {
    for kind in TuiTheme::ALL {
        let (_d, m) = model("normal");
        golden(
            &format!("theme-{}", kind.as_str()),
            &text(&draw(&m, &ui(kind))),
        );
    }
}

/// Verifies: SRS-TUI-002
#[test]
fn stale_and_missing_data_cover_the_screen_with_the_last_good_time() {
    let (_d, m) = model("stale");
    let mut u = ui(TuiTheme::Plain);
    u.now = t0() + Duration::from_secs(120);
    let s = text(&draw(&m, &u));
    assert!(s.contains("◷ STALE: NOT CURRENT"), "{s}");
    assert!(s.contains(&at(0)), "{s}");
    assert!(s.contains("not current"), "header says so too");
    for ok in ["● OK ok", "● OK no open", "of 4  ● OK"] {
        assert!(
            !s.contains(ok),
            "no live state is called OK while not current: {s}"
        );
    }

    let (_d, m) = model("nodata");
    let s = text(&draw(&m, &ui(TuiTheme::Plain)));
    assert!(s.contains("◷ NO DATA"), "{s}");
}

/// Verifies: SRS-TUI-006
#[test]
fn a_run_is_complete_only_with_a_manifest_digest() {
    let snap = normal(&at(0));
    let done = &snap.runs[1];
    let (line, sev) = run_line(done, '#');
    assert!(line.contains("complete "), "{line}");
    assert_eq!(sev, Severity::Ok);
    let mut no_digest = done.clone();
    no_digest.manifest_sha256 = None;
    let (line, sev) = run_line(&no_digest, '#');
    assert!(line.contains("complete?"), "{line}");
    assert_eq!(sev, Severity::Warn);
    let failed = &failed_run(&at(0)).runs[0];
    let (line, sev) = run_line(failed, '#');
    assert_eq!(sev, Severity::Fail);
    assert!(
        line.starts_with("✖ FAIL") && line.contains("1 FAILED") && line.contains("error: "),
        "{line}"
    );
    assert!(
        line.contains("790/812"),
        "progress as the snapshot gives it: {line}"
    );
}

fn find(b: &Buffer, needle: &str) -> Option<(u16, u16)> {
    let first = needle.chars().next()?.to_string();
    for y in 0..b.area.height {
        for x in 0..b.area.width {
            if b[(x, y)].symbol() == first {
                let rest: String = (x..b
                    .area
                    .width
                    .min(x + u16::try_from(needle.chars().count()).unwrap()))
                    .map(|i| b[(i, y)].symbol().to_owned())
                    .collect();
                if rest == needle {
                    return Some((x, y));
                }
            }
        }
    }
    None
}

/// Failures carry a word and a symbol, and stand out from text in every
/// theme and without colour.
///
/// Verifies: SRS-TUI-004
#[test]
fn failures_are_shown_in_words_and_stand_out_in_every_theme() {
    let (_dir, model) = model("failed");
    let mut themes: Vec<Theme> = TuiTheme::ALL
        .iter()
        .map(|k| Theme::new(*k, false))
        .collect();
    themes.push(Theme::new(TuiTheme::Matrix, true));
    for th in themes {
        let mut screen = ui(th.kind);
        screen.theme = th;
        let b = draw(&model, &screen);
        let (x, y) = find(&b, "✖ FAIL").unwrap_or_else(|| panic!("{:?}: no FAIL", th.kind));
        let cell = &b[(x, y)];
        let stands_out = cell.modifier.contains(Modifier::REVERSED)
            || (cell.fg != color(th.fg) && cell.modifier.contains(Modifier::BOLD));
        assert!(stands_out, "{:?}: {cell:?}", th.kind);
        assert!(text(&b).contains("run failed"), "banner");
    }
}

/// Verifies: SRS-TUI-004
#[test]
fn no_color_draws_in_terminal_colours_only() {
    let (_d, m) = model("outage");
    let mut u = ui(TuiTheme::Synthwave);
    u.theme = Theme::new(TuiTheme::Synthwave, true);
    let b = draw(&m, &u);
    for c in b.content() {
        assert!(matches!(c.fg, Color::Reset), "{c:?}");
        assert!(matches!(c.bg, Color::Reset), "{c:?}");
    }
}

/// Verifies: SRS-TUI-008
#[test]
fn decoration_moves_when_all_is_well_and_pauses_for_alerts_and_staleness() {
    for kind in [TuiTheme::Matrix, TuiTheme::Synthwave] {
        let (_d, m) = model("normal");
        let mut a = ui(kind);
        let mut b = ui(kind);
        b.tick = 40;
        assert_ne!(
            text(&draw(&m, &a)),
            text(&draw(&m, &b)),
            "{kind:?} animates"
        );

        let (_d, m) = model("outage");
        a.tick = 0;
        b.tick = 41;
        assert_eq!(
            text(&draw(&m, &a)),
            text(&draw(&m, &b)),
            "{kind:?} paused by the banner"
        );
        let mut m = m;
        m.acknowledge();
        assert_eq!(
            text(&draw(&m, &a)),
            text(&draw(&m, &b)),
            "{kind:?}: still paused while the alert is open"
        );

        let (_d, m) = model("stale");
        let mut a = ui(kind);
        a.now = t0() + Duration::from_secs(120);
        let mut b = a.clone();
        b.tick = 41;
        assert_eq!(
            text(&draw(&m, &a)),
            text(&draw(&m, &b)),
            "{kind:?} paused while stale"
        );
    }
}

/// Verifies: SRS-TUI-003
#[test]
fn the_banner_shows_until_acknowledged() {
    let (_d, mut m) = model("refused");
    let u = ui(TuiTheme::Phosphor);
    let s = text(&draw(&m, &u));
    assert!(s.contains("1 alert(s) to acknowledge"), "{s}");
    assert!(
        s.contains("refused /data/exp0/s0/20261101_PAW00000"),
        "cleanup names the run: {s}"
    );
    m.acknowledge();
    let s = text(&draw(&m, &u));
    assert!(!s.contains("to acknowledge"), "{s}");
    assert!(
        s.contains("manifest does not match the source"),
        "still in the alerts panel: {s}"
    );
}
