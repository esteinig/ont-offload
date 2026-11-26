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

/// A settled run whose last files are being copied reads "copying" with
/// its count, not "reconciling".
///
/// Verifies: SRS-TUI-006
#[test]
fn a_reconciling_run_reads_as_copying_n_of_m() {
    let snap = normal(&at(0));
    let mut run = snap.runs[0].clone();
    run.phase = "reconciling".into();
    run.files = 40;
    run.committed_files = 36;
    run.copying = 4;
    let (line, sev) = run_line(&run, '#');
    assert!(
        line.contains("copying ") && line.contains("36/40"),
        "{line}"
    );
    assert!(!line.contains("reconciling"), "{line}");
    assert_eq!(sev, Severity::Info);
    assert_eq!(run.phase_words(), "copying 36 of 40");
    let pal = Pal {
        th: ui(TuiTheme::Plain).theme,
        live: true,
    };
    let detail: String = run_detail(&run, &pal, 80)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                + "\n"
        })
        .collect();
    assert!(
        detail.contains("copying 36 of 40 (reconciling)"),
        "{detail}"
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

/// Verifies: SRS-TUI-010
#[test]
fn long_values_are_cut_in_the_middle_or_at_the_end() {
    assert_eq!(middle("short", 10), "short");
    let path = "exp1/sample_with_a_very_long_name/20261114_1200_X1_PAW00001_abcdef12";
    let cut = middle(path, 30);
    assert_eq!(cut.chars().count(), 30, "{cut}");
    assert!(cut.starts_with("exp1/s") && cut.contains("..."), "{cut}");
    assert!(
        cut.ends_with("PAW00001_abcdef12"),
        "the end tells runs apart: {cut}"
    );
    assert_eq!(middle(path, 3), "exp");
    assert_eq!(clip("a long message", 6), "a lon…");
    assert_eq!(clip("fits", 4), "fits");
    assert_eq!(clip("x", 0), "");
}

const LONG_RUN: &str = "minion-room-2/experiment_with_a_very_long_descriptive_name/sample_barcodes_01_to_24/20261114_1200_X1_PAW00001_abcdef12";

/// A model whose first run has a very long path, among runs with large
/// file counts, and `n` events, one of them long and about the long run.
fn long_model(n: u64) -> (tempfile::TempDir, Model) {
    let dir = tempfile::tempdir().unwrap();
    let mut snap = normal(&at(0));
    snap.runs[0].name = LONG_RUN.into();
    snap.runs[0].run_id = format!("id-{LONG_RUN}");
    snap.runs[0].source = format!("/data/{LONG_RUN}");
    snap.runs[0].destination = format!("/mnt/nas/ont/{LONG_RUN}");
    snap.runs[1].files = 123_456;
    snap.runs[1].committed_files = 123_456;
    write_state(dir.path(), &snap);
    let mut events: Vec<_> = (1..=n)
        .map(|i| {
            let secs = i64::try_from(n - i).unwrap();
            event(
                i,
                &at(-secs),
                Kind::FileCommitted,
                "info",
                &format!("committed pod5/batch_{i:04}.pod5"),
            )
        })
        .collect();
    let long = &mut events[0];
    long.run = Some(LONG_RUN.into());
    long.message = format!(
        "committed {}",
        "pod5/a_file_name_that_goes_on_and_on_".repeat(6)
    );
    append_events(dir.path(), &events);
    let mut m = Model::new(dir.path(), None);
    m.refresh(&RealFs, t0() + Duration::from_secs(1));
    (dir, m)
}

fn row_of(s: &str, needle: &str) -> String {
    s.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no {needle}: {s}"))
        .to_owned()
}

/// Verifies: SRS-TUI-006, SRS-TUI-010
#[test]
fn run_paths_are_cut_in_the_middle_and_columns_line_up() {
    let (_d, m) = long_model(3);
    let s = text(&draw(&m, &ui(TuiTheme::Plain)));
    let long = row_of(&s, "PAW00001_abcdef12");
    let short = row_of(&s, "exp2/s2/20261113_PAW00002");
    assert!(long.contains("minion-room-2/"), "the start is kept: {long}");
    assert!(long.contains("..."), "{long}");
    assert!(!long.contains(LONG_RUN), "{long}");
    assert!(long.contains("eta 15m 0s"), "notes still fit: {long}");
    let at_char = |line: &str, pat: &str| line.find(pat).map(|b| line[..b].chars().count());
    // Phase, bar, the slash of the counts and the bytes start in the same columns.
    assert_eq!(at_char(&long, "live"), at_char(&short, "complete"), "{s}");
    assert_eq!(at_char(&long, "███"), at_char(&short, "███"), "{s}");
    assert_eq!(at_char(&long, "/812"), at_char(&short, "/123456"), "{s}");
    assert_eq!(at_char(&long, "420 GB"), at_char(&short, "420 GB"), "{s}");
}

/// Verifies: SRS-TUI-010
#[test]
fn enter_shows_the_selected_run_in_full() {
    let (_d, m) = long_model(3);
    let mut u = ui(TuiTheme::Plain);
    u.move_by(1);
    u.move_by(5);
    u.clamp(&m);
    assert_eq!(u.scroll[Pane::Runs.index()], 1, "stops at the last run");
    u.move_by(-1);
    let s = text(&draw(&m, &u));
    assert!(row_of(&s, "PAW00001_abcdef12").contains('▸'), "{s}");
    u.open_detail(&m);
    assert_eq!(u.detail, Some(Detail::Run(format!("id-{LONG_RUN}"))));
    let s = text(&draw(&m, &u));
    golden("detail-run-plain", &s);
    assert!(s.contains(" Run (Esc to close"), "{s}");
    assert!(s.contains("790 of 812 committed"), "{s}");
    // Paths are broken across lines under their label, never shortened.
    assert_eq!(
        popup_value(&s, "destination"),
        format!("/mnt/nas/ont/{LONG_RUN}")
    );
    assert_eq!(popup_value(&s, "source"), format!("/data/{LONG_RUN}"));
}

/// The value after `label` in a detail popup, joined from its lines.
fn popup_value(screen: &str, label: &str) -> String {
    let rows: Vec<&str> = screen.lines().collect();
    let (y, row) = rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.contains(&format!("│{label:<16}")))
        .unwrap_or_else(|| panic!("no {label}: {screen}"));
    let x = row.find(&format!("│{label:<16}")).unwrap() + "│".len();
    let start = row[..x].chars().count();
    let cell = |r: &str| -> String {
        r.chars()
            .skip(start + 16)
            .take_while(|c| *c != '│')
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let mut value = cell(row);
    for r in &rows[y + 1..] {
        let indent: String = r.chars().skip(start).take(16).collect();
        if !indent.trim().is_empty() || cell(r).is_empty() {
            break;
        }
        value.push_str(&cell(r));
    }
    value
}

/// Verifies: SRS-TUI-010
#[test]
fn events_are_selected_from_the_newest_and_shown_in_full() {
    let (dir, mut m) = long_model(40);
    let mut u = ui(TuiTheme::Plain);
    u.focus = Pane::Events;
    let s = text(&draw(&m, &u));
    assert!(
        row_of(&s, "batch_0040").contains('▸'),
        "newest selected: {s}"
    );
    assert!(
        !s.contains("a_file_name"),
        "the oldest is not on screen: {s}"
    );
    // Up goes to older events, scrolling once past the top of the pane.
    u.move_by(-45);
    u.clamp(&m);
    let s = text(&draw(&m, &u));
    let long = row_of(&s, "a_file_name");
    assert!(long.contains('▸'), "{s}");
    assert!(long.contains('…'), "cut at the end: {long}");
    assert!(long.contains("..."), "run path cut in the middle: {long}");
    assert!(!s.contains("batch_0040"), "scrolled up: {s}");
    u.open_detail(&m);
    assert!(
        matches!(&u.detail, Some(Detail::Event { seq: 1, .. })),
        "{:?}",
        u.detail
    );
    let s = text(&draw(&m, &u));
    assert!(s.contains(" Event (Esc to close"), "{s}");
    assert_eq!(popup_value(&s, "run"), LONG_RUN, "{s}");
    assert_eq!(popup_value(&s, "kind"), "file_committed", "{s}");
    assert_eq!(popup_value(&s, "sequence"), "1", "{s}");

    // New events do not move the selection.
    u.detail = None;
    u.scroll[Pane::Events.index()] = 2;
    let anchor = u.anchor(&m);
    let before = selected_event(&m, &u).map(|e| e.seq);
    append_events(
        dir.path(),
        &[event(
            41,
            &at(1),
            Kind::FileCommitted,
            "info",
            "committed later",
        )],
    );
    m.refresh(&RealFs, t0() + Duration::from_secs(2));
    u.follow(&m, anchor);
    assert_eq!(selected_event(&m, &u).map(|e| e.seq), before);
    assert_eq!(u.scroll[Pane::Events.index()], 3);
}
