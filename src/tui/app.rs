// SPDX-License-Identifier: AGPL-3.0-or-later
//! The terminal loop: read at the refresh interval, draw, answer keys. The
//! terminal is restored on every exit, including a panic.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::TuiTheme;
use crate::error::{Error, Result};
use crate::fsio::Fs;

use super::launch::{Launch, describe};
use super::model::Model;
use super::theme::Theme;
use super::view::{Ui, render};

/// Animation step.
const TICK: Duration = Duration::from_millis(250);

/// Leaves raw mode and the alternate screen when dropped.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        Ok(guard)
    }
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// What a key does.
enum Action {
    Quit,
    Redraw,
    None,
}

fn next_theme(kind: TuiTheme) -> TuiTheme {
    let all = TuiTheme::ALL;
    let i = all.iter().position(|t| *t == kind).unwrap_or(0);
    all[(i + 1) % all.len()]
}

fn key(code: KeyCode, mods: KeyModifiers, m: &mut Model, ui: &mut Ui, no_color: bool) -> Action {
    if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }
    if ui.detail.is_some() {
        match code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Backspace => ui.detail = None,
            KeyCode::Up | KeyCode::Char('k') => {
                ui.detail_scroll = ui.detail_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => ui.detail_scroll += 1,
            KeyCode::PageUp => ui.detail_scroll = ui.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => ui.detail_scroll += 10,
            KeyCode::Home => ui.detail_scroll = 0,
            _ => return Action::None,
        }
        return Action::Redraw;
    }
    match code {
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Tab => ui.focus = ui.focus.next(),
        KeyCode::Up | KeyCode::Char('k') => ui.move_by(-1),
        KeyCode::Down | KeyCode::Char('j') => ui.move_by(1),
        KeyCode::PageUp => ui.move_by(-10),
        KeyCode::PageDown => ui.move_by(10),
        KeyCode::Home => ui.scroll = [0; 3],
        KeyCode::Enter => ui.open_detail(m),
        KeyCode::Char('a') => m.acknowledge(),
        KeyCode::Char('f') => {
            ui.filter = ui.filter.next();
            ui.scroll[2] = 0;
        }
        KeyCode::Char('l') => {
            ui.show_log = !ui.show_log;
            ui.scroll[2] = 0;
        }
        KeyCode::Char('t') => ui.theme = Theme::new(next_theme(ui.theme.kind), no_color),
        KeyCode::Char('b') => ui.bell = !ui.bell,
        KeyCode::Char('s') => {
            ui.show_sources = !ui.show_sources;
            ui.show_help = false;
        }
        KeyCode::Char('?' | 'h') => {
            ui.show_help = !ui.show_help;
            ui.show_sources = false;
        }
        _ => return Action::None,
    }
    ui.clamp(m);
    Action::Redraw
}

/// Run the dashboard until the operator quits. Reads only.
///
/// Implements: SRS-TUI-001, SRS-TUI-003, SRS-TUI-008
///
/// # Errors
/// [`Error::Config`] when standard output is not a terminal; terminal I/O
/// errors.
pub fn run(launch: &Launch, fs: &dyn Fs) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(Error::Config(
            "the dashboard needs a terminal; `ont-offload tui --check` shows its settings".into(),
        ));
    }
    let few_colors = crossterm::style::available_color_count() < 16;
    let no_color = launch.no_color || few_colors;
    let mut model = Model::new(&launch.state_dir.0, launch.log.0.as_deref());
    let mut ui = Ui::new(
        Theme::new(launch.theme.0, no_color),
        launch.bell.0,
        describe(launch, fs),
        SystemTime::now(),
    );
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;
    let refresh = launch.refresh.0;
    let mut next_read = Instant::now();
    let mut first = true;
    let mut next_tick = Instant::now() + TICK;
    loop {
        let now = Instant::now();
        if now >= next_read {
            let anchor = ui.anchor(&model);
            model.refresh(fs, SystemTime::now());
            ui.follow(&model, anchor);
            // No bell for what was already open at start: the banner shows it.
            if ui.bell && model.new_alerts > 0 && !first {
                let mut out = std::io::stdout();
                out.write_all(b"\x07")?;
                out.flush()?;
            }
            first = false;
            next_read = now + refresh;
        }
        if now >= next_tick {
            ui.tick = ui.tick.wrapping_add(1);
            next_tick = now + TICK;
        }
        ui.now = SystemTime::now();
        terminal.draw(|f| render(f, &model, &ui))?;
        let wait = next_read
            .min(next_tick)
            .saturating_duration_since(Instant::now());
        if event::poll(wait)? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    if let Action::Quit = key(k.code, k.modifiers, &mut model, &mut ui, no_color) {
                        return Ok(());
                    }
                    if ui.show_sources {
                        ui.sources = describe(launch, fs);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::fsio::RealFs;
    use crate::tui::fixtures::{at, normal, t0, write_state};

    /// Verifies: SRS-TUI-010
    #[test]
    fn enter_opens_the_details_and_esc_closes_them_before_quitting() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &normal(&at(0)));
        let mut m = Model::new(dir.path(), None);
        m.refresh(&RealFs, t0() + Duration::from_secs(1));
        let mut ui = Ui::new(
            Theme::new(TuiTheme::Plain, false),
            false,
            String::new(),
            t0(),
        );
        let none = KeyModifiers::NONE;
        assert!(matches!(
            key(KeyCode::Down, none, &mut m, &mut ui, false),
            Action::Redraw
        ));
        key(KeyCode::Down, none, &mut m, &mut ui, false);
        assert_eq!(ui.scroll[0], 1, "kept on the last run");
        key(KeyCode::Enter, none, &mut m, &mut ui, false);
        let id = m.snapshot.as_ref().unwrap().runs[1].run_id.clone();
        assert_eq!(ui.detail, Some(super::super::view::Detail::Run(id)));
        key(KeyCode::Down, none, &mut m, &mut ui, false);
        assert_eq!(ui.detail_scroll, 1, "arrows scroll the popup");
        assert_eq!(ui.scroll[0], 1, "not the list");
        assert!(matches!(
            key(KeyCode::Esc, none, &mut m, &mut ui, false),
            Action::Redraw
        ));
        assert!(ui.detail.is_none());
        assert!(matches!(
            key(KeyCode::Esc, none, &mut m, &mut ui, false),
            Action::Quit
        ));
    }
}
