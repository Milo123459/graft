//! Interactive viewer: live updates, zoom/pan, a scrubber cursor with a value readout,
//! range selection, stats panel, forecasts, a Rhai console, and full mouse/touch
//! support (tap, drag, scroll, toolbar) for use from a phone over SSH.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use ratatui::Frame;
use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, Widget, Wrap};

use crate::cli::{ChartKind, Cli, Window, parse_window};
use crate::data::Dataset;
use crate::interactive::{Live, Transition};
use crate::render::{
    ChartOpts, ChartWidget, HLine, Horizon, Overlay, PlotArea, plot_x_bounds, value_at,
};
use crate::script::{Action, display};
use crate::session::Session;
use crate::setup::Setup;
use crate::stats::SeriesReport;
use crate::transform::{Timeframe, timeframe_presets};
use crate::ui::{Button, Hits, TextInput, flow_height, render_buttons};

const HELP: &[(&str, &str)] = &[
    ("— moving around —", ""),
    ("← → / h l", "move left / right (when zoomed in)"),
    ("↑ ↓ / + -", "zoom in / out"),
    (
        "scroll",
        "zoom around the pointer; sideways scroll (or shift) moves",
    ),
    ("reset / 0", "show everything again"),
    ("— looking at values —", ""),
    (
        "tap / cursor / c",
        "a line showing every series' value at that point",
    ),
    (
        "drag the line",
        "scrub through the data, and into the forecast",
    ),
    (", . / shift ← →", "step the cursor point by point"),
    (
        "follow / o",
        "the cursor follows the mouse without clicking",
    ),
    ("— the toolbar —", ""),
    (
        "measure / v",
        "tap the start, then the end: stats for just that stretch",
    ),
    (
        "",
        "(or drag across, or click then shift+click); again to clear",
    ),
    (
        "stats / s",
        "table: trend per day, forecast, last, min, max…",
    ),
    (
        "forecast / f",
        "project each trend forward (targets: --target)",
    ),
    ("split / x", "one panel per series, each with its own scale"),
    ("all time / [ ]", "only show recent data: last 24h, 6h, 1h…"),
    (
        "⋯",
        "more: chart type, log scale, trend & target lines, pause,",
    ),
    (
        "",
        "format (change how lines are read), console, export, mouse",
    ),
    ("— other keys —", ""),
    ("1-9 / a", "show or hide a series / show all"),
    ("t m y", "chart type, marker, log scale"),
    ("T / G", "trend line over the data / target lines"),
    (
        "F",
        "change the format or source (builder, with suggestions)",
    ),
    ("e / space", "export to SVG / pause live updates"),
    ("M", "mouse on/off (off = select text in your terminal)"),
    (":", "Rhai console, e.g. :slope(\"node1\", \"1d\")"),
    ("q / Esc", "quit (Esc first clears a measurement)"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    PanLeft,
    PanRight,
    ZoomIn,
    ZoomOut,
    Reset,
    Cursor,
    Follow,
    Select,
    CursorPrev,
    CursorNext,
    Kind,
    Split,
    Log,
    Stats,
    Forecast,
    Pause,
    Format,
    Console,
    Export,
    Help,
    Quit,
    More,
    Marker,
    Mouse,
    Trend,
    Targets,
    /// Step through timeframe presets (-1 narrower, +1 wider).
    RangeStep(i8),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Hit {
    Cmd(Cmd),
    Series(usize),
    /// A timeframe preset (`None` = all data).
    Range(Option<Window>),
}

/// What a mouse press on the chart turned into.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    /// Pressed; becomes a tap or a selection depending on movement.
    Pending {
        col: u16,
        x: f64,
    },
    Selecting,
    /// Dragging the cursor line.
    Scrub,
}

struct Message {
    text: String,
    error: bool,
    at: Instant,
}

pub struct App {
    cli: Cli,
    session: Session,
    live: Live,
    opts: ChartOpts,
    alerts: Vec<String>,
    /// Every series (for scripts and alerts).
    full: Dataset,
    /// The selected series being displayed.
    ds: Dataset,
    built_rev: u64,
    last_build: Instant,
    view: Option<(f64, f64)>,
    cursor: Option<f64>,
    anchor: Option<f64>,
    /// "measure" is waiting for taps on the chart (start, then end).
    measuring: bool,
    selection: Option<(f64, f64)>,
    hidden: HashSet<String>,
    show_stats: bool,
    paused: bool,
    help: bool,
    console: Option<TextInput>,
    history: Vec<String>,
    history_pos: Option<usize>,
    message: Option<Message>,
    binds: HashMap<char, String>,
    firing: Vec<String>,
    mouse: bool,
    follow_pointer: bool,
    toolbar_more: bool,
    drag: Option<Drag>,
    /// Horizon to use when forecasting is switched on.
    horizon: Horizon,
    hits: Hits<Hit>,
    plots: RefCell<Vec<PlotArea>>,
    /// The part of the data shown and analysed.
    timeframe: Timeframe,
    /// Timeframes shorter than the data, for the picker.
    presets: Vec<Window>,
}

impl App {
    pub fn new(setup: Setup, live: Live) -> Self {
        let Setup {
            cli,
            mut session,
            opts,
            timeframe,
            ..
        } = setup;
        // Re-parse history kept from a previous configuration.
        for (line, meta) in &live.raw {
            let _ = session.feed(line, meta);
        }
        let binds = session
            .script
            .binds()
            .into_iter()
            .filter_map(|(k, code)| Some((k.chars().next()?, code)))
            .collect();
        let horizon = opts.forecast.unwrap_or(Horizon::Auto);
        let mut app = Self {
            alerts: cli.alert.clone(),
            show_stats: cli.stats,
            mouse: !cli.no_mouse,
            cli,
            session,
            live,
            opts,
            full: Dataset::default(),
            ds: Dataset::default(),
            built_rev: u64::MAX,
            last_build: Instant::now() - Duration::from_secs(1),
            view: None,
            cursor: None,
            anchor: None,
            measuring: false,
            selection: None,
            hidden: HashSet::new(),
            paused: false,
            help: false,
            console: None,
            history: Vec::new(),
            history_pos: None,
            message: None,
            binds,
            firing: Vec::new(),
            follow_pointer: false,
            toolbar_more: false,
            drag: None,
            horizon,
            hits: Hits::default(),
            plots: RefCell::new(Vec::new()),
            timeframe,
            presets: Vec::new(),
        };
        app.rebuild(true);
        app
    }

    /// The configuration as currently shown (interactive changes folded back in).
    pub fn current_cli(&self) -> Cli {
        let mut c = self.cli.clone();
        c.kind = self.opts.kind;
        c.marker = self.opts.marker;
        c.split = self.opts.split;
        c.log = self.opts.log;
        c.stats = self.show_stats;
        c.no_mouse = !self.mouse;
        c.target = self.opts.analysis.targets.to_specs();
        c.trend = self.opts.show_trend;
        c.no_targets = !self.opts.show_targets;
        c.last = self.timeframe.last;
        if self.timeframe.since.is_none() {
            c.since = None;
        }
        if self.timeframe.until.is_none() {
            c.until = None;
        }
        c.forecast = self.opts.forecast.map(|h| match h {
            Horizon::Auto => "auto".into(),
            Horizon::Span(s) => crate::time::fmt_duration_compact(s),
        });
        c
    }

    /// Hand over the source (and its history) to a new viewer.
    pub fn into_live(self) -> Live {
        self.live
    }

    pub fn live(&self) -> &Live {
        &self.live
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some(Message {
            text: text.into(),
            error,
            at: Instant::now(),
        });
    }

    /// Read input and refresh the view (called every frame).
    pub fn tick(&mut self) {
        let session = &mut self.session;
        let errors = self
            .live
            .pump(Duration::from_millis(40), &mut |line, meta| {
                let _ = session.feed(line, meta);
            });
        if let Some(e) = errors.into_iter().last() {
            self.say(e, true);
        }
        self.rebuild(false);
    }

    fn rebuild(&mut self, force: bool) {
        let rev = self.session.store.revision;
        if !force {
            // Throttle rebuilds while data is streaming in.
            let due = self.live.eof || self.last_build.elapsed() > Duration::from_millis(150);
            if self.paused || rev == self.built_rev || !due {
                return;
            }
        }
        let old_max = self.ds.x_range().map(|r| r.1);
        self.full = self.session.full_dataset();
        self.presets = timeframe_presets(&self.full);
        self.timeframe.apply(&mut self.full);
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
        plot_x_bounds(&self.ds, &self.opts, self.view, Some(&self.hidden))
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

    /// Everything there is to see: all data plus any forecast.
    fn full_bounds(&self) -> (f64, f64) {
        plot_x_bounds(&self.ds, &self.opts, None, Some(&self.hidden))
    }

    /// Show `[lo, hi]`, kept inside the data (and forecast) and no wider than it;
    /// zooming all the way out goes back to showing everything.
    fn set_view(&mut self, lo: f64, hi: f64) {
        let (a, b) = self.full_bounds();
        let full = b - a;
        if full <= 0.0 {
            return;
        }
        let span = (hi - lo).max(full / 5000.0);
        if span >= full * 0.999 {
            self.view = None;
            return;
        }
        let lo = lo.clamp(a, b - span);
        self.view = Some((lo, lo + span));
    }

    fn zoom_at(&mut self, factor: f64, centre: Option<f64>) {
        let (x0, x1) = self.bounds();
        let centre = centre
            .or(self.cursor)
            .unwrap_or((x0 + x1) / 2.0)
            .clamp(x0, x1);
        self.set_view(
            centre - (centre - x0) * factor,
            centre + (x1 - centre) * factor,
        );
    }

    fn pan(&mut self, frac: f64) {
        if self.view.is_none() {
            self.say(
                "everything is already in view: zoom in (↑, + or scroll) to move around",
                false,
            );
            return;
        }
        let (x0, x1) = self.bounds();
        let d = (x1 - x0) * frac;
        self.set_view(x0 + d, x1 + d);
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
            if x < x0 || x > x1 {
                let half = (x1 - x0) / 2.0;
                self.set_view(x - half, x + half);
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

    fn set_timeframe(&mut self, tf: Timeframe) {
        self.timeframe = tf;
        self.view = None;
        self.selection = None;
        self.anchor = None;
        self.rebuild(true);
        let label = self.timeframe.label(&self.ds);
        self.say(
            format!("timeframe: {label} — stats, trends and forecasts use this range"),
            false,
        );
    }

    /// Move through [presets…, all] (wrapping).
    fn step_timeframe(&mut self, dir: i8) {
        let mut options: Vec<Option<Window>> = self.presets.iter().copied().map(Some).collect();
        options.push(None);
        let current = if self.timeframe.since.is_some() || self.timeframe.until.is_some() {
            None
        } else {
            self.timeframe.last
        };
        let i = options
            .iter()
            .position(|o| *o == current)
            .unwrap_or(options.len() - 1) as isize;
        let n = options.len() as isize;
        let next = options[((i + dir as isize) % n + n) as usize % options.len()];
        self.set_timeframe(next.map_or_else(Timeframe::default, Timeframe::last));
    }

    fn toggle_forecast(&mut self) {
        self.opts.forecast = match self.opts.forecast {
            Some(h) => {
                self.horizon = h;
                None
            }
            None => Some(self.horizon),
        };
        if self.opts.forecast.is_some() && self.view.is_none() {
            self.say("forecast on: dotted lines project each trend (set targets with :target(\"name\", 50GB))", false);
        }
    }

    /// Keyboard measuring: `v` at the start, move the cursor, `v` at the end.
    fn start_selection(&mut self) {
        if self.cursor.is_none() {
            let (a, b) = self.bounds();
            self.set_cursor_near((a + b) / 2.0);
        }
        match (self.anchor.take(), self.cursor) {
            (None, Some(c)) => {
                self.anchor = Some(c);
                self.selection = None;
                self.say(
                    "measuring from here: move the cursor (, . or drag) and press v again",
                    false,
                );
            }
            (Some(a), Some(c)) => self.finish_measure(a, c),
            _ => {}
        }
    }

    fn finish_measure(&mut self, a: f64, b: f64) {
        self.selection = Some((a.min(b), a.max(b)));
        self.anchor = None;
        self.measuring = false;
        self.show_stats = true;
        self.say(
            "stats now cover only the measured stretch (see 'change'); tap measure or Esc to clear",
            false,
        );
    }

    /// The "measure" button: start measuring, or clear a measurement.
    fn toggle_measure(&mut self) {
        if self.measuring || self.anchor.is_some() || self.selection.is_some() {
            self.measuring = false;
            self.anchor = None;
            self.selection = None;
            self.say("measurement cleared", false);
        } else {
            self.measuring = true;
            self.say(
                "measure: tap where it starts, then where it ends (or drag across)",
                false,
            );
        }
    }

    /// A tap on the chart at x: part of a measurement, or placing the cursor.
    fn tap_at(&mut self, x: f64) {
        if self.measuring {
            match self.anchor {
                None => {
                    self.anchor = Some(x);
                    self.cursor = Some(x);
                    self.say("now tap where it ends", false);
                }
                Some(a) => {
                    self.cursor = Some(x);
                    self.finish_measure(a, x);
                }
            }
        } else {
            self.cursor = Some(x);
        }
    }

    fn run_cmd(&mut self, cmd: Cmd) -> Transition {
        match cmd {
            Cmd::PanLeft => self.pan(-0.2),
            Cmd::PanRight => self.pan(0.2),
            Cmd::CursorPrev => self.move_cursor(-1),
            Cmd::CursorNext => self.move_cursor(1),
            Cmd::ZoomIn => self.zoom_at(0.5, None),
            Cmd::ZoomOut => self.zoom_at(2.0, None),
            Cmd::Reset => {
                self.view = None;
                self.selection = None;
                self.anchor = None;
            }
            Cmd::Cursor => {
                if self.cursor.take().is_none() {
                    let (a, b) = self.bounds();
                    self.set_cursor_near((a + b) / 2.0);
                } else {
                    self.follow_pointer = false;
                }
            }
            Cmd::Follow => {
                self.follow_pointer = !self.follow_pointer;
                if self.follow_pointer {
                    self.say(
                        "cursor follows the pointer (needs a terminal that reports mouse movement)",
                        false,
                    );
                }
            }
            Cmd::Select => self.toggle_measure(),
            Cmd::Kind => self.opts.kind = self.opts.kind.next(),
            Cmd::Marker => self.opts.marker = self.opts.marker.next(),
            Cmd::Split => self.opts.split = !self.opts.split,
            Cmd::Log => self.opts.log = !self.opts.log,
            Cmd::Stats => self.show_stats = !self.show_stats,
            Cmd::Forecast => self.toggle_forecast(),
            Cmd::Pause => {
                self.paused = !self.paused;
                if !self.paused {
                    self.rebuild(true);
                }
            }
            Cmd::Format => return Transition::OpenBuilder,
            Cmd::Console => self.console = Some(TextInput::default()),
            Cmd::Export => self.export_view(),
            Cmd::Help => self.help = !self.help,
            Cmd::Quit => return Transition::Quit,
            Cmd::More => self.toolbar_more = !self.toolbar_more,
            Cmd::Trend => self.opts.show_trend = !self.opts.show_trend,
            Cmd::Targets => self.opts.show_targets = !self.opts.show_targets,
            Cmd::RangeStep(dir) => self.step_timeframe(dir),
            Cmd::Mouse => {
                self.mouse = !self.mouse;
                self.say(
                    if self.mouse {
                        "mouse on"
                    } else {
                        "mouse off: terminal text selection works; M turns it back on"
                    },
                    false,
                );
                return Transition::Mouse(self.mouse);
            }
        }
        Transition::None
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
                    self.set_view(b - span, b);
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
            Action::Target(name, value) => self.opts.analysis.targets.set(&name, value),
            Action::Forecast(h) => match h.map(|h| Horizon::parse(&h)).transpose() {
                Ok(h) => self.opts.forecast = h,
                Err(e) => self.say(format!("forecast: {e}"), true),
            },
            Action::Range(r) => match r.map(|r| parse_window(&r)).transpose() {
                Ok(w) => self.set_timeframe(w.map_or_else(Timeframe::default, Timeframe::last)),
                Err(e) => self.say(format!("range: {e}"), true),
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
                let code = std::mem::take(&mut input.value);
                self.console = None;
                self.history_pos = None;
                if !code.trim().is_empty() {
                    self.history.push(code.clone());
                    self.eval(&code);
                }
            }
            KeyCode::Up | KeyCode::Down if !self.history.is_empty() => {
                let last = self.history.len() - 1;
                let pos = match (key.code, self.history_pos) {
                    (KeyCode::Up, None) => last,
                    (KeyCode::Up, Some(p)) => p.saturating_sub(1),
                    (_, Some(p)) if p < last => p + 1,
                    _ => last,
                };
                self.history_pos = Some(pos);
                input.set(self.history[pos].clone());
            }
            _ => {
                input.key(key);
            }
        }
    }

    // ───────────── events ─────────────

    pub fn on_event(&mut self, ev: &TermEvent) -> Transition {
        match ev {
            TermEvent::Key(k) if k.kind == KeyEventKind::Press => self.on_key(*k),
            TermEvent::Mouse(m) => self.on_mouse(*m),
            _ => Transition::None,
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Transition {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Transition::Quit;
        }
        if self.console.is_some() {
            self.on_console_key(key);
            return Transition::None;
        }
        if self.help {
            self.help = false;
            return Transition::None;
        }
        if let KeyCode::Char(c) = key.code
            && let Some(code) = self.binds.get(&c).cloned()
        {
            self.eval(&code);
            return Transition::None;
        }
        let cmd = match key.code {
            KeyCode::Char('q') => Cmd::Quit,
            KeyCode::Esc if self.selection.is_some() || self.anchor.is_some() || self.measuring => {
                self.selection = None;
                self.anchor = None;
                self.measuring = false;
                return Transition::None;
            }
            KeyCode::Esc => Cmd::Quit,
            KeyCode::Char('?') => Cmd::Help,
            KeyCode::Char(':') => Cmd::Console,
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => Cmd::CursorPrev,
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => Cmd::CursorNext,
            KeyCode::Char(',' | '<') => Cmd::CursorPrev,
            KeyCode::Char('.' | '>') => Cmd::CursorNext,
            KeyCode::Left | KeyCode::Char('h') => Cmd::PanLeft,
            KeyCode::Right | KeyCode::Char('l') => Cmd::PanRight,
            KeyCode::Up | KeyCode::Char('k') => Cmd::ZoomIn,
            KeyCode::Down | KeyCode::Char('j') => Cmd::ZoomOut,
            KeyCode::Char('H') => {
                self.pan(-0.5);
                return Transition::None;
            }
            KeyCode::Char('L') => {
                self.pan(0.5);
                return Transition::None;
            }
            KeyCode::Char('+' | '=') => Cmd::ZoomIn,
            KeyCode::Char('-' | '_') => Cmd::ZoomOut,
            KeyCode::Char('0' | 'r') => Cmd::Reset,
            KeyCode::Char('c') => Cmd::Cursor,
            KeyCode::Char('o') => Cmd::Follow,
            KeyCode::Home | KeyCode::End => {
                let xs = self.visible_xs();
                self.cursor = if key.code == KeyCode::Home {
                    xs.first()
                } else {
                    xs.last()
                }
                .copied();
                self.view = None;
                return Transition::None;
            }
            KeyCode::Char('v') => {
                self.start_selection();
                return Transition::None;
            }
            KeyCode::Char(d @ '1'..='9') => {
                self.toggle_series(d as usize - '1' as usize);
                return Transition::None;
            }
            KeyCode::Char('a') => {
                self.hidden.clear();
                return Transition::None;
            }
            KeyCode::Char('t') => Cmd::Kind,
            KeyCode::Char('m') => Cmd::Marker,
            KeyCode::Char('y') => Cmd::Log,
            KeyCode::Char('x') => Cmd::Split,
            KeyCode::Char('s') => Cmd::Stats,
            KeyCode::Char('f') => Cmd::Forecast,
            KeyCode::Char('F') => Cmd::Format,
            KeyCode::Char('M') => Cmd::Mouse,
            KeyCode::Char('T') => Cmd::Trend,
            KeyCode::Char('G') => Cmd::Targets,
            KeyCode::Char('[') => Cmd::RangeStep(-1),
            KeyCode::Char(']') => Cmd::RangeStep(1),
            KeyCode::Char(' ' | 'p') => Cmd::Pause,
            KeyCode::Char('e') => Cmd::Export,
            _ => return Transition::None,
        };
        self.run_cmd(cmd)
    }

    fn plot_at(&self, col: u16, row: u16) -> Option<PlotArea> {
        self.plots
            .borrow()
            .iter()
            .find(|p| p.contains(col, row))
            .copied()
    }

    /// Any plot (they share an x axis) — for drags that leave the plot area.
    fn any_plot(&self) -> Option<PlotArea> {
        self.plots.borrow().first().copied()
    }

    fn on_mouse(&mut self, m: MouseEvent) -> Transition {
        let (col, row) = (m.column, m.row);
        if self.help {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.help = false;
            }
            return Transition::None;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, hit)) = self.hits.at(col, row) {
                    return match hit {
                        Hit::Cmd(cmd) => self.run_cmd(cmd),
                        Hit::Range(w) => {
                            self.set_timeframe(w.map_or_else(Timeframe::default, Timeframe::last));
                            Transition::None
                        }
                        Hit::Series(i) => {
                            self.toggle_series(i);
                            Transition::None
                        }
                    };
                }
                // Shift+click: measure from the cursor (or the previous shift+click) to here.
                if let Some(plot) = self.plot_at(col, row)
                    && m.modifiers.contains(KeyModifiers::SHIFT)
                {
                    let x = plot.x_at(col);
                    match self.anchor.or(self.cursor) {
                        Some(start) if (plot.col_of(start)).abs_diff(col) >= 1 => {
                            self.cursor = Some(x);
                            self.finish_measure(start, x);
                        }
                        _ => {
                            self.anchor = Some(x);
                            self.cursor = Some(x);
                            self.say("shift+click where the range ends", false);
                        }
                    }
                    return Transition::None;
                }
                if let Some(plot) = self.plot_at(col, row) {
                    let on_cursor = self
                        .cursor
                        .is_some_and(|c| plot.col_of(c).abs_diff(col) <= 1);
                    self.drag = Some(if on_cursor {
                        Drag::Scrub
                    } else {
                        Drag::Pending {
                            col,
                            x: plot.x_at(col),
                        }
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(plot) = self.plot_at(col, row).or_else(|| self.any_plot()) else {
                    return Transition::None;
                };
                let x = plot.x_at(col);
                match self.drag {
                    Some(Drag::Scrub) => self.cursor = Some(x),
                    Some(Drag::Pending { col: c0, x: x0 }) if c0.abs_diff(col) >= 2 => {
                        self.drag = Some(Drag::Selecting);
                        self.anchor = Some(x0);
                        self.selection = None;
                        self.cursor = Some(x);
                    }
                    Some(Drag::Selecting) => self.cursor = Some(x),
                    _ => {}
                }
            }
            MouseEventKind::Up(MouseButton::Left) => match self.drag.take() {
                Some(Drag::Pending { x, .. }) => self.tap_at(x),
                Some(Drag::Selecting) => {
                    if let (Some(a), Some(c)) = (self.anchor.take(), self.cursor) {
                        self.finish_measure(a, c);
                    }
                }
                _ => {}
            },
            MouseEventKind::Moved if self.follow_pointer => {
                if let Some(plot) = self.plot_at(col, row) {
                    self.cursor = Some(plot.x_at(col));
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if let Some(plot) = self.plot_at(col, row) {
                    // Small steps: trackpads send many events per swipe.
                    let up = m.kind == MouseEventKind::ScrollUp;
                    if m.modifiers.contains(KeyModifiers::SHIFT) {
                        self.pan(if up { -0.05 } else { 0.05 });
                    } else {
                        self.zoom_at(if up { 0.9 } else { 1.0 / 0.9 }, Some(plot.x_at(col)));
                    }
                }
            }
            MouseEventKind::ScrollLeft => self.pan(-0.05),
            MouseEventKind::ScrollRight => self.pan(0.05),
            _ => {}
        }
        Transition::None
    }

    // ───────────── drawing ─────────────

    fn toolbar(&self) -> Vec<Button<Hit>> {
        let b = |l: &str, c: Cmd| Button::new(l, Hit::Cmd(c));
        let mut v = vec![
            b("◀", Cmd::PanLeft),
            b("▶", Cmd::PanRight),
            b("+", Cmd::ZoomIn),
            b("−", Cmd::ZoomOut),
            b("reset", Cmd::Reset).active(self.view.is_some()),
            b("cursor", Cmd::Cursor).active(self.cursor.is_some()),
            b(
                if self.selection.is_some() {
                    "clear measure"
                } else {
                    "measure"
                },
                Cmd::Select,
            )
            .active(self.measuring || self.anchor.is_some() || self.selection.is_some()),
            b("stats", Cmd::Stats).active(self.show_stats),
            b("forecast", Cmd::Forecast).active(self.opts.forecast.is_some()),
            b("split", Cmd::Split).active(self.opts.split),
        ];
        // The timeframe picker (tap to step to a narrower range, wrapping to all).
        if !self.presets.is_empty() || !self.timeframe.is_all() {
            let label = if self.timeframe.is_all() {
                "all time".to_string()
            } else if self.timeframe.last.is_some() {
                format!("last {}", self.timeframe.label(&self.ds))
            } else {
                self.timeframe.label(&self.ds)
            };
            v.push(b(&label, Cmd::RangeStep(-1)).active(!self.timeframe.is_all()));
        }
        v.push(
            b(if self.toolbar_more { "less" } else { "⋯" }, Cmd::More).active(self.toolbar_more),
        );
        if self.toolbar_more {
            let current = (self.timeframe.since.is_none() && self.timeframe.until.is_none())
                .then_some(self.timeframe.last)
                .flatten();
            v.push(Button::new("all", Hit::Range(None)).active(self.timeframe.is_all()));
            for w in &self.presets {
                let label = match w {
                    Window::Span(d) => crate::time::fmt_duration_compact(*d),
                    Window::Points(n) => format!("{n} pts"),
                };
                v.push(Button::new(label, Hit::Range(Some(*w))).active(current == Some(*w)));
            }
            v.extend([
                b("trend", Cmd::Trend).active(self.opts.show_trend),
                b("targets", Cmd::Targets).active(self.opts.show_targets),
                b(self.opts.kind.name(), Cmd::Kind),
                b("log", Cmd::Log).active(self.opts.log),
                b("follow", Cmd::Follow).active(self.follow_pointer),
                b(if self.paused { "resume" } else { "pause" }, Cmd::Pause).active(self.paused),
                b("format", Cmd::Format),
                b(":", Cmd::Console),
                b("svg", Cmd::Export),
                b("mouse", Cmd::Mouse).active(self.mouse),
                b("?", Cmd::Help),
                b("quit", Cmd::Quit),
            ]);
        }
        v
    }

    fn chips(&self) -> Vec<Button<Hit>> {
        self.ds
            .series
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let shown = !self.hidden.contains(&s.name);
                Button::new(
                    format!("{} {}", if shown { "●" } else { "○" }, s.name),
                    Hit::Series(i),
                )
                .active(shown)
            })
            .collect()
    }

    pub fn draw(&self, f: &mut Frame) {
        self.hits.clear();
        self.plots.borrow_mut().clear();
        let area = f.area();
        let n_stats = self
            .ds
            .series
            .iter()
            .filter(|s| !self.hidden.contains(&s.name))
            .count() as u16;
        let stats_h = if self.show_stats {
            (n_stats + 3).min(area.height / 3).max(4)
        } else {
            0
        };
        let (chips, toolbar) = (self.chips(), self.toolbar());
        let (chips_h, tool_h) = if self.mouse {
            (
                flow_height(&chips, area.width).min(2),
                flow_height(&toolbar, area.width),
            )
        } else {
            (0, 0)
        };
        let console_h = u16::from(self.console.is_some());
        let [chart, stats, chips_area, tool_area, status, console] = Layout::vertical([
            Constraint::Min(6),
            Constraint::Length(stats_h),
            Constraint::Length(chips_h),
            Constraint::Length(tool_h),
            Constraint::Length(1),
            Constraint::Length(console_h),
        ])
        .areas(area);

        let selection = match (self.anchor, self.cursor) {
            (Some(a), Some(c)) => Some((a.min(c), a.max(c))),
            _ => self.selection,
        };
        let overlay = Overlay {
            view: self.view,
            cursor: self.cursor,
            selection,
            hidden: Some(&self.hidden),
            plots: Some(&self.plots),
        };
        f.render_widget(ChartWidget::new(&self.ds, &self.opts, overlay), chart);
        if self.show_stats {
            self.draw_stats(f, stats);
        }
        if self.mouse {
            self.draw_chips(&chips, chips_area, f);
            render_buttons(&toolbar, tool_area, f.buffer_mut(), &self.hits);
        }
        f.render_widget(Paragraph::new(self.status_line()), status);
        if let Some(input) = &self.console {
            let [prompt, field] =
                Layout::horizontal([Constraint::Length(1), Constraint::Min(1)]).areas(console);
            f.render_widget(
                Span::styled(":", Style::default().fg(Color::Yellow)),
                prompt,
            );
            input.render(field, f.buffer_mut(), true, "");
        }
        if self.help {
            self.draw_help(f);
        }
    }

    fn draw_chips(&self, chips: &[Button<Hit>], area: Rect, f: &mut Frame) {
        let (rects, _) = crate::ui::flow(chips, area.width);
        for ((i, chip), r) in chips.iter().enumerate().zip(rects) {
            if r.y >= area.height {
                break;
            }
            let rect = Rect {
                x: area.x + r.x,
                y: area.y + r.y,
                ..r
            };
            // No backgrounds: a coloured marker and the name, dimmed when hidden.
            let color = self.opts.color(i);
            let (mark, name) = chip.label.split_once(' ').unwrap_or(("●", &chip.label));
            let line = if chip.active {
                Line::from(vec![
                    Span::styled(format!(" {mark} "), Style::default().fg(color)),
                    Span::styled(
                        format!("{name} "),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ])
            } else {
                Line::from(Span::styled(
                    format!(" {mark} {name} "),
                    Style::default().fg(Color::DarkGray),
                ))
            };
            f.render_widget(line, rect);
            self.hits.push(rect, chip.action);
        }
    }

    fn status_line(&self) -> Line<'_> {
        let dim = Style::default().fg(Color::DarkGray);
        let mut spans = Vec::new();
        spans.push(if self.paused {
            Span::styled("⏸ paused ", Style::default().fg(Color::Yellow))
        } else if self.live.spec.is_live() && !self.live.eof {
            Span::styled("● live ", Style::default().fg(Color::Green))
        } else {
            Span::styled("■ ", dim)
        });
        // Lines are arriving but the format matches none of them: say so, rather than
        // leaving "waiting for data…" on screen.
        if self.session.matched == 0 && self.session.lines > 0 {
            spans.push(Span::styled(
                format!(
                    " {} lines read, none matched the format — press F to see them and fix it ",
                    self.session.lines
                ),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ));
            spans.push(Span::raw(" "));
        }
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
        let message = self
            .message
            .as_ref()
            .filter(|m| m.at.elapsed() < Duration::from_secs(8) || m.error);
        if let Some(c) = self.cursor {
            spans.push(Span::styled(
                self.ds.fmt_x_full(crate::render::round_x(&self.ds, c)),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            for (i, s) in self
                .ds
                .series
                .iter()
                .enumerate()
                .filter(|(_, s)| !self.hidden.contains(&s.name))
            {
                if let Some((y, projected)) = value_at(s, &self.opts, c) {
                    spans.push(Span::styled(
                        format!("  {}=", s.name),
                        Style::default().fg(self.opts.color(i)),
                    ));
                    spans.push(Span::raw(format!(
                        "{}{}",
                        if projected { "≈" } else { "" },
                        s.unit.format(y)
                    )));
                }
            }
            if let Some(m) = message {
                spans.push(Span::styled(
                    format!("   {}", m.text),
                    if m.error {
                        Style::default().fg(Color::LightRed)
                    } else {
                        dim
                    },
                ));
            }
        } else if let Some(m) = message {
            spans.push(Span::styled(
                m.text.clone(),
                if m.error {
                    Style::default().fg(Color::LightRed)
                } else {
                    Style::default()
                },
            ));
        } else {
            let points: usize = self.ds.series.iter().map(|s| s.points.len()).sum();
            let hint = if self.mouse {
                "tap to inspect · drag to measure · ? help"
            } else {
                "? help  : console  q quit"
            };
            spans.push(Span::styled(
                format!(
                    "{} · {} series · {points} pts{}   {hint}",
                    self.opts.kind.name(),
                    self.ds.series.len(),
                    if self.view.is_some() {
                        " · zoomed"
                    } else {
                        ""
                    }
                ),
                dim,
            ));
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
        let reports: Vec<(usize, SeriesReport)> = sub
            .series
            .iter()
            .enumerate()
            .filter(|(_, s)| !self.hidden.contains(&s.name))
            .filter_map(|(i, s)| Some((i, self.opts.analysis.report(&sub, s)?)))
            .collect();
        let label = reports
            .first()
            .map_or("day".to_string(), |r| r.1.per_label.clone());
        // Columns in order of importance: narrow screens drop from the end.
        let delta = format!("Δ/{label}");
        let headers = [
            "series",
            delta.as_str(),
            "forecast",
            "change",
            "last",
            "R²",
            "min",
            "max",
            "mean",
            "σ",
            "n",
        ];
        let rows: Vec<(Vec<String>, Color, Color)> = reports
            .iter()
            .map(|(i, r)| {
                let f = |v: f64| r.unit.format(v);
                let st = &r.stats;
                let trend_color = match r.direction() {
                    "falling" => Color::LightRed,
                    "rising" => Color::LightGreen,
                    _ => Color::Gray,
                };
                let trend = r
                    .trend_text()
                    .trim_end_matches(&format!("/{label}"))
                    .to_string();
                let cells = vec![
                    format!("{} {}", i + 1, r.name),
                    match r.direction() {
                        "falling" => format!("▼ {trend}"),
                        "rising" => format!("▲ {trend}"),
                        _ => trend,
                    },
                    r.forecast_cell(),
                    // How much it moved across the measured (or visible) stretch.
                    format!(
                        "{}{}",
                        if st.last < st.first { "-" } else { "+" },
                        f((st.last - st.first).abs())
                    ),
                    f(st.last),
                    r.fit.map_or("—".into(), |fit| format!("{:.2}", fit.r2)),
                    f(st.min),
                    f(st.max),
                    f(st.mean),
                    f(st.stddev),
                    st.count.to_string(),
                ];
                (cells, self.opts.color(*i), trend_color)
            })
            .collect();
        let mut widths: Vec<u16> = (0..headers.len())
            .map(|c| {
                rows.iter()
                    .map(|r| r.0[c].chars().count())
                    .chain([headers[c].chars().count()])
                    .max()
                    .unwrap_or(1) as u16
            })
            .collect();
        let inner_w = area.width.saturating_sub(2);
        let mut ncols = headers.len();
        while ncols > 2 && widths[..ncols].iter().sum::<u16>() + 2 * (ncols as u16 - 1) > inner_w {
            ncols -= 1;
        }
        widths.truncate(ncols);
        let title = match (self.selection, range) {
            (Some((a, b)), _) => format!(
                " stats · selection {} → {} ",
                self.ds.fmt_x_full(a),
                self.ds.fmt_x_full(b)
            ),
            (None, Some(_)) => " stats · visible range ".into(),
            _ => " stats ".into(),
        };
        let rows = rows.into_iter().map(|(cells, name_color, trend_color)| {
            Row::new(cells.into_iter().take(ncols).enumerate().map(|(c, text)| {
                let style = match c {
                    0 => Style::default().fg(name_color),
                    1 => Style::default().fg(trend_color),
                    _ => Style::default(),
                };
                Cell::from(text).style(style)
            }))
        });
        let table = Table::new(rows, widths.iter().map(|w| Constraint::Length(*w)))
            .column_spacing(2)
            .header(
                Row::new(headers.iter().take(ncols).map(|h| h.to_string()))
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
        let w = 92.min(area.width);
        let mut lines: Vec<Line> = HELP
            .iter()
            .map(|(k, d)| {
                Line::from(vec![
                    Span::styled(format!("{k:>15}  "), Style::default().fg(Color::Yellow)),
                    Span::raw(*d),
                ])
            })
            .collect();
        for (k, code) in &self.binds {
            lines.push(Line::from(vec![
                Span::styled(format!("{k:>15}  "), Style::default().fg(Color::Cyan)),
                Span::raw(format!("script: {code}")),
            ]));
        }
        let h = (lines.len() as u16 + 2).min(area.height);
        let rect = Rect {
            x: (area.width - w) / 2,
            y: (area.height - h) / 2,
            width: w,
            height: h,
        };
        Clear.render(rect, f.buffer_mut());
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .title(" graft · tap or press any key to close ")
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
            rect,
        );
    }
}

/// Hooks for rendering snapshots.
#[cfg(test)]
impl App {
    /// Load everything from a finite source.
    pub(crate) fn test_settle(&mut self) {
        while !self.live.eof {
            self.tick();
        }
        self.rebuild(true);
    }

    /// Put the cursor at a fraction (0..1) of the plotted x range.
    pub(crate) fn test_cursor_at(&mut self, frac: f64) {
        let (a, b) = self.bounds();
        self.cursor = Some(a + (b - a) * frac);
    }

    pub(crate) fn test_keys(&mut self, keys: &str) {
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                c => KeyCode::Char(c),
            };
            self.on_event(&TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive::Live;
    use clap::Parser;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    pub(crate) fn app(args: &[&str]) -> App {
        let cli = Cli::parse_from(args);
        let spec = crate::setup::source_spec(&cli, true).unwrap();
        let setup = crate::setup::build(cli, spec.clone(), false).unwrap();
        let mut app = App::new(setup, Live::new(spec));
        while !app.live.eof {
            app.tick();
        }
        app.rebuild(true);
        app
    }

    fn key(app: &mut App, code: KeyCode) -> Transition {
        app.on_event(&TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn mouse(app: &mut App, kind: MouseEventKind, col: u16, row: u16) -> Transition {
        app.on_event(&TermEvent::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }))
    }

    fn type_str(app: &mut App, s: &str) {
        s.chars().for_each(|c| {
            key(app, KeyCode::Char(c));
        });
    }

    fn screen_sized(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        crate::render::ansi::to_ansi(term.backend().buffer(), false)
    }

    fn screen(app: &App) -> String {
        screen_sized(app, 110, 34)
    }

    /// Screen position of a toolbar button / chip label.
    fn find(app: &App, text: &str) -> (u16, u16) {
        let s = screen(app);
        for (row, line) in s.lines().enumerate() {
            if let Some(byte) = line.find(text) {
                return (line[..byte].chars().count() as u16, row as u16);
            }
        }
        panic!("'{text}' not on screen:\n{s}");
    }

    const LOG: &str = "examples/reclaimable.log";

    #[test]
    fn draws_chart_stats_and_toolbar() {
        let mut app = app(&["graft", LOG, "--stats"]);
        assert_eq!(app.ds.series.len(), 2);
        let s = screen(&app);
        assert!(s.contains("node1 58.8 GB"), "{s}");
        assert!(s.contains("▼ -1.97 GB"), "{s}");
        assert!(s.contains(" forecast ") && s.contains(" ● node1 "), "{s}");
        for _ in 0..5 {
            key(&mut app, KeyCode::Char('t'));
            screen(&app);
        }
        assert_eq!(app.opts.kind, ChartKind::Line);
        // Phone-sized screens still render (toolbar wraps, stats columns drop).
        let small = screen_sized(&app, 44, 22);
        assert!(small.contains("node1"), "{small}");
    }

    #[test]
    fn keyboard_cursor_zoom_and_selection() {
        let mut app = app(&["graft", LOG]);
        key(&mut app, KeyCode::Home);
        assert_eq!(app.cursor, app.ds.x_range().map(|r| r.0));
        assert!(screen(&app).contains("2026-11-01 04:00  node1=59.9 GB  node2=422.6 GB"));
        key(&mut app, KeyCode::Char('v'));
        (0..6).for_each(|_| {
            key(&mut app, KeyCode::Char('.'));
        });
        key(&mut app, KeyCode::Char('v'));
        let (a, b) = app.selection.expect("selection");
        assert_eq!(b - a, 6.0 * 3600.0);
        assert!(screen(&app).contains("selection 2026-11-01 04:00 → 2026-11-01 10:00"));
        key(&mut app, KeyCode::Esc);
        assert!(app.selection.is_none());
        key(&mut app, KeyCode::Char('+'));
        assert!(app.view.is_some());
        key(&mut app, KeyCode::Char('0'));
        assert!(app.view.is_none());
        key(&mut app, KeyCode::Char('2'));
        assert!(app.hidden.contains("node2"));
        assert_eq!(key(&mut app, KeyCode::Char('q')), Transition::Quit);
    }

    #[test]
    fn mouse_tap_scrub_select_zoom_and_buttons() {
        let mut app = app(&["graft", LOG]);
        screen(&app);
        let plot = app.plots.borrow()[0];
        let mid = plot.rect.x + plot.rect.width / 2;
        let row = plot.rect.y + 2;
        // Tap → cursor and readout.
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), mid, row);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), mid, row);
        let c = app.cursor.expect("cursor");
        assert!((plot.col_of(c) as i32 - mid as i32).abs() <= 1);
        assert!(screen(&app).contains("node2 4"), "readout box shows values");
        // Drag the cursor line → scrub.
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), mid, row);
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            mid + 10,
            row,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            mid + 10,
            row,
        );
        assert!(app.cursor.unwrap() > c && app.selection.is_none());
        // Drag elsewhere → selection.
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            plot.rect.x + 2,
            row,
        );
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            plot.rect.x + 20,
            row,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            plot.rect.x + 20,
            row,
        );
        let (a, b) = app.selection.expect("selection");
        assert!(b > a && app.show_stats);
        // Scroll zooms around the pointer.
        mouse(&mut app, MouseEventKind::ScrollUp, mid, row);
        assert!(app.view.is_some());
        // Toolbar buttons and series chips are tappable.
        let (x, y) = find(&app, " reset ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.view.is_none());
        let (x, y) = find(&app, "● node2");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.hidden.contains("node2"));
        let (x, y) = find(&app, " ⋯ ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        let (x, y) = find(&app, " format ");
        assert_eq!(
            mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y),
            Transition::OpenBuilder
        );
        // Follow-pointer mode moves the cursor without clicking.
        key(&mut app, KeyCode::Char('o'));
        mouse(&mut app, MouseEventKind::Moved, plot.rect.x + 3, row);
        assert!((plot.col_of(app.cursor.unwrap()) as i32 - (plot.rect.x + 3) as i32).abs() <= 1);
    }

    #[test]
    fn forecast_targets_and_scrubbing_into_the_future() {
        let mut app = app(&[
            "graft",
            LOG,
            "--target",
            "node1=50GB,node2=200GB",
            "--forecast",
        ]);
        let s = screen(&app);
        assert!(s.contains("node1 target 50 GB"), "{s}");
        key(&mut app, KeyCode::Char('s'));
        let s = screen(&app);
        assert!(
            s.contains("50 GB in 3.7 days") && s.contains("200 GB in 27.6 days"),
            "{s}"
        );
        // Past the data, the cursor shows projected values.
        let last = app.ds.x_range().unwrap().1;
        app.cursor = Some(last + 86400.0);
        let s = screen(&app);
        assert!(s.contains("(forecast)"), "{s}");
        // Toggle off and on; set a target from the console.
        key(&mut app, KeyCode::Char('f'));
        assert!(app.opts.forecast.is_none());
        key(&mut app, KeyCode::Char('f'));
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "target(\"node2\", 300GB)");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.opts.analysis.targets.explicit("node2"), Some(300e9));
        assert!(
            app.current_cli().target.contains(&"node2=300G".to_string()),
            "{:?}",
            app.current_cli().target
        );
    }

    #[test]
    fn timeframe_picker_and_line_toggles() {
        let mut app = app(&["graft", LOG, "--forecast", "--target", "node1=50GB"]);
        // 28 h of data → presets 1h, 6h, 24h.
        assert_eq!(
            app.presets,
            [
                Window::Span(3600.0),
                Window::Span(21600.0),
                Window::Span(86400.0)
            ]
        );
        // Tap "range: all" → steps to the widest preset.
        let (x, y) = find(&app, "all time");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert_eq!(app.timeframe.last, Some(Window::Span(86400.0)));
        assert!(app.ds.series[0].points.len() < 29);
        key(&mut app, KeyCode::Char('['));
        assert_eq!(app.timeframe.last, Some(Window::Span(21600.0)));
        assert_eq!(app.ds.series[0].points.len(), 7, "02:00..08:00");
        key(&mut app, KeyCode::Char(']'));
        key(&mut app, KeyCode::Char(']'));
        assert!(app.timeframe.is_all());
        // Preset buttons under ⋯, and the console.
        let (x, y) = find(&app, " ⋯ ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        let (x, y) = find(&app, " 6h ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert_eq!(app.timeframe.last, Some(Window::Span(21600.0)));
        assert_eq!(app.current_cli().last, Some(Window::Span(21600.0)));
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "range(\"all\")");
        key(&mut app, KeyCode::Enter);
        assert!(app.timeframe.is_all());
        // Line toggles.
        assert!(screen(&app).contains("node1 target 50 GB"));
        key(&mut app, KeyCode::Char('G'));
        assert!(!screen(&app).contains("node1 target"));
        key(&mut app, KeyCode::Char('T'));
        assert!(app.opts.show_trend && app.current_cli().trend);
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
        key(&mut app, KeyCode::Char('D'));
        let msg = &app.message.as_ref().unwrap().text;
        assert!(
            msg.starts_with("node1 -1.97 GB/day, empty in 29.1 days"),
            "{msg}"
        );
        key(&mut app, KeyCode::Char('W'));
        assert_eq!(app.opts.hlines[0].value, 50e9);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "nope(");
        key(&mut app, KeyCode::Enter);
        assert!(app.message.as_ref().unwrap().error);
        key(&mut app, KeyCode::Char(':'));
        key(&mut app, KeyCode::Up);
        assert_eq!(
            app.console.as_ref().map(|c| c.value.as_str()),
            Some("nope(")
        );
        key(&mut app, KeyCode::Esc);
        assert!(app.console.is_none());
    }

    #[test]
    fn zoom_and_pan_stay_inside_the_data() {
        let mut app = app(&["graft", LOG, "--forecast", "--target", "node1=50GB"]);
        screen(&app);
        let plot = app.plots.borrow()[0];
        let (col, row) = (plot.rect.x + plot.rect.width / 2, plot.rect.y + 2);
        // A trackpad swipe: lots of scroll-downs. Zooming out stops at "everything".
        for _ in 0..200 {
            mouse(&mut app, MouseEventKind::ScrollDown, col, row);
        }
        assert_eq!(app.view, None);
        assert!(!screen(&app).contains("EB"), "no runaway forecast scale");
        // Arrows: ↑ zooms in, ← → move but never past the data (or forecast).
        let (a, b) = app.full_bounds();
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        for _ in 0..50 {
            key(&mut app, KeyCode::Left);
        }
        assert_eq!(app.view.unwrap().0, a);
        for _ in 0..50 {
            key(&mut app, KeyCode::Right);
        }
        assert!((app.view.unwrap().1 - b).abs() < 1e-6);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.view, None);
        // Panning with everything in view explains itself.
        key(&mut app, KeyCode::Left);
        assert!(app.message.as_ref().unwrap().text.contains("zoom in"));
    }

    #[test]
    fn measure_by_tapping_start_and_end() {
        let mut app = app(&["graft", LOG]);
        screen(&app);
        let plot = app.plots.borrow()[0];
        let row = plot.rect.y + 2;
        let tap = |app: &mut App, col: u16| {
            mouse(app, MouseEventKind::Down(MouseButton::Left), col, row);
            mouse(app, MouseEventKind::Up(MouseButton::Left), col, row);
        };
        let (x, y) = find(&app, " measure ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.measuring);
        tap(&mut app, plot.rect.x + 5);
        tap(&mut app, plot.rect.x + 40);
        let (a, b) = app.selection.expect("measured");
        assert!(b > a && app.show_stats && !app.measuring);
        let s = screen(&app);
        assert!(s.contains("change") && s.contains("clear measure"), "{s}");
        let (x, y) = find(&app, " clear measure ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.selection.is_none());
    }

    #[test]
    fn shift_click_measures_from_the_cursor() {
        let mut app = app(&["graft", LOG]);
        screen(&app);
        let plot = app.plots.borrow()[0];
        let row = plot.rect.y + 2;
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            plot.rect.x + 4,
            row,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            plot.rect.x + 4,
            row,
        );
        let shift_click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: plot.rect.x + 30,
            row,
            modifiers: KeyModifiers::SHIFT,
        };
        app.on_event(&TermEvent::Mouse(shift_click));
        let (a, b) = app.selection.expect("measured");
        assert!((plot.col_of(a) as i32 - (plot.rect.x + 4) as i32).abs() <= 1);
        assert!((plot.col_of(b) as i32 - (plot.rect.x + 30) as i32).abs() <= 1);
        assert!(app.show_stats);
        // Without a cursor, the first shift+click sets the start.
        let mut app = super::tests::app(&["graft", LOG]);
        screen(&app);
        app.on_event(&TermEvent::Mouse(shift_click));
        assert!(app.selection.is_none() && app.anchor.is_some());
        app.on_event(&TermEvent::Mouse(MouseEvent {
            column: plot.rect.x + 10,
            ..shift_click
        }));
        assert!(app.selection.is_some());
    }

    #[test]
    fn says_when_nothing_matches() {
        let app = app(&["graft", LOG, "-F", "nothing like this {x}"]);
        assert!(screen(&app).contains("29 lines read, none matched the format"));
    }

    #[test]
    fn alerts_show_in_status() {
        let app = app(&["graft", LOG, "--alert", "node1 < 59GB"]);
        assert_eq!(app.firing, vec!["node1 < 59GB".to_string()]);
        assert!(screen(&app).contains("ALERT node1 < 59GB"));
    }
}
