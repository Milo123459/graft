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
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, Widget};

use crate::cli::{ChartKind, Cli, TrendStyle, Window, parse_window};
use crate::data::Dataset;
use crate::interactive::{Live, Transition};
use crate::render::panes::{Dir as PDir, Pane, Panes, Split as PSplit};
use crate::render::{
    ChartOpts, ChartWidget, HLine, Horizon, Overlay, PlotArea, plot_x_bounds, value_at,
};
use crate::script::{Action, Flag, PaneAction, display};
use crate::session::Session;
use crate::setup::Setup;
use crate::stats::SeriesReport;
use crate::transform::{Timeframe, timeframe_presets};
use crate::ui::{
    Button, Hits, Item, TextInput, flow_height, flow_items, render_buttons, render_items,
};

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
    (
        "all time ▾ / R",
        "timeframe menu: last 1d, 6h, 5m… or custom",
    ),
    (
        "lines / T",
        "lines menu: live, average, median, p95, window",
    ),
    (
        "panes / w",
        "several charts: split, pick, move, resize (drag borders)",
    ),
    ("", "each pane has its own series, chart type and lines"),
    ("Tab / Alt+arrows", "pick the next pane / the pane that way"),
    ("⋯", "more: chart type, log scale, target lines, pause,"),
    (
        "",
        "format (change how lines are read), console, export, mouse",
    ),
    ("— other keys —", ""),
    ("1-9 / a", "show or hide a series / show all"),
    ("t m y", "chart type, marker, log scale"),
    ("[ ]", "narrower / wider timeframe"),
    ("G", "target lines on/off"),
    (
        "F",
        "change the format or source (builder, with suggestions)",
    ),
    (
        "e / space",
        "export to SVG / pause the chart (data keeps being read)",
    ),
    ("M", "mouse on/off (off = select text in your terminal)"),
    (":", "Rhai console, e.g. :slope(\"node1\", \"1d\")"),
    ("q / Esc", "quit (Esc first clears a measurement)"),
];

/// `:help` — the console's commands. Series names go in quotes.
const CONSOLE_HELP: &[(&str, &str)] = &[
    ("— the console —", ""),
    (
        "node2 - node1",
        "series names are variables holding their latest value",
    ),
    (
        "50GB  200ms  1d",
        "units work in numbers, e.g. last(\"node1\") < 50GB",
    ),
    (
        "",
        "functions use the visible range, or the measured stretch if there is one",
    ),
    ("— values —", ""),
    ("last(\"s\")  first(\"s\")", "the latest / first value"),
    (
        "min max mean median",
        "smallest, largest, average (also avg), middle value",
    ),
    (
        "stddev p95 sum count",
        "spread, 95th percentile, total, number of points",
    ),
    ("percentile(\"s\", 90)", "any percentile"),
    (
        "change pct_change",
        "difference from first to last (pct_change: in %)",
    ),
    (
        "values(\"s\")  xs(\"s\")",
        "all values / x positions, as arrays",
    ),
    ("series()", "the names of all series"),
    ("— trends & forecasts —", ""),
    (
        "slope(\"s\", \"1d\")",
        "average change per day (\"1h\", \"1w\"…; no 2nd arg: per second)",
    ),
    (
        "r2(\"s\")",
        "how well a straight line fits: 0 (noise) to 1 (perfect)",
    ),
    (
        "eta(\"s\", 50GB)",
        "seconds until the trend reaches 50 GB (nothing if never)",
    ),
    (
        "forecast(\"s\", 3d)",
        "the trend's value 3 days after the last point",
    ),
    ("— formatting —", ""),
    (
        "fmt(\"s\", v)",
        "v in the series' unit, e.g. fmt(\"node1\", 5e10) → 50 GB",
    ),
    ("fmt_dur(secs)", "a duration in words, e.g. 3.5 days"),
    (
        "fmt_time(t)  bytes(v)",
        "a timestamp / a byte count, readable",
    ),
    ("— every button has a command —", ""),
    (
        "timeframe(1h)",
        "only recent data: 1h, 7d, \"500\" (points) or \"all\"",
    ),
    (
        "timeframe(\"2026-11-01\", \"\")",
        "from a time on (or between two times)",
    ),
    (
        "lines(\"live,avg\")",
        "which lines to show: live, avg, median, p95, line",
    ),
    ("window(50)", "points in the rolling average (\"auto\")"),
    (
        "stats() split() log()",
        "toggle a button; or set it: stats(true), split(false)…",
    ),
    (
        "side_by_side() targets()",
        "also live() pause() follow() cursor() measure() mouse()",
    ),
    (
        "forecast(\"7d\")",
        "project trends 7 days ahead (\"auto\", \"off\")",
    ),
    (
        "target(\"s\", 50GB)",
        "a forecast target for a series (clear_target(\"s\"))",
    ),
    ("chart(\"bar\")", "line, scatter, bar, histogram, sparkline"),
    ("show(\"s\")  hide(\"s\")", "show or hide a series"),
    (
        "zoom(6h)  reset()",
        "zoom to the last 6 hours / show everything",
    ),
    (
        "zoom_in() zoom_out()",
        "pan(\"left\") pan(\"right\") — like the ◀ ▶ + − buttons",
    ),
    (
        "hline(50GB, \"label\")",
        "a reference line (clear_hlines() removes them)",
    ),
    (
        "svg(\"out.svg\")  format()  quit()",
        "export / change the format / quit",
    ),
    (
        "copy()  copy(text)",
        "copy this view's command (or any text) to your clipboard",
    ),
    (
        "save(\"disks\")",
        "save this view (source, format, panes…): graft -p disks",
    ),
    ("— panes —", ""),
    (
        "split_right() split_down()",
        "split the picked pane (close_pane() closes it)",
    ),
    (
        "pane(\"node1 kind=bar\")",
        "what the picked pane shows: series, kind=…, lines=…",
    ),
    (
        "layout(\"(1 | 2) / 3\")",
        "arrange: | side by side, / stacked, |60% sizes",
    ),
    (
        "focus_pane(2)",
        "pick a pane (or \"left\" \"right\" \"up\" \"down\")",
    ),
    (
        "move_pane(\"left\")",
        "swap with the pane that way; resize_pane(\"right\", 10)",
    ),
    ("even_panes()  single()", "equal sizes / back to one chart"),
    ("print(x)", "show a value (several prints are joined)"),
    ("— examples —", ""),
    (
        "fmt_dur(eta(\"node1\", 50GB))",
        "when will node1 reach 50 GB?",
    ),
    (
        "fmt(\"node2\", slope(\"node2\", \"1d\"))",
        "how much node2 changes per day",
    ),
    (
        "mean(\"node2\") - mean(\"node1\")",
        "the average gap between two series",
    ),
    (
        "target(\"node1\", 50GB); forecast(\"auto\")",
        "aim for 50 GB and draw the forecast",
    ),
    (
        "timeframe(6h); lines(\"live,median\"); stats(true)",
        "several at once, separated by ;",
    ),
    (
        "",
        "your own functions and key bindings: --script file.rhai (see README)",
    ),
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
    SideBySide,
    Targets,
    /// Step through timeframe presets (-1 narrower, +1 wider).
    RangeStep(i8),
    RangeMenu,
    Panes,
    Save,
    Copy,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Hit {
    Cmd(Cmd),
    Series(usize),
    /// A timeframe preset (`None` = all data).
    Range(Option<Window>),
    /// Lines menu: toggle a layer (`None` = live data), resize the window, close.
    /// (layer, right-hand chart of side by side)
    Layer(Option<TrendStyle>, bool),
    Window(bool),
    CloseMenu,
    /// Pane panel buttons.
    Pane(PaneOp),
    /// Timeframe dropdown: type a range in the console.
    CustomRange,
    MenuBackground,
}

fn arrow_dir(code: KeyCode) -> PDir {
    match code {
        KeyCode::Left | KeyCode::Char('h' | 'H') => PDir::Left,
        KeyCode::Right | KeyCode::Char('l' | 'L') => PDir::Right,
        KeyCode::Up | KeyCode::Char('k' | 'K') => PDir::Up,
        _ => PDir::Down,
    }
}

fn parse_dir(s: &str) -> Option<PDir> {
    match s.trim() {
        "left" | "l" => Some(PDir::Left),
        "right" | "r" => Some(PDir::Right),
        "up" | "u" => Some(PDir::Up),
        "down" | "d" => Some(PDir::Down),
        _ => None,
    }
}

/// Things the pane panel (and `w` keys, and the console) do to panes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PaneOp {
    Split(PSplit),
    Close,
    Focus(PDir),
    Move(PDir),
    Resize(PDir),
    Even,
    Single,
    Kind,
    Lines,
    Series(usize),
    AllSeries,
    Done,
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
    /// Dragging the border between panes: the split's path (bits, depth) and area.
    Border(u32, u8, Rect),
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
    /// `:help` (the console reference) is open.
    console_help: bool,
    help_scroll: u16,
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
    /// The "lines" menu (live / avg / median / p95 / trend line) is open.
    lines_menu: bool,
    /// The timeframe dropdown is open.
    range_menu: bool,
    /// A transition requested by the console or a key binding (quit, builder, mouse).
    pending: Option<Transition>,
    /// The pane panel (split, close, move, resize…) is open.
    pane_menu: bool,
    /// Text to put on the clipboard (written to the terminal by the event loop).
    clipboard: Option<String>,
    /// Where the chart area was last drawn (for pane taps and border drags).
    chart_rect: std::cell::Cell<Rect>,
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
            console_help: false,
            help_scroll: 0,
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
            lines_menu: false,
            range_menu: false,
            pending: None,
            pane_menu: false,
            clipboard: None,
            chart_rect: std::cell::Cell::new(Rect::default()),
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
        c.trend = self.opts.trends.clone();
        c.trend_only = !self.opts.trends.is_empty() && !self.opts.show_raw;
        c.trend_window = self.opts.trend_window;
        c.right_panel = self.opts.side_by_side.then(|| self.opts.right.clone());
        c.pane = self
            .opts
            .panes
            .iter()
            .flat_map(|p| p.panes.iter().map(Pane::to_spec))
            .collect();
        c.layout = self.opts.panes.as_ref().map(Panes::layout_expr);
        c.side_by_side = self.opts.side_by_side;
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
        // Before any data arrives every alert would just say "variable not found".
        if self.full.series.iter().all(|s| s.points.is_empty()) {
            self.firing.clear();
            return;
        }
        self.session.script.set_data(self.full.clone(), None);
        let before = std::mem::take(&mut self.firing);
        let mut newly = Vec::new();
        for a in &self.alerts {
            match self.session.script.eval_bool(a) {
                Ok(true) => {
                    if !before.contains(a) {
                        newly.push(a.clone());
                    }
                    self.firing.push(a.clone());
                }
                Ok(false) => {}
                // Shown, but not worth a notification.
                Err(e) => self.firing.push(format!("{a}: {e}")),
            }
        }
        // Notify once each time an alert starts firing.
        if !newly.is_empty() && !self.cli.notify.is_empty() {
            let msg = newly
                .iter()
                .map(|a| format!("ALERT: {a}"))
                .collect::<Vec<_>>()
                .join("\n");
            crate::notify::send_in_background(
                self.cli.notify.clone(),
                self.opts.title.clone(),
                msg,
            );
        }
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
        if self.opts.panes.is_some() {
            return self.pane_op(PaneOp::Series(idx));
        }
        if let Some(s) = self.ds.series.get(idx) {
            let name = s.name.clone();
            if !self.hidden.remove(&name) {
                self.hidden.insert(name);
            }
        }
    }

    fn export_view(&mut self, path: Option<String>) {
        // With panes, the picked pane is exported.
        let (mut ds, opts) = match self.focused_pane() {
            Some(p) => crate::render::pane_view(&self.ds, &self.opts, p),
            None => (self.ds.clone(), self.opts.clone()),
        };
        ds.series.retain(|s| !self.hidden.contains(&s.name));
        if let Some((a, b)) = self.view {
            ds.series
                .iter_mut()
                .for_each(|s| s.points = s.range(a, b).to_vec());
        }
        let name = path.unwrap_or_else(|| {
            format!("graft-{}.svg", chrono::Local::now().format("%Y%m%d-%H%M%S"))
        });
        match crate::export::export(std::path::Path::new(&name), &ds, &opts) {
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

    /// Copy `text`, or the command that reproduces this view.
    fn copy(&mut self, text: Option<String>) {
        let what = text.is_none();
        let text =
            text.unwrap_or_else(|| crate::recent::portable(&self.current_cli()).command_line());
        self.say(
            if what {
                format!("copied: {text}")
            } else {
                "copied".into()
            },
            false,
        );
        self.clipboard = Some(text);
    }

    /// Clipboard text waiting to be sent to the terminal.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    // ───────────── panes ─────────────

    fn focused_pane(&self) -> Option<&Pane> {
        self.opts.panes.as_ref().map(|p| &p.panes[p.focus])
    }

    fn global_lines(&self) -> crate::cli::Lines {
        crate::cli::Lines {
            live: self.opts.show_raw || self.opts.trends.is_empty(),
            trends: self.opts.trends.clone(),
        }
    }

    /// The lines the lines menu edits: the focused pane's, or the chart's.
    fn cur_lines(&self) -> crate::cli::Lines {
        match self.focused_pane() {
            Some(p) => p.lines.clone().unwrap_or_else(|| self.global_lines()),
            None => self.global_lines(),
        }
    }

    fn set_lines(&mut self, l: crate::cli::Lines) {
        match self.opts.panes.as_mut() {
            Some(p) => {
                let f = p.focus;
                p.panes[f].lines = Some(l);
            }
            None => {
                self.opts.show_raw = l.live || l.trends.is_empty();
                self.opts.trends = l.trends;
            }
        }
    }

    /// Panes to work on: side by side becomes two panes, a plain chart one.
    fn ensure_panes(&mut self) -> &mut Panes {
        if self.opts.panes.is_none() {
            let here = Pane {
                lines: Some(self.global_lines()),
                ..Pane::default()
            };
            self.opts.panes = Some(if self.opts.side_by_side {
                self.opts.side_by_side = false;
                let right = if self.opts.right.is_empty() {
                    crate::cli::Lines::default_right()
                } else {
                    self.opts.right.clone()
                };
                Panes::pair(
                    here,
                    Pane {
                        lines: Some(right),
                        ..Pane::default()
                    },
                    PSplit::Across,
                )
            } else {
                Panes::single(here)
            });
        }
        self.opts.panes.as_mut().expect("just made")
    }

    fn pane_op(&mut self, op: PaneOp) {
        let area = self.chart_rect.get();
        match op {
            PaneOp::Done => return self.pane_menu = false,
            PaneOp::Single => {
                // Keep the focused pane's lines and chart type for the single chart.
                if let Some(p) = self.focused_pane().cloned() {
                    if let Some(k) = p.kind {
                        self.opts.kind = k;
                    }
                    self.opts.panes = None;
                    if let Some(l) = p.lines {
                        self.set_lines(l);
                    }
                }
                self.opts.panes = None;
                self.opts.side_by_side = false;
                self.pane_menu = false;
                return self.say("one chart", false);
            }
            PaneOp::Lines => {
                self.lines_menu = true;
                return;
            }
            _ => {}
        }
        let names: Vec<String> = self.ds.series.iter().map(|s| s.name.clone()).collect();
        let kind = self.opts.kind;
        let p = self.ensure_panes();
        let f = p.focus;
        let complaint = match op {
            PaneOp::Split(sp) => {
                p.split(sp);
                None
            }
            PaneOp::Close => {
                (!p.close()).then_some("that's the last pane — \"one chart\" leaves panes")
            }
            PaneOp::Focus(d) => (!p.focus_dir(area, d)).then_some("no pane that way"),
            PaneOp::Move(d) => (!p.move_dir(area, d)).then_some("no pane that way to swap with"),
            PaneOp::Resize(d) => (!p.resize(d, 0.05)).then_some("no border to move that way"),
            PaneOp::Even => {
                p.equalize();
                None
            }
            PaneOp::Kind => {
                let k = p.panes[f].kind.unwrap_or(kind).next();
                p.panes[f].kind = Some(k);
                None
            }
            PaneOp::Series(i) => {
                let pane = &mut p.panes[f];
                match names.get(i) {
                    None => None,
                    Some(name) => {
                        if pane.series.is_empty() {
                            pane.series = names.clone();
                        }
                        if let Some(at) = pane.series.iter().position(|s| s == name) {
                            if pane.series.len() > 1 {
                                pane.series.remove(at);
                                None
                            } else {
                                Some("a pane needs at least one series")
                            }
                        } else {
                            pane.series.push(name.clone());
                            pane.series
                                .sort_by_key(|s| names.iter().position(|n| n == s));
                            if pane.series.len() == names.len() {
                                pane.series.clear();
                            }
                            None
                        }
                    }
                }
            }
            PaneOp::AllSeries => {
                p.panes[f].series.clear();
                None
            }
            PaneOp::Done | PaneOp::Single | PaneOp::Lines => None,
        };
        if let Some(c) = complaint {
            self.say(c, false);
        }
    }

    /// Keys while the pane panel is open; false = not a pane key (falls through).
    fn on_pane_key(&mut self, key: KeyEvent) -> bool {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let op = match key.code {
            KeyCode::Char('v' | '|') => PaneOp::Split(PSplit::Across),
            KeyCode::Char('s' | '_') => PaneOp::Split(PSplit::Down),
            KeyCode::Char('x') => PaneOp::Close,
            KeyCode::Char('=') => PaneOp::Even,
            KeyCode::Char('o') => PaneOp::Single,
            KeyCode::Char('t') => PaneOp::Kind,
            KeyCode::Char('T') => PaneOp::Lines,
            KeyCode::Char('a') => PaneOp::AllSeries,
            KeyCode::Char(d @ '1'..='9') => PaneOp::Series(d as usize - '1' as usize),
            KeyCode::Char('<') => PaneOp::Resize(PDir::Left),
            KeyCode::Char('>') => PaneOp::Resize(PDir::Right),
            KeyCode::Char('-') => PaneOp::Resize(PDir::Up),
            KeyCode::Char('+') => PaneOp::Resize(PDir::Down),
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if ctrl => {
                PaneOp::Resize(arrow_dir(key.code))
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if shift => {
                PaneOp::Move(arrow_dir(key.code))
            }
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Char('h' | 'j' | 'k' | 'l') => PaneOp::Focus(arrow_dir(key.code)),
            KeyCode::Char('H' | 'J' | 'K' | 'L') => PaneOp::Move(arrow_dir(key.code)),
            KeyCode::Tab => {
                if let Some(p) = self.opts.panes.as_mut() {
                    p.focus = (p.focus + 1) % p.count();
                }
                return true;
            }
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('w' | 'q') => PaneOp::Done,
            _ => return false,
        };
        self.pane_op(op);
        true
    }

    fn pane_items(&self) -> Vec<Item<Hit>> {
        let b = |label: &str, op: PaneOp| Item::Btn(Button::new(label, Hit::Pane(op)));
        let Some(p) = self.opts.panes.as_ref() else {
            return Vec::new();
        };
        let pane = &p.panes[p.focus];
        let mut v = vec![
            Item::Label(format!("pane {} of {}", p.focus + 1, p.count())),
            b("split right", PaneOp::Split(PSplit::Across)),
            b("split down", PaneOp::Split(PSplit::Down)),
            b("close", PaneOp::Close),
            b("even", PaneOp::Even),
            b("one chart", PaneOp::Single),
            b("done", PaneOp::Done),
            Item::Break,
        ];
        for (label, op) in [
            ("focus", PaneOp::Focus as fn(PDir) -> PaneOp),
            ("move", PaneOp::Move),
            ("size", PaneOp::Resize),
        ] {
            v.push(Item::Label(label.into()));
            for (arrow, d) in [
                ("◀", PDir::Left),
                ("▲", PDir::Up),
                ("▼", PDir::Down),
                ("▶", PDir::Right),
            ] {
                v.push(b(arrow, op(d)));
            }
        }
        v.push(Item::Break);
        v.push(Item::Label("chart".into()));
        v.push(b(pane.kind.unwrap_or(self.opts.kind).name(), PaneOp::Kind));
        v.push(Item::Label("lines".into()));
        v.push(b(&self.cur_lines().label(), PaneOp::Lines));
        v.push(Item::Label("series".into()));
        for (i, s) in self.ds.series.iter().enumerate() {
            let on = pane.shows(&s.name);
            v.push(Item::Btn(
                Button::new(
                    format!("{} {}", if on { "●" } else { "○" }, s.name),
                    Hit::Pane(PaneOp::Series(i)),
                )
                .active(on),
            ));
        }
        v.push(b("all", PaneOp::AllSeries));
        v
    }

    fn pane_panel_height(&self, width: u16) -> u16 {
        if !self.pane_menu || self.opts.panes.is_none() {
            return 0;
        }
        flow_items(&self.pane_items(), width.saturating_sub(2)).1 + 3
    }

    fn draw_pane_panel(&self, f: &mut Frame, area: Rect) {
        if area.height < 3 {
            return;
        }
        let block = Block::bordered()
            .title(" panes · tap a pane to pick it · drag a border to resize ")
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let items = self.pane_items();
        let rows = inner.height.saturating_sub(1);
        render_items(
            &items,
            Rect {
                height: rows,
                ..inner
            },
            f.buffer_mut(),
            &self.hits,
        );
        f.render_widget(
            Span::styled(
                "keys: v s split · x close · ←→↑↓ pick · shift move · ctrl or < > + - size · t chart · T lines · 1-9 series · Esc",
                Style::default().fg(Color::DarkGray),
            ),
            Rect { y: inner.bottom() - 1, height: 1, ..inner },
        );
    }

    fn apply_pane_action(&mut self, a: PaneAction) {
        let op = match a {
            PaneAction::Split(down) => {
                PaneOp::Split(if down { PSplit::Down } else { PSplit::Across })
            }
            PaneAction::Close => PaneOp::Close,
            PaneAction::Even => PaneOp::Even,
            PaneAction::Single => PaneOp::Single,
            PaneAction::Focus(to) => {
                if let Ok(n) = to.trim().parse::<usize>() {
                    let p = self.ensure_panes();
                    if (1..=p.count()).contains(&n) {
                        p.focus = n - 1;
                    } else {
                        let c = p.count();
                        self.say(format!("focus_pane: there are {c} panes"), true);
                    }
                    return;
                }
                match parse_dir(&to) {
                    Some(d) => PaneOp::Focus(d),
                    None => {
                        return self.say(
                            "focus_pane: \"left\", \"right\", \"up\", \"down\" or a number",
                            true,
                        );
                    }
                }
            }
            PaneAction::Move(d) => match parse_dir(&d) {
                Some(d) => PaneOp::Move(d),
                None => {
                    return self.say("move_pane: \"left\", \"right\", \"up\" or \"down\"", true);
                }
            },
            PaneAction::Resize(d, by) => {
                match parse_dir(&d) {
                    Some(d) => {
                        if !self.ensure_panes().resize(d, by) {
                            self.say("no border to move that way", false);
                        }
                    }
                    None => self.say("resize_pane: \"left\", \"right\", \"up\" or \"down\"", true),
                }
                return;
            }
            PaneAction::Layout(expr) => {
                let old = self.ensure_panes().panes.clone();
                match crate::render::panes::Panes::from_args(
                    &old.iter().map(Pane::to_spec).collect::<Vec<_>>(),
                    Some(&expr),
                ) {
                    Ok(Some(mut p)) => {
                        // Keep panes' settings as they were (specs round-trip them).
                        for (i, pane) in p.panes.iter_mut().enumerate() {
                            if let Some(o) = old.get(i) {
                                *pane = o.clone();
                            }
                        }
                        self.opts.panes = Some(p);
                    }
                    Ok(None) => {}
                    Err(e) => self.say(e, true),
                }
                return;
            }
            PaneAction::Set(spec) => {
                match Pane::parse(&spec) {
                    Ok(new) => {
                        let names: Vec<&str> =
                            self.ds.series.iter().map(|s| s.name.as_str()).collect();
                        if let Some(bad) = new.series.iter().find(|s| !names.contains(&s.as_str()))
                        {
                            return self.say(
                                format!(
                                    "pane: no series called '{bad}' (have: {})",
                                    names.join(", ")
                                ),
                                true,
                            );
                        }
                        let p = self.ensure_panes();
                        let f = p.focus;
                        p.panes[f] = new;
                    }
                    Err(e) => self.say(e, true),
                }
                return;
            }
        };
        self.pane_op(op);
    }

    /// Turn a layer on/off: `None` is the live data. At least one layer stays visible.
    fn toggle_layer(&mut self, layer: Option<TrendStyle>) {
        let mut l = self.cur_lines();
        l.toggle(layer);
        if l.is_empty() {
            match layer {
                // Live off with nothing else: show the average instead.
                None => l.trends.push(TrendStyle::Avg),
                // The last trend off: the live data comes back.
                Some(_) => l.live = true,
            }
        }
        self.set_lines(l);
    }

    /// Turn a line on/off in the right-hand chart of side by side (one always stays).
    fn toggle_right(&mut self, layer: Option<TrendStyle>) {
        self.opts.right.toggle(layer);
        if self.opts.right.is_empty() {
            self.opts.right.toggle(layer);
            self.say("each chart needs at least one line", true);
        }
    }

    /// Side by side on: trends move to the right-hand chart, live data stays left.
    /// Off: the right-hand trends come back over the live data.
    fn toggle_side_by_side(&mut self) {
        // From panes: back to a plain side by side.
        if self.opts.panes.take().is_some() {
            self.pane_menu = false;
            self.opts.side_by_side = false;
        }
        self.opts.side_by_side = !self.opts.side_by_side;
        if self.opts.side_by_side {
            if !self.opts.trends.is_empty() {
                self.opts.right = crate::cli::Lines {
                    live: false,
                    trends: std::mem::take(&mut self.opts.trends),
                };
                self.opts.show_raw = true;
            }
        } else if self.opts.trends.is_empty() {
            self.opts.trends = self.opts.right.trends.clone();
        }
    }

    /// Grow/shrink the rolling window (average, median, p95).
    fn change_window(&mut self, bigger: bool) {
        let n = self
            .ds
            .series
            .iter()
            .map(|s| s.points.len())
            .max()
            .unwrap_or(10);
        let w = self.opts.window_for(n) as f64;
        let new = if bigger {
            (w * 1.5).ceil()
        } else {
            (w / 1.5).floor()
        };
        self.opts.trend_window = Some((new as usize).clamp(2, n.max(2)));
    }

    /// Data on a time axis (log timestamps or arrival times) can be cut to a timeframe.
    fn has_timeframes(&self) -> bool {
        !self.presets.is_empty() || !self.timeframe.is_all()
    }

    /// Entries of the timeframe dropdown: all, then presets widest first.
    fn range_options(&self) -> Vec<Option<Window>> {
        let mut v = vec![None];
        v.extend(self.presets.iter().rev().copied().map(Some));
        v
    }

    fn current_preset(&self) -> Option<Option<Window>> {
        if self.timeframe.is_all() {
            return Some(None);
        }
        (self.timeframe.since.is_none() && self.timeframe.until.is_none())
            .then_some(self.timeframe.last.map(Some))
            .flatten()
    }

    fn on_range_menu_key(&mut self, key: KeyEvent) {
        let opts = self.range_options();
        let cur = self
            .current_preset()
            .and_then(|c| opts.iter().position(|o| *o == c));
        match key.code {
            KeyCode::Down | KeyCode::Char('j' | ']') => {
                let i = cur.map_or(0, |i| (i + 1).min(opts.len() - 1));
                self.set_timeframe(opts[i].map_or_else(Timeframe::default, Timeframe::last));
            }
            KeyCode::Up | KeyCode::Char('k' | '[') => {
                let i = cur.map_or(0, |i| i.saturating_sub(1));
                self.set_timeframe(opts[i].map_or_else(Timeframe::default, Timeframe::last));
            }
            KeyCode::Char('c' | ':') => {
                self.range_menu = false;
                self.console = Some(TextInput::new("range(\""));
            }
            _ => self.range_menu = false,
        }
    }

    /// A dropdown above the timeframe button (or centred when the toolbar is hidden).
    fn draw_range_menu(&self, f: &mut Frame) {
        let area = f.area();
        let opts = self.range_options();
        let current = self.current_preset();
        let mut items: Vec<(String, Hit, bool)> = opts
            .iter()
            .map(|o| {
                let label = match o {
                    None => "all time".to_string(),
                    Some(Window::Span(d)) => {
                        format!("last {}", crate::time::fmt_duration_compact(*d))
                    }
                    Some(Window::Points(n)) => format!("last {n} points"),
                };
                (label, Hit::Range(*o), current == Some(*o))
            })
            .collect();
        let custom_on = current.is_none();
        items.push((
            if custom_on {
                format!("{} (edit…)", self.timeframe.label(&self.ds))
            } else {
                "custom…".into()
            },
            Hit::CustomRange,
            custom_on,
        ));
        let w = (items.iter().map(|i| i.0.chars().count()).max().unwrap_or(8) as u16 + 6)
            .min(area.width);
        let h = (items.len() as u16 + 2).min(area.height);
        let anchor = self.hits.find(|h| matches!(h, Hit::Cmd(Cmd::RangeMenu)));
        let (x, y) = match anchor {
            Some(r) => (
                r.x.min(area.width.saturating_sub(w)),
                if r.y >= h {
                    r.y - h
                } else {
                    (r.y + 1).min(area.height.saturating_sub(h))
                },
            ),
            None => ((area.width - w) / 2, area.height.saturating_sub(h) / 2),
        };
        let rect = Rect {
            x,
            y,
            width: w,
            height: h,
        };
        Clear.render(rect, f.buffer_mut());
        let block = Block::bordered()
            .title(" timeframe ")
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.hits.push(rect, Hit::MenuBackground);
        for (row, (label, hit, on)) in items.into_iter().enumerate() {
            let y = inner.y + row as u16;
            if y >= inner.bottom() {
                break;
            }
            let r = Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            };
            let style = if on {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let mark = if on { "●" } else { " " };
            f.render_widget(
                Span::styled(
                    format!(
                        "{mark} {label:<width$}",
                        width = r.width.saturating_sub(2) as usize
                    ),
                    style,
                ),
                r,
            );
            self.hits.push(r, hit);
        }
    }

    fn lines_label(&self) -> String {
        if self.opts.panes.is_some() {
            return format!("lines: {}", self.cur_lines().label());
        }
        let mut parts: Vec<&str> = Vec::new();
        if self.opts.show_raw || self.opts.trends.is_empty() {
            parts.push("live");
        }
        parts.extend(self.opts.trends.iter().map(|t| t.label()));
        if self.opts.side_by_side {
            return format!("lines: {} / {}", parts.join(" + "), self.opts.right.label());
        }
        format!("lines: {}", parts.join(" + "))
    }

    /// How much time `points` samples cover, on a time axis.
    fn window_span(&self, points: usize) -> Option<f64> {
        if self.ds.x_kind != crate::data::XKind::Time {
            return None;
        }
        let s = self.ds.series.iter().max_by_key(|s| s.points.len())?;
        let (a, b) = (s.points.first()?.0, s.points.last()?.0);
        (s.points.len() > 1).then(|| (b - a) / (s.points.len() - 1) as f64 * points as f64)
    }

    fn on_lines_menu_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('l') => self.toggle_layer(None),
            KeyCode::Char('a') => self.toggle_layer(Some(TrendStyle::Avg)),
            KeyCode::Char('m') => self.toggle_layer(Some(TrendStyle::Median)),
            KeyCode::Char('p') => self.toggle_layer(Some(TrendStyle::P95)),
            KeyCode::Char('+' | '=') => self.change_window(true),
            KeyCode::Char('-' | '_') => self.change_window(false),
            KeyCode::Char('L') => self.toggle_right(None),
            KeyCode::Char('A') => self.toggle_right(Some(TrendStyle::Avg)),
            KeyCode::Char('M') => self.toggle_right(Some(TrendStyle::Median)),
            KeyCode::Char('P') => self.toggle_right(Some(TrendStyle::P95)),
            KeyCode::Char('D') => self.toggle_side_by_side(),
            _ => self.lines_menu = false,
        }
    }

    fn draw_lines_menu(&self, f: &mut Frame) {
        let area = f.area();
        let w = if self.opts.side_by_side { 72 } else { 64 }.min(area.width);
        let n = self
            .ds
            .series
            .iter()
            .map(|s| s.points.len())
            .max()
            .unwrap_or(0);
        // The straight fit isn't offered here: it's what `forecast` draws.
        let mut rows: Vec<(Option<TrendStyle>, &str, &str)> = vec![
            (None, "live", "the data as it comes in"),
            (
                Some(TrendStyle::Avg),
                "average",
                "moving average (spikes pull it up)",
            ),
            (
                Some(TrendStyle::Median),
                "median",
                "the typical value, ignores spikes",
            ),
            (
                Some(TrendStyle::P95),
                "p95",
                "95% of values are below this line",
            ),
        ];
        let cur = self.cur_lines();
        if cur.trends.contains(&TrendStyle::Fit) {
            rows.push((
                Some(TrendStyle::Fit),
                "trend line",
                "straight fit (as used by forecast)",
            ));
        }
        let h = (rows.len() as u16 + 7 + u16::from(self.opts.side_by_side)).min(area.height);
        let rect = Rect {
            x: (area.width - w) / 2,
            y: (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        Clear.render(rect, f.buffer_mut());
        let block = Block::bordered()
            .title(match &self.opts.panes {
                Some(p) if p.count() > 1 => {
                    format!(" lines in pane {} · tap to turn on/off ", p.focus + 1)
                }
                _ => " lines to show · tap to turn on/off ".to_string(),
            })
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.hits.push(rect, Hit::MenuBackground);
        let mut y = inner.y;
        let sides = self.opts.side_by_side;
        if sides {
            f.render_widget(
                Span::styled(
                    "left     right    (side by side: pick lines for each chart)",
                    Style::default().fg(Color::Gray),
                ),
                Rect {
                    x: inner.x + 1,
                    y,
                    width: inner.width.saturating_sub(1),
                    height: 1,
                },
            );
            y += 1;
        }
        let toggle = |buf: &mut ratatui::buffer::Buffer,
                      x: u16,
                      y: u16,
                      width: u16,
                      label: String,
                      on: bool,
                      hit: Hit| {
            let style = if on {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default().fg(Color::White).bg(Color::DarkGray)
            };
            let width = width.min(inner.right().saturating_sub(x));
            let r = Rect {
                x,
                y,
                width,
                height: 1,
            };
            buf.set_stringn(
                x,
                y,
                format!(" {label:<w$}", w = width.saturating_sub(1) as usize),
                width as usize,
                style,
            );
            self.hits.push(r, hit);
        };
        let check = |on: bool| if on { "☑" } else { "☐" };
        for (layer, name, desc) in rows {
            if y >= inner.bottom() {
                break;
            }
            let left_on = cur.has(layer);
            let x_desc = if sides {
                let right_on = self.opts.right.has(layer);
                toggle(
                    f.buffer_mut(),
                    inner.x,
                    y,
                    8,
                    check(left_on).to_string(),
                    left_on,
                    Hit::Layer(layer, false),
                );
                toggle(
                    f.buffer_mut(),
                    inner.x + 9,
                    y,
                    8,
                    check(right_on).to_string(),
                    right_on,
                    Hit::Layer(layer, true),
                );
                let x = inner.x + 18;
                f.buffer_mut().set_stringn(
                    x,
                    y,
                    name,
                    inner.right().saturating_sub(x) as usize,
                    Style::default().add_modifier(Modifier::BOLD),
                );
                x + 11
            } else {
                toggle(
                    f.buffer_mut(),
                    inner.x,
                    y,
                    14,
                    format!("{} {name}", check(left_on)),
                    left_on,
                    Hit::Layer(layer, false),
                );
                inner.x + 15
            };
            if x_desc < inner.right() {
                f.buffer_mut().set_stringn(
                    x_desc,
                    y,
                    desc,
                    (inner.right() - x_desc) as usize,
                    Style::default().fg(Color::Gray),
                );
            }
            y += 1;
        }
        y += 1;
        if y < inner.bottom() {
            let pts = self.opts.window_for(n.max(1));
            let span = self
                .window_span(pts)
                .map(|d| format!(" (≈{})", crate::time::fmt_duration_compact(d)))
                .unwrap_or_default();
            let text = format!("window: {pts} points{span}  ");
            f.render_widget(
                Span::raw(text.clone()),
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
            );
            let x = inner.x + text.chars().count() as u16;
            let buttons = vec![
                Button::new("−", Hit::Window(false)),
                Button::new("+", Hit::Window(true)),
            ];
            render_buttons(
                &buttons,
                Rect {
                    x,
                    y,
                    width: inner.right().saturating_sub(x),
                    height: 1,
                },
                f.buffer_mut(),
                &self.hits,
            );
            y += 2;
        }
        if y < inner.bottom() {
            let buttons = vec![
                Button::new("side by side", Hit::Cmd(Cmd::SideBySide))
                    .active(self.opts.side_by_side),
                Button::new("done", Hit::CloseMenu),
            ];
            render_buttons(
                &buttons,
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                f.buffer_mut(),
                &self.hits,
            );
        }
        if inner.bottom() > inner.y {
            f.render_widget(
                Span::styled(
                    if self.opts.side_by_side {
                        "keys: l a m p left · L A M P right · + − window · Esc"
                    } else {
                        "keys: l a m p toggle · + − window · Esc closes"
                    },
                    Style::default().fg(Color::DarkGray),
                ),
                Rect {
                    x: inner.x,
                    y: inner.bottom() - 1,
                    width: inner.width,
                    height: 1,
                },
            );
        }
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
            Cmd::Kind if self.opts.panes.is_some() => self.pane_op(PaneOp::Kind),
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
            Cmd::Export => self.export_view(None),
            Cmd::Help => {
                self.help = !self.help;
                self.help_scroll = 0;
            }
            Cmd::Quit => return Transition::Quit,
            Cmd::More => self.toolbar_more = !self.toolbar_more,
            Cmd::Trend => {
                self.lines_menu = !self.lines_menu;
                self.range_menu = false;
            }
            Cmd::SideBySide => self.toggle_side_by_side(),
            Cmd::Targets => self.opts.show_targets = !self.opts.show_targets,
            Cmd::RangeStep(dir) => self.step_timeframe(dir),
            Cmd::Copy => self.copy(None),
            Cmd::Save => {
                self.console = Some(TextInput::new("save(\""));
                self.say(
                    "name this view, e.g. save(\"disks\") — then graft -p disks opens it",
                    false,
                );
            }
            Cmd::Panes => {
                self.pane_menu = !self.pane_menu;
                if self.pane_menu {
                    self.ensure_panes();
                }
            }
            Cmd::RangeMenu => {
                self.range_menu = !self.range_menu;
                self.lines_menu = false;
            }
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
        if matches!(code.trim(), "help" | "help()" | "?" | "commands") {
            self.console_help = true;
            self.help_scroll = 0;
            return;
        }
        let script = self.session.script.clone();
        script.set_data(self.full.clone(), self.focus());
        let result = script.eval(code);
        let printed = script.take_output();
        self.message = None;
        for action in script.take_actions() {
            self.apply(action);
        }
        // A command that complained (e.g. an unknown line name) keeps its message.
        if self.message.as_ref().is_some_and(|m| m.error) {
            return;
        }
        // …and so does one that explained itself (e.g. "timeframe: 6h — …").
        if self.message.is_some() && printed.is_empty() && matches!(&result, Ok(v) if v.is_unit()) {
            return;
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
                Ok(kind) => match self.opts.panes.as_mut() {
                    Some(p) => {
                        let f = p.focus;
                        p.panes[f].kind = Some(kind);
                    }
                    None => self.opts.kind = kind,
                },
                Err(_) => self.say(format!("unknown chart type '{k}'"), true),
            },
            Action::Target(name, value) => self.opts.analysis.targets.set(&name, value),
            Action::Forecast(h) => match h.map(|h| Horizon::parse(&h)).transpose() {
                Ok(h) => self.opts.forecast = h,
                Err(e) => self.say(format!("forecast: {e}"), true),
            },
            Action::Trend(t) => match t {
                None => {
                    self.opts.trends.clear();
                    self.opts.show_raw = true;
                }
                Some(list) => match list.split(',').map(|x| TrendStyle::from_str(x.trim(), true)).collect::<Result<Vec<_>, _>>() {
                    Ok(v) => self.opts.trends = v,
                    Err(_) => self.say(format!("trend: use avg, median, p95 or line (comma separated), or \"off\" — not \"{list}\""), true),
                },
            },
            Action::Lines(left, right) => {
                let parse = |l: &str| crate::cli::Lines::parse(l).map_err(|e| format!("lines: {e}"));
                match (parse(&left), right.as_deref().map(parse).transpose()) {
                    (Ok(l), Ok(r)) => {
                        self.set_lines(l);
                        if let Some(r) = r {
                            self.opts.right = r;
                            self.opts.side_by_side = true;
                        }
                    }
                    (Err(e), _) | (_, Err(e)) => self.say(e, true),
                }
            }
            Action::TrendWindow(n) => self.opts.trend_window = n,
            Action::RangeNumber(v) => {
                let w = if self.full.x_kind == crate::data::XKind::Time {
                    Window::Span(v.abs())
                } else {
                    Window::Points(v.abs().round().max(1.0) as usize)
                };
                self.set_timeframe(Timeframe::last(w));
            }
            Action::Between(a, b) => {
                let mut cli = self.current_cli();
                cli.last = None;
                cli.since = Some(a);
                cli.until = b;
                match crate::setup::timeframe(&cli) {
                    Ok(tf) => self.set_timeframe(tf),
                    Err(e) => self.say(format!("timeframe: {e:#}"), true),
                }
            }
            Action::Flag(flag, on) => self.set_flag(flag, on),
            Action::Pan(f) => self.pan(f),
            Action::ZoomStep(zoom_in) => self.zoom_at(if zoom_in { 0.5 } else { 2.0 }, None),
            Action::Svg(path) => self.export_view(path),
            Action::Pane(a) => self.apply_pane_action(a),
            Action::Copy(text) => self.copy(text),
            Action::SaveProfile(name) => {
                let name = name.trim().to_string();
                if name.is_empty() || name.contains(char::is_whitespace) {
                    return self.say("save: give a one-word name, e.g. save(\"disks\")", true);
                }
                let args: Vec<std::ffi::OsString> = std::iter::once("graft".to_string())
                    .chain(crate::recent::portable(&self.current_cli()).to_args())
                    .map(Into::into)
                    .collect();
                match crate::profile::save(&name, &args) {
                    Ok(path) => self.say(
                        format!("saved '{name}' to {} — open it again with: graft -p {name}", path.display()),
                        false,
                    ),
                    Err(e) => self.say(format!("save: {e:#}"), true),
                }
            }
            Action::Help => {
                self.help = true;
                self.help_scroll = 0;
            }
            Action::Quit => self.pending = Some(Transition::Quit),
            Action::Format => self.pending = Some(Transition::OpenBuilder),
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
        let t = match ev {
            TermEvent::Key(k) if k.kind == KeyEventKind::Press => self.on_key(*k),
            TermEvent::Mouse(m) => self.on_mouse(*m),
            _ => Transition::None,
        };
        match (t, self.pending.take()) {
            (Transition::None, Some(p)) => p,
            (t, _) => t,
        }
    }

    /// Whether an on/off button is on.
    fn flag(&self, flag: Flag) -> bool {
        match flag {
            Flag::Live => self.opts.show_raw || self.opts.trends.is_empty(),
            Flag::SideBySide => self.opts.side_by_side,
            Flag::Stats => self.show_stats,
            Flag::Split => self.opts.split,
            Flag::Log => self.opts.log,
            Flag::Targets => self.opts.show_targets,
            Flag::Pause => self.paused,
            Flag::Follow => self.follow_pointer,
            Flag::Cursor => self.cursor.is_some(),
            Flag::Mouse => self.mouse,
            Flag::Measure => self.measuring || self.anchor.is_some() || self.selection.is_some(),
        }
    }

    /// Set (or toggle, with `None`) an on/off button, as if it were tapped.
    fn set_flag(&mut self, flag: Flag, on: Option<bool>) {
        if on == Some(self.flag(flag)) {
            return;
        }
        let cmd = match flag {
            Flag::Live => return self.toggle_layer(None),
            Flag::SideBySide => Cmd::SideBySide,
            Flag::Stats => Cmd::Stats,
            Flag::Split => Cmd::Split,
            Flag::Log => Cmd::Log,
            Flag::Targets => Cmd::Targets,
            Flag::Pause => Cmd::Pause,
            Flag::Follow => Cmd::Follow,
            Flag::Cursor => Cmd::Cursor,
            Flag::Mouse => Cmd::Mouse,
            Flag::Measure => Cmd::Select,
        };
        let t = self.run_cmd(cmd);
        if t != Transition::None {
            self.pending = Some(t);
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
        if self.lines_menu {
            self.on_lines_menu_key(key);
            return Transition::None;
        }
        if self.range_menu {
            self.on_range_menu_key(key);
            return Transition::None;
        }
        if self.pane_menu && self.on_pane_key(key) {
            return Transition::None;
        }
        if self.help || self.console_help {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => self.help_scroll += 1,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::PageDown | KeyCode::Char(' ') => self.help_scroll += 10,
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
                _ => {
                    self.help = false;
                    self.console_help = false;
                }
            }
            return Transition::None;
        }
        if let KeyCode::Char(c) = key.code
            && let Some(code) = self.binds.get(&c).cloned()
        {
            self.eval(&code);
            return Transition::None;
        }
        // Alt+arrows pick a pane (zellij-style).
        if matches!(
            key.code,
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
        ) && key.modifiers.contains(KeyModifiers::ALT)
            && self.opts.panes.is_some()
        {
            self.pane_op(PaneOp::Focus(arrow_dir(key.code)));
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
                if self.opts.panes.is_some() {
                    self.pane_op(PaneOp::AllSeries);
                } else {
                    self.hidden.clear();
                }
                return Transition::None;
            }
            KeyCode::Char('w') => {
                self.pane_menu = !self.pane_menu;
                if self.pane_menu {
                    self.ensure_panes();
                }
                return Transition::None;
            }
            KeyCode::Tab | KeyCode::BackTab if self.opts.panes.is_some() => {
                if let Some(p) = self.opts.panes.as_mut() {
                    let n = p.count();
                    p.focus = if key.code == KeyCode::Tab {
                        (p.focus + 1) % n
                    } else {
                        (p.focus + n - 1) % n
                    };
                }
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
            KeyCode::Char('D') => Cmd::SideBySide,
            KeyCode::Char('G') => Cmd::Targets,
            KeyCode::Char('R') => Cmd::RangeMenu,
            KeyCode::Char('C') => Cmd::Copy,
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
        if self.help || self.console_help {
            match m.kind {
                MouseEventKind::Down(_) => {
                    self.help = false;
                    self.console_help = false;
                }
                MouseEventKind::ScrollDown => self.help_scroll += 2,
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(2),
                _ => {}
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
                            self.range_menu = false;
                            Transition::None
                        }
                        Hit::CustomRange => {
                            self.range_menu = false;
                            self.console = Some(TextInput::new("range(\""));
                            self.say("e.g. range(\"2h\"), range(\"2026-11-01\", \"2026-11-02 12:00\") or range(\"all\")", false);
                            Transition::None
                        }
                        Hit::Series(i) => {
                            self.toggle_series(i);
                            Transition::None
                        }
                        Hit::Layer(l, right) => {
                            if right {
                                self.toggle_right(l);
                            } else {
                                self.toggle_layer(l);
                            }
                            Transition::None
                        }
                        Hit::Window(bigger) => {
                            self.change_window(bigger);
                            Transition::None
                        }
                        Hit::CloseMenu => {
                            self.lines_menu = false;
                            Transition::None
                        }
                        Hit::MenuBackground => Transition::None,
                        Hit::Pane(op) => {
                            self.pane_op(op);
                            Transition::None
                        }
                    };
                }
                if self.lines_menu || self.range_menu {
                    // A tap outside a menu closes it.
                    self.lines_menu = false;
                    self.range_menu = false;
                    return Transition::None;
                }
                // Panes: grab a border to resize, or tap a pane to focus it.
                let area = self.chart_rect.get();
                if let Some(p) = self.opts.panes.as_mut() {
                    if p.count() > 1
                        && let Some((path, a)) = p.border_at(area, col, row)
                    {
                        let bits = path
                            .iter()
                            .enumerate()
                            .fold(0u32, |b, (i, &x)| b | (u32::from(x) << i));
                        self.drag = Some(Drag::Border(bits, path.len() as u8, a));
                        return Transition::None;
                    }
                    if let Some((id, _)) = p
                        .rects(area)
                        .into_iter()
                        .find(|(_, r)| r.contains((col, row).into()))
                    {
                        p.focus = id;
                    }
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
                if let Some(Drag::Border(bits, len, a)) = self.drag {
                    let path: Vec<bool> = (0..len).map(|i| bits >> i & 1 == 1).collect();
                    if let Some(p) = self.opts.panes.as_mut() {
                        p.drag_border(&path, a, col, row);
                    }
                    return Transition::None;
                }
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
            b(&self.lines_label(), Cmd::Trend)
                .active(self.lines_menu || !self.opts.trends.is_empty()),
            b("side by side", Cmd::SideBySide).active(self.opts.side_by_side),
            b(
                &match &self.opts.panes {
                    Some(p) if p.count() > 1 => format!("panes ({})", p.count()),
                    _ => "panes".to_string(),
                },
                Cmd::Panes,
            )
            .active(self.pane_menu || self.opts.panes.is_some()),
            b("forecast", Cmd::Forecast).active(self.opts.forecast.is_some()),
            b("split", Cmd::Split).active(self.opts.split),
        ];
        // The timeframe dropdown.
        if self.has_timeframes() {
            let label = if self.timeframe.is_all() {
                "all time".to_string()
            } else if self.timeframe.last.is_some() {
                format!("last {}", self.timeframe.label(&self.ds))
            } else {
                self.timeframe.label(&self.ds)
            };
            v.push(
                b(&format!("{label} ▾"), Cmd::RangeMenu)
                    .active(self.range_menu || !self.timeframe.is_all()),
            );
        }
        v.push(
            b(if self.toolbar_more { "less" } else { "⋯" }, Cmd::More).active(self.toolbar_more),
        );
        if self.toolbar_more {
            v.extend([
                b("targets", Cmd::Targets).active(self.opts.show_targets),
                b(self.opts.kind.name(), Cmd::Kind),
                b("log", Cmd::Log).active(self.opts.log),
                b("follow", Cmd::Follow).active(self.follow_pointer),
                b(if self.paused { "resume" } else { "pause" }, Cmd::Pause).active(self.paused),
                b("format", Cmd::Format),
                b("save", Cmd::Save),
                b("copy cmd", Cmd::Copy),
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
                let shown = !self.hidden.contains(&s.name)
                    && self.focused_pane().is_none_or(|p| p.shows(&s.name));
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
        let pane_h = self.pane_panel_height(area.width);
        let [
            chart,
            pane_area,
            stats,
            chips_area,
            tool_area,
            status,
            console,
        ] = Layout::vertical([
            Constraint::Min(6),
            Constraint::Length(pane_h),
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
            pane_focus: self.opts.panes.as_ref().map(|p| p.focus),
        };
        self.chart_rect.set(chart);
        f.render_widget(ChartWidget::new(&self.ds, &self.opts, overlay), chart);
        if pane_h > 0 {
            self.draw_pane_panel(f, pane_area);
        }
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
            input.render(
                field,
                f.buffer_mut(),
                true,
                "type help to see every command · Enter runs · Esc closes",
            );
        }
        if self.lines_menu {
            self.draw_lines_menu(f);
        }
        if self.range_menu {
            self.draw_range_menu(f);
        }
        if self.help {
            let mut entries: Vec<(String, String, Color)> = HELP
                .iter()
                .map(|(k, d)| (k.to_string(), d.to_string(), Color::Yellow))
                .collect();
            entries.extend(
                self.binds
                    .iter()
                    .map(|(k, code)| (k.to_string(), format!("script: {code}"), Color::Cyan)),
            );
            entries.push((
                String::new(),
                "type : then help for the console's commands".into(),
                Color::Yellow,
            ));
            self.draw_overlay(
                f,
                " graft keys · ↑↓ scroll · any other key or tap closes ",
                &entries,
            );
        }
        if self.console_help {
            let entries: Vec<(String, String, Color)> = CONSOLE_HELP
                .iter()
                .map(|(k, d)| (k.to_string(), d.to_string(), Color::Cyan))
                .collect();
            self.draw_overlay(
                f,
                " console commands (Rhai) · ↑↓ scroll · any other key or tap closes ",
                &entries,
            );
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

    /// A centred, scrollable two-column reference (keys / commands and what they do).
    /// Long commands get a line of their own; descriptions wrap with an indent.
    fn draw_overlay(&self, f: &mut Frame, title: &str, entries: &[(String, String, Color)]) {
        let area = f.area();
        let w = 100.min(area.width);
        let inner_w = w.saturating_sub(2) as usize;
        let key_w = entries
            .iter()
            .filter(|e| !e.1.is_empty())
            .map(|e| e.0.chars().count())
            .filter(|n| *n <= 26)
            .max()
            .unwrap_or(10)
            .min(inner_w / 2);
        let desc_w = inner_w.saturating_sub(key_w + 2).max(10);
        let wrap = |text: &str| -> Vec<String> {
            let mut out = vec![String::new()];
            for word in text.split(' ') {
                let cur = out.last_mut().expect("non-empty");
                if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > desc_w {
                    out.push(word.to_string());
                } else {
                    if !cur.is_empty() {
                        cur.push(' ');
                    }
                    cur.push_str(word);
                }
            }
            out
        };
        let mut lines: Vec<Line> = Vec::new();
        for (k, d, color) in entries {
            if d.is_empty() {
                lines.push(Line::styled(
                    k.clone(),
                    Style::default().fg(*color).add_modifier(Modifier::BOLD),
                ));
                continue;
            }
            let key_style = Style::default().fg(*color);
            let pad = " ".repeat(key_w + 2);
            let mut desc = wrap(d).into_iter();
            if k.chars().count() > key_w {
                lines.push(Line::styled(format!("  {k}"), key_style));
            } else {
                let first = desc.next().unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(format!("{k:>key_w$}  "), key_style),
                    Span::raw(first),
                ]));
            }
            lines.extend(desc.map(|rest| Line::raw(format!("{pad}{rest}"))));
        }
        let h = (lines.len() as u16 + 2).min(area.height);
        let rect = Rect {
            x: (area.width - w) / 2,
            y: (area.height - h) / 2,
            width: w,
            height: h,
        };
        let max_scroll = (lines.len() as u16 + 2).saturating_sub(h);
        Clear.render(rect, f.buffer_mut());
        f.render_widget(
            Paragraph::new(lines)
                .scroll((self.help_scroll.min(max_scroll), 0))
                .block(
                    Block::bordered()
                        .title(title.to_string())
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

    fn app_from(args: &[String]) -> App {
        let mut v = vec!["graft"];
        v.extend(args.iter().map(String::as_str));
        app(&v)
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
        for _ in 0..6 {
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
        // 28 h of hourly data → presets 6h, 24h (1h would hold a single point).
        assert_eq!(app.presets, [Window::Span(21600.0), Window::Span(86400.0)]);
        // Tap "all time ▾" → a dropdown; tap "last 1d" in it.
        let (x, y) = find(&app, "all time ▾");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.range_menu);
        let s = screen(&app);
        assert!(
            s.contains("timeframe") && s.contains("last 6h") && s.contains("custom…"),
            "{s}"
        );
        let (x, y) = find(&app, "last 1d");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(!app.range_menu);
        assert_eq!(app.timeframe.last, Some(Window::Span(86400.0)));
        assert!(app.ds.series[0].points.len() < 29);
        key(&mut app, KeyCode::Char('['));
        assert_eq!(app.timeframe.last, Some(Window::Span(21600.0)));
        assert_eq!(app.ds.series[0].points.len(), 7, "02:00..08:00");
        key(&mut app, KeyCode::Char(']'));
        key(&mut app, KeyCode::Char(']'));
        assert!(app.timeframe.is_all());
        // Keyboard: R opens the dropdown, ↓ moves to narrower ranges, Esc closes.
        key(&mut app, KeyCode::Char('R'));
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.timeframe.last, Some(Window::Span(21600.0)));
        key(&mut app, KeyCode::Esc);
        assert!(!app.range_menu);
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
        key(&mut app, KeyCode::Char('a'));
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.current_cli().trend, [TrendStyle::Avg]);
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
    fn zoomed_between_two_samples_the_y_axis_fits_the_line() {
        let mut app = app(&["graft", LOG, "-s", "node1"]);
        let (a, _) = app.ds.x_range().unwrap();
        // Hourly samples; look at 10 minutes in the middle of the first hour.
        app.set_view(a + 1200.0, a + 1800.0);
        let s = screen(&app);
        // A flat line: its value, a little above and below — not 0 to 1.
        assert!(s.contains("60 GB│") && s.contains("59 GB│"), "{s}");
        assert!(!s.contains(" 0 B") && !s.contains("│1 B"), "{s}");
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
    fn console_help_and_trend_views() {
        let mut app = app(&["graft", LOG]);
        key(&mut app, KeyCode::Char(':'));
        assert!(screen(&app).contains("type help to see every command"));
        type_str(&mut app, "help");
        key(&mut app, KeyCode::Enter);
        let s = screen(&app);
        assert!(
            s.contains("console commands") && s.contains("slope(\"s\", \"1d\")"),
            "{s}"
        );
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Char('x'));
        assert!(!app.console_help);
        // The lines menu: tick any mix of layers.
        let (x, y) = find(&app, "lines: live");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.lines_menu);
        let (x, y) = find(&app, "☐ average");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        let (x, y) = find(&app, "☐ median");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert_eq!(app.opts.trends, [TrendStyle::Avg, TrendStyle::Median]);
        key(&mut app, KeyCode::Char('l'));
        assert!(!app.opts.show_raw);
        let w = app.opts.window_for(29);
        key(&mut app, KeyCode::Char('+'));
        assert!(app.opts.window_for(29) > w);
        let (x, y) = find(&app, " done ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(!app.lines_menu);
        let s = screen(&app);
        assert!(
            s.contains("lines: avg + median") && s.contains("node1 avg"),
            "{s}"
        );
        let cli = app.current_cli();
        assert!(cli.trend_only && cli.trend == [TrendStyle::Avg, TrendStyle::Median]);
        // Turning every trend off brings the live data back.
        key(&mut app, KeyCode::Char('T'));
        key(&mut app, KeyCode::Char('a'));
        key(&mut app, KeyCode::Char('m'));
        assert!(app.opts.trends.is_empty() && app.opts.show_raw);
        key(&mut app, KeyCode::Esc);
        // Console control, several layers, side by side.
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "trend(\"avg,p95\"); side_by_side(true)");
        key(&mut app, KeyCode::Enter);
        // Side by side: the trends move to the right-hand chart, live stays left.
        assert!(app.opts.trends.is_empty() && app.opts.show_raw);
        assert_eq!(app.opts.right.trends, [TrendStyle::Avg, TrendStyle::P95]);
        let s = screen(&app);
        assert!(s.contains("· live ") && s.contains("· avg + p95"), "{s}");
        let cli = app.current_cli();
        assert!(cli.side_by_side && cli.trend.is_empty());
        assert_eq!(cli.right_panel.unwrap().to_arg(), "avg,p95");
        // Pick lines per side in the menu: median on the left, live on the right.
        key(&mut app, KeyCode::Char('T'));
        let s = screen(&app);
        assert!(s.contains("left     right"), "{s}");
        key(&mut app, KeyCode::Char('m'));
        key(&mut app, KeyCode::Char('L'));
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.opts.trends, [TrendStyle::Median]);
        assert!(app.opts.right.live);
        assert!(screen(&app).contains("lines: live + median / live + avg + p95"));
        // Or from the console, both at once.
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "lines(\"live\", \"median,p95\")");
        key(&mut app, KeyCode::Enter);
        assert!(app.opts.trends.is_empty() && app.opts.show_raw);
        assert_eq!(app.opts.right.to_arg(), "median,p95");
        // Off again: the right-hand trends come back over the live data.
        key(&mut app, KeyCode::Char('D'));
        assert!(!app.opts.side_by_side);
        assert_eq!(app.opts.trends, [TrendStyle::Median, TrendStyle::P95]);
    }

    #[test]
    fn every_button_has_a_console_command() {
        let mut app = app(&["graft", LOG]);
        let run = |app: &mut App, code: &str| {
            key(app, KeyCode::Char(':'));
            type_str(app, code);
            key(app, KeyCode::Enter);
        };
        run(&mut app, "timeframe(6h)");
        assert_eq!(app.timeframe.last, Some(Window::Span(21600.0)));
        run(&mut app, "timeframe(\"2026-11-02\", \"2026-11-02 03:00\")");
        assert_eq!(app.ds.series[0].points.len(), 4);
        run(&mut app, "timeframe(\"all\")");
        assert!(app.timeframe.is_all());
        run(&mut app, "lines(\"median,live\"); window(5)");
        assert!(app.opts.show_raw && app.opts.trends == [TrendStyle::Median]);
        assert_eq!(app.opts.trend_window, Some(5));
        run(&mut app, "lines(\"avg\")");
        assert!(!app.opts.show_raw && app.opts.trends == [TrendStyle::Avg]);
        run(&mut app, "lines(\"bogus\")");
        assert!(app.message.as_ref().is_some_and(|m| m.error));
        run(
            &mut app,
            "stats(); split(true); split(true); log(); targets(false)",
        );
        assert!(app.show_stats && app.opts.split && app.opts.log && !app.opts.show_targets);
        run(&mut app, "stats(); log(false); pause(true); cursor(true)");
        assert!(!app.show_stats && !app.opts.log && app.paused && app.cursor.is_some());
        run(&mut app, "zoom_in(); pan(\"left\")");
        assert!(app.view.is_some());
        run(&mut app, "reset()");
        assert!(app.view.is_none());
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "quit()");
        let t = app.on_event(&TermEvent::Key(KeyEvent::from(KeyCode::Enter)));
        assert_eq!(t, Transition::Quit);
    }

    #[test]
    fn panes_split_pick_move_resize_and_round_trip() {
        let mut app = app(&["graft", LOG]);
        screen(&app);
        // `w` opens the pane panel; split right, then down.
        key(&mut app, KeyCode::Char('w'));
        assert!(app.pane_menu && app.opts.panes.is_some());
        let s = screen(&app);
        assert!(
            s.contains("pane 1 of 1") && s.contains("split right"),
            "{s}"
        );
        let (x, y) = find(&app, " split right ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        key(&mut app, KeyCode::Char('s'));
        assert_eq!(
            app.opts.panes.as_ref().unwrap().layout_expr(),
            "1 | (2 / 3)"
        );
        // Pane 3 (bottom right): bar chart of node2 only.
        key(&mut app, KeyCode::Char('t'));
        key(&mut app, KeyCode::Char('t'));
        key(&mut app, KeyCode::Char('1'));
        let p = app.opts.panes.as_ref().unwrap();
        assert_eq!((p.focus, p.panes[2].kind), (2, Some(ChartKind::Bar)));
        assert_eq!(p.panes[2].series, ["node2"]);
        // Lines for the focused pane only.
        key(&mut app, KeyCode::Char('T'));
        key(&mut app, KeyCode::Char('a'));
        key(&mut app, KeyCode::Esc);
        let p = app.opts.panes.as_ref().unwrap();
        assert_eq!(p.panes[2].lines.as_ref().unwrap().to_arg(), "live,avg");
        assert_eq!(p.panes[0].lines.as_ref().unwrap().to_arg(), "live");
        let s = screen(&app);
        assert!(
            s.contains("node2 avg"),
            "bar chart shows the trend as its own bar: {s}"
        );
        // Pick panes by direction, move one, resize.
        key(&mut app, KeyCode::Left);
        assert_eq!(app.opts.panes.as_ref().unwrap().focus, 0);
        key(&mut app, KeyCode::Char('>'));
        let w0 = app.opts.panes.as_ref().unwrap().rects(app.chart_rect.get())[0]
            .1
            .width;
        assert!(w0 > app.chart_rect.get().width / 2, "{w0}");
        key(&mut app, KeyCode::Char('='));
        // Drag the border between the columns.
        screen(&app);
        let area = app.chart_rect.get();
        let border = app.opts.panes.as_ref().unwrap().rects(area)[1].1.x;
        let row = area.y + 3;
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            border,
            row,
        );
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            area.x + area.width / 4,
            row,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            area.x + area.width / 4,
            row,
        );
        let w0 = app.opts.panes.as_ref().unwrap().rects(area)[0].1.width;
        assert!(w0 < area.width / 3, "{w0}");
        // Tap a pane to focus it.
        let r = app.opts.panes.as_ref().unwrap().rects(area)[2].1;
        key(&mut app, KeyCode::Esc);
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            r.x + r.width / 2,
            r.y + r.height / 2,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            r.x + r.width / 2,
            r.y + r.height / 2,
        );
        assert_eq!(app.opts.panes.as_ref().unwrap().focus, 2);
        // The command line reproduces it.
        let cli = app.current_cli();
        assert_eq!(cli.pane.len(), 3);
        assert_eq!(cli.pane[2], "node2 kind=bar lines=live,avg");
        let args = cli.to_args();
        assert!(args.contains(&"--layout".to_string()), "{args:?}");
        let again = app_from(&args);
        assert_eq!(
            again.opts.panes.as_ref().unwrap().panes,
            app.opts.panes.as_ref().unwrap().panes
        );
        // Console: layout, pane settings, back to one chart.
        key(&mut app, KeyCode::Char(':'));
        type_str(
            &mut app,
            "layout(\"3 / (1 | 2)\"); focus_pane(1); pane(\"node1 kind=sparkline\")",
        );
        key(&mut app, KeyCode::Enter);
        let p = app.opts.panes.as_ref().unwrap();
        assert_eq!(p.layout_expr(), "3 / (1 | 2)");
        assert_eq!(p.panes[0].kind, Some(ChartKind::Sparkline));
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "single()");
        key(&mut app, KeyCode::Enter);
        assert!(app.opts.panes.is_none());
        assert_eq!(
            app.opts.kind,
            ChartKind::Sparkline,
            "the focused pane's chart type stays"
        );
    }

    #[test]
    fn side_by_side_works_for_every_chart_type() {
        for kind in ["bar", "histogram", "sparkline", "scatter"] {
            let app = app(&["graft", LOG, "--side-by-side", "-k", kind]);
            let s = screen(&app);
            assert!(
                s.contains("· live") && s.contains("· avg + median"),
                "{kind}: {s}"
            );
        }
        let app = app(&["graft", LOG, "--side-by-side", "-k", "bar"]);
        assert!(screen(&app).contains("node1 median"));
    }

    #[test]
    fn save_the_view_as_a_profile() {
        let dir = std::env::temp_dir().join(format!("graft-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("profiles.toml");
        crate::profile::TEST_PROFILES.with(|t| *t.borrow_mut() = Some(file.clone()));
        let mut app = app(&["graft", LOG, "--pane", "node1", "--pane", "node2 kind=bar"]);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "save(\"nodes\")");
        key(&mut app, KeyCode::Enter);
        let saved = std::fs::read_to_string(&file).unwrap();
        crate::profile::TEST_PROFILES.with(|t| *t.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            saved.contains("[profiles.nodes]") && saved.contains("node2 kind=bar"),
            "{saved}"
        );
        assert!(
            app.message
                .as_ref()
                .is_some_and(|m| m.text.contains("graft -p nodes"))
        );
    }

    #[test]
    fn copy_and_alert_notifications() {
        let file = std::env::temp_dir().join(format!("graft-alert-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let notify = format!("cmd:echo \"$GRAFT_ALERT\" >> {}", file.display());
        let mut app = app(&["graft", LOG, "--alert", "node1 < 59GB", "--notify", &notify]);
        // Fired once when it started firing (in the background).
        let mut text = String::new();
        for _ in 0..100 {
            text = std::fs::read_to_string(&file).unwrap_or_default();
            if !text.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(text, "ALERT: node1 < 59GB\n");
        // Still firing on the next check: no repeat.
        app.check_alerts();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        let _ = std::fs::remove_file(&file);
        // Copy: the command line, with the path made absolute.
        key(&mut app, KeyCode::Char('C'));
        let copied = app.take_clipboard().unwrap();
        assert!(
            copied.starts_with("graft --alert") && copied.contains("/examples/reclaimable.log"),
            "{copied}"
        );
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "copy(fmt(\"node1\", last(\"node1\")))");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.take_clipboard().as_deref(), Some("58.8 GB"));
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
