// SPDX-License-Identifier: AGPL-3.0-or-later
//! Drawing the dashboard with Ratatui. Everything shown comes from the
//! model as read; nothing is derived that the snapshot does not say.

use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Sparkline, Wrap};

use crate::config::units::Bytes;
use crate::sched::human;
use crate::state::{Event, RunState, Snapshot};

use super::model::{Health, Model};
use super::theme::{Decoration, Frame as FrameStyle, Named, Paint, Severity, Theme};

/// The panes that scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// Runs table.
    Runs,
    /// Alerts.
    Alerts,
    /// Events (or the log).
    Events,
}

impl Pane {
    fn index(self) -> usize {
        match self {
            Self::Runs => 0,
            Self::Alerts => 1,
            Self::Events => 2,
        }
    }

    /// The next pane.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Runs => Self::Alerts,
            Self::Alerts => Self::Events,
            Self::Events => Self::Runs,
        }
    }
}

/// Which events are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// All.
    All,
    /// Warnings and errors.
    Warn,
    /// Errors only.
    Error,
}

impl Filter {
    /// The next filter.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Warn,
            Self::Warn => Self::Error,
            Self::Error => Self::All,
        }
    }

    fn admits(self, level: &str) -> bool {
        match self {
            Self::All => true,
            Self::Warn => level == "warning" || level == "error",
            Self::Error => level == "error",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Warn => "warn+",
            Self::Error => "errors",
        }
    }
}

/// The item a detail popup shows, kept by identity so that the popup stays
/// on the same run or event while the lists change underneath.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detail {
    /// A run, by run id.
    Run(String),
    /// An event, by sequence number and time (the sequence restarts with
    /// the daemon).
    Event {
        /// Sequence number.
        seq: u64,
        /// Time (RFC 3339).
        ts: String,
    },
}

/// Screen state that is not in the files.
#[allow(clippy::struct_excessive_bools)] // independent screen toggles
#[derive(Debug, Clone)]
pub struct Ui {
    /// Theme.
    pub theme: Theme,
    /// Animation step.
    pub tick: u64,
    /// Focused pane.
    pub focus: Pane,
    /// Position per pane: the selected run (from the top), the alerts
    /// scrolled past, and the selected event or log line (from the
    /// newest).
    pub scroll: [usize; 3],
    /// Event filter.
    pub filter: Filter,
    /// Show the log instead of the events.
    pub show_log: bool,
    /// Show the sources screen.
    pub show_sources: bool,
    /// Text of the sources screen.
    pub sources: String,
    /// Show the key help.
    pub show_help: bool,
    /// The detail popup, when open.
    pub detail: Option<Detail>,
    /// Lines scrolled in the detail popup.
    pub detail_scroll: usize,
    /// Bell on.
    pub bell: bool,
    /// Wall-clock time of this frame.
    pub now: SystemTime,
}

impl Ui {
    /// A screen in `theme` at `now`.
    #[must_use]
    pub fn new(theme: Theme, bell: bool, sources: String, now: SystemTime) -> Self {
        Self {
            theme,
            tick: 0,
            focus: Pane::Runs,
            scroll: [0; 3],
            filter: Filter::All,
            show_log: false,
            show_sources: false,
            sources,
            show_help: false,
            detail: None,
            detail_scroll: 0,
            bell,
            now,
        }
    }

    /// Move the selection of the focused pane `down` lines (negative: up).
    /// Runs and alerts count from the top, events and the log from the
    /// newest at the bottom, so up always goes up the screen.
    pub fn move_by(&mut self, down: isize) {
        let delta = if self.focus == Pane::Events {
            -down
        } else {
            down
        };
        let s = &mut self.scroll[self.focus.index()];
        *s = s.saturating_add_signed(delta);
    }

    /// Keep each position inside its list.
    pub fn clamp(&mut self, m: &Model) {
        for pane in [Pane::Runs, Pane::Alerts, Pane::Events] {
            let n = pane_len(m, self, pane);
            let s = &mut self.scroll[pane.index()];
            *s = (*s).min(n.saturating_sub(1));
        }
    }

    /// The event selected in the event pane, if one is selected above the
    /// newest (see [`Ui::follow`]).
    #[must_use]
    pub fn anchor(&self, m: &Model) -> Option<(u64, String)> {
        if self.show_log || self.scroll[Pane::Events.index()] == 0 {
            return None;
        }
        selected_event(m, self).map(|e| (e.seq, e.ts.clone()))
    }

    /// After a refresh, keep the selection on the event `anchor` named, so
    /// that new events do not move it. Without an anchor the newest event
    /// stays selected and the pane follows the stream.
    pub fn follow(&mut self, m: &Model, anchor: Option<(u64, String)>) {
        if let Some((seq, ts)) = anchor {
            let shown = shown_events(m, self.filter);
            if let Some(i) = shown.iter().rposition(|e| e.seq == seq && e.ts == ts) {
                self.scroll[Pane::Events.index()] = shown.len() - 1 - i;
            }
        }
        self.clamp(m);
    }

    /// Open the detail popup on the selected run or event (nothing for the
    /// alerts and the log, whose lines are shown in full).
    pub fn open_detail(&mut self, m: &Model) {
        self.detail = match self.focus {
            Pane::Runs => selected_run(m, self).map(|r| Detail::Run(r.run_id.clone())),
            Pane::Events if !self.show_log => selected_event(m, self).map(|e| Detail::Event {
                seq: e.seq,
                ts: e.ts.clone(),
            }),
            _ => None,
        };
        self.detail_scroll = 0;
    }
}

fn runs_of(m: &Model) -> &[RunState] {
    m.snapshot
        .as_ref()
        .map(|s| s.runs.as_slice())
        .unwrap_or_default()
}

/// The events the filter admits, oldest first.
fn shown_events(m: &Model, filter: Filter) -> Vec<&Event> {
    m.events
        .iter()
        .filter(|e| filter.admits(&e.level))
        .collect()
}

fn pane_len(m: &Model, ui: &Ui, pane: Pane) -> usize {
    match pane {
        Pane::Runs => runs_of(m).len(),
        Pane::Alerts => m.alerts.len() + m.cleared.len(),
        Pane::Events if ui.show_log => m.log.len(),
        Pane::Events => shown_events(m, ui.filter).len(),
    }
}

fn selected_run<'a>(m: &'a Model, ui: &Ui) -> Option<&'a RunState> {
    let list = runs_of(m);
    list.get(ui.scroll[Pane::Runs.index()].min(list.len().saturating_sub(1)))
}

fn selected_event<'a>(m: &'a Model, ui: &Ui) -> Option<&'a Event> {
    let shown = shown_events(m, ui.filter);
    let back = ui.scroll[Pane::Events.index()].min(shown.len().saturating_sub(1));
    shown.len().checked_sub(back + 1).map(|i| shown[i])
}

/// `s` in at most `width` characters: the start, `...`, and the end, which
/// for a run path holds the flow cell and run id that tell runs apart.
fn middle(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return s.to_owned();
    }
    if width <= 3 {
        return s.chars().take(width).collect();
    }
    let keep = width - 3;
    let head = keep * 2 / 5;
    let tail = keep - head;
    let start: String = s.chars().take(head).collect();
    let end: String = s.chars().skip(n - tail).collect();
    format!("{start}...{end}")
}

/// `s` in at most `width` characters, cut at the end with `…`.
fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

fn color(p: Paint) -> Color {
    match p {
        Paint::Default => Color::Reset,
        Paint::Named(Named::Red) => Color::Red,
        Paint::Named(Named::Yellow) => Color::Yellow,
        Paint::Named(Named::Green) => Color::Green,
        Paint::Named(Named::Cyan) => Color::Cyan,
        Paint::Named(Named::Gray) => Color::DarkGray,
        Paint::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Styles for one frame: everything dims while the data is not current.
struct Pal {
    th: Theme,
    live: bool,
}

impl Pal {
    fn base(&self) -> Style {
        Style::default().fg(color(self.th.fg)).bg(color(self.th.bg))
    }
    fn text(&self) -> Style {
        if self.live { self.base() } else { self.dim() }
    }
    fn dim(&self) -> Style {
        self.base().fg(color(self.th.dim))
    }
    fn accent(&self) -> Style {
        if self.live {
            self.base()
                .fg(color(self.th.accent))
                .add_modifier(Modifier::BOLD)
        } else {
            self.dim()
        }
    }
    fn border(&self) -> Style {
        self.base().fg(color(self.th.border))
    }
    /// The style of a severity. Failures stay prominent even when stale.
    fn sev(&self, s: Severity) -> Style {
        let st = self.base().fg(color(self.th.paint(s)));
        match s {
            Severity::Fail if self.th.fail_reverse => {
                st.add_modifier(Modifier::REVERSED | Modifier::BOLD)
            }
            Severity::Fail | Severity::Warn => st.add_modifier(Modifier::BOLD),
            _ if !self.live => self.dim(),
            _ => st,
        }
    }
    /// "OK" only for current data: otherwise not known.
    fn ok(&self) -> Severity {
        if self.live {
            Severity::Ok
        } else {
            Severity::Stale
        }
    }
    fn block<'a>(&self, title: &'a str, focused: bool) -> Block<'a> {
        let bt = match self.th.frame {
            FrameStyle::Plain => BorderType::Plain,
            FrameStyle::Rounded => BorderType::Rounded,
            FrameStyle::Double => BorderType::Double,
            FrameStyle::Thick => BorderType::Thick,
        };
        let title_style = if focused {
            self.accent().add_modifier(Modifier::UNDERLINED)
        } else {
            self.accent()
        };
        Block::default()
            .borders(Borders::ALL)
            .border_type(bt)
            .border_style(self.border())
            .style(self.base())
            .title(Span::styled(format!(" {title} "), title_style))
    }
}

fn clock(now: SystemTime) -> String {
    let t = humantime::format_rfc3339_seconds(now).to_string();
    t.get(11..19).unwrap_or("--:--:--").to_owned() + " UTC"
}

fn bar(done: u64, total: u64, width: usize, fill: char) -> String {
    let n = if total == 0 {
        0
    } else {
        usize::try_from(done.min(total) * width as u64 / total).unwrap_or(width)
    };
    let mut s: String = std::iter::repeat_n(fill, n).collect();
    s.extend(std::iter::repeat_n('·', width - n));
    s
}

const RAIN: &[char] = &[
    'ｱ', 'ｲ', 'ｳ', 'ｴ', 'ｵ', 'ｶ', 'ｷ', 'ｸ', 'ｹ', 'ｺ', 'ｻ', 'ｼ', 'ｽ', 'ｾ', 'ｿ', 'ﾀ', 'ﾁ', 'ﾂ', 'ﾃ',
    'ﾄ', 'ﾅ', 'ﾆ', 'ﾇ', 'ﾈ', 'ﾉ', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
];

fn mix(a: u64) -> u64 {
    let mut x = a.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 29;
    x.wrapping_mul(0xBF58_476D_1CE4_E5B9)
}

/// The decoration line of a theme at animation step `tick` (empty while
/// paused).
fn decoration(th: &Theme, tick: u64, width: usize, paused: bool) -> String {
    if paused {
        return String::new();
    }
    match th.decoration {
        Decoration::Rain => (0..width)
            .map(|i| {
                let h = mix(i as u64 * 7919 + tick / 2);
                if h % 3 == 0 {
                    RAIN[usize::try_from(h >> 8).unwrap_or(0) % RAIN.len()]
                } else {
                    ' '
                }
            })
            .collect(),
        Decoration::Horizon => {
            const WAVE: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
            (0..width)
                .map(|i| {
                    let p = (i as u64 + tick) % 14;
                    WAVE[usize::try_from(if p < 7 { p } else { 14 - p }).unwrap_or(0)]
                })
                .collect()
        }
        Decoration::Cursor | Decoration::None => String::new(),
    }
}

/// Draw the whole dashboard: the header with the destination beside it,
/// the decoration band, the banner, alerts beside throughput, the runs,
/// the cleanup pass, and the events (or the log) across the full width.
///
/// Implements: SRS-TUI-002, SRS-TUI-003, SRS-TUI-004, SRS-TUI-006, SRS-TUI-008, SRS-TUI-010
pub fn render(f: &mut Frame, m: &Model, ui: &Ui) {
    let health = m.health.clone().unwrap_or(Health::NoData {
        why: "not read yet".into(),
    });
    let pal = Pal {
        th: ui.theme,
        live: health.live(),
    };
    let area = f.area();
    f.render_widget(Block::default().style(pal.base()), area);
    let pending = m.unacknowledged();
    let banner_h = if pending.is_empty() { 0 } else { 3 };
    let deco_h = u16::from(matches!(
        ui.theme.decoration,
        Decoration::Horizon | Decoration::Rain
    ));
    let cleanup_h = if m.snapshot.as_ref().is_some_and(|s| s.cleanup.is_some()) {
        4
    } else {
        0
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(deco_h),
            Constraint::Length(banner_h),
            Constraint::Length(7),
            Constraint::Fill(1),
            Constraint::Length(cleanup_h),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .split(area);
    // Decoration stops while anything needs attention.
    let paused =
        !pending.is_empty() || m.alerts.iter().any(|a| a.level != "info") || !health.live();
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    header(f, top[0], m, ui, &pal, &health);
    destination(f, top[1], m.snapshot.as_ref(), &pal);
    if deco_h > 0 {
        let line = decoration(&ui.theme, ui.tick, rows[1].width as usize, paused);
        let style = if ui.theme.decoration == Decoration::Rain {
            pal.dim()
        } else {
            pal.base().fg(color(ui.theme.accent))
        };
        f.render_widget(Paragraph::new(line).style(style), rows[1]);
    }
    if banner_h > 0 {
        banner(f, rows[2], &pending, &pal);
    }
    let middle_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[3]);
    alerts(f, middle_row[0], m, ui, &pal);
    throughput(f, middle_row[1], m, &pal);
    runs(f, rows[4], m, ui, &pal);
    if cleanup_h > 0 {
        cleanup(f, rows[5], m, &pal);
    }
    if ui.show_log {
        log_pane(f, rows[6], m, ui, &pal);
    } else {
        events(f, rows[6], m, ui, &pal, paused);
    }
    footer(f, rows[7], ui, &pal);
    if !health.live() {
        overlay_health(f, area, &health, &pal);
    }
    if ui.show_sources {
        overlay_text(f, area, "Sources (s to close)", &ui.sources, &pal);
    } else if ui.show_help {
        overlay_text(f, area, "Keys (? to close)", HELP, &pal);
    } else if let Some(d) = &ui.detail {
        overlay_detail(f, area, m, d, ui.detail_scroll, &pal);
    }
}

const HELP: &str = "q          quit (nothing is changed anywhere)\n\
Esc        close the details, else quit\n\
Tab        next pane\n\
↑ ↓ PgUp PgDn Home   select a run or event, scroll the alerts\n\
Enter      details of the selected run or event: full path and values\n\
a          acknowledge alerts: hides the banner on this screen only\n\
f          event filter: all, warnings and errors, errors\n\
l          show the log instead of the events\n\
t          next theme\n\
b          bell on or off\n\
s          sources: which files are read, from which setting\n\
?          this help";

fn header(f: &mut Frame, r: Rect, m: &Model, ui: &Ui, pal: &Pal, health: &Health) {
    let (status, sev) = match (health, m.snapshot.as_ref()) {
        (Health::Live, Some(s)) if s.status == "outage" => ("outage".to_owned(), Severity::Fail),
        (Health::Live, Some(s)) => (s.status.clone(), Severity::Ok),
        (Health::Stale { .. }, _) => ("not current".to_owned(), Severity::Stale),
        (Health::NoData { .. }, _) => ("no data".to_owned(), Severity::Stale),
        (Health::Unsupported { .. }, _) => ("unsupported".to_owned(), Severity::Fail),
        (Health::Live, None) => ("?".to_owned(), Severity::Stale),
    };
    let age = m.snapshot_age(ui.now).map_or_else(
        || "-".to_owned(),
        |a| human(Duration::from_secs(a.as_secs())),
    );
    let (host, version) = m
        .snapshot
        .as_ref()
        .map_or(("-".to_owned(), "-".to_owned()), |s| {
            (s.host.clone(), s.tool_version.clone())
        });
    let lines = vec![
        Line::from(vec![
            Span::styled("ONT-OFFLOAD ", pal.accent()),
            Span::styled(format!("{host} v{version}"), pal.text()),
        ]),
        Line::from(vec![
            Span::styled(format!("{} {status}", Severity::label(sev)), pal.sev(sev)),
            Span::styled(format!("   state written {age} ago"), pal.text()),
        ]),
        Line::from(vec![
            Span::styled(clock(ui.now), pal.accent()),
            Span::styled(format!("   theme {}", ui.theme.kind.as_str()), pal.dim()),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines).block(pal.block("ont-offload dashboard (read-only)", false)),
        r,
    );
}

fn banner(f: &mut Frame, r: Rect, pending: &[&super::model::AlertItem], pal: &Pal) {
    let newest = pending[0];
    let sev = Severity::of_level(&newest.level);
    let text = format!(
        " {}  {} alert(s) to acknowledge. Newest {}{}: {}   [a] acknowledge ",
        sev.label(),
        pending.len(),
        newest.since,
        newest
            .run
            .as_deref()
            .map(|r| format!(" {r}"))
            .unwrap_or_default(),
        newest.message
    );
    let style = pal.sev(sev).add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(text)
            .style(style)
            .block(Block::default().borders(Borders::ALL).border_style(style)),
        r,
    );
}

fn destination(f: &mut Frame, r: Rect, snap: Option<&Snapshot>, pal: &Pal) {
    let mut lines = Vec::new();
    if let Some(s) = snap {
        let d = &s.destination;
        let sev = if d.status == "ok" && d.guard_ok {
            pal.ok()
        } else if d.status == "starting" {
            Severity::Info
        } else {
            Severity::Fail
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{} {} ", sev.label(), d.status), pal.sev(sev)),
            Span::styled(
                format!(" {} ({})", d.root, d.fstype.as_deref().unwrap_or("?")),
                pal.text(),
            ),
        ]));
        lines.push(Line::styled(
            format!(
                "guard {}   share free {}   source free {}",
                if d.guard_ok { "passed" } else { "NOT PASSED" },
                d.free_bytes
                    .map_or_else(|| "?".to_owned(), |b| Bytes(b).to_string()),
                s.source_free_bytes
                    .map_or_else(|| "?".to_owned(), |b| Bytes(b).to_string()),
            ),
            pal.text(),
        ));
        if let Some(o) = &d.outage {
            let sev = if o.level == "notice" {
                Severity::Warn
            } else {
                Severity::Fail
            };
            lines.push(Line::styled(
                format!(
                    "{} outage {} for {} ({}){}",
                    sev.label(),
                    o.level,
                    human(Duration::from_secs(o.duration_s)),
                    o.reason,
                    if o.recovering { ", recovering" } else { "" }
                ),
                pal.sev(sev),
            ));
            lines.push(Line::styled(
                format!(
                    "check {} in {}; waiting {} file(s), {}; {} worker(s) stuck",
                    o.checks + 1,
                    human(Duration::from_secs(o.next_check_s)),
                    o.backlog_files,
                    Bytes(o.backlog_bytes),
                    o.stuck_workers
                ),
                pal.text(),
            ));
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(pal.block("Destination", false)),
        r,
    );
}

fn throughput(f: &mut Frame, r: Rect, m: &Model, pal: &Pal) {
    let Some(s) = m.snapshot.as_ref() else {
        f.render_widget(pal.block("Throughput", false), r);
        return;
    };
    let limit = s.rate.limit_bytes_per_s;
    let title = format!(
        "Throughput {}/s of {}",
        Bytes(s.rate.current_bytes_per_s),
        limit.map_or_else(|| "no limit".to_owned(), |l| format!("{}/s", Bytes(l)))
    );
    let block = pal.block(&title, false);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);
    let data: Vec<u64> = m.rates.iter().copied().collect();
    let width = parts[0].width as usize;
    let shown = &data[data.len().saturating_sub(width)..];
    let max = limit
        .unwrap_or(0)
        .max(shown.iter().copied().max().unwrap_or(0))
        .max(1);
    f.render_widget(
        Sparkline::default()
            .data(shown)
            .max(max)
            .bar_set(symbols::bar::NINE_LEVELS)
            .style(pal.base().fg(color(pal.th.ok))),
        parts[0],
    );
    let stalled = s.workers.busy.iter().filter(|w| w.stalled).count();
    let wsev = if stalled > 0 {
        Severity::Fail
    } else {
        pal.ok()
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(
                "workers {} busy of {}{}  ",
                s.workers.busy.len(),
                s.workers.count,
                if s.workers.paused { " (paused)" } else { "" }
            ),
            pal.text(),
        ),
        Span::styled(
            if stalled > 0 {
                format!("{} {stalled} STALLED", Severity::Fail.label())
            } else {
                wsev.label().to_owned()
            },
            pal.sev(wsev),
        ),
    ])];
    if let Some(w) = s.workers.busy.first() {
        lines.push(Line::styled(
            format!("  {} {} ({})", w.run, w.task, Bytes(w.bytes)),
            pal.dim(),
        ));
    }
    f.render_widget(Paragraph::new(lines), parts[1]);
}

/// Width of the severity column.
const LABEL_W: usize = 7;
/// Width of the phase column (`reconciling` is the longest phase).
const PHASE_W: usize = 12;
/// Width of the progress bar.
const BAR_W: usize = 16;
/// Width of the bytes column.
const BYTES_W: usize = 9;
/// The run path column is never narrower than this while the pane allows.
const NAME_MIN: usize = 16;
/// Room the notes column asks for before the run path takes the rest.
const NOTES_WANT: usize = 30;

/// The severity and phase of a run: "complete" only with a manifest
/// digest, and failures always visible.
///
/// Implements: SRS-TUI-006
fn run_status(r: &RunState) -> (Severity, String) {
    let complete = r.phase == "complete" && r.manifest_sha256.is_some();
    let mut sev = if r.error.is_some() || !r.failed.is_empty() || r.phase == "failed" {
        Severity::Fail
    } else if complete {
        Severity::Ok
    } else if r.integrity_mismatches > 0 {
        Severity::Warn
    } else {
        Severity::Info
    };
    if r.phase == "complete" && !complete {
        sev = Severity::Warn;
    }
    let phase = if complete {
        "complete".to_owned()
    } else if r.phase == "complete" {
        "complete?".to_owned()
    } else {
        r.phase.clone()
    };
    (sev, phase)
}

/// What else the run line says, failures first so that a narrow pane cuts
/// the least important part.
///
/// Implements: SRS-TUI-006
fn run_notes(r: &RunState) -> String {
    let mut notes = Vec::new();
    if !r.failed.is_empty() {
        notes.push(format!("{} FAILED", r.failed.len()));
    }
    if let Some(e) = &r.error {
        notes.push(format!("error: {e}"));
    }
    if r.integrity_mismatches > 0 {
        notes.push(format!("{} mismatch(es) retried", r.integrity_mismatches));
    }
    if r.backlog_files > 0 {
        notes.push(format!(
            "waiting {} ({})",
            r.backlog_files,
            Bytes(r.backlog_bytes)
        ));
    }
    if let Some(e) = r.eta_s {
        notes.push(format!("eta {}", human(Duration::from_secs(e))));
    }
    notes.join("  ")
}

fn digits(n: u64) -> usize {
    n.checked_ilog10().map_or(1, |d| d as usize + 1)
}

/// Column widths of the runs table, the same for every row so that paths,
/// bars and numbers line up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunCols {
    name: usize,
    count: usize,
    notes: usize,
}

impl RunCols {
    /// Fit `list` into `width` characters (after the selection marker):
    /// the run path gets what it needs unless the notes would have less
    /// than they ask for, and is then shortened in the middle.
    fn fit(list: &[RunState], width: usize) -> Self {
        let count = list
            .iter()
            .map(|r| digits(r.files).max(digits(r.committed_files)))
            .max()
            .unwrap_or(1)
            .max(3);
        // The severity, then two spaces before each column.
        let fixed = LABEL_W + 2 + 2 + PHASE_W + 2 + BAR_W + 2 + (2 * count + 1) + 2 + BYTES_W;
        let longest = list
            .iter()
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(0);
        let notes_max = list
            .iter()
            .map(|r| run_notes(r).chars().count())
            .max()
            .unwrap_or(0);
        let room = width.saturating_sub(fixed);
        let want_notes = if notes_max == 0 {
            0
        } else {
            notes_max.min(NOTES_WANT) + 2
        };
        let name = longest
            .min(room.saturating_sub(want_notes).max(NAME_MIN))
            .min(room);
        let notes = room.saturating_sub(name + 2);
        Self { name, count, notes }
    }
}

/// One row of the runs table, after the severity column.
fn run_row(r: &RunState, fill: char, cols: RunCols) -> String {
    let (_, phase) = run_status(r);
    let mut s = format!(
        "  {:<nw$}  {:<PHASE_W$}  {}  {:>cw$}/{:<cw$}  {:>BYTES_W$}",
        middle(&r.name, cols.name),
        phase,
        bar(r.committed_files, r.files, BAR_W, fill),
        r.committed_files,
        r.files,
        Bytes(r.bytes).to_string(),
        nw = cols.name,
        cw = cols.count,
    );
    let notes = run_notes(r);
    if !notes.is_empty() && cols.notes > 0 {
        s.push_str("  ");
        s.push_str(&clip(&notes, cols.notes));
    }
    s
}

/// The whole line of a run with nothing shortened.
///
/// Implements: SRS-TUI-006
#[cfg(test)]
fn run_line(r: &RunState, fill: char) -> (String, Severity) {
    let (sev, _) = run_status(r);
    let cols = RunCols::fit(std::slice::from_ref(r), usize::MAX / 4);
    (
        format!("{:<LABEL_W$}{}", sev.label(), run_row(r, fill, cols)),
        sev,
    )
}

/// The first line shown so that `selected` is on screen: the list starts
/// at the top and scrolls once the selection passes the last line.
fn window_start(selected: usize, visible: usize) -> usize {
    (selected + 1).saturating_sub(visible.max(1))
}

/// The selection marker: a pointer on the selected row of the focused pane.
fn marker(selected: bool, pal: &Pal) -> Span<'static> {
    if selected {
        Span::styled("▸", pal.accent())
    } else {
        Span::styled(" ", pal.text())
    }
}

fn row_style(selected: bool, pal: &Pal) -> Style {
    if selected {
        pal.text().add_modifier(Modifier::REVERSED)
    } else {
        pal.text()
    }
}

fn runs(f: &mut Frame, r: Rect, m: &Model, ui: &Ui, pal: &Pal) {
    let list = runs_of(m);
    let focused = ui.focus == Pane::Runs;
    let title = if focused && !list.is_empty() {
        format!("Runs ({})  Enter: details", list.len())
    } else {
        format!("Runs ({})", list.len())
    };
    let lines: Vec<Line> = if list.is_empty() {
        vec![Line::styled("no run being offloaded or watched", pal.dim())]
    } else {
        let width = r.width.saturating_sub(2) as usize;
        let visible = r.height.saturating_sub(2) as usize;
        let cols = RunCols::fit(list, width.saturating_sub(1));
        let sel = ui.scroll[Pane::Runs.index()].min(list.len() - 1);
        list.iter()
            .enumerate()
            .skip(window_start(sel, visible))
            .map(|(i, run)| {
                let selected = focused && i == sel;
                let (severity, _) = run_status(run);
                Line::from(vec![
                    marker(selected, pal),
                    Span::styled(format!("{:<LABEL_W$}", severity.label()), pal.sev(severity)),
                    Span::styled(run_row(run, ui.theme.gauge, cols), row_style(selected, pal)),
                ])
            })
            .collect()
    };
    f.render_widget(Paragraph::new(lines).block(pal.block(&title, focused)), r);
}

fn cleanup(f: &mut Frame, r: Rect, m: &Model, pal: &Pal) {
    let Some(c) = m.snapshot.as_ref().and_then(|s| s.cleanup.as_ref()) else {
        return;
    };
    let mut lines = vec![Line::styled(
        format!(
            "{} (gate {}){}  next pass in {}  last: {} deleted ({}), {} would delete, {} held, {} waiting  ",
            c.mode,
            c.gate,
            if c.running { ", running" } else { "" },
            human(Duration::from_secs(c.next_pass_s)),
            c.deleted_runs,
            Bytes(c.deleted_bytes),
            c.would_delete,
            c.held,
            c.waiting
        ),
        pal.text(),
    )];
    let mut spans = Vec::new();
    if c.refused > 0 {
        let names: Vec<String> = m
            .last_report
            .as_ref()
            .map(|rep| {
                rep.runs
                    .iter()
                    .filter(|r| r.outcome == crate::cleanup::Outcome::Refused)
                    .map(|r| r.source.clone())
                    .collect()
            })
            .unwrap_or_default();
        spans.push(Span::styled(
            format!(
                "{} {} refused {}  ",
                Severity::Fail.label(),
                c.refused,
                names.join(", ")
            ),
            pal.sev(Severity::Fail),
        ));
    }
    if let Some(why) = &c.stopped {
        spans.push(Span::styled(
            format!("{} stopped: {why}  ", Severity::Fail.label()),
            pal.sev(Severity::Fail),
        ));
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines).block(pal.block("Cleanup", false)), r);
}

fn alerts(f: &mut Frame, r: Rect, m: &Model, ui: &Ui, pal: &Pal) {
    let mut lines: Vec<Line> = Vec::new();
    if m.alerts.is_empty() {
        let text = if pal.live {
            "no open alerts"
        } else {
            "no alerts known: data not current"
        };
        lines.push(Line::styled(
            format!("{} {text}", pal.ok().label()),
            pal.sev(pal.ok()),
        ));
    }
    for a in &m.alerts {
        let sev = Severity::of_level(&a.level);
        lines.push(Line::from(vec![
            Span::styled(format!("{:<7}", sev.label()), pal.sev(sev)),
            Span::styled(
                format!(
                    " {} {}{}",
                    a.since.get(11..19).unwrap_or(&a.since),
                    a.run
                        .as_deref()
                        .map(|r| format!("{r}: "))
                        .unwrap_or_default(),
                    a.message
                ),
                pal.text(),
            ),
        ]));
    }
    for a in &m.cleared {
        lines.push(Line::styled(
            format!(
                "✓ cleared {} {}",
                a.since.get(11..19).unwrap_or(&a.since),
                a.message
            ),
            pal.dim(),
        ));
    }
    let skip = ui.scroll[Pane::Alerts.index()].min(lines.len().saturating_sub(1));
    let title = format!("Alerts ({} open)", m.alerts.len());
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(skip).collect::<Vec<_>>())
            .wrap(Wrap { trim: true })
            .block(pal.block(&title, ui.focus == Pane::Alerts)),
        r,
    );
}

/// Width of the event severity column.
const EVENT_LABEL_W: usize = 7;
/// The run column of the events is never wider than this.
const EVENT_RUN_MAX: usize = 32;

fn events(f: &mut Frame, r: Rect, m: &Model, ui: &Ui, pal: &Pal, paused: bool) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(e) = &m.events_error {
        lines.push(Line::styled(
            format!("{} {e}", Severity::Warn.label()),
            pal.sev(Severity::Warn),
        ));
    }
    if m.missed_events > 0 {
        lines.push(Line::styled(
            format!(
                "{} {} event(s) not shown (gap in the stream)",
                Severity::Warn.label(),
                m.missed_events
            ),
            pal.sev(Severity::Warn),
        ));
    }
    if m.restarts > 0 {
        lines.push(Line::styled(
            format!("· daemon restarted {} time(s) while watched", m.restarts),
            pal.dim(),
        ));
    }
    let shown = shown_events(m, ui.filter);
    let focused = ui.focus == Pane::Events;
    let width = r.width.saturating_sub(2) as usize;
    let visible = (r.height.saturating_sub(2) as usize).saturating_sub(lines.len());
    // Counted from the newest: 0 keeps the pane on the newest event.
    let back = ui.scroll[Pane::Events.index()].min(shown.len().saturating_sub(1));
    let end = shown.len() - (back + 1).saturating_sub(visible.max(1)).min(shown.len());
    let start = end.saturating_sub(visible);
    let run_w = shown
        .iter()
        .filter_map(|e| e.run.as_deref())
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0)
        .min(EVENT_RUN_MAX)
        .min(width / 4);
    let cursor = ui.theme.decoration == Decoration::Cursor;
    // marker, severity, time, run column, and room for the cursor.
    let fixed = 1 + EVENT_LABEL_W + 10 + if run_w > 0 { run_w + 2 } else { 0 };
    let msg_w = width.saturating_sub(fixed + if cursor { 2 } else { 0 });
    for (i, e) in shown[start..end].iter().enumerate() {
        let index = start + i;
        let selected = focused && index + 1 + back == shown.len();
        let sev = Severity::of_level(&e.level);
        let mut text = format!(" {} ", e.ts.get(11..19).unwrap_or(&e.ts));
        if run_w > 0 {
            text.push_str(&format!(
                "{:<run_w$}  ",
                middle(e.run.as_deref().unwrap_or(""), run_w)
            ));
        }
        text.push_str(&clip(&e.message, msg_w));
        let mut spans = vec![
            marker(selected, pal),
            Span::styled(format!("{:<EVENT_LABEL_W$}", sev.label()), pal.sev(sev)),
            Span::styled(text, row_style(selected, pal)),
        ];
        let last = index + 1 == shown.len();
        if last && cursor && !paused && ui.tick % 2 == 0 {
            spans.push(Span::styled(" █", pal.accent()));
        }
        lines.push(Line::from(spans));
    }
    let title = if focused && !shown.is_empty() {
        format!("Events [{}]  Enter: details", ui.filter.as_str())
    } else {
        format!("Events [{}]", ui.filter.as_str())
    };
    f.render_widget(Paragraph::new(lines).block(pal.block(&title, focused)), r);
}

fn log_pane(f: &mut Frame, r: Rect, m: &Model, ui: &Ui, pal: &Pal) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(e) = &m.log_error {
        lines.push(Line::styled(
            format!("{} {e}", Severity::Warn.label()),
            pal.sev(Severity::Warn),
        ));
    }
    let height = r.height.saturating_sub(2) as usize;
    let skip = ui.scroll[Pane::Events.index()];
    let end = m.log.len().saturating_sub(skip);
    let start = end.saturating_sub(height.saturating_sub(lines.len()));
    for l in m.log.range(start..end) {
        let sev = if l.contains("ERROR") || l.contains("\"level\":\"ERROR\"") {
            Severity::Fail
        } else if l.contains("WARN") {
            Severity::Warn
        } else {
            Severity::Info
        };
        lines.push(Line::styled(
            l.clone(),
            if sev == Severity::Info {
                pal.text()
            } else {
                pal.sev(sev)
            },
        ));
    }
    f.render_widget(
        Paragraph::new(lines).block(pal.block("Log (context only)", ui.focus == Pane::Events)),
        r,
    );
}

fn footer(f: &mut Frame, r: Rect, ui: &Ui, pal: &Pal) {
    let text = format!(
        " q quit  Tab pane  ↑↓ select  Enter details  a acknowledge  f filter  l {}  t theme  b bell {}  s sources  ? help",
        if ui.show_log { "events" } else { "log" },
        if ui.bell { "on" } else { "off" }
    );
    f.render_widget(Paragraph::new(text).style(pal.dim()), r);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// The STALE, NO DATA and UNSUPPORTED screens over everything else.
///
/// Implements: SRS-TUI-002, SRS-TUI-007
fn overlay_health(f: &mut Frame, area: Rect, h: &Health, pal: &Pal) {
    let (title, text) = match h {
        Health::Stale { age, updated_at } => (
            "◷ STALE: NOT CURRENT",
            format!(
                "The daemon has not written its state for {} (last at {updated_at}).\n\
                 It is stopped, hung or cannot write. Everything below is from then\n\
                 and is NOT the current state of the offload.",
                human(Duration::from_secs(age.as_secs()))
            ),
        ),
        Health::NoData { why } => (
            "◷ NO DATA",
            format!(
                "No state to show: {why}.\nIs `ont-offload watch` running, and is the state folder right (s: sources)?"
            ),
        ),
        Health::Unsupported { why } => ("✖ UNSUPPORTED STATE", why.clone()),
        Health::Live => return,
    };
    let r = centered(area, area.width.saturating_sub(8), 7);
    let style = pal.sev(Severity::Fail);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .style(style)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Double)
                    .border_style(style)
                    .title(Span::styled(format!(" {title} "), style)),
            ),
        r,
    );
}

fn overlay_text(f: &mut Frame, area: Rect, title: &str, text: &str, pal: &Pal) {
    let h = u16::try_from(text.lines().count() + 2).unwrap_or(u16::MAX);
    let w = u16::try_from(text.lines().map(str::len).max().unwrap_or(20) + 4).unwrap_or(u16::MAX);
    let r = centered(area, w, h);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text.to_owned())
            .style(pal.base())
            .block(pal.block(title, true)),
        r,
    );
}

/// Width of the label column of a detail popup.
const FIELD_W: usize = 16;

/// Lines of a detail popup `width` characters wide.
struct DetailText<'p> {
    pal: &'p Pal,
    width: usize,
    lines: Vec<Line<'static>>,
}

impl<'p> DetailText<'p> {
    fn new(pal: &'p Pal, width: usize) -> Self {
        Self {
            pal,
            width: width.max(FIELD_W + 8),
            lines: Vec::new(),
        }
    }

    /// `head` (at most [`FIELD_W`] wide) and `value`, the value broken
    /// into the remaining width and continued under itself, so that a
    /// path stays in one block whatever its length.
    fn field(&mut self, head: Span<'static>, value: &str, style: Style) {
        let room = self.width - FIELD_W;
        let chars: Vec<char> = value.chars().collect();
        let mut chunks = chars.chunks(room).map(|c| c.iter().collect::<String>());
        let first = chunks.next().unwrap_or_default();
        let pad = FIELD_W.saturating_sub(head.width());
        self.lines.push(Line::from(vec![
            head,
            Span::styled(format!("{:pad$}{first}", ""), style),
        ]));
        for c in chunks {
            self.lines.push(Line::from(Span::styled(
                format!("{:FIELD_W$}{c}", ""),
                style,
            )));
        }
    }

    fn value(&mut self, label: &str, value: &str) {
        let head = Span::styled(label.to_owned(), self.pal.accent());
        self.field(head, value, self.pal.text());
    }

    /// A line of its own, broken into the width with a two-space indent.
    fn text(&mut self, value: &str, style: Style) {
        let chars: Vec<char> = value.chars().collect();
        for (i, c) in chars.chunks(self.width - 2).enumerate() {
            let indent = if i == 0 { "" } else { "  " };
            self.lines.push(Line::styled(
                format!("{indent}{}", c.iter().collect::<String>()),
                style,
            ));
        }
    }

    fn blank(&mut self) {
        self.lines.push(Line::default());
    }
}

/// Everything the snapshot says about one run, paths in full.
///
/// Implements: SRS-TUI-006, SRS-TUI-010
fn run_detail(r: &RunState, pal: &Pal, width: usize) -> Vec<Line<'static>> {
    let (sev, phase) = run_status(r);
    let opt = |v: Option<&String>| v.cloned().unwrap_or_else(|| "-".to_owned());
    let mut d = DetailText::new(pal, width);
    d.field(
        Span::styled(sev.label().to_owned(), pal.sev(sev)),
        &r.name,
        pal.accent(),
    );
    d.blank();
    d.value("phase", &phase);
    d.value("run id", &r.run_id);
    if let Some(src) = &r.source_name {
        d.value("source name", src);
    }
    d.value("source", &r.source);
    d.value("destination", &r.destination);
    d.value(
        "files",
        &format!("{} of {} committed", r.committed_files, r.files),
    );
    d.value("bytes", &Bytes(r.bytes).to_string());
    d.value(
        "waiting",
        &format!("{} file(s), {}", r.backlog_files, Bytes(r.backlog_bytes)),
    );
    d.value("copying", &format!("{} file(s)", r.copying));
    d.value(
        "eta",
        &r.eta_s
            .map_or_else(|| "-".to_owned(), |e| human(Duration::from_secs(e))),
    );
    d.value(
        "mismatches",
        &format!("{} (each retried)", r.integrity_mismatches),
    );
    d.value("complete marker", &opt(r.complete_marker.as_ref()));
    d.value("manifest sha256", &opt(r.manifest_sha256.as_ref()));
    if let Some(e) = &r.error {
        let fail = pal.sev(Severity::Fail);
        d.field(Span::styled("error", fail), e, fail);
    }
    if !r.failed.is_empty() {
        d.blank();
        d.text(
            &format!(
                "{} {} file(s) FAILED",
                Severity::Fail.label(),
                r.failed.len()
            ),
            pal.sev(Severity::Fail),
        );
        for f in &r.failed {
            d.text(&format!("  {}: {}", f.file, f.reason), pal.text());
        }
    }
    if !r.skipped.is_empty() {
        d.blank();
        d.text(
            &format!(
                "{} entries not copied (symlinks, special files)",
                r.skipped.len()
            ),
            pal.dim(),
        );
        for s in &r.skipped {
            d.text(&format!("  {s}"), pal.text());
        }
    }
    d.lines
}

/// Everything one event says, with its details.
///
/// Implements: SRS-TUI-010
fn event_detail(e: &Event, pal: &Pal, width: usize) -> Vec<Line<'static>> {
    let sev = Severity::of_level(&e.level);
    let kind = serde_json::to_value(e.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{:?}", e.kind));
    let mut d = DetailText::new(pal, width);
    d.field(
        Span::styled(sev.label().to_owned(), pal.sev(sev)),
        &e.message,
        pal.accent(),
    );
    d.blank();
    d.value("time", &e.ts);
    d.value("sequence", &e.seq.to_string());
    d.value("kind", &kind);
    d.value("level", &e.level);
    d.value("run", e.run.as_deref().unwrap_or("-"));
    d.value("file", e.file.as_deref().unwrap_or("-"));
    let empty = e.data.is_null() || e.data.as_object().is_some_and(serde_json::Map::is_empty);
    if !empty {
        d.blank();
        d.text("details", pal.accent());
        let text = serde_json::to_string_pretty(&e.data).unwrap_or_default();
        for l in text.lines() {
            d.text(&format!("  {l}"), pal.text());
        }
    }
    d.lines
}

/// The detail popup of a run or an event, scrolled by `scroll` lines.
///
/// Implements: SRS-TUI-010
fn overlay_detail(f: &mut Frame, area: Rect, m: &Model, detail: &Detail, scroll: usize, pal: &Pal) {
    let width = area.width.saturating_sub(8).min(120);
    let inner = usize::from(width.saturating_sub(2));
    let gone = |what: &str| vec![Line::styled(what.to_owned(), pal.dim())];
    let (title, lines) = match detail {
        Detail::Run(id) => (
            "Run (Esc to close, ↑↓ scroll)",
            runs_of(m).iter().find(|r| &r.run_id == id).map_or_else(
                || gone("This run is no longer in the snapshot."),
                |r| run_detail(r, pal, inner),
            ),
        ),
        Detail::Event { seq, ts } => (
            "Event (Esc to close, ↑↓ scroll)",
            m.events
                .iter()
                .find(|e| e.seq == *seq && &e.ts == ts)
                .map_or_else(
                    || gone("This event is no longer kept by the dashboard."),
                    |e| event_detail(e, pal, inner),
                ),
        ),
    };
    let height = u16::try_from(lines.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(2));
    let r = centered(area, width, height);
    let top = u16::try_from(scroll.min(lines.len().saturating_sub(1))).unwrap_or(u16::MAX);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines)
            .scroll((top, 0))
            .style(pal.base())
            .block(pal.block(title, true)),
        r,
    );
}

#[cfg(test)]
mod tests;
