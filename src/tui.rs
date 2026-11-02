//! Interactive viewer: live updates, zoom/pan, cursor, range selection, stats panel
//! and a Rhai console.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::ValueEnum;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table};

use crate::cli::ChartKind;
use crate::data::Dataset;
use crate::render::{ChartOpts, ChartWidget, HLine, Overlay, x_bounds};
use crate::script::{Action, display};
use crate::session::Session;
use crate::source::{Event, SourceSpec};
use crate::stats::{ReportOpts, Stats, slope_unit};

const HELP: &[(&str, &str)] = &[
    ("← → / h l", "pan (moves the cursor when it's on)"),
    ("H L", "pan by half a screen"),
    ("+ / -", "zoom in / out"),
    ("0 / r", "reset view"),
    ("c", "toggle cursor"),
    ("Home / End", "cursor to first / last point"),
    (
        "v",
        "start / finish a range selection (stats for that range)",
    ),
    ("1-9", "show / hide a series"),
    ("a", "show all series"),
    ("t", "cycle chart type"),
    ("m", "cycle marker"),
    ("y", "toggle log y-axis"),
    ("x", "split series into separate panels"),
    ("s", "toggle stats panel"),
    ("space / p", "pause live updates"),
    ("e", "export the view to an SVG file"),
    (":", "Rhai console, e.g. :slope(\"node1\", \"1d\")"),
    ("q / Esc", "quit (Esc first clears a selection)"),
];

struct Message {
    text: String,
    error: bool,
    at: Instant,
}

pub struct App {
    session: Session,
    rx: Receiver<Event>,
    opts: ChartOpts,
    report: ReportOpts,
    alerts: Vec<String>,
    spec: SourceSpec,
    /// Every series (for scripts and alerts).
    full: Dataset,
    /// The selected series being displayed.
    ds: Dataset,
    built_rev: u64,
    last_build: Instant,
    eof: bool,
    view: Option<(f64, f64)>,
    cursor: Option<f64>,
    anchor: Option<f64>,
    selection: Option<(f64, f64)>,
    hidden: HashSet<String>,
    show_stats: bool,
    paused: bool,
    help: bool,
    console: Option<String>,
    history: Vec<String>,
    history_pos: Option<usize>,
    message: Option<Message>,
    binds: HashMap<char, String>,
    firing: Vec<String>,
    quit: bool,
}

impl App {
    pub fn new(
        session: Session,
        rx: Receiver<Event>,
        opts: ChartOpts,
        report: ReportOpts,
        alerts: Vec<String>,
        spec: SourceSpec,
        show_stats: bool,
    ) -> Self {
        let binds = session
            .script
            .binds()
            .into_iter()
            .filter_map(|(k, code)| Some((k.chars().next()?, code)))
            .collect();
        Self {
            session,
            rx,
            opts,
            report,
            alerts,
            spec,
            full: Dataset::default(),
            ds: Dataset::default(),
            built_rev: u64::MAX,
            last_build: Instant::now() - Duration::from_secs(1),
            eof: false,
            view: None,
            cursor: None,
            anchor: None,
            selection: None,
            hidden: HashSet::new(),
            show_stats,
            paused: false,
            help: false,
            console: None,
            history: Vec::new(),
            history_pos: None,
            message: None,
            binds,
            firing: Vec::new(),
            quit: false,
        }
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some(Message {
            text: text.into(),
            error,
            at: Instant::now(),
        });
    }

    /// Drain pending input without blocking the UI for too long.
    fn pump(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(40);
        while Instant::now() < deadline {
            match self.rx.try_recv() {
                Ok(Event::Line(line, meta)) => {
                    let _ = self.session.feed(&line, &meta);
                }
                Ok(Event::Error(e)) => self.say(e, true),
                Ok(Event::SampleDone) => {}
                Ok(Event::Eof) | Err(TryRecvError::Disconnected) => {
                    self.eof = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
    }

    fn rebuild(&mut self, force: bool) {
        let rev = self.session.store.revision;
        if !force {
            // Throttle rebuilds while data is streaming in.
            let due = self.eof || self.last_build.elapsed() > Duration::from_millis(150);
            if self.paused || rev == self.built_rev || !due {
                return;
            }
        }
        let old_max = self.ds.x_range().map(|r| r.1);
        self.full = self.session.full_dataset();
        self.ds = self.session.select(&self.full);
        self.built_rev = rev;
        self.last_build = Instant::now();
        // A view pinned to the newest data keeps following it.
        if let (Some((a, b)), Some(old), Some((_, new))) = (self.view, old_max, self.ds.x_range())
            && b >= old
            && new > b
        {
            self.view = Some((a + new - b, new));
        }
        self.check_alerts();
    }

    fn check_alerts(&mut self) {
        if self.alerts.is_empty() {
            return;
        }
        self.session.script.set_data(self.full.clone(), None);
        self.firing = self
            .alerts
            .iter()
            .filter_map(|a| match self.session.script.eval_bool(a) {
                Ok(true) => Some(a.clone()),
                Ok(false) => None,
                Err(e) => Some(format!("{a}: {e}")),
            })
            .collect();
    }

    // ───────────── view helpers ─────────────

    fn bounds(&self) -> (f64, f64) {
        x_bounds(&self.ds, self.view)
    }

    /// The range stats and the console operate on.
    fn focus(&self) -> Option<(f64, f64)> {
        self.selection.or(self.view)
    }

    fn visible_xs(&self) -> Vec<f64> {
        let mut xs: Vec<f64> = self
            .ds
            .series
            .iter()
            .filter(|s| !self.hidden.contains(&s.name))
            .flat_map(|s| s.points.iter().map(|p| p.0))
            .collect();
        xs.sort_by(f64::total_cmp);
        xs.dedup();
        xs
    }

    fn zoom(&mut self, factor: f64) {
        let (x0, x1) = self.bounds();
        let centre = self.cursor.unwrap_or((x0 + x1) / 2.0);
        let (lo, hi) = (
            centre - (centre - x0) * factor,
            centre + (x1 - centre) * factor,
        );
        match self.ds.x_range() {
            Some((a, b)) if factor > 1.0 && hi - lo >= b - a => self.view = None,
            _ if hi - lo > 1e-9 => self.view = Some((lo, hi)),
            _ => {}
        }
    }

    fn pan(&mut self, frac: f64) {
        let (x0, x1) = self.bounds();
        let d = (x1 - x0) * frac;
        self.view = Some((x0 + d, x1 + d));
    }

    fn move_cursor(&mut self, dir: i32) {
        let xs = self.visible_xs();
        if xs.is_empty() {
            return;
        }
        let (x0, x1) = self.bounds();
        let c = self.cursor.unwrap_or((x0 + x1) / 2.0);
        let next = if dir > 0 {
            xs.iter().find(|&&x| x > c).or(xs.last())
        } else {
            xs.iter().rev().find(|&&x| x < c).or(xs.first())
        };
        if let Some(&x) = next {
            self.cursor = Some(x);
            // Keep the cursor on screen.
            if x < x0 || x > x1 {
                let half = (x1 - x0) / 2.0;
                self.view = Some((x - half, x + half));
            }
        }
    }

    fn set_cursor_near(&mut self, target: f64) {
        let xs = self.visible_xs();
        self.cursor = xs
            .iter()
            .copied()
            .min_by(|a, b| (a - target).abs().total_cmp(&(b - target).abs()));
    }

    fn toggle_series(&mut self, idx: usize) {
        if let Some(s) = self.ds.series.get(idx) {
            let name = s.name.clone();
            if !self.hidden.remove(&name) {
                self.hidden.insert(name);
            }
        }
    }

    fn export_view(&mut self) {
        let mut ds = self.ds.clone();
        ds.series.retain(|s| !self.hidden.contains(&s.name));
        if let Some((a, b)) = self.view {
            ds.series
                .iter_mut()
                .for_each(|s| s.points = s.range(a, b).to_vec());
        }
        let name = format!("graft-{}.svg", chrono::Local::now().format("%Y%m%d-%H%M%S"));
        match crate::export::export(std::path::Path::new(&name), &ds, &self.opts) {
            Ok(()) => self.say(format!("saved {name}"), false),
            Err(e) => self.say(format!("export failed: {e:#}"), true),
        }
    }

    // ───────────── console ─────────────

    fn eval(&mut self, code: &str) {
        let script = self.session.script.clone();
        script.set_data(self.full.clone(), self.focus());
        let result = script.eval(code);
        let printed = script.take_output();
        for action in script.take_actions() {
            self.apply(action);
        }
        match result {
            Ok(v) => {
                let mut text = printed.join(" ");
                if !v.is_unit() || text.is_empty() {
                    if !text.is_empty() {
                        text.push_str("  ");
                    }
                    text.push_str(&display(&v));
                }
                self.say(text, false);
            }
            Err(e) => self.say(e.to_string(), true),
        }
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::HLine(value, label) => self.opts.hlines.push(HLine {
                value,
                color: Some(Color::Red),
                label,
            }),
            Action::ClearHLines => self.opts.hlines.clear(),
            Action::Zoom(span) => {
                if let Some((_, b)) = self.ds.x_range() {
                    self.view = Some((b - span, b));
                }
            }
            Action::ResetView => {
                self.view = None;
                self.selection = None;
            }
            Action::Show(n) => {
                self.hidden.remove(&n);
            }
            Action::Hide(n) => {
                self.hidden.insert(n);
            }
            Action::Chart(k) => match ChartKind::from_str(&k, true) {
                Ok(kind) => self.opts.kind = kind,
                Err(_) => self.say(format!("unknown chart type '{k}'"), true),
            },
        }
    }

    fn on_console_key(&mut self, key: KeyEvent) {
        let Some(input) = self.console.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.console = None,
            KeyCode::Enter => {
                let code = std::mem::take(input);
                self.console = None;
                self.history_pos = None;
                if !code.trim().is_empty() {
                    self.history.push(code.clone());
                    self.eval(&code);
                }
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => input.clear(),
            KeyCode::Up | KeyCode::Down if !self.history.is_empty() => {
                let last = self.history.len() - 1;
                let pos = match (key.code, self.history_pos) {
                    (KeyCode::Up, None) => last,
                    (KeyCode::Up, Some(p)) => p.saturating_sub(1),
                    (_, Some(p)) if p < last => p + 1,
                    _ => last,
                };
                self.history_pos = Some(pos);
                *input = self.history[pos].clone();
            }
            KeyCode::Char(c) => input.push(c),
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.console.is_some() {
            return self.on_console_key(key);
        }
        if self.help {
            self.help = false;
            return;
        }
        if let KeyCode::Char(c) = key.code
            && let Some(code) = self.binds.get(&c).cloned()
        {
            return self.eval(&code);
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc if self.selection.is_some() || self.anchor.is_some() => {
                self.selection = None;
                self.anchor = None;
            }
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(':') => self.console = Some(String::new()),
            KeyCode::Left | KeyCode::Char('h') if self.cursor.is_some() => self.move_cursor(-1),
            KeyCode::Right | KeyCode::Char('l') if self.cursor.is_some() => self.move_cursor(1),
            KeyCode::Left | KeyCode::Char('h') => self.pan(-0.1),
            KeyCode::Right | KeyCode::Char('l') => self.pan(0.1),
            KeyCode::Char('H') => self.pan(-0.5),
            KeyCode::Char('L') => self.pan(0.5),
            KeyCode::Char('+' | '=') => self.zoom(0.5),
            KeyCode::Char('-' | '_') => self.zoom(2.0),
            KeyCode::Char('0' | 'r') => {
                self.view = None;
                self.selection = None;
                self.anchor = None;
            }
            KeyCode::Char('c') => {
                if self.cursor.take().is_none() {
                    let (a, b) = self.bounds();
                    self.set_cursor_near((a + b) / 2.0);
                }
            }
            KeyCode::Home => {
                let xs = self.visible_xs();
                self.cursor = xs.first().copied();
                self.view = None;
            }
            KeyCode::End => {
                let xs = self.visible_xs();
                self.cursor = xs.last().copied();
                self.view = None;
            }
            KeyCode::Char('v') => {
                if self.cursor.is_none() {
                    let (a, b) = self.bounds();
                    self.set_cursor_near((a + b) / 2.0);
                }
                match (self.anchor.take(), self.cursor) {
                    (None, Some(c)) => {
                        self.anchor = Some(c);
                        self.selection = None;
                        self.say("selecting: move the cursor, then press v again", false);
                    }
                    (Some(a), Some(c)) => {
                        self.selection = Some((a.min(c), a.max(c)));
                        self.show_stats = true;
                    }
                    _ => {}
                }
            }
            KeyCode::Char(d @ '1'..='9') => self.toggle_series(d as usize - '1' as usize),
            KeyCode::Char('a') => self.hidden.clear(),
            KeyCode::Char('t') => self.opts.kind = self.opts.kind.next(),
            KeyCode::Char('m') => self.opts.marker = self.opts.marker.next(),
            KeyCode::Char('y') => self.opts.log = !self.opts.log,
            KeyCode::Char('x') => self.opts.split = !self.opts.split,
            KeyCode::Char('s') => self.show_stats = !self.show_stats,
            KeyCode::Char(' ' | 'p') => {
                self.paused = !self.paused;
                if !self.paused {
                    self.rebuild(true);
                }
            }
            KeyCode::Char('e') => self.export_view(),
            _ => {}
        }
    }

    // ───────────── drawing ─────────────

    fn draw(&self, f: &mut Frame) {
        let n_stats = self
            .ds
            .series
            .iter()
            .filter(|s| !self.hidden.contains(&s.name))
            .count() as u16;
        let stats_h = if self.show_stats {
            (n_stats + 3).min(f.area().height / 2)
        } else {
            0
        };
        let console_h = u16::from(self.console.is_some());
        let [chart, stats, status, console] = Layout::vertical([
            Constraint::Min(5),
            Constraint::Length(stats_h),
            Constraint::Length(1),
            Constraint::Length(console_h),
        ])
        .areas(f.area());

        let selection = match (self.anchor, self.cursor) {
            (Some(a), Some(c)) => Some((a.min(c), a.max(c))),
            _ => self.selection,
        };
        let overlay = Overlay {
            view: self.view,
            cursor: self.cursor,
            selection,
            hidden: Some(&self.hidden),
        };
        f.render_widget(ChartWidget::new(&self.ds, &self.opts, overlay), chart);
        if self.show_stats {
            self.draw_stats(f, stats);
        }
        f.render_widget(self.status_line(), status);
        if let Some(input) = &self.console {
            let line = Line::from(vec![
                Span::styled(":", Style::default().fg(Color::Yellow)),
                Span::raw(input.as_str()),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
            ]);
            f.render_widget(Paragraph::new(line), console);
        }
        if self.help {
            self.draw_help(f);
        }
    }

    fn status_line(&self) -> Line<'_> {
        let dim = Style::default().fg(Color::DarkGray);
        let mut spans = Vec::new();
        let state = if self.paused {
            Span::styled("⏸ paused ", Style::default().fg(Color::Yellow))
        } else if self.spec.is_live() && !self.eof {
            Span::styled("● live ", Style::default().fg(Color::Green))
        } else {
            Span::styled("■ ", dim)
        };
        spans.push(state);
        for a in &self.firing {
            spans.push(Span::styled(
                format!(" ALERT {a} "),
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
        }
        if let Some(c) = self.cursor {
            spans.push(Span::styled(
                self.ds.fmt_x_full(c),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            for (i, s) in self
                .ds
                .series
                .iter()
                .enumerate()
                .filter(|(_, s)| !self.hidden.contains(&s.name))
            {
                let idx = s.points.partition_point(|p| p.0 < c);
                let near = [idx.checked_sub(1), Some(idx)]
                    .into_iter()
                    .flatten()
                    .filter_map(|i| s.points.get(i))
                    .min_by(|a, b| (a.0 - c).abs().total_cmp(&(b.0 - c).abs()));
                if let Some(&(_, y)) = near {
                    spans.push(Span::styled(
                        format!("  {}=", s.name),
                        Style::default().fg(self.opts.color(i)),
                    ));
                    spans.push(Span::raw(s.unit.format(y)));
                }
            }
        } else if let Some(m) = self
            .message
            .as_ref()
            .filter(|m| m.at.elapsed() < Duration::from_secs(8) || m.error)
        {
            let style = if m.error {
                Style::default().fg(Color::LightRed)
            } else {
                Style::default()
            };
            spans.push(Span::styled(m.text.clone(), style));
        } else {
            let points: usize = self.ds.series.iter().map(|s| s.points.len()).sum();
            spans.push(Span::styled(
                format!(
                    "{} · {} series · {points} points · {} lines{}   ? help  : console  q quit",
                    self.opts.kind.name(),
                    self.ds.series.len(),
                    self.session.lines,
                    if self.view.is_some() {
                        " · zoomed (0 resets)"
                    } else {
                        ""
                    },
                ),
                dim,
            ));
        }
        if self.cursor.is_some()
            && let Some(m) = self
                .message
                .as_ref()
                .filter(|m| m.at.elapsed() < Duration::from_secs(8))
        {
            let style = if m.error {
                Style::default().fg(Color::LightRed)
            } else {
                dim
            };
            spans.push(Span::styled(format!("   {}", m.text), style));
        }
        Line::from(spans)
    }

    fn draw_stats(&self, f: &mut Frame, area: Rect) {
        let range = self.focus();
        let mut sub = self.ds.clone();
        if let Some((a, b)) = range {
            sub.series
                .iter_mut()
                .for_each(|s| s.points = s.range(a, b).to_vec());
        }
        let (per, label) = slope_unit(&sub, self.report.per);
        let header = [
            "series",
            "n",
            "last",
            "min",
            "mean",
            "max",
            "σ",
            &format!("Δ/{label}"),
            "R²",
            "forecast",
        ];
        // (cells, colour of the name, colour of the trend)
        let mut rows: Vec<(Vec<String>, Color, Color)> = Vec::new();
        for (i, s) in sub
            .series
            .iter()
            .enumerate()
            .filter(|(_, s)| !self.hidden.contains(&s.name))
        {
            let Some(st) = Stats::compute(&s.points) else {
                continue;
            };
            let f = |v: f64| s.unit.format(v);
            let trend = st.slope * per;
            let target = self
                .report
                .target
                .or_else(|| (st.slope < 0.0 && st.last > 0.0).then_some(0.0));
            let eta = match (target, sub.x_kind) {
                (Some(t), crate::data::XKind::Time) if !st.is_flat() => st
                    .eta(t)
                    .map(|d| format!("{} in {}", f(t), crate::time::fmt_duration(d)))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let (trend_text, r2, trend_color) = if st.count < 2 {
                ("—".into(), "—".into(), Color::Gray)
            } else {
                let color = if st.is_flat() {
                    Color::Gray
                } else if trend < 0.0 {
                    Color::LightRed
                } else {
                    Color::LightGreen
                };
                (
                    format!(
                        "{}{}",
                        if trend < 0.0 { "▼ -" } else { "▲ +" },
                        f(trend.abs())
                    ),
                    format!("{:.2}", st.r2),
                    color,
                )
            };
            let cells = vec![
                format!("{} {}", i + 1, s.name),
                st.count.to_string(),
                f(st.last),
                f(st.min),
                f(st.mean),
                f(st.max),
                f(st.stddev),
                trend_text,
                r2,
                eta,
            ];
            rows.push((cells, self.opts.color(i), trend_color));
        }
        let title = match (self.selection, range) {
            (Some((a, b)), _) => format!(
                " stats · selection {} → {} ",
                self.ds.fmt_x_full(a),
                self.ds.fmt_x_full(b)
            ),
            (None, Some(_)) => " stats · visible range ".into(),
            _ => " stats ".into(),
        };
        let widths: Vec<Constraint> = (0..header.len())
            .map(|c| {
                let w = rows
                    .iter()
                    .map(|r| r.0[c].chars().count())
                    .chain([header[c].chars().count()])
                    .max()
                    .unwrap_or(1);
                Constraint::Length(w as u16)
            })
            .collect();
        let rows = rows.into_iter().map(|(cells, name_color, trend_color)| {
            Row::new(cells.into_iter().enumerate().map(|(c, text)| {
                let style = match c {
                    0 => Style::default().fg(name_color),
                    7 => Style::default().fg(trend_color),
                    _ => Style::default(),
                };
                Cell::from(text).style(style)
            }))
        });
        let table = Table::new(rows, widths)
            .column_spacing(2)
            .header(
                Row::new(header.map(String::from))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(
                Block::bordered()
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(title),
            );
        f.render_widget(table, area);
    }

    fn draw_help(&self, f: &mut Frame) {
        let area = f.area();
        let w = 64.min(area.width);
        let h = (HELP.len() as u16 + 4 + self.binds.len() as u16).min(area.height);
        let rect = Rect {
            x: (area.width - w) / 2,
            y: (area.height - h) / 2,
            width: w,
            height: h,
        };
        let mut lines: Vec<Line> = HELP
            .iter()
            .map(|(k, d)| {
                Line::from(vec![
                    Span::styled(format!("{k:>12}  "), Style::default().fg(Color::Yellow)),
                    Span::raw(*d),
                ])
            })
            .collect();
        for (k, code) in &self.binds {
            lines.push(Line::from(vec![
                Span::styled(format!("{k:>12}  "), Style::default().fg(Color::Cyan)),
                Span::raw(format!("script: {code}")),
            ]));
        }
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .title(" graft keys · any key closes ")
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
            rect,
        );
    }
}

pub fn run(mut app: App) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut app, &mut terminal);
    ratatui::restore();
    result
}

fn event_loop(app: &mut App, terminal: &mut DefaultTerminal) -> Result<()> {
    while !app.quit {
        app.pump();
        app.rebuild(false);
        terminal.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(100))?
            && let TermEvent::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::render::DEFAULT_PALETTE;
    use crate::script::ScriptHost;
    use clap::Parser;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::rc::Rc;

    fn app(args: &[&str]) -> App {
        let cli = Cli::parse_from(args);
        let script = Rc::new(ScriptHost::new(cli.script.as_deref(), false).unwrap());
        let session = Session::new(&cli, script).unwrap();
        let spec = SourceSpec::File {
            path: cli.input.clone().unwrap(),
            follow: false,
        };
        let opts = ChartOpts {
            kind: cli.kind,
            marker: cli.marker,
            title: "test".into(),
            palette: DEFAULT_PALETTE.to_vec(),
            legend: cli.legend,
            log: false,
            ymin: None,
            ymax: None,
            hlines: vec![],
            bins: 10,
            split: false,
        };
        let report = ReportOpts {
            per: None,
            target: None,
            color: false,
        };
        let mut app = App::new(
            session,
            crate::source::spawn(spec.clone()),
            opts,
            report,
            cli.alert.clone(),
            spec,
            cli.stats,
        );
        while !app.eof {
            app.pump();
        }
        app.rebuild(true);
        app
    }

    fn key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_str(app: &mut App, s: &str) {
        s.chars().for_each(|c| key(app, KeyCode::Char(c)));
    }

    fn screen(app: &App) -> String {
        let mut term = Terminal::new(TestBackend::new(110, 32)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        crate::render::ansi::to_ansi(&buf, false)
    }

    const LOG: &str = "examples/reclaimable.log";

    #[test]
    fn draws_chart_and_stats() {
        let mut app = app(&["graft", LOG, "--stats"]);
        assert_eq!(app.ds.series.len(), 2);
        let s = screen(&app);
        assert!(s.contains("node1 58.8 GB"), "{s}");
        assert!(s.contains("stats"), "{s}");
        assert!(s.contains("▼ -1.97 GB"), "{s}");
        // Every chart type renders.
        for _ in 0..5 {
            key(&mut app, KeyCode::Char('t'));
            screen(&app);
        }
        assert_eq!(app.opts.kind, ChartKind::Line);
    }

    #[test]
    fn cursor_zoom_and_selection() {
        let mut app = app(&["graft", LOG]);
        key(&mut app, KeyCode::Home);
        assert_eq!(app.cursor, app.ds.x_range().map(|r| r.0));
        assert!(screen(&app).contains("2026-11-01 04:00  node1=59.9 GB  node2=422.6 GB"));
        key(&mut app, KeyCode::Char('v'));
        (0..6).for_each(|_| key(&mut app, KeyCode::Right));
        key(&mut app, KeyCode::Char('v'));
        let (a, b) = app.selection.expect("selection");
        assert_eq!(b - a, 6.0 * 3600.0);
        assert!(app.show_stats);
        assert!(screen(&app).contains("selection 2026-11-01 04:00 → 2026-11-01 10:00"));
        key(&mut app, KeyCode::Esc);
        assert!(app.selection.is_none() && !app.quit);
        key(&mut app, KeyCode::Char('+'));
        let (x0, x1) = app.view.expect("zoomed");
        assert!(x1 - x0 < 28.0 * 3600.0);
        key(&mut app, KeyCode::Char('0'));
        assert!(app.view.is_none());
        key(&mut app, KeyCode::Char('2'));
        assert!(app.hidden.contains("node2"));
        key(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn console_and_script_bindings() {
        let mut app = app(&["graft", LOG, "--script", "examples/keys.rhai"]);
        key(&mut app, KeyCode::Char(':'));
        type_str(
            &mut app,
            "fmt(\"node2\", last(\"node2\") - last(\"node1\"))",
        );
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            Some("356.9 GB")
        );
        // Console sees series hidden from the chart, and script functions.
        key(&mut app, KeyCode::Char('D'));
        let msg = &app.message.as_ref().unwrap().text;
        assert!(
            msg.starts_with("node1 -1.97 GB/day, empty in 29.1 days"),
            "{msg}"
        );
        key(&mut app, KeyCode::Char('W'));
        assert_eq!(app.opts.hlines.len(), 1);
        assert_eq!(app.opts.hlines[0].value, 50e9);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "nope(");
        key(&mut app, KeyCode::Enter);
        assert!(app.message.as_ref().unwrap().error);
        // History recall.
        key(&mut app, KeyCode::Char(':'));
        key(&mut app, KeyCode::Up);
        assert_eq!(app.console.as_deref(), Some("nope("));
        key(&mut app, KeyCode::Esc);
        assert!(app.console.is_none());
    }

    #[test]
    fn alerts_show_in_status() {
        let app = app(&["graft", LOG, "--alert", "node1 < 59GB"]);
        assert_eq!(app.firing, vec!["node1 < 59GB".to_string()]);
        assert!(screen(&app).contains("ALERT node1 < 59GB"));
    }
}
