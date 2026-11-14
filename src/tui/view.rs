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
use crate::state::{RunState, Snapshot};

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
    /// Scroll offset per pane.
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
            bell,
            now,
        }
    }

    /// Scroll the focused pane.
    pub fn scroll_by(&mut self, delta: isize) {
        let s = &mut self.scroll[self.focus.index()];
        *s = s.saturating_add_signed(delta);
    }
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

/// Draw the whole dashboard.
///
/// Implements: SRS-TUI-002, SRS-TUI-003, SRS-TUI-004, SRS-TUI-006, SRS-TUI-008
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
    let deco_h = u16::from(ui.theme.decoration == Decoration::Horizon);
    let cleanup_h = if m.snapshot.as_ref().is_some_and(|s| s.cleanup.is_some()) {
        4
    } else {
        0
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(deco_h),
            Constraint::Length(banner_h),
            Constraint::Length(7),
            Constraint::Min(5),
            Constraint::Length(cleanup_h),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);
    // Decoration stops while anything needs attention.
    let paused =
        !pending.is_empty() || m.alerts.iter().any(|a| a.level != "info") || !health.live();
    header(f, rows[0], m, ui, &pal, &health, paused);
    if deco_h > 0 {
        let line = decoration(&ui.theme, ui.tick, rows[1].width as usize, paused);
        f.render_widget(
            Paragraph::new(line).style(pal.base().fg(color(ui.theme.accent))),
            rows[1],
        );
    }
    if banner_h > 0 {
        banner(f, rows[2], &pending, &pal);
    }
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[3]);
    destination(f, top[0], m.snapshot.as_ref(), &pal);
    throughput(f, top[1], m, &pal);
    runs(f, rows[4], m.snapshot.as_ref(), ui, &pal);
    if cleanup_h > 0 {
        cleanup(f, rows[5], m, &pal);
    }
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[6]);
    alerts(f, bottom[0], m, ui, &pal);
    if ui.show_log {
        log_pane(f, bottom[1], m, ui, &pal);
    } else {
        events(f, bottom[1], m, ui, &pal, paused);
    }
    footer(f, rows[7], ui, &pal);
    if !health.live() {
        overlay_health(f, area, &health, &pal);
    }
    if ui.show_sources {
        overlay_text(f, area, "Sources (s to close)", &ui.sources, &pal);
    } else if ui.show_help {
        overlay_text(f, area, "Keys (? to close)", HELP, &pal);
    }
}

const HELP: &str = "q / Esc   quit (nothing is changed anywhere)\n\
Tab       next pane\n\
↑ ↓ PgUp PgDn   scroll the pane\n\
a         acknowledge alerts: hides the banner on this screen only\n\
f         event filter: all, warnings and errors, errors\n\
l         show the log instead of the events\n\
t         next theme\n\
b         bell on or off\n\
s         sources: which files are read, from which setting\n\
?         this help";

fn header(f: &mut Frame, r: Rect, m: &Model, ui: &Ui, pal: &Pal, health: &Health, paused: bool) {
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
    let left = vec![
        Span::styled("ONT-OFFLOAD ", pal.accent()),
        Span::styled(format!("{host} v{version}  "), pal.text()),
        Span::styled(format!(" {} {status} ", Severity::label(sev)), pal.sev(sev)),
        Span::styled(format!("  state written {age} ago  "), pal.text()),
        Span::styled(clock(ui.now), pal.accent()),
        Span::styled(format!("  theme {}", ui.theme.kind.as_str()), pal.dim()),
    ];
    let used: usize = left.iter().map(Span::width).sum();
    let mut spans = left;
    if ui.theme.decoration == Decoration::Rain {
        let w = (r.width as usize).saturating_sub(used + 4);
        spans.push(Span::styled(
            format!(" {}", decoration(&ui.theme, ui.tick, w, paused)),
            pal.dim(),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans))
            .block(pal.block("ont-offload dashboard (read-only)", false)),
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

/// The phase shown for a run: "complete" only with a manifest digest, and
/// failures always visible.
///
/// Implements: SRS-TUI-006
fn run_line(r: &RunState, fill: char) -> (String, Severity) {
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
    let mut s = format!(
        "{:<8} {:<34} {:<12} {} {:>5}/{:<5} {:>9}",
        sev.label(),
        r.name,
        phase,
        bar(r.committed_files, r.files, 16, fill),
        r.committed_files,
        r.files,
        Bytes(r.bytes).to_string(),
    );
    if r.backlog_files > 0 {
        s.push_str(&format!(
            "  waiting {} ({})",
            r.backlog_files,
            Bytes(r.backlog_bytes)
        ));
    }
    if let Some(e) = r.eta_s {
        s.push_str(&format!("  eta {}", human(Duration::from_secs(e))));
    }
    if !r.failed.is_empty() {
        s.push_str(&format!("  {} FAILED", r.failed.len()));
    }
    if r.integrity_mismatches > 0 {
        s.push_str(&format!(
            "  {} mismatch(es) retried",
            r.integrity_mismatches
        ));
    }
    if let Some(e) = &r.error {
        s.push_str(&format!("  error: {e}"));
    }
    (s, sev)
}

fn runs(f: &mut Frame, r: Rect, snap: Option<&Snapshot>, ui: &Ui, pal: &Pal) {
    let list = snap.map(|s| s.runs.as_slice()).unwrap_or_default();
    let title = format!("Runs ({})", list.len());
    let skip = ui.scroll[Pane::Runs.index()].min(list.len().saturating_sub(1));
    let lines: Vec<Line> = if list.is_empty() {
        vec![Line::styled("no run being offloaded or watched", pal.dim())]
    } else {
        list.iter()
            .skip(skip)
            .map(|run| {
                let (text, sev) = run_line(run, ui.theme.gauge);
                let label_len = sev.label().chars().count().max(8);
                let (lab, rest) = text.split_at(
                    text.char_indices()
                        .nth(label_len)
                        .map_or(text.len(), |(i, _)| i),
                );
                Line::from(vec![
                    Span::styled(lab.to_owned(), pal.sev(sev)),
                    Span::styled(rest.to_owned(), pal.text()),
                ])
            })
            .collect()
    };
    f.render_widget(
        Paragraph::new(lines).block(pal.block(&title, ui.focus == Pane::Runs)),
        r,
    );
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
    let shown: Vec<_> = m
        .events
        .iter()
        .filter(|e| ui.filter.admits(&e.level))
        .collect();
    let height = r.height.saturating_sub(2) as usize;
    let skip = ui.scroll[Pane::Events.index()];
    let end = shown.len().saturating_sub(skip);
    let start = end.saturating_sub(height.saturating_sub(lines.len()));
    for (i, e) in shown[start..end].iter().enumerate() {
        let sev = Severity::of_level(&e.level);
        let mut spans = vec![
            Span::styled(format!("{:<7}", sev.label()), pal.sev(sev)),
            Span::styled(
                format!(" {} {}", e.ts.get(11..19).unwrap_or(&e.ts), e.message),
                pal.text(),
            ),
        ];
        let last = start + i + 1 == shown.len();
        if last && ui.theme.decoration == Decoration::Cursor && !paused && ui.tick % 2 == 0 {
            spans.push(Span::styled(" █", pal.accent()));
        }
        lines.push(Line::from(spans));
    }
    let title = format!("Events [{}]", ui.filter.as_str());
    f.render_widget(
        Paragraph::new(lines).block(pal.block(&title, ui.focus == Pane::Events)),
        r,
    );
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
        " q quit  Tab pane  ↑↓ scroll  a acknowledge  f filter  l {}  t theme  b bell {}  s sources  ? help",
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

#[cfg(test)]
mod tests;
