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
    match code {
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return Action::Quit,
        KeyCode::Tab => ui.focus = ui.focus.next(),
        KeyCode::Up | KeyCode::Char('k') => ui.scroll_by(1),
        KeyCode::Down | KeyCode::Char('j') => ui.scroll_by(-1),
        KeyCode::PageUp => ui.scroll_by(10),
        KeyCode::PageDown => ui.scroll_by(-10),
        KeyCode::Home => ui.scroll = [0; 3],
        KeyCode::Char('a') => m.acknowledge(),
        KeyCode::Char('f') => ui.filter = ui.filter.next(),
        KeyCode::Char('l') => ui.show_log = !ui.show_log,
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
            model.refresh(fs, SystemTime::now());
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
