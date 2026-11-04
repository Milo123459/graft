//! Interactive viewer: live updates, zoom/pan, a scrubber cursor with a value readout,
//! range selection, stats panel, forecasts, a Rhai console, and full mouse/touch
//! support (tap, drag, scroll, toolbar) for use from a phone over SSH.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use rhai::Dynamic;

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
use crate::data::{Dataset, Series};
use crate::interactive::{Live, Transition};
use crate::render::panes::{Dir as PDir, Pane, Panes, Split as PSplit};
use crate::render::{
    ChartOpts, ChartWidget, HLine, Horizon, Overlay, PlotArea, plot_x_bounds, value_at,
};
use crate::script::{Action, Flag, PaneAction};
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
    ("show all / 0", "show everything again"),
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
        "all time ▾ / r",
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
    (
        "Ctrl+Z / u",
        "undo (Ctrl+Y / U redo) — also the ↶ undo button",
    ),
    ("K", "markers: edit, delete or add rules (notify per rule)"),
    (
        "N",
        "alerts & notifications: add, edit, where they go, test",
    ),
    ("C", "copy the command for this view"),
    (
        "discover / E",
        "invent formulas and strategies from the data, paper-trade them",
    ),
    ("⋯", "more: chart type, log scale, target lines, pause,"),
    (
        "",
        "format (change how lines are read), console, export, mouse",
    ),
    ("— other keys —", ""),
    ("1-9 / a", "show or hide a series / show all"),
    ("t b y", "chart type, marker style, log scale"),
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
    ("m", "mouse on/off (off = select text in your terminal)"),
    (":", "Rhai console, e.g. :slope(\"node1\", \"1d\")"),
    ("q", "quit (Esc only clears a measurement or the cursor)"),
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
    Markers,
    Alerts,
    Sources,
    Legend,
    Undo,
    Discover,
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
    /// Source picker: use item `n`.
    PickSource(usize),
    /// Lines menu: edit custom line `n` (`None`: add one), or delete it.
    LineAction(Option<usize>),
    DeleteLine(usize),
    /// Markers menu.
    ToggleMarkNotify(usize),
    CycleMarkSymbol(usize),
    CycleMarkColor(usize),
    CycleMarkEffect(usize),
    /// The candles chip: a candlestick chart's four series together.
    Candles,
    /// Close the table of results above the legend.
    CloseNote,
    /// Alerts menu.
    EditAlert(Option<usize>),
    DeleteAlert(usize),
    ToggleTarget(&'static str),
    AddTarget(&'static str),
    RemoveTarget(usize),
    TestNotify,
    CloseAlerts,
    EditRule(usize),
    DeleteRule(usize),
    DeleteManual(usize),
    MarkAction(MarkAction),
    /// Timeframe dropdown: type a range in the console.
    CustomRange,
    /// Sources menu: edit one (`None`: the main source), remove a merged one, add one.
    SourceEdit(Option<usize>),
    SourceRemove(usize),
    /// The settings menu: open it for a pane (`None`: the chart), set the chart
    /// type, close it.
    PaneSettings(Option<usize>),
    SetKind(ChartKind),
    CloseSettings,
    /// A source parameter's dropdown (whose, which), and a choice in it.
    ParamMenu(ParamTarget, usize),
    ParamPick(ParamTarget, usize, usize),
    /// Pick another source for pane `n`, or edit the one it has.
    SourcePane(usize),
    SourcePaneEdit(usize),
    SourceAdd,
    CloseSources,
    /// A console suggestion (by position in the hint row).
    Complete(usize),
    /// A command in the `help` list: put it in the console.
    InsertCommand(&'static str),
    /// The tutorial panel.
    Tutorial(TutorialHit),
    /// The discover panel.
    Lab(LabHit),
    MenuBackground,
}

/// The discover panel's parts.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LabHit {
    /// Show strategy n's trades on the chart.
    Pick(usize),
    Keep,
    SearchNow,
    /// Strategies or theories.
    View,
    Stop,
}

/// Whose parameters: the main source, merged source `n`, or pane `n`'s own source.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ParamTarget {
    Main,
    Merge(usize),
    Pane(usize),
}

/// Tab completion in progress.
#[derive(Debug, Clone)]
struct TabCycle {
    base: String,
    cursor: usize,
    comps: crate::script::Completions,
    i: usize,
}

/// The tutorial panel's parts, laid out for a width.
#[derive(Default)]
struct TutorialContent {
    body: Vec<Line<'static>>,
    /// For each body row, the example it shows (to copy on a tap).
    example_rows: Vec<Option<usize>>,
    footer: Vec<Line<'static>>,
    buttons: Vec<Button<Hit>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TutorialHit {
    /// Copy example n of the step (or put a console one in the console).
    Example(usize),
    Next,
    Back,
    Fold,
    Copy,
    End,
}

/// Greedy word wrap to `width` columns (long words are split).
/// A strategy (rule pair NAME in / NAME out) traded on the close (else the series its
/// markers are on): long, short, and the best side, stop and take-profit.
struct StrategyResult {
    name: String,
    on: String,
    long: crate::backtest::Trades,
    short: crate::backtest::Trades,
    best: Option<crate::backtest::Best>,
    /// Holding: all of it, the part the best way was picked on, and the rest.
    hold: [f64; 3],
}

fn strategy_results(
    ds: &crate::data::Dataset,
    marks: &[crate::render::Mark],
    labels: &[String],
    fee: f64,
) -> Vec<StrategyResult> {
    use crate::backtest::{Prices, Rules, Side};
    let of = |i: usize| {
        marks
            .iter()
            .filter(|m| m.rule == Some(i))
            .collect::<Vec<_>>()
    };
    let xs = |m: &[&crate::render::Mark]| m.iter().map(|m| m.x).collect::<Vec<_>>();
    let all = (f64::NEG_INFINITY, f64::INFINITY);
    let mut out: Vec<StrategyResult> = crate::backtest::pairs(labels)
        .into_iter()
        .filter_map(|(name, a, b)| {
            let (ins, outs) = (of(a), of(b));
            let close = crate::backtest::series_for(ds, None, &ins)?;
            let p = Prices::of(ds, close);
            let (ins, outs) = (xs(&ins), xs(&outs));
            let side = |side| Rules {
                side,
                ..Rules::long(fee)
            };
            let (first, last) = (close.points.first()?.0, close.points.last()?.0);
            let split = first + (last - first) * crate::backtest::TRAIN;
            let h = |a, b| crate::backtest::hold_in(close, a, b).unwrap_or(0.0);
            Some(StrategyResult {
                long: crate::backtest::simulate(&ins, &outs, &p, &side(Side::Long), all),
                short: crate::backtest::simulate(&ins, &outs, &p, &side(Side::Short), all),
                best: crate::backtest::best(&ins, &outs, &p, fee, crate::backtest::TRAIN),
                hold: [h(first, last), h(first, split), h(split, last)],
                on: close.name.clone(),
                name,
            })
        })
        .collect();
    let after = |r: &StrategyResult| r.best.as_ref().map_or(f64::MIN, |b| b.after.total());
    out.sort_by(|a, b| after(b).total_cmp(&after(a)));
    out
}

/// `+2.40% (9 trades, 67% won)`.
fn trades_brief(t: &crate::backtest::Trades) -> String {
    let n = t.closed.len();
    if n == 0 {
        return "no trades".into();
    }
    let won = t.closed.iter().filter(|r| **r > 0.0).count() as f64 / n as f64 * 100.0;
    format!(
        "{:+.2}% ({}, {won:.0}% won)",
        t.total(),
        plural(n, "trade", "trades")
    )
}

/// `1 strategy`, `3 strategies`.
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let width = width.max(4);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let mut word = word.to_string();
        loop {
            let need = if line.is_empty() {
                word.width()
            } else {
                line.width() + 1 + word.width()
            };
            if need <= width {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(&word);
                break;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                continue;
            }
            // A word longer than the line: split it.
            let mut cut = 0;
            let mut w = 0;
            for (i, c) in word.char_indices() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if w + cw > width {
                    cut = i;
                    break;
                }
                w += cw;
            }
            out.push(word[..cut].to_string());
            word = word[cut..].to_string();
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
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

/// What undo restores: everything you set up, not where you're looking (cursor, zoom).
#[derive(Debug, Clone)]
struct Snapshot {
    opts: ChartOpts,
    timeframe: Timeframe,
    hidden: Vec<String>,
    show_stats: bool,
    mark_rules: Vec<crate::marks::MarkRule>,
    manual_marks: Vec<crate::render::Mark>,
    custom: Vec<crate::lines::CustomLine>,
    alerts: Vec<String>,
    /// Settings kept only in the configuration: title, unit, hlines, notify targets.
    cli_bits: CliBits,
    /// For comparing setups (computed markers left out).
    key: String,
}

#[derive(Debug, Clone, PartialEq)]
struct CliBits {
    title: Option<String>,
    unit: Option<String>,
    hline: Vec<String>,
    notify: Vec<String>,
}

/// How many steps undo remembers.
const UNDO_DEPTH: usize = 100;

/// A marker rule as listed (without the `notify` prefix, shown as a button instead).
struct MarkRuleView<'a>(&'a crate::marks::MarkRule);

impl MarkRuleView<'_> {
    fn text(&self) -> String {
        match &self.0.label {
            Some(l) => format!("{} => {l}", self.0.expr),
            None => self.0.expr.clone(),
        }
    }
}

/// Buttons at the bottom of the markers menu.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MarkAction {
    AddRule,
    MarkNow,
    ClearAll,
    Close,
}

/// An entry in the source picker: its label and the source name (`None`: main chart).
type SourceChoice = (String, Option<String>);

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
    /// Open the source picker for the picked pane.
    Source,
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
    /// Discovering strategies (`discover()`), and where its markers start in
    /// `opts.marks`.
    lab: Option<crate::lab::Lab>,
    lab_marks_at: usize,
    /// Discovering was started at some point (the tutorial's check).
    lab_started: bool,
    /// The discover panel shows theories instead of strategies.
    lab_theories: bool,
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
    /// The sources menu is open.
    sources_menu: bool,
    /// The pane settings menu is open (for the picked pane, or the chart).
    settings_menu: bool,
    /// Legends shown (the boxes in panes, the series in the legend row).
    show_legend: bool,
    /// A parameter's dropdown is open: whose, which.
    param_menu: Option<(ParamTarget, usize)>,
    /// Fetching older data (scrolled back to the oldest): that run of the command,
    /// and the oldest time when it started.
    backfill: Option<(Live, f64)>,
    /// The source had nothing older to give.
    backfill_done: bool,
    /// The open popup's area (as last drawn), and which of its buttons the arrow
    /// keys have picked.
    popup_rect: std::cell::Cell<Option<Rect>>,
    popup_focus: std::cell::Cell<usize>,
    /// Which popup that was (the pick starts over when another opens).
    popup_kind: std::cell::Cell<u8>,
    /// The source the builder is to edit: `Some(None)` is a new one.
    source_to_edit: Option<Option<String>>,
    /// …or a pane's own source: that pane.
    pane_to_edit: Option<usize>,
    /// The console's last output (a result or an error), shown above the status line.
    console_note: Option<Message>,
    /// Tab pressed: the suggestions being cycled through, and the input before.
    tab_cycle: Option<TabCycle>,
    /// Where in the command the last error is (to put the cursor there).
    error_at: Option<usize>,
    /// `--tutorial`: the guided tour, shown above the chart.
    tutorial: Option<crate::tutorial::Tutorial>,
    tutorial_rect: std::cell::Cell<Rect>,
    /// Sources of panes with `source=` (presets, profiles, recent charts).
    subs: Vec<crate::sources::Sub>,
    /// Their latest data, as drawn.
    sub_views: Vec<crate::render::SourceView>,
    /// Marker rules (`--mark`, `mark_when`).
    mark_rules: Vec<crate::marks::MarkRule>,
    /// The source settings the chart was built with (lines and markers saw these).
    built_params: Vec<crate::params::Param>,
    /// The legend's rows so far, for the width they were for.
    legend_rows: std::cell::Cell<(u16, u16)>,
    /// Paged back from a "last N" timeframe (the window moved, the timeframe kept).
    paged: bool,
    /// Whether the data was last built without the timeframe's cut.
    built_uncut: bool,
    /// Markers placed by hand with `mark("…")`.
    manual_marks: Vec<crate::render::Mark>,
    /// The markers menu is open, with this row picked.
    marks_menu: Option<usize>,
    /// The console is editing this rule: the next `mark_when` replaces it.
    editing_rule: Option<usize>,
    /// Earlier setups to go back to (Ctrl+Z / u), and undone ones to redo.
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Panes put aside while side by side is on.
    panes_before_side_by_side: Option<Panes>,
    /// This event was an undo or redo (not a step of its own).
    stepped: bool,
    /// Custom line errors already reported.
    shown_line_errors: Vec<String>,
    /// `--merge` sources, running.
    merge_subs: Vec<crate::sources::Sub>,
    /// Live sources' revisions at the last redraw (for redrawing in rounds).
    sync_revs: Vec<Option<u64>>,
    /// The longest to wait for a full round before redrawing anyway.
    sync_wait: Duration,
    /// How many times the data was rebuilt (for tests).
    builds: u64,
    /// Merged-source problems already reported.
    shown_merge_problems: Vec<String>,
    /// The alerts & notifications menu is open.
    alerts_menu: bool,
    /// The console is editing alert `n` (running it replaces that alert).
    editing_alert: Option<usize>,
    /// For each notifying marker rule, the newest marker already told about.
    notified_marks: HashMap<String, f64>,
    /// When the viewer opened (markers found while catching up aren't news).
    started: Instant,
    /// Where the console's text field was last drawn (clicks move its cursor there).
    console_rect: std::cell::Cell<Rect>,
    /// The source picker for the focused pane: (label, source name).
    source_menu: Option<(Vec<SourceChoice>, usize)>,
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
        let tutorial = cli.tutorial.then(crate::tutorial::Tutorial::new);
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
            alerts: cli.with_params().alert,
            built_params: crate::params::parse_all(&cli.param),
            legend_rows: std::cell::Cell::new((0, 0)),
            paged: false,
            built_uncut: false,
            show_stats: cli.stats,
            lab: cli.discover.map(|fee| {
                crate::lab::Lab::new(crate::discover::Settings {
                    fee,
                    ..Default::default()
                })
            }),
            lab_marks_at: 0,
            lab_started: cli.discover.is_some(),
            lab_theories: false,
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
            tutorial,
            console_note: None,
            sources_menu: false,
            param_menu: None,
            settings_menu: false,
            show_legend: true,
            backfill: None,
            backfill_done: false,
            popup_rect: std::cell::Cell::new(None),
            popup_focus: std::cell::Cell::new(0),
            popup_kind: std::cell::Cell::new(0),
            source_to_edit: None,
            pane_to_edit: None,
            error_at: None,
            tab_cycle: None,
            tutorial_rect: std::cell::Cell::new(Rect::default()),
            subs: Vec::new(),
            mark_rules: Vec::new(),
            manual_marks: Vec::new(),
            marks_menu: None,
            editing_rule: None,
            undo: Vec::new(),
            redo: Vec::new(),
            panes_before_side_by_side: None,
            stepped: false,
            shown_line_errors: Vec::new(),
            merge_subs: Vec::new(),
            sync_revs: Vec::new(),
            sync_wait: Duration::from_secs(2),
            builds: 0,
            shown_merge_problems: Vec::new(),
            alerts_menu: false,
            editing_alert: None,
            notified_marks: HashMap::new(),
            started: Instant::now(),
            console_rect: std::cell::Cell::new(Rect::default()),
            sub_views: Vec::new(),
            source_menu: None,
            chart_rect: std::cell::Cell::new(Rect::default()),
            hits: Hits::default(),
            plots: RefCell::new(Vec::new()),
            timeframe,
            presets: Vec::new(),
        };
        app.start_merges();
        app.hidden.extend(app.cli.hide.iter().cloned());
        for spec in app.cli.bind.clone() {
            if let Ok((k, code)) = crate::cli::split_bind(&spec) {
                app.binds.insert(k, code);
            }
        }
        app.mark_rules = app
            .cli
            .mark
            .iter()
            .filter_map(|m| crate::marks::MarkRule::parse(m).ok())
            .collect();
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
        c.discover = self.lab.as_ref().map(|l| l.settings.fee);
        c.no_mouse = !self.mouse;
        c.target = self.opts.analysis.targets.to_specs();
        c.trend = self.opts.trends.clone();
        c.trend_only = !self.opts.trends.is_empty() && !self.opts.show_raw;
        c.trend_window = self.opts.trend_window;
        c.right_panel = self.opts.side_by_side.then(|| self.opts.right.clone());
        let panes = self.opts.panes.as_ref().filter(|_| !self.trivial_panes());
        c.pane = panes
            .iter()
            .flat_map(|p| p.panes.iter().map(Pane::to_spec))
            .collect();
        c.layout = panes.map(|p| p.layout_expr());
        // Lines, markers and alerts run with the source's settings filled in; the
        // ones written with {name} are saved that way.
        let params = &self.built_params;
        let unfill = |now: Vec<String>, raw: &[String], norm: &dyn Fn(&str) -> Option<String>| {
            now.into_iter()
                .map(|n| {
                    raw.iter()
                        .find(|r| {
                            r.contains('{')
                                && norm(&crate::params::fill(r, params)).as_ref() == Some(&n)
                        })
                        .cloned()
                        .unwrap_or(n)
                })
                .collect::<Vec<_>>()
        };
        c.mark = unfill(
            self.mark_rules.iter().map(|r| r.to_spec()).collect(),
            &self.cli.mark,
            &|s| crate::marks::MarkRule::parse(s).ok().map(|r| r.to_spec()),
        );
        c.line = unfill(
            self.session.custom.iter().map(|l| l.to_spec()).collect(),
            &self.cli.line,
            &|s| crate::lines::CustomLine::parse(s).ok().map(|l| l.to_spec()),
        );
        c.alert = unfill(self.alerts.clone(), &self.cli.alert, &|s| {
            Some(s.to_string())
        });
        let mut hidden: Vec<String> = self.hidden.iter().cloned().collect();
        hidden.sort();
        c.hide = hidden;
        c.front = self.opts.front.clone();
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
        for m in &mut self.merge_subs {
            m.pump(Duration::from_millis(10));
        }
        self.tick_backfill();
        // A merged source in trouble says so, by name.
        let problems: Vec<String> = self
            .merge_subs
            .iter()
            .filter_map(|m| m.problem().map(|p| format!("{}: {p}", m.name)))
            .collect();
        if problems != self.shown_merge_problems {
            if let Some(p) = problems.first() {
                self.say(p.clone(), true);
            }
            self.shown_merge_problems = problems;
        }
        self.rebuild(false);
        self.tick_lab();
        // Back at the newest (or the view reset): the timeframe cuts again.
        if (self.paged && self.view.is_some()) != self.built_uncut {
            if self.view.is_none() {
                self.paged = false;
            }
            self.rebuild(true);
        }
        self.tick_sources(false);
    }

    /// Start/stop pane sources to match the panes, then read what they produced.
    fn tick_sources(&mut self, force: bool) {
        if self.opts.panes.is_none() && self.subs.is_empty() {
            return;
        }
        crate::sources::sync(
            &mut self.subs,
            &self.opts,
            true,
            self.cli.ssh.as_deref(),
            &self.cli.source,
        );
        let mut changed = force || self.sub_views.len() != self.subs.len();
        for sub in &mut self.subs {
            if sub.pump(Duration::from_millis(10)) && (!self.paused || force) {
                sub.rebuild(&self.timeframe);
                changed = true;
            }
        }
        if changed {
            self.sub_views = self.subs.iter().map(|s| s.view.clone()).collect();
        }
    }

    fn rebuild(&mut self, force: bool) {
        let rev = self
            .merge_subs
            .iter()
            .fold(self.session.store.revision, |r, m| {
                r.wrapping_add(m.revision())
            });
        if !force {
            // Throttle rebuilds while data is streaming in.
            let due = self.live.eof || self.last_build.elapsed() > Duration::from_millis(150);
            if self.paused || rev == self.built_rev || !due || !self.round_complete() {
                return;
            }
        }
        self.sync_revs = self.source_revisions();
        self.builds += 1;
        let old_max = self.ds.x_range().map(|r| r.1);
        self.full = self.session.base_dataset();
        self.apply_unit();
        // Merged sources join in, named after themselves; then custom lines, which can
        // combine them.
        if let Some(name) = &self.cli.as_name {
            crate::sources::rename(name, &mut self.full);
        }
        for m in &self.merge_subs {
            let mut merged = m.merged();
            if self.full.series.is_empty() {
                self.full.x_kind = merged.x_kind;
            }
            self.full.series.append(&mut merged.series);
        }
        self.session.add_custom_lines(&mut self.full);
        self.presets = timeframe_presets(&self.full);
        // Paged back from "last 1h": all the data, seen through the moved window.
        let uncut = self.paged && self.view.is_some();
        if uncut {
            Timeframe {
                last: None,
                ..self.timeframe
            }
            .apply(&mut self.full);
        } else {
            self.timeframe.apply(&mut self.full);
        }
        self.built_uncut = uncut;
        self.ds = self.session.select(&self.full);
        for m in &self.merge_subs {
            let prefix = format!("{}.", m.name);
            for s in &self.full.series {
                if (s.name == m.name || s.name.starts_with(&prefix))
                    && !self.ds.series.iter().any(|d| d.name == s.name)
                {
                    self.ds.series.push(s.clone());
                }
            }
        }
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
        self.update_marks();
        let errors = self.session.line_errors.borrow().clone();
        if errors != self.shown_line_errors {
            match errors.first() {
                Some(e) => self.say(e.clone(), true),
                // Fixed (e.g. a source that was late has arrived): stop showing it.
                None if self
                    .message
                    .as_ref()
                    .is_some_and(|m| m.error && m.text.starts_with("line ")) =>
                {
                    self.message = None;
                }
                None => {}
            }
            self.shown_line_errors = errors;
        }
    }

    /// Rule `removed` is gone: the rule being edited moves up (or is gone too).
    fn shift_editing_rule(&mut self, removed: usize) {
        self.editing_rule = match self.editing_rule {
            Some(i) if i == removed => None,
            Some(i) if i > removed => Some(i - 1),
            other => other,
        };
    }

    /// Place markers from the rules (and those set by hand) on the current data.
    fn update_marks(&mut self) {
        let mut new = self.session.script.take_mark_rules();
        // An edit replaces the rule it started from.
        // (Only once the edited rule arrives: updates while you're still typing keep it.)
        if !new.is_empty()
            && let Some(i) = self.editing_rule.take()
            && i < self.mark_rules.len()
        {
            let old = self.mark_rules[i].clone();
            let mut edited = new.remove(0);
            // mark_when() doesn't spell out the style or notify: keep them.
            edited.symbol = edited.symbol.or(old.symbol);
            edited.color = edited.color.or(old.color);
            edited.notify |= old.notify;
            self.mark_rules[i] = edited;
        }
        self.mark_rules.extend(new);
        let (mut marks, errors) =
            crate::marks::evaluate(&self.session.script, &self.mark_rules, &self.ds);
        if let Some(e) = errors.into_iter().next() {
            // Each failing rule is dropped so the message isn't repeated on every update.
            let before = self.mark_rules.len();
            let mut k = 0;
            while k < self.mark_rules.len() {
                if e.contains(&format!("mark '{}'", self.mark_rules[k].expr)) {
                    self.mark_rules.remove(k);
                    self.shift_editing_rule(k);
                } else {
                    k += 1;
                }
            }
            if self.mark_rules.len() != before {
                // Markers point at rules by position: place them again.
                marks = crate::marks::evaluate(&self.session.script, &self.mark_rules, &self.ds).0;
            }
            self.say(e, true);
        }
        // Notifying rules: tell about markers newer than any told about before. On a
        // rule's first run (or a source that isn't live) nothing is sent: those are history.
        // History: before any data, or while the source is first being read.
        let catching_up = self.ds.series.iter().all(|s| s.points.is_empty())
            || self.started.elapsed() < Duration::from_secs(2);
        // Live: the main source, or any merged into it (e.g. price feeds over a file).
        let live = (self.live.spec.is_live() || self.merge_subs.iter().any(|m| m.is_live()))
            && !catching_up;
        let mut news = Vec::new();
        for (i, rule) in self.mark_rules.iter().enumerate().filter(|(_, r)| r.notify) {
            let newest = marks
                .iter()
                .filter(|m| m.rule == Some(i))
                .map(|m| m.x)
                .fold(f64::NEG_INFINITY, f64::max);
            let key = rule.to_spec();
            match self.notified_marks.get(&key) {
                Some(&seen) if live => {
                    for m in marks.iter().filter(|m| m.rule == Some(i) && m.x > seen) {
                        news.push(format!("{} ({})", m.label, self.ds.fmt_x_full(m.x)));
                    }
                }
                _ => {}
            }
            if newest > f64::NEG_INFINITY || !self.notified_marks.contains_key(&key) {
                let prev = self
                    .notified_marks
                    .get(&key)
                    .copied()
                    .unwrap_or(f64::NEG_INFINITY);
                self.notified_marks.insert(key, newest.max(prev));
            }
        }
        if !news.is_empty() {
            let msg = if news.len() > 3 {
                format!("{} and {} more", news[..3].join("; "), news.len() - 3)
            } else {
                news.join("; ")
            };
            self.notify_now(msg);
        }
        marks.extend(self.manual_marks.iter().cloned());
        self.opts.marks = marks;
        self.lab_marks_at = self.opts.marks.len();
        if let Some(lab) = &self.lab {
            self.opts.marks.extend(lab.marks());
        }
    }

    // ───────────── discover ─────────────

    fn lab_hit(&mut self, h: LabHit) {
        match h {
            LabHit::Pick(i) => {
                if let Some(lab) = self.lab.as_mut() {
                    lab.selected = i;
                }
                self.place_lab_marks();
            }
            LabHit::Keep => match self.discover_keep(None) {
                Ok(text) => self.note(text, false),
                Err(e) => self.say(e, true),
            },
            LabHit::SearchNow => {
                if let Some(lab) = self.lab.as_mut() {
                    lab.rerun();
                }
                self.tick_lab();
            }
            LabHit::View => self.lab_theories = !self.lab_theories,
            LabHit::Stop => self.stop_discover(),
        }
    }

    /// Run the discovery lab: searches in the background, paper trades on new data.
    fn tick_lab(&mut self) {
        let Some(lab) = self.lab.as_mut() else {
            return;
        };
        let before = (lab.runs, lab.report.is_some(), lab.selected);
        let news = lab.tick(&self.ds);
        let changed = before != (lab.runs, lab.report.is_some(), lab.selected) || news.is_some();
        let trading = !lab.tracked.is_empty();
        if let Some(n) = news {
            self.say(n, false);
        }
        if changed || trading {
            self.place_lab_marks();
        }
    }

    fn place_lab_marks(&mut self) {
        self.opts
            .marks
            .truncate(self.lab_marks_at.min(self.opts.marks.len()));
        if let Some(lab) = &self.lab {
            self.opts.marks.extend(lab.marks());
        }
    }

    /// `discover(fee, series)`: start (or restart) the lab.
    fn start_discover(&mut self, fee: f64, series: Option<String>) {
        if let Some(name) = &series
            && crate::discover::price_series(&self.ds, Some(name)).is_none()
        {
            let names: Vec<&str> = self.ds.series.iter().map(|s| s.name.as_str()).collect();
            self.say(
                format!(
                    "discover: there's no series called \"{name}\" (there's {})",
                    names.join(", ")
                ),
                true,
            );
            return;
        }
        let settings = crate::discover::Settings {
            fee,
            series,
            ..Default::default()
        };
        self.lab = Some(crate::lab::Lab::new(settings));
        self.lab_started = true;
        self.lab_theories = false;
        let on = crate::discover::price_series(
            &self.ds,
            self.lab.as_ref().and_then(|l| l.settings.series.as_deref()),
        )
        .map_or("the price".to_string(), |s| s.name.clone());
        self.say(
            format!(
                "discover: inventing formulas on {on} and testing them (fee {fee}% a side) — strategies are paper-traded from when they're found"
            ),
            false,
        );
        self.tick_lab();
    }

    fn stop_discover(&mut self) {
        self.lab = None;
        self.place_lab_marks();
    }

    /// `discover_keep(n)`: the strategy's formulas become (hidden) lines and its entries
    /// a marker rule, kept with the chart.
    fn discover_keep(&mut self, n: Option<usize>) -> Result<String, String> {
        let lab = self
            .lab
            .as_ref()
            .ok_or("discover_keep: discover isn't running — discover() starts it")?;
        let i = match n {
            Some(n) => n
                .checked_sub(1)
                .ok_or("discover_keep: strategies count from 1")?,
            None => lab.selected,
        };
        let t = lab
            .tracked
            .get(i)
            .ok_or_else(|| format!("discover_keep: there's no strategy {} yet", i + 1))?;
        let report = lab
            .report
            .as_ref()
            .ok_or("discover_keep: nothing found yet")?;
        let s = &t.found.strategy;
        let mut k = 1;
        let taken = |k: usize, me: &Self| {
            let p = format!("disc{k}");
            me.session.custom.iter().any(|l| l.name.starts_with(&p))
                || me.mark_rules.iter().any(|r| {
                    r.label
                        .as_deref()
                        .is_some_and(|l| l.starts_with(&format!("{p} ")))
                })
        };
        while taken(k, self) {
            k += 1;
        }
        let mut conds = Vec::new();
        let mut names = Vec::new();
        for (j, c) in s.conds.iter().enumerate() {
            let name = format!("disc{k}_{}", j + 1);
            let line = crate::lines::CustomLine::parse(&format!(
                "{name} = formula({:?})",
                c.feature.to_string()
            ))?;
            self.session.custom.push(line);
            self.hidden.insert(name.clone());
            let th = format!("{:.8}", c.threshold);
            let th = th.trim_end_matches('0').trim_end_matches('.');
            conds.push(format!("{name} {} {th}", if c.above { ">" } else { "<" }));
            names.push(name);
        }
        let long = s.side == crate::discover::Side::Long;
        let label = format!("disc{k} {}", if long { "long" } else { "short" });
        let spec = format!(
            "{} => {label} | {}",
            conds.join(" && "),
            if long { "▲ lime" } else { "▼ red" }
        );
        let rule = crate::marks::MarkRule::parse(&spec)?;
        self.mark_rules.push(rule);
        let exit = s.describe(&report.input);
        let exit = exit.split(" · ").last().unwrap_or("").to_string();
        let hold = s.exit.hold;
        self.rebuild(true);
        Ok(format!(
            "kept as the marker rule \"{label}\" (lines {}, hidden) — it was tested as: {exit}. Try backtest(\"{label}\", {hold}), or notify on it from the markers menu (K)",
            names.join(", ")
        ))
    }

    /// Send a notification to the `--notify` targets (the bell if there are none), and
    /// show it in the status line.
    fn notify_now(&mut self, msg: String) {
        let targets = if self.cli.notify.is_empty() {
            vec!["bell".to_string()]
        } else {
            self.cli.notify.clone()
        };
        crate::notify::send_in_background(targets, self.opts.title.clone(), msg.clone());
        self.say(format!("🔔 {msg}"), false);
    }

    // ───────────── tutorial ─────────────

    /// Has the current step's task been done? (Read-only steps always are.)
    fn tutorial_check(&self, c: crate::tutorial::Check) -> bool {
        use crate::tutorial::Check as C;
        let t = self.tutorial.as_ref();
        match c {
            C::Read => true,
            C::Cursor => self.cursor.is_some(),
            C::Zoomed => self.view.is_some(),
            C::Selected => self.selection.is_some(),
            C::Hidden => !self.hidden.is_empty(),
            C::Trends => !self.opts.trends.is_empty(),
            C::Timeframe => !self.timeframe.is_all(),
            C::Stats => self.show_stats,
            C::Forecast => self.opts.forecast.is_some(),
            C::Kind => {
                self.opts.kind != ChartKind::Line
                    || self.opts.panes.as_ref().is_some_and(|p| {
                        p.panes
                            .iter()
                            .any(|q| q.kind.is_some_and(|k| k != ChartKind::Line))
                    })
            }
            C::Panes => {
                self.opts.side_by_side || self.opts.panes.as_ref().is_some_and(|p| p.count() > 1)
            }
            C::HLine => !self.opts.hlines.is_empty(),
            C::Marker => !self.mark_rules.is_empty() || !self.manual_marks.is_empty(),
            C::Alert => !self.alerts.is_empty(),
            C::Line => !self.session.custom.is_empty(),
            C::Undo => t.is_some_and(|t| t.undone),
            C::Copied => t.is_some_and(|t| t.copied),
            C::Discover => self.lab_started,
        }
    }

    /// Tick off the current step once its task is done.
    fn update_tutorial(&mut self) {
        let Some(t) = &self.tutorial else {
            return;
        };
        let (step, check) = (t.step, t.current().check);
        if !t.done[step]
            && self.tutorial_check(check)
            && let Some(t) = self.tutorial.as_mut()
        {
            t.done[step] = true;
        }
    }

    fn tutorial_hit(&mut self, h: TutorialHit) -> Transition {
        let Some(t) = self.tutorial.as_mut() else {
            return Transition::None;
        };
        match h {
            TutorialHit::Next => {
                if !t.next() {
                    return self.tutorial_hit(TutorialHit::End);
                }
                t.folded = false;
            }
            TutorialHit::Back => t.back(),
            TutorialHit::Example(n) => {
                let Some(&(ex, _)) = t.current().examples.get(n) else {
                    return Transition::None;
                };
                match ex.strip_prefix(':') {
                    // A console command: into the console, ready to run or change.
                    Some(code) => {
                        let code = code.replace("  :", "; ");
                        self.console = Some(TextInput::new(code));
                        self.say("in the console: Enter runs it", false);
                    }
                    None => {
                        self.copy(Some(ex.to_string()));
                        self.say(format!("copied: {ex}"), false);
                    }
                }
                return Transition::None;
            }
            TutorialHit::Fold => t.folded = !t.folded,
            TutorialHit::Copy => {
                if let Some(c) = t.current().command {
                    self.copy(Some(c.to_string()));
                    self.say(format!("copied: {c}"), false);
                }
            }
            TutorialHit::End => {
                self.tutorial = None;
                self.say("tutorial closed · graft --tutorial runs it again", false);
            }
        }
        // A step whose task you did earlier (or just now) is ticked straight away.
        self.update_tutorial();
        Transition::None
    }

    /// The panel at this width: the explanation, keys and examples (scrollable), what
    /// to try (always shown), and its buttons.
    fn tutorial_content(&self, width: u16) -> TutorialContent {
        let Some(t) = &self.tutorial else {
            return TutorialContent::default();
        };
        let step = t.current();
        let w = width.saturating_sub(2).max(10) as usize;
        let mut body: Vec<Line<'static>> = Vec::new();
        let text = Style::default().fg(Color::White);
        let dim = Style::default().fg(Color::Gray);
        let head = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let code = Style::default().fg(Color::Cyan);
        for para in step.text.split('\n') {
            // Indented lines are shown as they are (sample input).
            if para.starts_with("  ") {
                for l in wrap(para.trim_start(), w.saturating_sub(2)) {
                    body.push(Line::styled(format!("  {l}"), code));
                }
            } else {
                body.extend(wrap(para, w).into_iter().map(|l| Line::styled(l, text)));
            }
        }
        if let Some(c) = step.command {
            for l in wrap(c, w.saturating_sub(2)) {
                body.push(Line::styled(format!("  {l}"), code));
            }
        }
        // Two columns when there's room, else the description under the key.
        // Which example each body row belongs to (a tap copies it).
        let mut example_rows: Vec<Option<usize>> = Vec::new();
        let table = |body: &mut Vec<Line<'static>>,
                     rows_of: &mut Vec<Option<usize>>,
                     title: &str,
                     rows: &[(&str, &str)],
                     style: Style,
                     tappable: bool| {
            if rows.is_empty() {
                return;
            }
            rows_of.resize(body.len(), None);
            body.push(Line::styled(
                if tappable {
                    format!("{title} · tap one to copy it (: ones go into the console)")
                } else {
                    title.to_string()
                },
                head,
            ));
            rows_of.push(None);
            let col = rows
                .iter()
                .map(|r| unicode_width::UnicodeWidthStr::width(r.0))
                .max()
                .unwrap_or(0);
            let two = w >= col + 30;
            for (n, (k, d)) in rows.iter().enumerate() {
                let before = body.len();
                if two {
                    let lines = wrap(d, w - col - 4);
                    for (i, l) in lines.into_iter().enumerate() {
                        let key = if i == 0 { *k } else { "" };
                        let pad = col - unicode_width::UnicodeWidthStr::width(key);
                        body.push(Line::from(vec![
                            Span::styled(format!("  {key}{}", " ".repeat(pad)), style),
                            Span::styled(format!("  {l}"), dim),
                        ]));
                    }
                } else {
                    for l in wrap(k, w.saturating_sub(2)) {
                        body.push(Line::styled(format!("  {l}"), style));
                    }
                    for l in wrap(d, w.saturating_sub(4)) {
                        body.push(Line::styled(format!("    {l}"), dim));
                    }
                }
                rows_of.resize(before, None);
                rows_of.extend((before..body.len()).map(|_| tappable.then_some(n)));
            }
        };
        table(
            &mut body,
            &mut example_rows,
            "keys",
            step.keys,
            Style::default().fg(Color::Yellow),
            false,
        );
        table(
            &mut body,
            &mut example_rows,
            "examples",
            step.examples,
            code,
            true,
        );
        example_rows.resize(body.len(), None);
        let mut footer = Vec::new();
        if let Some(task) = step.task {
            if t.is_done() {
                footer.push(Line::styled(
                    "✓ done! Enter or tap next to go on",
                    Style::default()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                for (i, l) in wrap(task, w.saturating_sub(7)).into_iter().enumerate() {
                    let lead = if i == 0 { "▸ try: " } else { "       " };
                    footer.push(Line::styled(
                        format!("{lead}{l}"),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            }
        }
        let last = t.step + 1 == crate::tutorial::STEPS.len();
        let mut buttons = Vec::new();
        if t.step > 0 {
            buttons.push(Button::new("◀ back", Hit::Tutorial(TutorialHit::Back)));
        }
        buttons.push(
            Button::new(
                if last { "finish ✓" } else { "next ▶" },
                Hit::Tutorial(TutorialHit::Next),
            )
            .active(t.is_done()),
        );
        if step.command.is_some() {
            buttons.push(Button::new("copy", Hit::Tutorial(TutorialHit::Copy)));
        }
        buttons.push(Button::new("hide", Hit::Tutorial(TutorialHit::Fold)));
        if !last {
            buttons.push(Button::new("end tour", Hit::Tutorial(TutorialHit::End)));
        }
        TutorialContent {
            body,
            example_rows,
            footer,
            buttons,
        }
    }

    fn tutorial_height(&self, width: u16, height: u16) -> u16 {
        let Some(t) = &self.tutorial else {
            return 0;
        };
        if t.folded {
            return 1;
        }
        let c = self.tutorial_content(width);
        let want = (c.body.len() + c.footer.len()) as u16
            + flow_height(&c.buttons, width.saturating_sub(2))
            + 2;
        // Up to about half the screen: the chart needs the rest (longer steps scroll).
        want.min((height * 11 / 20).max(8))
            .min(height.saturating_sub(6))
    }

    /// Scroll the panel by `rows` (negative: up).
    fn scroll_tutorial(&mut self, rows: i32) {
        let size = ratatui::layout::Size::from(self.tutorial_rect.get());
        let Some(t) = &self.tutorial else {
            return;
        };
        let c = self.tutorial_content(size.width);
        let fixed =
            c.footer.len() as u16 + flow_height(&c.buttons, size.width.saturating_sub(2)) + 2;
        let room = size.height.saturating_sub(fixed);
        let max = (c.body.len() as u16).saturating_sub(room);
        let to = (t.scroll as i32 + rows).clamp(0, max as i32) as u16;
        if let Some(t) = self.tutorial.as_mut() {
            t.scroll = to;
        }
    }

    fn draw_tutorial(&self, f: &mut Frame, area: Rect) {
        let Some(t) = &self.tutorial else {
            return;
        };
        self.tutorial_rect.set(area);
        let n = crate::tutorial::STEPS.len();
        let step = t.current();
        let progress = format!("tutorial {}/{n}", t.step + 1);
        if t.folded {
            let line = Line::from(vec![
                Span::styled(
                    format!(" {progress} · {} ", step.title),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if t.is_done() { " ✓ " } else { "" },
                    Style::default().fg(Color::LightGreen),
                ),
                Span::styled(" show ▾", Style::default().fg(Color::Gray)),
            ]);
            f.render_widget(line, area);
            self.hits.push(area, Hit::Tutorial(TutorialHit::Fold));
            return;
        }
        Clear.render(area, f.buffer_mut());
        let c = self.tutorial_content(area.width);
        let mut block = Block::bordered()
            .border_style(Style::default().fg(Color::Yellow))
            .title(Line::from(vec![
                Span::styled(format!(" {progress} "), Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("{} ", step.title),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        let inner = block.inner(area);
        let bh = flow_height(&c.buttons, inner.width);
        let footer_h = c.footer.len() as u16;
        let body_h = inner.height.saturating_sub(bh + footer_h);
        let max_scroll = (c.body.len() as u16).saturating_sub(body_h);
        let scroll = t.scroll.min(max_scroll);
        // Say when there's more to read, and how to get to it.
        let more = match (scroll > 0, scroll < max_scroll) {
            (true, true) => Some(" ↑↓ more · scroll / PgUp PgDn "),
            (false, true) => Some(" ↓ more · scroll / PgDn "),
            (true, false) => Some(" ↑ PgUp "),
            (false, false) => None,
        };
        if let Some(m) = more {
            block = block
                .title_bottom(Line::styled(m, Style::default().fg(Color::Yellow)).right_aligned());
        }
        f.render_widget(block, area);
        // Taps on the panel's background do nothing (not the chart under it).
        self.hits.push(area, Hit::MenuBackground);
        // Example rows can be tapped (pushed after the background, so they win).
        for (k, ex) in c.example_rows.iter().enumerate().skip(scroll as usize) {
            let y = inner.y + (k as u16 - scroll);
            if y >= inner.y + body_h {
                break;
            }
            if let Some(n) = ex {
                self.hits.push(
                    Rect {
                        y,
                        height: 1,
                        ..inner
                    },
                    Hit::Tutorial(TutorialHit::Example(*n)),
                );
            }
        }
        f.render_widget(
            Paragraph::new(c.body).scroll((scroll, 0)),
            Rect {
                height: body_h,
                ..inner
            },
        );
        f.render_widget(
            Paragraph::new(c.footer),
            Rect {
                y: inner.y + body_h,
                height: footer_h.min(inner.height.saturating_sub(body_h)),
                ..inner
            },
        );
        let by = inner.y + body_h + footer_h;
        render_buttons(
            &c.buttons,
            Rect {
                y: by,
                height: inner.bottom().saturating_sub(by),
                ..inner
            },
            f.buffer_mut(),
            &self.hits,
        );
    }

    // ───────────── alerts menu ─────────────

    fn edit_alert(&mut self, i: Option<usize>) {
        let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let code = match i.and_then(|i| self.alerts.get(i)) {
            Some(a) => format!("alert({})", q(a)),
            None => {
                let first = self
                    .ds
                    .series
                    .first()
                    .map_or("name".to_string(), |s| s.name.clone());
                format!("alert(\"last(\\\"{first}\\\") > 100\")")
            }
        };
        self.alerts_menu = false;
        self.editing_alert = i;
        self.console = Some(TextInput::new(code));
        self.say(
            "an alert is a condition on the whole chart, e.g. last(\"eth\") < 3000 or slope(\"x\", \"1h\") < 0 · Enter saves",
            false,
        );
    }

    fn toggle_target(&mut self, t: &str) {
        match self.cli.notify.iter().position(|x| x == t) {
            Some(i) => {
                self.cli.notify.remove(i);
            }
            None => self.cli.notify.push(t.to_string()),
        }
    }

    fn draw_alerts_menu(&self, f: &mut Frame) {
        if !self.alerts_menu {
            return;
        }
        let area = f.area();
        let w = 84.min(area.width);
        let notifying: Vec<&crate::marks::MarkRule> =
            self.mark_rules.iter().filter(|r| r.notify).collect();
        let custom: Vec<(usize, &String)> = self
            .cli
            .notify
            .iter()
            .enumerate()
            .filter(|(_, t)| t.contains(':'))
            .collect();
        let on = |t: &str| self.cli.notify.iter().any(|x| x == t);
        let target_buttons = vec![
            Button::new("bell", Hit::ToggleTarget("bell"))
                .active(on("bell") || self.cli.notify.is_empty()),
            Button::new("desktop", Hit::ToggleTarget("desktop")).active(on("desktop")),
            Button::new("+ ntfy (phone)", Hit::AddTarget("ntfy")),
            Button::new("+ webhook", Hit::AddTarget("webhook")),
            Button::new("+ command", Hit::AddTarget("cmd")),
        ];
        let action_buttons = vec![
            Button::new("+ add alert", Hit::EditAlert(None)),
            Button::new("send a test", Hit::TestNotify),
            Button::new("done", Hit::CloseAlerts),
        ];
        // On a narrow screen the buttons wrap: make room for every row of them.
        let inner_w = w.saturating_sub(2);
        let (target_h, action_h) = (
            flow_height(&target_buttons, inner_w),
            flow_height(&action_buttons, inner_w),
        );
        let rows = self.alerts.len().max(1)
            + notifying.len().max(1)
            + custom.len()
            + 9
            + (target_h + action_h) as usize;
        let h = (rows as u16).min(area.height);
        let rect = Rect {
            x: (area.width - w) / 2,
            y: area.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        };
        Clear.render(rect, f.buffer_mut());
        self.popup_rect.set(Some(rect));
        let block = Block::bordered()
            .title(" alerts & notifications ")
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.hits.push(rect, Hit::MenuBackground);
        let dim = Style::default().fg(Color::Gray);
        let head = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let mut y = inner.y;
        let row = |y: u16| Rect {
            y,
            height: 1,
            ..inner
        };
        let x_button = |f: &mut Frame, y: u16, hit: Hit| {
            if y < inner.bottom() && inner.width > 4 {
                let r = Rect {
                    x: inner.right() - 3,
                    y,
                    width: 3,
                    height: 1,
                };
                f.render_widget(
                    Span::styled(" ✕ ", Style::default().fg(Color::White).bg(Color::DarkGray)),
                    r,
                );
                self.hits.push(r, hit);
            }
        };
        f.render_widget(
            Line::styled(
                "alerts — a condition on the whole chart; notifies when it starts holding",
                head,
            ),
            row(y),
        );
        y += 1;
        if self.alerts.is_empty() {
            f.render_widget(Line::styled("  none yet", dim), row(y));
            y += 1;
        }
        for (i, a) in self.alerts.iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }
            let firing = self.firing.iter().any(|f| f == a);
            let r = Rect {
                width: inner.width.saturating_sub(4),
                ..row(y)
            };
            f.render_widget(
                Line::from(vec![
                    Span::styled(" ✎ ", Style::default().fg(Color::Yellow)),
                    Span::raw(a.clone()),
                    if firing {
                        Span::styled(
                            "  ● firing",
                            Style::default()
                                .fg(Color::LightRed)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        Span::styled("  ○ quiet", dim)
                    },
                ]),
                r,
            );
            self.hits.push(r, Hit::EditAlert(Some(i)));
            x_button(f, y, Hit::DeleteAlert(i));
            y += 1;
        }
        y += 1;
        if y < inner.bottom() {
            f.render_widget(
                Line::styled(
                    "markers that notify (turn on with notify in the markers menu, K)",
                    head,
                ),
                row(y),
            );
            y += 1;
        }
        if notifying.is_empty() && y < inner.bottom() {
            f.render_widget(Line::styled("  none", dim), row(y));
            y += 1;
        }
        for r in &notifying {
            if y >= inner.bottom() {
                break;
            }
            f.render_widget(Line::raw(format!("  ▼ {}", MarkRuleView(r).text())), row(y));
            y += 1;
        }
        y += 1;
        if y < inner.bottom() {
            f.render_widget(Line::styled("send notifications to", head), row(y));
            y += 1;
        }
        let rows_at = |y: u16, n: u16| Rect {
            y,
            height: n.min(inner.bottom().saturating_sub(y)),
            ..inner
        };
        if y < inner.bottom() {
            render_buttons(
                &target_buttons,
                rows_at(y, target_h),
                f.buffer_mut(),
                &self.hits,
            );
            y += target_h;
        }
        for (i, t) in custom {
            if y >= inner.bottom() {
                break;
            }
            f.render_widget(
                Line::raw(format!("  → {t}")),
                Rect {
                    width: inner.width.saturating_sub(4),
                    ..row(y)
                },
            );
            x_button(f, y, Hit::RemoveTarget(i));
            y += 1;
        }
        y += 1;
        if y < inner.bottom() {
            render_buttons(
                &action_buttons,
                rows_at(y, action_h),
                f.buffer_mut(),
                &self.hits,
            );
        }
        if inner.height > 0 {
            f.render_widget(
                Line::styled(
                    "tap an alert to edit it · ✕ removes · Esc closes",
                    Style::default().fg(Color::DarkGray),
                ),
                row(inner.bottom() - 1),
            );
        }
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
        // On "last 1h", going back pages that hour back through the data: still an
        // hour wide, still "last 1h" (→ back at the newest returns to it).
        if self.view.is_none() && frac < 0.0 && self.timeframe.last.is_some() {
            let (a, b) = self.bounds();
            self.paged = true;
            self.view = Some((a, b));
            self.rebuild(true);
            let d = (b - a) * frac;
            self.set_view(a + d, b + d);
            // Nothing older loaded yet: stay on this hour and ask the source for more.
            if self.view.is_none() {
                self.view = Some((a, b));
                self.start_backfill();
            }
            return;
        }
        if self.view.is_none() {
            // Everything's in view: going back means loading older data, if the
            // source can be asked for it.
            if frac < 0.0 && self.start_backfill() {
                return;
            }
            if frac < 0.0 && self.backfill_done {
                self.say("no older data from the source", false);
                return;
            }
            self.say(
                "everything is already in view: zoom in (↑, + or scroll) to move around",
                false,
            );
            return;
        }
        let (x0, x1) = self.bounds();
        // Already at the newest data: moving on means back to everything ("live").
        let (_, end) = self.full_bounds();
        if frac > 0.0 && x1 >= end - (x1 - x0) * 0.01 {
            self.view = None;
            if self.paged {
                self.say("back to the newest", false);
            } else {
                self.say("showing everything", false);
            }
            return;
        }
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
        self.paged = false;
        self.view = None;
        self.selection = None;
        self.anchor = None;
        self.rebuild(true);
        for sub in &mut self.subs {
            sub.rebuild(&self.timeframe);
        }
        self.sub_views = self.subs.iter().map(|s| s.view.clone()).collect();
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

    /// Open the console to add a custom line, or edit line `i` (same name replaces it).
    fn edit_custom_line(&mut self, i: Option<usize>) {
        let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let code = match i.and_then(|i| self.session.custom.get(i)) {
            Some(l) => {
                let name = match &l.unit {
                    Some(u) => format!("{}:{u}", l.name),
                    None => l.name.clone(),
                };
                format!("add_line({}, {})", q(&name), q(&l.expr))
            }
            None => {
                let first = self
                    .ds
                    .series
                    .first()
                    .map_or("name".to_string(), |s| s.name.clone());
                format!("add_line(\"ema30\", \"ema(values(\\\"{first}\\\"), 30)\")")
            }
        };
        self.lines_menu = false;
        self.console = Some(TextInput::new(code));
        self.say(
            "a Rhai expression returning an array (one value per point), [x, y] pairs or a number · Enter saves",
            false,
        );
    }

    // ───────────── markers menu ─────────────

    /// Rows of the markers menu: the rules, then markers placed by hand.
    fn mark_rows(&self) -> usize {
        self.mark_rules.len() + self.manual_marks.len()
    }

    /// A rule's markers, and a name for it (its label, else its condition).
    fn rule_marks(
        &self,
        rule: &crate::backtest::RuleRef,
    ) -> Result<(String, Vec<&crate::render::Mark>), String> {
        let Some(i) = rule.find(&self.mark_rules) else {
            let have: Vec<String> = self
                .mark_rules
                .iter()
                .enumerate()
                .map(|(k, r)| format!("{} {}", k + 1, r.label.as_deref().unwrap_or(&r.expr)))
                .collect();
            return Err(if have.is_empty() {
                "no marker rules yet: add one with mark_when(…) or --mark".into()
            } else {
                format!("no marker rule {rule}: there's {}", have.join(", "))
            });
        };
        let r = &self.mark_rules[i];
        let name = r.label.clone().unwrap_or_else(|| r.expr.clone());
        let marks = self
            .opts
            .marks
            .iter()
            .filter(|m| m.rule == Some(i))
            .collect();
        Ok((name, marks))
    }

    /// `backtest(rule, series, after)`: what followed each of the rule's markers.
    fn backtest(
        &self,
        rule: &crate::backtest::RuleRef,
        series: Option<&str>,
        after: crate::backtest::After,
    ) -> Result<String, String> {
        let (name, marks) = self.rule_marks(rule)?;
        let s = crate::backtest::series_for(&self.ds, series, &marks).ok_or_else(|| {
            format!("{name}: say which series to measure, e.g. backtest(1, \"close\", 10)")
        })?;
        let after_text = match after {
            crate::backtest::After::Points(1) => "1 point".to_string(),
            crate::backtest::After::Points(n) => format!("{n} points"),
            crate::backtest::After::Span(d) => crate::time::fmt_duration(d),
        };
        Ok(crate::backtest::followed(&marks, s, after).summary(&name, &s.name, &after_text))
    }

    /// `strategies(fee)`: every NAME in / NAME out pair — long, short, and the best
    /// side, stop and take-profit (picked on the first 70%, checked on the rest).
    fn strategies(&self, fee: f64) -> String {
        let labels = self.rule_labels();
        let results = strategy_results(&self.ds, &self.opts.marks, &labels, fee);
        let Some(first) = results.first() else {
            return "strategies: label rule pairs NAME in and NAME out (or buy/sell), e.g. --mark 'cross_above(ema9, ema21) => cross in' --mark 'cross_below(ema9, ema21) => cross out'".into();
        };
        let [all, a, b] = first.hold;
        let mut lines = vec![format!(
            "{} on {} · holding {all:+.2}% (first 70% {a:+.2}%, last 30% {b:+.2}%) · fee {fee}% a side",
            plural(results.len(), "strategy", "strategies"),
            first.on,
        )];
        for r in &results {
            let best = r.best.as_ref().map_or(String::new(), |b| {
                format!(
                    " · best on the first 70%: {} {} → last 30% {}",
                    b.rules.describe(),
                    trades_brief(&b.picked_on),
                    trades_brief(&b.after)
                )
            });
            lines.push(format!(
                "{}: long {} · short {}{best}",
                r.name,
                trades_brief(&r.long),
                trades_brief(&r.short)
            ));
        }
        lines.join("\n")
    }

    /// `sweep(fee)`: the strategies again for every combination of the settings the
    /// lines and rules use (not the source's: those would fetch other data).
    fn sweep(&mut self, fee: f64) -> Result<String, String> {
        let raw = self.current_cli();
        let fetches = format!(
            "{} {}",
            raw.cmd.clone().unwrap_or_default(),
            raw.input
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
        let all = crate::params::parse_all(&raw.param);
        let free: Vec<&crate::params::Param> = all
            .iter()
            .filter(|p| !fetches.contains(&format!("{{{}}}", p.name)))
            .collect();
        if free.is_empty() {
            return Err("sweep: give the lines settings to try, e.g. --param 'fast = 9 | 12 | 20' and ema(values(\"close\"), {fast})".into());
        }
        // Every combination (each setting's choices), up to a few hundred.
        let mut combos: Vec<Vec<usize>> = vec![Vec::new()];
        for p in &free {
            combos = combos
                .into_iter()
                .flat_map(|c| {
                    (0..p.choices.len()).map(move |k| {
                        let mut c = c.clone();
                        c.push(k);
                        c
                    })
                })
                .collect();
        }
        if combos.len() > 400 {
            return Err(format!(
                "sweep: {} combinations is too many (400 at most) — fewer choices",
                combos.len()
            ));
        }
        let base = self.session.base_dataset();
        let host = &self.session.script;
        let binary = self.cli.binary_units;
        // What holding returned (the same for every setting).
        let mut hold: Option<(String, [f64; 3])> = None;
        // strategy → its best (settings, way to trade) so far, and every result.
        type Found = (String, crate::backtest::Best, Vec<f64>);
        let mut by_name: Vec<(String, Found)> = Vec::new();
        for combo in &combos {
            let mut params = all.clone();
            for (p, &k) in free.iter().zip(combo) {
                if let Some(q) = params.iter_mut().find(|q| q.name == p.name) {
                    q.current = k;
                }
            }
            let fill = |t: &String| crate::params::fill(t, &params);
            let lines: Vec<crate::lines::CustomLine> = raw
                .line
                .iter()
                .filter_map(|l| crate::lines::CustomLine::parse(&fill(l)).ok())
                .collect();
            let rules: Vec<crate::marks::MarkRule> = raw
                .mark
                .iter()
                .filter_map(|m| crate::marks::MarkRule::parse(&fill(m)).ok())
                .collect();
            let mut ds = base.clone();
            crate::lines::apply(host, &lines, &mut ds, binary);
            let (marks, _) = crate::marks::evaluate(host, &rules, &ds);
            let labels: Vec<String> = rules
                .iter()
                .map(|r| r.label.clone().unwrap_or_else(|| r.expr.clone()))
                .collect();
            let setting = free
                .iter()
                .zip(combo)
                .map(|(p, &k)| format!("{}={}", p.name, p.choices[k]))
                .collect::<Vec<_>>()
                .join(" ");
            for r in strategy_results(&ds, &marks, &labels, fee) {
                hold = Some((r.on.clone(), r.hold));
                let Some(b) = r.best else { continue };
                match by_name.iter_mut().find(|f| f.0 == r.name) {
                    Some((_, (set, kept, every))) => {
                        every.extend(b.all.iter().copied());
                        if b.picked_on.total() > kept.picked_on.total() {
                            *set = setting.clone();
                            *kept = b;
                        }
                    }
                    None => {
                        let every = b.all.clone();
                        by_name.push((r.name, (setting.clone(), b, every)));
                    }
                }
            }
        }
        // The lines and rules ran on other data: put the chart's back.
        self.rebuild(true);
        if by_name.is_empty() {
            return Err("sweep: no strategies — label rule pairs NAME in and NAME out".into());
        }
        let (on, [h_all, h_a, h_b]) = hold.unwrap_or_default();
        // Ranked by how the pick did on the part it wasn't picked on.
        by_name.sort_by(|a, b| b.1.1.after.total().total_cmp(&a.1.1.after.total()));
        let ways = crate::backtest::STOPS.len() * crate::backtest::TAKES.len() * 2;
        let mut out = vec![format!(
            "sweep: {} × {} × {ways} ways to trade (long/short, SL, TP) on {on} · picked on the first 70%, checked on the last 30% · holding {h_all:+.2}% (first 70% {h_a:+.2}%, last 30% {h_b:+.2}%) · fee {fee}% a side",
            plural(combos.len(), "setting", "settings"),
            plural(by_name.len(), "strategy", "strategies"),
        )];
        for (name, (setting, b, mut every)) in by_name {
            every.sort_by(f64::total_cmp);
            let median = every.get(every.len() / 2).copied().unwrap_or(0.0);
            out.push(format!(
                "{name}: {} with {setting} → first 70% {} · last 30% {} · median of all {median:+.2}%",
                b.rules.describe(),
                trades_brief(&b.picked_on),
                trades_brief(&b.after),
            ));
        }
        Ok(out.join("\n"))
    }

    /// `strategy(entry, exit, series)`: trades from one rule's markers to another's.
    fn strategy(
        &self,
        entry: &crate::backtest::RuleRef,
        exit: &crate::backtest::RuleRef,
        series: Option<&str>,
    ) -> Result<String, String> {
        let (in_name, ins) = self.rule_marks(entry)?;
        let (out_name, outs) = self.rule_marks(exit)?;
        let s = crate::backtest::series_for(&self.ds, series, &ins).ok_or_else(|| {
            "say which series to trade, e.g. strategy(1, 2, \"close\")".to_string()
        })?;
        Ok(crate::backtest::trades(&ins, &outs, s, 0.0).summary(&in_name, &out_name, &s.name))
    }

    /// Open the console on a rule; running it replaces the rule.
    fn edit_rule(&mut self, i: usize) {
        let Some(rule) = self.mark_rules.get(i) else {
            return;
        };
        let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let code = match &rule.label {
            Some(l) => format!("mark_when({}, {})", q(&rule.expr), q(l)),
            None => format!("mark_when({})", q(&rule.expr)),
        };
        self.marks_menu = None;
        self.editing_rule = Some(i);
        self.console = Some(TextInput::new(code));
        self.say("edit the rule · Enter saves · Esc cancels", false);
    }

    fn delete_mark_row(&mut self, row: usize) {
        if row < self.mark_rules.len() {
            self.mark_rules.remove(row);
            self.shift_editing_rule(row);
        } else if row - self.mark_rules.len() < self.manual_marks.len() {
            self.manual_marks.remove(row - self.mark_rules.len());
        }
        self.update_marks();
        let last = self.mark_rows().saturating_sub(1);
        if let Some(sel) = self.marks_menu.as_mut() {
            *sel = (*sel).min(last);
        }
    }

    fn mark_action(&mut self, a: MarkAction) {
        match a {
            MarkAction::AddRule => {
                self.marks_menu = None;
                self.editing_rule = None;
                self.console = Some(TextInput::new("mark_when(\""));
                self.say(
                    "e.g. mark_when(\"delta < -1GB\", \"dropped {delta}\") · value prev delta pct rate time name",
                    false,
                );
            }
            MarkAction::MarkNow => {
                self.marks_menu = None;
                self.editing_rule = None;
                self.console = Some(TextInput::new("mark(\""));
                self.say("label the marker, e.g. mark(\"router reboot\") (at the cursor, else the newest data)", false);
            }
            MarkAction::ClearAll => {
                self.editing_rule = None;
                self.mark_rules.clear();
                self.manual_marks.clear();
                self.update_marks();
            }
            MarkAction::Close => self.marks_menu = None,
        }
    }

    fn on_marks_menu_key(&mut self, key: KeyEvent) {
        let n = self.mark_rows();
        let sel = self.marks_menu.unwrap_or(0);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.marks_menu = Some((sel + 1) % n),
            KeyCode::Up | KeyCode::Char('k') if n > 0 => self.marks_menu = Some((sel + n - 1) % n),
            KeyCode::Enter | KeyCode::Char('e') if sel < self.mark_rules.len() => {
                self.edit_rule(sel)
            }
            KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace if sel < n => {
                self.delete_mark_row(sel)
            }
            KeyCode::Char('a') => self.mark_action(MarkAction::AddRule),
            KeyCode::Char('n') => self.mark_action(MarkAction::MarkNow),
            _ => self.marks_menu = None,
        }
    }

    fn draw_marks_menu(&self, f: &mut Frame) {
        let Some(sel) = self.marks_menu else {
            return;
        };
        let area = f.area();
        let w = 96.min(area.width);
        // A rule takes two lines (itself, then its buttons); a marker by hand one.
        let rows = (self.mark_rules.len() * 2 + self.manual_marks.len()).max(1) as u16;
        let h = (rows + 9).min(area.height);
        let rect = Rect {
            x: (area.width - w) / 2,
            y: area.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        };
        Clear.render(rect, f.buffer_mut());
        let block = Block::bordered()
            .title(" markers · tap a rule to edit it, its buttons to change its look ")
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.hits.push(rect, Hit::MenuBackground);
        let dim = Style::default().fg(Color::Gray);
        let mut y = inner.y;
        let line = |f: &mut Frame, y: u16, text: Line| {
            if y < inner.bottom() {
                f.render_widget(
                    text,
                    Rect {
                        y,
                        height: 1,
                        ..inner
                    },
                );
            }
        };
        let delete = |f: &mut Frame, y: u16, hit: Hit| {
            if y < inner.bottom() && inner.width > 4 {
                let r = Rect {
                    x: inner.right() - 3,
                    y,
                    width: 3,
                    height: 1,
                };
                f.render_widget(
                    Span::styled(" ✕ ", Style::default().fg(Color::White).bg(Color::DarkGray)),
                    r,
                );
                self.hits.push(r, hit);
            }
        };
        line(
            f,
            y,
            Line::styled(
                " a rule is checked at every point: value prev delta pct rate time name,",
                dim,
            ),
        );
        line(
            f,
            y + 1,
            Line::styled(
                " other series by name (ema30, ema30_prev) · notify: tell me about new ones",
                dim,
            ),
        );
        y += 2;
        if self.mark_rules.is_empty() && self.manual_marks.is_empty() {
            line(
                f,
                y,
                Line::styled(" no markers yet: add a rule, or mark the moment", dim),
            );
            y += 1;
        }
        for (i, rule) in self.mark_rules.iter().enumerate() {
            let count = self.opts.marks.iter().filter(|m| m.rule == Some(i)).count();
            let style = if sel == i {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let r = Rect {
                y,
                height: 1,
                width: inner.width.saturating_sub(4),
                ..inner
            };
            if y < inner.bottom() {
                let shown = MarkRuleView(rule).text();
                f.render_widget(
                    Line::from(vec![
                        Span::styled(" ✎ ", Style::default().fg(Color::Yellow)),
                        Span::styled(shown, style),
                    ]),
                    r,
                );
                self.hits.push(r, Hit::EditRule(i));
            }
            delete(f, y, Hit::DeleteRule(i));
            y += 1;
            // Its look: symbol, colour, effect (fixed widths so rows line up), notify.
            if y < inner.bottom() {
                let effect = match rule.effects.as_slice() {
                    [] => "plain".to_string(),
                    [one] => one.clone(),
                    [one, ..] => format!("{one}+"),
                };
                let b = Rect {
                    x: inner.x + 3,
                    y,
                    width: inner.width.saturating_sub(3),
                    height: 1,
                };
                render_buttons(
                    &[
                        Button::new(rule.symbol(), Hit::CycleMarkSymbol(i)),
                        Button::new(
                            format!("{:<7}", rule.color.as_deref().unwrap_or("auto")),
                            Hit::CycleMarkColor(i),
                        ),
                        Button::new(format!("{effect:<9}"), Hit::CycleMarkEffect(i)),
                        Button::new("notify", Hit::ToggleMarkNotify(i)).active(rule.notify),
                    ],
                    b,
                    f.buffer_mut(),
                    &self.hits,
                );
                if inner.width > 60 {
                    f.render_widget(
                        Span::styled(format!("▼ {count} placed"), dim),
                        Rect {
                            x: inner.right().saturating_sub(16),
                            y,
                            width: 14,
                            height: 1,
                        },
                    );
                }
            }
            y += 1;
        }
        for (j, m) in self.manual_marks.iter().enumerate() {
            let style = if sel == self.mark_rules.len() + j {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            line(
                f,
                y,
                Line::from(vec![
                    Span::styled(" ▼ ", Style::default().fg(Color::Yellow)),
                    Span::styled(format!("{}  {}", self.ds.fmt_x_full(m.x), m.label), style),
                    Span::styled("  by hand", dim),
                ]),
            );
            delete(f, y, Hit::DeleteManual(j));
            y += 1;
        }
        y += 1;
        if y < inner.bottom() {
            let buttons = vec![
                Button::new("+ add rule", Hit::MarkAction(MarkAction::AddRule)),
                Button::new("mark now", Hit::MarkAction(MarkAction::MarkNow)),
                Button::new("clear all", Hit::MarkAction(MarkAction::ClearAll)),
                Button::new("done", Hit::MarkAction(MarkAction::Close)),
            ];
            render_buttons(
                &buttons,
                Rect {
                    y,
                    height: inner.bottom() - y,
                    ..inner
                },
                f.buffer_mut(),
                &self.hits,
            );
        }
        line(
            f,
            inner.bottom().saturating_sub(1),
            Line::styled(
                "keys: ↑↓ pick · Enter edit · x delete · a add rule · n mark now · Esc",
                Style::default().fg(Color::DarkGray),
            ),
        );
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
        if let Some(t) = self.tutorial.as_mut() {
            t.copied = true;
        }
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
            PaneOp::Done => return self.close_pane_menu(),
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
            PaneOp::Source => {
                self.ensure_panes();
                return self.open_source_menu();
            }
            _ => {}
        }
        let names = self.pane_series_names();
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
            PaneOp::Done | PaneOp::Single | PaneOp::Lines | PaneOp::Source => None,
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
            KeyCode::Char('d') => PaneOp::Source,
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

    /// Series the picked pane can show: from its own source, or the main chart.
    fn pane_series_names(&self) -> Vec<String> {
        let ds = match self.focused_pane().and_then(|p| p.source.as_ref()) {
            Some(src) => self
                .sub_views
                .iter()
                .find(|v| &v.name == src)
                .map(|v| &v.ds),
            None => Some(&self.ds),
        };
        ds.map(|d| d.series.iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Give the picked pane a source (`None`: the main chart's data).
    fn set_pane_source(&mut self, source: Option<String>) {
        let p = self.ensure_panes();
        let f = p.focus;
        let pane = &mut p.panes[f];
        if pane.source != source {
            pane.source = source;
            // Series names belong to the old source.
            pane.series.clear();
        }
        self.source_menu = None;
        self.tick_sources(true);
    }

    fn open_source_menu(&mut self) {
        let title = crate::params::fill(&self.opts.title, &self.params_of(ParamTarget::Main));
        let mut items = vec![(format!("this chart's data · {title}"), None)];
        for p in crate::profile::presets() {
            if p.available() && !items.iter().any(|i| i.1.as_deref() == Some(p.name)) {
                items.push((
                    format!("{} · {}", p.title, p.description),
                    Some(p.name.to_string()),
                ));
            }
        }
        for (name, _) in crate::profile::saved() {
            items.push((format!("{name} · saved"), Some(name)));
        }
        let now = chrono::Local::now().timestamp();
        for (i, r) in crate::recent::list().iter().take(5).enumerate() {
            items.push((
                format!("{} · recent, {}", r.title(), r.summary(now)),
                Some(format!("@{}", i + 1)),
            ));
        }
        let current = self.focused_pane().and_then(|p| p.source.clone());
        let sel = items.iter().position(|i| i.1 == current).unwrap_or(0);
        self.source_menu = Some((items, sel));
    }

    fn on_source_menu_key(&mut self, key: KeyEvent) {
        let Some((items, sel)) = self.source_menu.as_mut() else {
            return;
        };
        let n = items.len();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1) % n,
            KeyCode::Up | KeyCode::Char('k') => *sel = (*sel + n - 1) % n,
            KeyCode::Enter => {
                let src = items[*sel].1.clone();
                self.set_pane_source(src);
            }
            _ => self.source_menu = None,
        }
    }

    fn draw_source_menu(&self, f: &mut Frame) {
        let Some((items, sel)) = &self.source_menu else {
            return;
        };
        let area = f.area();
        let w = 70.min(area.width);
        let h = (items.len() as u16 + 2).min(area.height);
        let rect = Rect {
            x: (area.width - w) / 2,
            y: area.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        };
        Clear.render(rect, f.buffer_mut());
        let block = Block::bordered()
            .title(" data for this pane · tap one ")
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.hits.push(rect, Hit::MenuBackground);
        // Scroll so the selection stays visible.
        let rows = inner.height as usize;
        let first = sel.saturating_sub(rows.saturating_sub(1));
        for (row, (i, (label, _))) in items.iter().enumerate().skip(first).take(rows).enumerate() {
            let r = Rect {
                y: inner.y + row as u16,
                height: 1,
                ..inner
            };
            let style = if i == *sel {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            f.render_widget(Span::styled(format!(" {label}"), style), r);
            self.hits.push(r, Hit::PickSource(i));
        }
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
        v.push(Item::Label("source".into()));
        v.push(b(
            &pane
                .source
                .clone()
                .map_or_else(|| "this chart ▾".to_string(), |s| format!("{s} ▾")),
            PaneOp::Source,
        ));
        v.push(Item::Label("chart".into()));
        v.push(b(pane.kind.unwrap_or(self.opts.kind).name(), PaneOp::Kind));
        v.push(Item::Label("lines".into()));
        v.push(b(&self.cur_lines().label(), PaneOp::Lines));
        v.push(Item::Label("series".into()));
        for (i, s) in self.pane_series_names().iter().enumerate() {
            let on = pane.shows(s);
            v.push(Item::Btn(
                Button::new(
                    format!("{} {}", if on { "●" } else { "○" }, s),
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
                "keys: v s split · x close · ←→↑↓ pick · shift move · ctrl or < > + - size · d source · t chart · T lines · 1-9 series · Esc",
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
            PaneAction::Source(src) => {
                let src = src.trim();
                let src = (!src.is_empty() && src != "main").then(|| src.to_string());
                if let Some(name) = &src
                    && let Err(e) =
                        crate::sources::cli_for(name, self.cli.ssh.as_deref(), &self.cli.source)
                {
                    return self.say(format!("pane_source: {e:#}"), true);
                }
                return self.set_pane_source(src);
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
        // From panes: side by side for now, and the panes come back when it's off.
        if let Some(p) = self.opts.panes.take() {
            self.pane_menu = false;
            self.panes_before_side_by_side = Some(p);
            self.opts.side_by_side = true;
            return;
        }
        if self.opts.side_by_side
            && let Some(p) = self.panes_before_side_by_side.take()
        {
            self.opts.side_by_side = false;
            self.opts.panes = Some(p);
            return;
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
        self.popup_rect.set(Some(rect));
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
        let custom_rows = self.session.custom.len() as u16 + 2;
        let h = (rows.len() as u16 + 7 + custom_rows + u16::from(self.opts.side_by_side))
            .min(area.height);
        let rect = Rect {
            x: (area.width - w) / 2,
            y: (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        Clear.render(rect, f.buffer_mut());
        self.popup_rect.set(Some(rect));
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
        // Custom lines (Rhai), with edit and delete.
        y += 1;
        if y < inner.bottom() {
            let x = inner.x;
            f.buffer_mut().set_stringn(
                x,
                y,
                "custom lines (Rhai):",
                inner.width as usize,
                Style::default().fg(Color::Gray),
            );
            let label_w = 21;
            if inner.width > label_w + 16 {
                render_buttons(
                    &[Button::new("+ custom line", Hit::LineAction(None))],
                    Rect {
                        x: x + label_w,
                        y,
                        width: 16,
                        height: 1,
                    },
                    f.buffer_mut(),
                    &self.hits,
                );
            }
            y += 1;
        }
        for (i, l) in self.session.custom.iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }
            let r = Rect {
                y,
                height: 1,
                width: inner.width.saturating_sub(4),
                ..inner
            };
            f.render_widget(
                Line::from(vec![
                    Span::styled(" ✎ ", Style::default().fg(Color::Yellow)),
                    Span::raw(l.to_spec()),
                ]),
                r,
            );
            self.hits.push(r, Hit::LineAction(Some(i)));
            let del = Rect {
                x: inner.right().saturating_sub(3),
                y,
                width: 3,
                height: 1,
            };
            f.render_widget(
                Span::styled(" ✕ ", Style::default().fg(Color::White).bg(Color::DarkGray)),
                del,
            );
            self.hits.push(del, Hit::DeleteLine(i));
            y += 1;
        }
        y += 1;
        if y < inner.bottom() {
            let pts = self.opts.window_for(n.max(1));
            let span = self
                .window_span(pts)
                .map(|d| format!(" (≈{})", crate::time::fmt_duration(d)))
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
        match self.opts.forecast {
            Some(_) if self.view.is_none() => self.say(
                "forecast on: dotted lines project each trend (set targets with :target(\"name\", 50GB))",
                false,
            ),
            Some(_) => self.say("forecast on", false),
            // Say so: the "on" message would otherwise still be showing.
            None => self.say("forecast off", false),
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
            // A tap outside a measured stretch lets go of it.
            if self.selection.is_some_and(|(a, b)| x < a || x > b) {
                self.selection = None;
                self.say("range cleared", false);
            }
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
            Cmd::Discover if self.lab.is_some() => self.stop_discover(),
            Cmd::Discover => self.start_discover(0.0, None),
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
            Cmd::Targets if !self.has_targets() => self.say(
                "no targets set: add one with :target(\"name\", 50GB) or --target name=50GB",
                false,
            ),
            Cmd::Targets => self.opts.show_targets = !self.opts.show_targets,
            Cmd::RangeStep(dir) => self.step_timeframe(dir),
            Cmd::Copy => self.copy(None),
            Cmd::Undo => return self.step_back(),
            Cmd::Alerts => self.alerts_menu = !self.alerts_menu,
            Cmd::Sources => self.sources_menu = !self.sources_menu,
            Cmd::Legend => {
                self.show_legend = !self.show_legend;
                self.say(
                    if self.show_legend {
                        "legends on"
                    } else {
                        "legends off (i brings them back)"
                    },
                    false,
                );
            }
            Cmd::Markers => {
                self.marks_menu = match self.marks_menu {
                    Some(_) => None,
                    None => Some(0),
                };
            }
            Cmd::Save => {
                self.console = Some(TextInput::new("save(\""));
                self.say(
                    "name this view, e.g. save(\"disks\") — then graft -p disks opens it",
                    false,
                );
            }
            Cmd::Panes => {
                if self.pane_menu {
                    self.close_pane_menu();
                } else {
                    self.pane_menu = true;
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
                        "mouse off: terminal text selection works; m turns it back on"
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
            // Every command, above an empty console to type one in.
            self.console_help = true;
            self.console = Some(TextInput::default());
            return;
        }
        let script = self.session.script.clone();
        let binds_before = script.binds().len();
        self.console_note = None;
        self.error_at = None;
        script.set_data(self.full.clone(), self.focus());
        script.set_cursor(self.cursor);
        let result = script.eval(code);
        let printed = script.take_output();
        self.message = None;
        for action in script.take_actions() {
            self.apply(action);
        }
        // bind() from the console works straight away, and stays in the command line.
        for (k, code) in script.binds().into_iter().skip(binds_before) {
            if let Some(c) = k.chars().next() {
                self.binds.insert(c, code.clone());
                self.cli
                    .bind
                    .retain(|b| crate::cli::split_bind(b).map_or(true, |(bk, _)| bk != c));
                self.cli.bind.push(format!("{c} = {code}"));
            }
        }
        if self.session.script.has_new_mark_rules() {
            self.update_marks();
        }
        // A command that answered in the console itself (backtest): that's the output.
        if self.console_note.is_some() && result.is_ok() {
            return;
        }
        // A command that complained (e.g. an unknown line name): its message is the
        // console's output.
        if let Some(m) = self.message.take_if(|m| m.error) {
            let names: Vec<String> = self.full.series.iter().map(|s| s.name.clone()).collect();
            let (text, _) = crate::script::explain_error(&m.text, code, &names);
            self.note(text, true);
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
                    text.push_str(&self.describe_result(code, &v));
                }
                self.note(text, false);
            }
            Err(e) => {
                let names: Vec<String> = self.full.series.iter().map(|s| s.name.clone()).collect();
                let (text, at) = crate::script::explain_error(&e.to_string(), code, &names);
                self.error_at = at;
                self.note(text, true);
            }
        }
    }

    /// Show console output above the status line.
    fn note(&mut self, text: String, error: bool) {
        self.console_note = Some(Message {
            text,
            error,
            at: Instant::now(),
        });
    }

    // ───────────── toolbar and legend row ─────────────

    /// The chart options as the viewer draws them: the legend is the row below the
    /// chart, and titles stay short.
    fn view_opts(&self) -> ChartOpts {
        let mut o = self.opts.clone();
        o.tidy = true;
        // Titles can say a source's settings: --title '{coin} {interval}'.
        if o.title.contains('{') {
            o.title = crate::params::fill(&o.title, &self.params_of(ParamTarget::Main));
        }
        if let Some(p) = o.panes.as_mut() {
            for (i, pane) in p.panes.iter_mut().enumerate() {
                if let Some(t) = pane.title.as_mut().filter(|t| t.contains('{')) {
                    let own = self.params_of(ParamTarget::Pane(i));
                    let main = self.params_of(ParamTarget::Main);
                    *t = crate::params::fill(&crate::params::fill(t, &own), &main);
                }
            }
        }
        // With several panes the legend row only covers the picked one: each pane
        // keeps its own legend box (unless legends are off).
        if !self.show_legend || self.opts.panes.as_ref().is_none_or(|p| p.count() <= 1) {
            o.legend = crate::cli::LegendPos::None;
        }
        o
    }

    /// The short toolbar: what's used most, zoom controls only while zoomed in,
    /// everything else under ⋯.
    fn toolbar(&self) -> Vec<Button<Hit>> {
        let full = self.toolbar_with(true);
        let pick = |c: Cmd| full.iter().find(|b| b.action == Hit::Cmd(c)).cloned();
        let mut v: Vec<Button<Hit>> = Vec::new();
        if self.view.is_some() {
            v.extend(
                [Cmd::PanLeft, Cmd::PanRight, Cmd::Reset]
                    .into_iter()
                    .filter_map(pick),
            );
        }
        v.extend(
            [
                Cmd::Undo,
                Cmd::Trend,
                Cmd::RangeMenu,
                Cmd::Stats,
                Cmd::Panes,
                Cmd::Sources,
            ]
            .into_iter()
            .filter_map(pick),
        );
        // The main source's settings, as dropdowns: interval: 1h ▾
        for (k, p) in self.params_of(ParamTarget::Main).iter().enumerate() {
            let at = v
                .iter()
                .position(|b| b.action == Hit::Cmd(Cmd::RangeMenu))
                .map_or(v.len(), |i| i + 1 + k);
            v.insert(
                at.min(v.len()),
                Button::new(
                    format!("{}: {} ▾", p.name, p.value()),
                    Hit::ParamMenu(ParamTarget::Main, k),
                )
                .active(self.param_menu == Some((ParamTarget::Main, k))),
            );
        }
        // Markers and alerts, once there are some.
        if self.mark_rows() > 0 {
            v.extend(pick(Cmd::Markers));
        }
        if !self.alerts.is_empty() {
            v.extend(pick(Cmd::Alerts));
        }
        if self.lab.is_some() {
            v.extend(pick(Cmd::Discover));
        }
        if self.toolbar_more {
            for b in &full {
                if b.action != Hit::Cmd(Cmd::More) && !v.iter().any(|x| x.action == b.action) {
                    v.push(b.clone());
                }
            }
        }
        v.push(
            Button::new(
                if self.toolbar_more { "less" } else { "⋯" },
                Hit::Cmd(Cmd::More),
            )
            .active(self.toolbar_more),
        );
        v
    }

    /// The series as the legend shows them: (chip index, name, colour, value, shown).
    #[allow(clippy::type_complexity)]
    fn legend_items(&self) -> Vec<(Hit, String, Color, Option<String>, bool)> {
        let ds = match self.focused_pane().and_then(|p| p.source.as_ref()) {
            Some(src) => self
                .sub_views
                .iter()
                .find(|v| &v.name == src)
                .map_or(&self.ds, |v| &v.ds),
            None => &self.ds,
        };
        let value_of = |name: &str| {
            ds.series.iter().find(|s| s.name == name).and_then(|s| {
                let y = match self.cursor {
                    Some(c) => value_at(s, &self.opts, c).map(|v| v.0),
                    None => s.points.last().map(|p| p.1),
                }?;
                Some((y, s.unit.format(y)))
            })
        };
        self.chips()
            .into_iter()
            .map(|chip| {
                let name = chip.label.split_once(' ').map_or("", |p| p.1).to_string();
                match chip.action {
                    // The candle: its close, green or red against its open.
                    Hit::Candles => {
                        let c = self.candle_series();
                        let (open, close) = c
                            .as_ref()
                            .map_or((None, None), |c| (value_of(&c[0]), value_of(&c[3])));
                        let color = match (&open, &close) {
                            (Some(o), Some(c)) if c.0 < o.0 => Color::Red,
                            _ => Color::Green,
                        };
                        (Hit::Candles, name, color, close.map(|c| c.1), chip.active)
                    }
                    Hit::Series(i) => (
                        Hit::Series(i),
                        name.clone(),
                        self.opts.color(i),
                        value_of(&name).map(|v| v.1),
                        chip.active,
                    ),
                    other => (other, name, Color::Gray, None, chip.active),
                }
            })
            .collect()
    }

    /// The legend row's pieces: groups of (text, style) kept on one line, each with
    /// what tapping it does.
    #[allow(clippy::type_complexity)]
    fn legend_groups(&self) -> Vec<(Vec<(String, Style)>, Option<Hit>)> {
        let mut g: Vec<(Vec<(String, Style)>, Option<Hit>)> = Vec::new();
        let (dot, style) = if self.paused {
            ("⏸", Style::default().fg(Color::Yellow))
        } else if self.live.spec.is_live() && !self.live.eof {
            ("●", Style::default().fg(Color::Green))
        } else {
            ("■", Style::default().fg(Color::DarkGray))
        };
        let mut head = vec![(format!("{dot} "), style)];
        // With the date when the data covers more than a day; else to the second.
        let span = self.ds.x_range().map_or(0.0, |r| r.1 - r.0);
        let when = |x: f64| {
            if span > 86_400.0 {
                self.ds.fmt_x_full(x)
            } else {
                self.ds.fmt_x(x, 1.0)
            }
        };
        let time = match self.cursor {
            Some(c) => Some((when(crate::render::round_x(&self.ds, c)), Color::Yellow)),
            None => self.ds.x_range().map(|r| (when(r.1), Color::Gray)),
        };
        if self.cursor.is_some() {
            // Not the newest data: say so.
            head.push(("cursor ".into(), Style::default().fg(Color::Yellow)));
        }
        if let Some((t, c)) = time {
            head.push((t, Style::default().fg(c).add_modifier(Modifier::BOLD)));
        }
        g.push((head, None));
        // Lines are arriving but the format matches none of them: say so, rather than
        // leaving an empty chart.
        if self.session.matched == 0 && self.session.lines > 0 {
            g.push((
                vec![(
                    format!(
                        " {} lines read, none matched the format — press F to fix it ",
                        self.session.lines
                    ),
                    Style::default().fg(Color::Black).bg(Color::Yellow),
                )],
                Some(Hit::Cmd(Cmd::Format)),
            ));
        }
        if !self.firing.is_empty() {
            // One alert: what it is; several: how many (the alerts menu lists them).
            let text = match self.firing.as_slice() {
                [one] => format!(" ALERT {one} "),
                all => format!(" ALERT ({}) ", all.len()),
            };
            g.push((
                vec![(
                    text,
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                )],
                Some(Hit::Cmd(Cmd::Alerts)),
            ));
        }
        for (hit, name, color, value, shown) in
            self.legend_items().into_iter().filter(|_| self.show_legend)
        {
            let mut parts = vec![(
                if shown { "● " } else { "○ " }.to_string(),
                Style::default().fg(color),
            )];
            parts.push((
                name,
                if shown {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ));
            if shown && let Some(v) = value {
                parts.push((format!(" {v}"), Style::default().fg(color)));
            }
            g.push((parts, Some(hit)));
        }
        // Target lines, with what they're at.
        if self.opts.show_targets {
            for (i, s) in self.ds.series.iter().enumerate() {
                if self.hidden.contains(&s.name) {
                    continue;
                }
                if let Some(t) = self.opts.analysis.targets.explicit(&s.name) {
                    g.push((
                        vec![(
                            format!("┄ {} target {}", s.name, s.unit.format(t)),
                            Style::default().fg(self.opts.color(i)),
                        )],
                        Some(Hit::Cmd(Cmd::Targets)),
                    ));
                }
            }
        }
        if !self.mouse {
            g.push((
                vec![(
                    "mouse off · m turns it back on".into(),
                    Style::default().fg(Color::DarkGray),
                )],
                None,
            ));
        }
        g
    }

    /// The latest message, on the chart's bottom edge (right-aligned) so it never
    /// moves anything: errors stay until the next one, others for 8 seconds.
    fn draw_message(&self, f: &mut Frame, chart: Rect) {
        let Some(m) = self
            .message
            .as_ref()
            .filter(|m| m.at.elapsed() < Duration::from_secs(8) || m.error)
        else {
            return;
        };
        if chart.height == 0 || chart.width < 6 {
            return;
        }
        let style = if m.error {
            Style::default().fg(Color::LightRed)
        } else {
            Style::default().fg(Color::Gray)
        };
        let room = chart.width.saturating_sub(4) as usize;
        let mut text: String = format!(" {} ", m.text);
        if unicode_width::UnicodeWidthStr::width(text.as_str()) > room {
            text = text
                .chars()
                .take(room.saturating_sub(2))
                .collect::<String>()
                + "… ";
        }
        let w = unicode_width::UnicodeWidthStr::width(text.as_str()) as u16;
        let x = chart.right().saturating_sub(w + 2);
        f.buffer_mut()
            .set_string(x, chart.bottom() - 1, &text, style);
    }

    /// Where each legend group goes: (row, column) per group, and the rows used.
    fn legend_layout(&self, width: u16) -> (Vec<Option<(u16, u16)>>, u16) {
        let gap = 3;
        let mut out = Vec::new();
        let (mut row, mut x) = (0u16, 0u16);
        for (k, (parts, _)) in self.legend_groups().iter().enumerate() {
            let w: u16 = parts
                .iter()
                .map(|p| unicode_width::UnicodeWidthStr::width(p.0.as_str()) as u16)
                .sum();
            let room = width;
            if x > 0 && x + w > room {
                if row == 2 {
                    out.push(None);
                    continue;
                }
                row += 1;
                x = 0;
            }
            let _ = k;
            out.push(Some((row, x)));
            x += w + gap;
        }
        (out, row + 1)
    }

    /// The legend's rows: only grows (for a width), so values changing length don't
    /// make the chart jump by a row.
    fn legend_height(&self, width: u16) -> u16 {
        let rows = self.legend_layout(width).1;
        let (w, before) = self.legend_rows.get();
        let rows = if w == width { rows.max(before) } else { rows };
        self.legend_rows.set((width, rows));
        rows
    }

    /// Live/paused, the time (newest, or the cursor's), and each series with its
    /// value: tap one to hide it.
    fn draw_legend_row(&self, f: &mut Frame, area: Rect) {
        let (places, _) = self.legend_layout(area.width);
        let buf = f.buffer_mut();
        for ((parts, hit), place) in self.legend_groups().into_iter().zip(places) {
            let Some((row, col)) = place else { continue };
            let y = area.y + row;
            if y >= area.bottom() {
                continue;
            }
            let mut x = area.x + col;
            let start = x;
            for (text, style) in parts {
                let room = area.right().saturating_sub(x) as usize;
                let text: String = text.chars().take(room).collect();
                buf.set_string(x, y, &text, style);
                x += unicode_width::UnicodeWidthStr::width(text.as_str()) as u16;
            }
            if let Some(h) = hit {
                self.hits.push(
                    Rect {
                        x: start,
                        y,
                        width: x - start,
                        height: 1,
                    },
                    h,
                );
            }
        }
    }

    // ───────────── keyboard in popups ─────────────

    /// The popup the arrow keys work in (0: none).
    fn popup_open(&self) -> u8 {
        if self.range_menu {
            1
        } else if self.lines_menu {
            2
        } else if self.sources_menu {
            3
        } else if self.alerts_menu {
            4
        } else if self.param_menu.is_some() {
            5
        } else if self.settings_menu {
            6
        } else {
            0
        }
    }

    /// The open popup's buttons, as drawn: top to bottom, left to right.
    fn popup_buttons(&self) -> Vec<(Rect, Hit)> {
        let Some(area) = self.popup_rect.get() else {
            return Vec::new();
        };
        let mut v: Vec<(Rect, Hit)> = self
            .hits
            .all()
            .into_iter()
            .filter(|(r, h)| *h != Hit::MenuBackground && area.contains((r.x, r.y).into()))
            .collect();
        v.sort_by_key(|(r, _)| (r.y, r.x));
        v.dedup_by_key(|(r, _)| (r.x, r.y));
        v
    }

    /// Arrow keys move the pick to the nearest button that way; Tab to the next.
    fn popup_move(&mut self, key: KeyCode) {
        let items = self.popup_buttons();
        if items.is_empty() {
            return;
        }
        let i = self.popup_focus.get().min(items.len() - 1);
        let cur = items[i].0;
        let mid = |r: Rect| r.x as i32 + r.width as i32 / 2;
        let next = match key {
            KeyCode::Tab => Some((i + 1) % items.len()),
            KeyCode::BackTab => Some((i + items.len() - 1) % items.len()),
            KeyCode::Left => i.checked_sub(1),
            KeyCode::Right => (i + 1 < items.len()).then_some(i + 1),
            KeyCode::Up | KeyCode::Down => {
                // The nearest row that way, then the button closest across.
                let down = key == KeyCode::Down;
                let row = items
                    .iter()
                    .map(|(r, _)| r.y)
                    .filter(|&y| if down { y > cur.y } else { y < cur.y })
                    .reduce(|a, b| if down { a.min(b) } else { a.max(b) });
                row.and_then(|y| {
                    items
                        .iter()
                        .enumerate()
                        .filter(|(_, (r, _))| r.y == y)
                        .min_by_key(|(_, (r, _))| (mid(*r) - mid(cur)).abs())
                        .map(|(k, _)| k)
                })
            }
            _ => None,
        };
        if let Some(n) = next {
            self.popup_focus.set(n);
        }
    }

    /// Enter: press the picked button, as a tap would.
    fn popup_press(&mut self) -> Transition {
        let items = self.popup_buttons();
        let Some((r, _)) = items.get(self.popup_focus.get().min(items.len().saturating_sub(1)))
        else {
            return Transition::None;
        };
        let t = self.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: r.x,
            row: r.y,
            modifiers: KeyModifiers::NONE,
        });
        self.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: r.x,
            row: r.y,
            modifiers: KeyModifiers::NONE,
        });
        t
    }

    /// Show which button the arrow keys have picked.
    fn draw_popup_focus(&self, f: &mut Frame) {
        let kind = self.popup_open();
        if kind != self.popup_kind.get() {
            // Another popup: start from its first button.
            self.popup_kind.set(kind);
            self.popup_focus.set(0);
        }
        if kind == 0 {
            return;
        }
        let items = self.popup_buttons();
        let Some((r, _)) = items.get(self.popup_focus.get().min(items.len().saturating_sub(1)))
        else {
            return;
        };
        let buf = f.buffer_mut();
        for x in r.x..r.right() {
            if let Some(cell) = buf.cell_mut((x, r.y)) {
                cell.set_style(Style::default().fg(Color::Black).bg(Color::Yellow));
            }
        }
    }

    // ───────────── older data ─────────────

    /// The oldest point read so far (before any timeframe cut).
    fn oldest_x(&self) -> Option<f64> {
        self.session
            .store
            .series
            .values()
            .filter_map(|s| s.points.first().map(|p| p.0))
            .min_by(f64::total_cmp)
    }

    /// Scrolled back to the oldest data, and the command asks for a time window
    /// ({from_ms:6h} … {now_ms}): run it once more for the window before that, and add
    /// what comes back (--replace takes care of the overlap).
    fn tick_backfill(&mut self) {
        if let Some((live, from)) = &mut self.backfill {
            let session = &mut self.session;
            let errors = live.pump(Duration::from_millis(40), &mut |line, meta| {
                let _ = session.feed(line, meta);
            });
            let from = *from;
            let done = live.eof;
            if let Some(e) = errors.into_iter().last() {
                self.say(e, true);
            }
            if done {
                self.backfill = None;
                if self.oldest_x().is_none_or(|o| o >= from - 1e-9) {
                    self.backfill_done = true;
                    self.say("no older data from the source", false);
                } else {
                    self.say("older data loaded", false);
                }
                self.rebuild(true);
            }
            return;
        }
        if self.paused {
            return;
        }
        let (Some((a, b)), Some(oldest)) = (self.view, self.oldest_x()) else {
            return;
        };
        // At (or very near) the start of what's loaded.
        if a <= oldest + (b - a) * 0.02 {
            self.start_backfill();
        }
    }

    /// Ask the source for the window before the oldest data, if its command has one
    /// (`{from…}` … `{now…}`). False when it can't (or has nothing older).
    fn start_backfill(&mut self) -> bool {
        if self.backfill.is_some() {
            return true;
        }
        if self.backfill_done {
            return false;
        }
        let crate::source::SourceSpec::Command {
            cmd, every, ssh, ..
        } = &self.live.spec
        else {
            return false;
        };
        if !crate::params::has_window(cmd) {
            return false;
        }
        let Some(oldest) = self.oldest_x() else {
            return false;
        };
        let spec = crate::source::SourceSpec::Command {
            cmd: crate::params::fill_times(cmd, oldest),
            every: *every,
            count: Some(1),
            ssh: ssh.clone(),
        };
        self.backfill = Some((Live::new(spec), oldest));
        self.say("loading older data…", false);
        true
    }

    // ───────────── pane settings menu ─────────────

    /// Open the settings for pane `id` (`None`: the whole chart, or the picked pane).
    fn open_settings(&mut self, id: Option<usize>) {
        if let (Some(id), Some(p)) = (id, self.opts.panes.as_mut())
            && id < p.count()
        {
            p.focus = id;
        }
        self.settings_menu = true;
    }

    /// Whose settings the menu shows: the picked pane's source (if it has one of its
    /// own), else the main source.
    fn settings_target(&self) -> ParamTarget {
        match self.focused_pane() {
            Some(p) if p.source.is_some() => {
                ParamTarget::Pane(self.opts.panes.as_ref().map_or(0, |p| p.focus))
            }
            _ => ParamTarget::Main,
        }
    }

    fn current_kind(&self) -> ChartKind {
        self.focused_pane()
            .and_then(|p| p.kind)
            .unwrap_or(self.opts.kind)
    }

    fn set_kind(&mut self, k: ChartKind) {
        match self.opts.panes.as_mut() {
            Some(p) => {
                let f = p.focus;
                p.panes[f].kind = Some(k);
            }
            None => self.opts.kind = k,
        }
        if k == ChartKind::Candles {
            self.bring_in_ohlc();
        }
    }

    /// Candles need open, high, low and close: put them in the pane (or show them) if
    /// the data has them by name, else say what's missing.
    fn bring_in_ohlc(&mut self) {
        let names = self.chip_names();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let found = crate::render::candle_roles_named(&refs);
        let [Some(o), Some(h), Some(l), Some(c)] = found else {
            if self.candle_series().is_none() {
                self.say(
                    "candles need open, high, low and close series (named open/high/low/close or o/h/l/c)",
                    true,
                );
            }
            return;
        };
        let ohlc = [o, h, l, c].map(|k| names[k].clone());
        for n in &ohlc {
            self.hidden.remove(n);
        }
        if let Some(p) = self.opts.panes.as_mut() {
            let f = p.focus;
            let pane = &mut p.panes[f];
            if !pane.series.is_empty() {
                for n in &ohlc {
                    if !pane.series.contains(n) {
                        pane.series.push(n.clone());
                    }
                }
                pane.series
                    .sort_by_key(|s| names.iter().position(|n| n == s));
            }
        }
    }

    /// One place for everything about a pane (or the chart): its source and the
    /// source's settings, chart type, lines, series, and splitting or closing it.
    fn draw_settings_menu(&self, f: &mut Frame) {
        if !self.settings_menu {
            return;
        }
        let area = f.area();
        let w = 74.min(area.width);
        let label_w = 9u16;
        let inner_w = w.saturating_sub(2);
        let room = inner_w.saturating_sub(label_w);
        let t = self.settings_target();
        let panes = self.opts.panes.as_ref();
        let pane_id = panes.map(|p| p.focus);
        // (label, text after it, buttons)
        let mut rows: Vec<(String, String, Vec<Button<Hit>>)> = Vec::new();
        // The source.
        match (pane_id, self.focused_pane().and_then(|p| p.source.clone())) {
            (Some(id), Some(name)) => rows.push((
                "source".into(),
                format!("{name} "),
                vec![
                    Button::new("edit", Hit::SourcePaneEdit(id)),
                    Button::new("other source…", Hit::SourcePane(id)),
                ],
            )),
            (Some(id), None) => rows.push((
                "source".into(),
                "this chart's data ".into(),
                vec![Button::new("other source…", Hit::SourcePane(id))],
            )),
            (None, _) => rows.push((
                "source".into(),
                format!("{} ", self.live.spec.describe()),
                vec![Button::new("sources…", Hit::Cmd(Cmd::Sources))],
            )),
        }
        // Its settings (--param).
        for (k, p) in self.params_of(t).iter().enumerate() {
            rows.push((
                p.name.clone(),
                String::new(),
                p.choices
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        Button::new(c.clone(), Hit::ParamPick(t, k, i)).active(i == p.current)
                    })
                    .collect(),
            ));
        }
        let kind = self.current_kind();
        rows.push((
            "chart".into(),
            String::new(),
            ChartKind::value_variants()
                .iter()
                .map(|k| Button::new(k.name(), Hit::SetKind(*k)).active(*k == kind))
                .collect(),
        ));
        let lines = self.cur_lines();
        let mut line_buttons =
            vec![Button::new("live", Hit::Layer(None, false)).active(lines.live)];
        for (style, name) in [
            (TrendStyle::Avg, "average"),
            (TrendStyle::Median, "median"),
            (TrendStyle::P95, "p95"),
        ] {
            line_buttons.push(
                Button::new(name, Hit::Layer(Some(style), false))
                    .active(lines.trends.contains(&style)),
            );
        }
        rows.push(("lines".into(), String::new(), line_buttons));
        rows.push((
            "series".into(),
            String::new(),
            self.chips()
                .into_iter()
                .map(|b| {
                    let active = b.active;
                    let name = b
                        .label
                        .split_once(' ')
                        .map_or(b.label.clone(), |p| p.1.to_string());
                    Button::new(name, b.action).active(active)
                })
                .collect(),
        ));
        let mut actions = vec![
            Button::new("split right", Hit::Pane(PaneOp::Split(PSplit::Across))),
            Button::new("split down", Hit::Pane(PaneOp::Split(PSplit::Down))),
        ];
        if panes.is_some_and(|p| p.count() > 1) {
            actions.push(Button::new("close pane", Hit::Pane(PaneOp::Close)));
        }
        actions.push(Button::new("done", Hit::CloseSettings));
        let row_h = |r: &(String, String, Vec<Button<Hit>>)| {
            let text = r.1.chars().count() as u16;
            if r.2.is_empty() {
                1
            } else {
                flow_height(&r.2, room.saturating_sub(text).max(10)).max(1)
            }
        };
        let body: u16 = rows.iter().map(row_h).sum::<u16>() + flow_height(&actions, inner_w) + 2;
        let h = (body + 2).min(area.height);
        let rect = Rect {
            x: (area.width - w) / 2,
            y: area.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        };
        Clear.render(rect, f.buffer_mut());
        self.popup_rect.set(Some(rect));
        let title = match pane_id {
            Some(id) => format!(" pane {} settings ", id + 1),
            None => " chart settings ".to_string(),
        };
        let block = Block::bordered()
            .title(title)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.hits.push(rect, Hit::MenuBackground);
        let head = Style::default().fg(Color::Yellow);
        let mut y = inner.y;
        for r in &rows {
            if y >= inner.bottom() {
                break;
            }
            let h = row_h(r);
            f.buffer_mut().set_string(inner.x, y, &r.0, head);
            let text_w = r.1.chars().count() as u16;
            f.buffer_mut()
                .set_string(inner.x + label_w, y, &r.1, Style::default().fg(Color::Gray));
            render_buttons(
                &r.2,
                Rect {
                    x: inner.x + label_w + text_w,
                    y,
                    width: room
                        .saturating_sub(text_w)
                        .max(10)
                        .min(inner.width.saturating_sub(label_w + text_w)),
                    height: h.min(inner.bottom() - y),
                },
                f.buffer_mut(),
                &self.hits,
            );
            y += h;
        }
        y += 1;
        if y < inner.bottom() {
            render_buttons(
                &actions,
                Rect {
                    y,
                    height: inner.bottom() - y,
                    ..inner
                },
                f.buffer_mut(),
                &self.hits,
            );
        }
        if inner.height > 0 {
            f.buffer_mut().set_string(
                inner.x,
                inner.bottom() - 1,
                "right-click or ⚙ opens this · arrows + Enter · Esc closes",
                Style::default().fg(Color::DarkGray),
            );
        }
    }

    /// A ⚙ at the end of each pane's title (or the chart's): tap it for the settings.
    fn draw_settings_buttons(&self, f: &mut Frame, chart: Rect) {
        let mut spots: Vec<(Rect, Option<usize>)> = match &self.opts.panes {
            Some(p) if p.count() > 1 => p
                .rects(chart)
                .into_iter()
                .map(|(id, r)| (r, Some(id)))
                .collect(),
            _ => vec![(chart, None)],
        };
        spots.retain(|(r, _)| r.width > 8 && r.height > 2);
        for (r, id) in spots {
            let at = Rect {
                x: r.right() - 4,
                y: r.y,
                width: 3,
                height: 1,
            };
            f.buffer_mut()
                .set_string(at.x, at.y, " ⚙ ", Style::default().fg(Color::Gray));
            self.hits.push(at, Hit::PaneSettings(id));
        }
    }

    // ───────────── source parameters ─────────────

    /// The parameters of a source: the main one, a merged one, or a pane's own.
    fn params_of(&self, t: ParamTarget) -> Vec<crate::params::Param> {
        let specs = match t {
            ParamTarget::Main => self.cli.param.clone(),
            ParamTarget::Merge(i) => self
                .cli
                .merge
                .get(i)
                .and_then(|s| crate::sources::Merge::parse(s).ok())
                .map(|m| m.cli().param)
                .unwrap_or_default(),
            ParamTarget::Pane(id) => self
                .opts
                .panes
                .as_ref()
                .and_then(|p| p.panes.get(id))
                .and_then(|p| p.source.clone())
                .and_then(|name| {
                    crate::sources::cli_for(&name, self.cli.ssh.as_deref(), &self.cli.source).ok()
                })
                .map(|c| c.param)
                .unwrap_or_default(),
        };
        crate::params::parse_all(&specs)
    }

    /// Set a source's parameter and run it again with the new value.
    fn set_param(&mut self, t: ParamTarget, param: &str, value: &str) -> Result<(), String> {
        let args = |c: &Cli| {
            c.to_args()
                .iter()
                .map(|a| crate::cli::shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        };
        match t {
            ParamTarget::Main => {
                self.cli.param =
                    crate::params::set(&self.cli.param, param, value).map_err(|e| e.to_string())?;
                self.restart_main().map_err(|e| format!("{e:#}"))?;
            }
            ParamTarget::Merge(i) => {
                let m = self
                    .cli
                    .merge
                    .get(i)
                    .and_then(|s| crate::sources::Merge::parse(s).ok())
                    .ok_or("no such merged source")?;
                let mut c = m.cli();
                c.param = crate::params::set(&c.param, param, value).map_err(|e| e.to_string())?;
                let spec = format!("{} = {}", m.name, args(&c));
                self.set_merged_source(Some(&m.name), spec)
                    .map_err(|e| format!("{e:#}"))?;
            }
            ParamTarget::Pane(id) => {
                let name = self
                    .opts
                    .panes
                    .as_ref()
                    .and_then(|p| p.panes.get(id))
                    .and_then(|p| p.source.clone())
                    .ok_or("that pane has no source of its own")?;
                let mut c =
                    crate::sources::cli_for(&name, self.cli.ssh.as_deref(), &self.cli.source)
                        .map_err(|e| format!("{e:#}"))?;
                c.param = crate::params::set(&c.param, param, value).map_err(|e| e.to_string())?;
                let spec = format!("{name} = {}", args(&c));
                self.set_pane_source_def(id, Some(&name), spec)
                    .map_err(|e| format!("{e:#}"))?;
            }
        }
        self.say(format!("{param}: {value}"), false);
        Ok(())
    }

    /// Run the main source again (its parameters changed): fresh data, same view.
    fn restart_main(&mut self) -> anyhow::Result<()> {
        let cli = self.current_cli();
        let spec = crate::setup::source_spec(&cli, true)?;
        let setup = crate::setup::build(cli, spec.clone(), false)?;
        self.session = setup.session;
        self.built_params = crate::params::parse_all(&setup.cli.param);
        self.alerts = setup.cli.with_params().alert;
        self.cli = setup.cli;
        self.live = Live::new(spec);
        self.backfill = None;
        self.backfill_done = false;
        // Where you were stays: the window (paged back or zoomed), the cursor and a
        // measured stretch are times, as good for BTC as for ETH. If the new data
        // doesn't reach back that far, older data is fetched until it does.
        self.anchor = None;
        self.started = Instant::now();
        self.rebuild(true);
        Ok(())
    }

    /// `param("interval", "1h")`: the picked pane's source if it has that setting,
    /// else the main source, else a merged one.
    fn param_from_console(&mut self, name: &str, value: &str) {
        let mut targets = Vec::new();
        if let Some(p) = &self.opts.panes
            && p.panes.get(p.focus).is_some_and(|q| q.source.is_some())
        {
            targets.push(ParamTarget::Pane(p.focus));
        }
        targets.push(ParamTarget::Main);
        targets.extend((0..self.cli.merge.len()).map(ParamTarget::Merge));
        let Some(t) = targets
            .into_iter()
            .find(|t| self.params_of(*t).iter().any(|p| p.name == name))
        else {
            let all: Vec<String> = self
                .params_of(ParamTarget::Main)
                .into_iter()
                .map(|p| p.name)
                .collect();
            let have = if all.is_empty() {
                "this chart's source has none (add one with --param 'interval = 1m | 1h')"
                    .to_string()
            } else {
                format!("it has {}", all.join(", "))
            };
            return self.say(format!("param: no setting called '{name}' — {have}"), true);
        };
        if let Err(e) = self.set_param(t, name, value) {
            self.say(format!("param: {e}"), true);
        }
    }

    /// A parameter's choices, under its toolbar button.
    fn draw_param_menu(&self, f: &mut Frame) {
        let Some((t, k)) = self.param_menu else {
            return;
        };
        let Some(p) = self.params_of(t).into_iter().nth(k) else {
            return;
        };
        let area = f.area();
        let w = (p
            .choices
            .iter()
            .map(|c| c.chars().count())
            .max()
            .unwrap_or(4) as u16
            + 6)
        .max(p.name.chars().count() as u16 + 4)
        .min(area.width);
        let h = (p.choices.len() as u16 + 2).min(area.height);
        let anchor = self
            .hits
            .find(|h| matches!(h, Hit::ParamMenu(pt, pk) if *pt == t && *pk == k));
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
        self.popup_rect.set(Some(rect));
        let block = Block::bordered()
            .title(format!(" {} ", p.name))
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.hits.push(rect, Hit::MenuBackground);
        for (i, c) in p.choices.iter().enumerate() {
            let y = inner.y + i as u16;
            if y >= inner.bottom() {
                break;
            }
            let r = Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            };
            let on = i == p.current;
            let style = if on {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let mark = if on { "●" } else { " " };
            f.render_widget(
                Span::styled(
                    format!(
                        "{mark} {c:<width$}",
                        width = r.width.saturating_sub(2) as usize
                    ),
                    style,
                ),
                r,
            );
            self.hits.push(r, Hit::ParamPick(t, k, i));
        }
    }

    // ───────────── sources menu ─────────────

    /// The merged sources, as (name, what it reads).
    fn merged_sources(&self) -> Vec<(String, String)> {
        self.cli
            .merge
            .iter()
            .filter_map(|s| crate::sources::Merge::parse(s).ok())
            .map(|m| {
                let what = crate::setup::source_spec(&m.cli(), false)
                    .map_or_else(|_| m.args.join(" "), |s| s.describe());
                (m.name, what)
            })
            .collect()
    }

    /// Panes showing a source of their own: (pane, source name, what it reads).
    fn pane_sources(&self) -> Vec<(usize, String, String)> {
        let Some(p) = &self.opts.panes else {
            return Vec::new();
        };
        p.panes
            .iter()
            .enumerate()
            .filter_map(|(id, pane)| {
                let name = pane.source.clone()?;
                let what =
                    crate::sources::cli_for(&name, self.cli.ssh.as_deref(), &self.cli.source)
                        .ok()
                        .and_then(|c| crate::setup::source_spec(&c, false).ok())
                        .map_or_else(|| "a profile or recent chart".to_string(), |s| s.describe());
                Some((id, name, what))
            })
            .collect()
    }

    /// Edit a source in the builder: `None` the main one, `Some(i)` merged source `i`,
    /// `Some(usize::MAX)` a new one.
    fn edit_source(&mut self, which: Option<usize>) -> Transition {
        self.sources_menu = false;
        match which {
            None => Transition::OpenBuilder,
            Some(i) => {
                self.source_to_edit = Some(
                    self.cli
                        .merge
                        .get(i)
                        .and_then(|s| crate::sources::Merge::parse(s).ok())
                        .map(|m| m.name),
                );
                Transition::EditSource
            }
        }
    }

    /// The source the builder is about to edit: its name (`None`: a new one, named
    /// here) and its arguments.
    #[cfg(test)]
    pub fn ds_names(&self) -> Vec<String> {
        self.ds.series.iter().map(|s| s.name.clone()).collect()
    }

    /// For editing a pane's own source: which pane (taken by the builder).
    #[cfg(test)]
    pub fn edit_pane_source_for_test(&mut self, id: usize) -> Transition {
        self.sources_menu = false;
        self.pane_to_edit = Some(id);
        Transition::EditSource
    }

    /// What a pane's or merged source has read so far, by name.
    pub fn source_history(&self, name: &str) -> Vec<(String, crate::data::LineMeta)> {
        self.subs
            .iter()
            .chain(&self.merge_subs)
            .find(|s| s.name == name)
            .map(|s| s.history().to_vec())
            .unwrap_or_default()
    }

    pub fn take_pane_to_edit(&mut self) -> Option<usize> {
        self.pane_to_edit.take()
    }

    pub fn take_source_edit(&mut self) -> (String, Cli) {
        // A pane's source: its name and what it runs now.
        if let Some(id) = self.pane_to_edit
            && let Some(name) = self
                .opts
                .panes
                .as_ref()
                .and_then(|p| p.panes.get(id))
                .and_then(|p| p.source.clone())
        {
            self.source_to_edit = None;
            let cli = crate::sources::cli_for(&name, self.cli.ssh.as_deref(), &self.cli.source)
                .unwrap_or_else(|_| <Cli as clap::Parser>::parse_from(["graft"]));
            return (name, cli);
        }
        let name = self.source_to_edit.take().flatten();
        let found = name.as_ref().and_then(|n| {
            self.cli
                .merge
                .iter()
                .filter_map(|s| crate::sources::Merge::parse(s).ok())
                .find(|m| &m.name == n)
        });
        match found {
            Some(m) => (m.name.clone(), m.cli()),
            None => {
                let taken: Vec<String> = self.merged_sources().into_iter().map(|m| m.0).collect();
                let name = (2..)
                    .map(|i| format!("source{i}"))
                    .find(|n| !taken.contains(n))
                    .expect("a free name");
                (name, <Cli as clap::Parser>::parse_from(["graft"]))
            }
        }
    }

    /// A pane's source, edited in the builder: kept as a named source of this chart
    /// (`--source 'NAME = ARGS'`, in place of a preset of that name) and shown in pane
    /// `id` (`old`: the name it had).
    pub fn set_pane_source_def(
        &mut self,
        id: usize,
        old: Option<&str>,
        spec: String,
    ) -> anyhow::Result<()> {
        let m = crate::sources::Merge::parse(&spec)?;
        self.cli
            .source
            .retain(|s| crate::sources::Merge::parse(s).map_or(true, |o| o.name != m.name));
        self.cli.source.push(m.spec);
        if let Some(p) = self.opts.panes.as_mut()
            && let Some(pane) = p.panes.get_mut(id)
        {
            pane.source = Some(m.name.clone());
            p.focus = id;
        }
        // Start it again with its new arguments.
        self.subs
            .retain(|s| s.name != m.name && Some(s.name.as_str()) != old);
        self.tick_sources(true);
        self.rebuild(true);
        self.say(format!("pane {} now reads '{}'", id + 1, m.name), false);
        Ok(())
    }

    /// Add or replace a merged source (from the builder): `old` is the name it had.
    pub fn set_merged_source(&mut self, old: Option<&str>, spec: String) -> anyhow::Result<()> {
        let m = crate::sources::Merge::parse(&spec)?;
        self.cli.merge.retain(|s| {
            crate::sources::Merge::parse(s)
                .map_or(true, |o| o.name != m.name && Some(o.name.as_str()) != old)
        });
        self.cli.merge.push(m.spec);
        self.start_merges();
        self.rebuild(true);
        self.say(format!("source '{}' merged in", m.name), false);
        Ok(())
    }

    fn remove_source(&mut self, i: usize) {
        if i < self.cli.merge.len() {
            self.cli.merge.remove(i);
            self.start_merges();
            self.rebuild(true);
        }
    }

    fn on_sources_menu_key(&mut self, key: KeyEvent) -> Transition {
        match key.code {
            KeyCode::Char('a') => return self.edit_source(Some(usize::MAX)),
            KeyCode::Char('e') => return self.edit_source(None),
            KeyCode::Char(d @ '1'..='9') => {
                let i = d as usize - '1' as usize;
                if i < self.cli.merge.len() {
                    return self.edit_source(Some(i));
                }
            }
            _ => self.sources_menu = false,
        }
        Transition::None
    }

    /// Where the data comes from: the main source and each merged one, to edit,
    /// remove or add to.
    fn draw_sources_menu(&self, f: &mut Frame) {
        if !self.sources_menu {
            return;
        }
        let area = f.area();
        let merged = self.merged_sources();
        let panes = self.pane_sources();
        let w = 72.min(area.width);
        let inner_w = w.saturating_sub(2);
        let actions = vec![
            Button::new("+ add source", Hit::SourceAdd),
            Button::new("done", Hit::CloseSources),
        ];
        let rows = 2
            + 2 * (1 + merged.len() + panes.len())
            + 1
            + flow_height(&actions, inner_w) as usize
            + 1;
        let h = (rows as u16 + 2).min(area.height);
        let rect = Rect {
            x: (area.width - w) / 2,
            y: area.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        };
        Clear.render(rect, f.buffer_mut());
        self.popup_rect.set(Some(rect));
        let block = Block::bordered()
            .title(" sources ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.hits.push(rect, Hit::MenuBackground);
        let dim = Style::default().fg(Color::Gray);
        let mut y = inner.y;
        let line = |f: &mut Frame, y: u16, l: Line| {
            if y < inner.bottom() {
                f.render_widget(
                    l,
                    Rect {
                        y,
                        height: 1,
                        ..inner
                    },
                );
            }
        };
        line(f, y, Line::styled("where the data comes from", dim));
        y += 2;
        let fit = |s: &str, n: usize| -> String {
            if s.chars().count() > n {
                s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
            } else {
                s.to_string()
            }
        };
        // Each source: its name and what it reads, then its buttons on the right.
        let row = |f: &mut Frame, y: u16, name: &str, what: &str, buttons: Vec<Button<Hit>>| {
            if y >= inner.bottom() {
                return;
            }
            let bw: u16 = buttons
                .iter()
                .map(|b| b.label.chars().count() as u16 + 3)
                .sum();
            line(
                f,
                y,
                Line::from(vec![Span::styled(
                    format!("{name} "),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )]),
            );
            line(
                f,
                y + 1,
                Line::styled(
                    format!(
                        "  {}",
                        fit(what, inner.width.saturating_sub(bw + 3) as usize)
                    ),
                    dim,
                ),
            );
            render_buttons(
                &buttons,
                Rect {
                    x: inner.right().saturating_sub(bw),
                    y: y + 1,
                    width: bw,
                    height: 1,
                },
                f.buffer_mut(),
                &self.hits,
            );
        };
        let main = if self.live.spec.is_live()
            || !matches!(self.live.spec, crate::source::SourceSpec::Stdin)
        {
            self.live.spec.describe()
        } else {
            "stdin".to_string()
        };
        // In a dashboard, which panes show it.
        let shown_in: Vec<String> = self.opts.panes.as_ref().map_or(Vec::new(), |p| {
            p.panes
                .iter()
                .enumerate()
                .filter(|(_, q)| q.source.is_none())
                .map(|(i, _)| (i + 1).to_string())
                .collect()
        });
        let main_label = match shown_in.len() {
            0 if self.opts.panes.is_some() => "main · no pane shows it".to_string(),
            0 => "main".to_string(),
            1 => format!("main · pane {}", shown_in[0]),
            _ => format!("main · panes {}", shown_in.join(", ")),
        };
        row(
            f,
            y,
            &main_label,
            &main,
            vec![Button::new("edit", Hit::SourceEdit(None))],
        );
        y += 2;
        for (i, (name, what)) in merged.iter().enumerate() {
            row(
                f,
                y,
                &format!("{} · {name}", i + 1),
                what,
                vec![
                    Button::new("edit", Hit::SourceEdit(Some(i))),
                    Button::new("✕", Hit::SourceRemove(i)),
                ],
            );
            y += 2;
        }
        // Panes with a source of their own (a dashboard): change picks another one.
        for (id, name, what) in &panes {
            row(
                f,
                y,
                &format!("pane {} · {name}", id + 1),
                what,
                vec![
                    Button::new("edit", Hit::SourcePaneEdit(*id)),
                    Button::new("other source…", Hit::SourcePane(*id)),
                ],
            );
            y += 2;
        }
        y += 1;
        if y < inner.bottom() {
            render_buttons(
                &actions,
                Rect {
                    y,
                    height: inner.bottom() - y,
                    ..inner
                },
                f.buffer_mut(),
                &self.hits,
            );
        }
        if inner.height > 0 {
            line(
                f,
                inner.bottom() - 1,
                Line::styled(
                    "a add · e edit main · 1-9 edit · Esc",
                    Style::default().fg(Color::DarkGray),
                ),
            );
        }
    }

    // ───────────── console completion ─────────────

    /// What the console can suggest where its cursor is.
    fn completions(&self) -> crate::script::Completions {
        if let Some(t) = &self.tab_cycle {
            return t.comps.clone();
        }
        let Some(input) = &self.console else {
            return Default::default();
        };
        let names: Vec<String> = self.full.series.iter().map(|s| s.name.clone()).collect();
        crate::script::complete_with(
            &input.value,
            input.cursor,
            &names,
            &self.param_choices(),
            &self.rule_labels(),
        )
    }

    /// The marker rules as backtest() names them: their labels (else conditions).
    fn rule_labels(&self) -> Vec<String> {
        self.mark_rules
            .iter()
            .map(|r| r.label.clone().unwrap_or_else(|| r.expr.clone()))
            .collect()
    }

    /// Every source setting the console's param() can change, with its choices.
    fn param_choices(&self) -> Vec<(String, Vec<String>)> {
        let mut targets = vec![ParamTarget::Main];
        if let Some(p) = &self.opts.panes {
            targets.extend((0..p.count()).map(ParamTarget::Pane));
        }
        targets.extend((0..self.cli.merge.len()).map(ParamTarget::Merge));
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for t in targets {
            for p in self.params_of(t) {
                if !out.iter().any(|o| o.0 == p.name) {
                    out.push((p.name, p.choices));
                }
            }
        }
        out
    }

    /// Tab: put the next suggestion in (Shift+Tab: the one before).
    fn tab_complete(&mut self, forward: bool) {
        let Some(input) = &self.console else {
            return;
        };
        let t = match self.tab_cycle.take() {
            Some(mut t) if !t.comps.items.is_empty() => {
                let n = t.comps.items.len();
                t.i = if forward {
                    (t.i + 1) % n
                } else {
                    (t.i + n - 1) % n
                };
                t
            }
            _ => {
                let names: Vec<String> = self.full.series.iter().map(|s| s.name.clone()).collect();
                let comps = crate::script::complete_with(
                    &input.value,
                    input.cursor,
                    &names,
                    &self.param_choices(),
                    &self.rule_labels(),
                );
                if comps.items.is_empty() {
                    return;
                }
                TabCycle {
                    base: input.value.clone(),
                    cursor: input.cursor,
                    i: if forward { 0 } else { comps.items.len() - 1 },
                    comps,
                }
            }
        };
        self.put_suggestion(&t.base, t.cursor, &t.comps, t.i);
        // Only one: nothing to cycle through, so the next Tab looks afresh (`slope("`
        // then the series).
        if t.comps.items.len() > 1 {
            self.tab_cycle = Some(t);
        }
    }

    /// Replace the word being typed with suggestion `i`.
    fn put_suggestion(
        &mut self,
        base: &str,
        cursor: usize,
        comps: &crate::script::Completions,
        i: usize,
    ) {
        let Some(s) = comps.items.get(i) else {
            return;
        };
        let chars: Vec<char> = base.chars().collect();
        let cursor = cursor.min(chars.len());
        let start = comps.start.min(cursor);
        let value: String = chars[..start]
            .iter()
            .chain(s.insert.chars().collect::<Vec<_>>().iter())
            .chain(chars[cursor..].iter())
            .collect();
        let mut input = TextInput::new(value);
        input.cursor = start + s.insert.chars().count();
        self.console = Some(input);
    }

    /// The row above the console: how the call you're in is used, then suggestions
    /// (Tab or tap to use one).
    fn hint_rows(&self, width: u16) -> (Option<Line<'static>>, Vec<Button<Hit>>) {
        if self.console.is_none() {
            return (None, Vec::new());
        }
        let comps = self.completions();
        let selected = self.tab_cycle.as_ref().map(|t| t.i);
        // How to call: the call you're in, else the suggestion that Tab would use.
        let shown = comps.signature.or_else(|| {
            let s = comps.items.get(selected.unwrap_or(0))?;
            let name = s.label.strip_suffix("()")?;
            crate::script::usage(name).map(|u| (u.1, u.2))
        });
        let names: Vec<String> = self.full.series.iter().map(|s| s.name.clone()).collect();
        let line = shown.map(|(how, what)| {
            // The example in this chart's own series.
            let how = crate::script::example_for(how, &names);
            let mut text = format!("{how} — {what}");
            let w = width.saturating_sub(1) as usize;
            if text.chars().count() > w {
                text = text.chars().take(w.saturating_sub(1)).collect::<String>() + "…";
            }
            Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(text, Style::default().fg(Color::Gray)),
            ])
        });
        let mut buttons: Vec<Button<Hit>> = comps
            .items
            .iter()
            .enumerate()
            .take(20)
            .map(|(i, s)| {
                Button::new(s.label.clone(), Hit::Complete(i)).active(selected == Some(i))
            })
            .collect();
        if !buttons.is_empty() && selected.is_none() {
            buttons.insert(0, Button::new("Tab", Hit::Complete(0)));
        }
        (line, buttons)
    }

    // ───────────── console output ─────────────

    /// A console result in words: values in their series' unit, saying which point
    /// they come from (and, with a cursor placed, the value there too); trends as
    /// "rising 2.1 ms per hour"; other numbers without scientific notation.
    fn describe_result(&self, code: &str, v: &Dynamic) -> String {
        static CALL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"^\s*([a-z_0-9]+)\(\s*"([^"]+)"\s*(?:,\s*(.+?))?\s*\)\s*;?\s*$"#)
                .expect("valid")
        });
        let num = v
            .as_float()
            .ok()
            .or_else(|| v.as_int().ok().map(|i| i as f64));
        // `last(api)` reads as `last("api")`.
        let names: Vec<String> = self.full.series.iter().map(|s| s.name.clone()).collect();
        let quoted = crate::script::quote_series_names(code, &names);
        let code = quoted.trim();
        // `api` on its own is its newest value.
        let (func, name, arg) = match CALL.captures(code) {
            Some(c) => (
                c[1].to_string(),
                c[2].to_string(),
                c.get(3).map(|m| m.as_str().to_string()),
            ),
            None => match self
                .full
                .series
                .iter()
                .find(|s| crate::script::ident(&s.name) == code)
            {
                Some(s) => ("last".to_string(), s.name.clone(), None),
                None => return self.describe_plain(code, v),
            },
        };
        let Some(s) = self.full.series.iter().find(|s| s.name == name) else {
            return self.describe_plain(code, v);
        };
        let pts = s.in_view(self.focus());
        let when = |x: f64| self.ds.fmt_x(crate::render::round_x(&self.ds, x), 1.0);
        let shown = if self.selection.is_some() {
            "the measured stretch"
        } else if self.view.is_some() {
            "what's in view"
        } else {
            "what's shown"
        };
        let u = &s.unit;
        // With a cursor placed, the value there too: it's what the status line shows.
        let at_cursor = || {
            let c = self.cursor?;
            let (y, projected) = value_at(s, &self.opts, c)?;
            Some(format!(
                " · at the cursor ({}): {}{}",
                when(c),
                u.format(y),
                if projected { " (forecast)" } else { "" }
            ))
        };
        let Some(n) = num else {
            return crate::script::display(v);
        };
        let point_with = |y: f64| pts.iter().find(|p| p.1 == y).map(|p| when(p.0));
        match func.as_str() {
            "last" => {
                let x = pts.last().map(|p| when(p.0)).unwrap_or_default();
                let which = if self.selection.is_some() {
                    "the end of the measured stretch"
                } else if self.view.is_some() {
                    "the newest in view"
                } else {
                    "the newest value"
                };
                format!(
                    "{name}: {} — {which} ({x}){}",
                    u.format(n),
                    at_cursor().unwrap_or_default()
                )
            }
            "first" => format!(
                "{name}: {} — the oldest of {shown} ({}){}",
                u.format(n),
                pts.first().map(|p| when(p.0)).unwrap_or_default(),
                at_cursor().unwrap_or_default()
            ),
            "at" => match self.cursor {
                Some(c) => format!("{name}: {} at the cursor ({})", u.format(n), when(c)),
                None => format!(
                    "{name}: {} — the newest value (no cursor placed: tap the chart or press c)",
                    u.format(n)
                ),
            },
            "min" | "max" => format!(
                "{name}: {} — the {} of {shown}{}{}",
                u.format(n),
                if func == "min" { "smallest" } else { "largest" },
                point_with(n)
                    .map(|t| format!(", at {t}"))
                    .unwrap_or_default(),
                at_cursor().unwrap_or_default()
            ),
            "mean" | "avg" | "median" | "p95" | "percentile" | "stddev" | "sum" => {
                let what = match func.as_str() {
                    "mean" | "avg" => "the average".to_string(),
                    "median" => "the middle value".to_string(),
                    "p95" => "the 95th percentile".to_string(),
                    "percentile" => format!("the {}th percentile", arg.as_deref().unwrap_or("?")),
                    "stddev" => "the spread (standard deviation)".to_string(),
                    _ => "the total".to_string(),
                };
                format!(
                    "{name}: {} — {what} of {shown} ({} points){}",
                    u.format(n),
                    pts.len(),
                    at_cursor().unwrap_or_default()
                )
            }
            "change" => format!(
                "{name}: {}{} from the first to the last of {shown}",
                if n > 0.0 { "+" } else { "" },
                u.format(n)
            ),
            "pct_change" => format!("{name}: {n:+.1}% from the first to the last of {shown}"),
            "count" => format!("{name}: {} points in {shown}", n as i64),
            "span" => format!("{name}: {shown} covers {}", crate::time::fmt_duration(n)),
            "slope" => {
                // Per the duration asked for, else one that reads well for the data.
                let (per, per_secs) = match &arg {
                    Some(a) => {
                        let a = a.trim().trim_matches('"').to_string();
                        let secs = crate::time::parse_duration_secs(&a)
                            .ok()
                            .or_else(|| a.parse().ok())
                            .unwrap_or(1.0);
                        (secs, true)
                    }
                    None => {
                        let span = pts.last().zip(pts.first()).map_or(0.0, |(b, a)| b.0 - a.0);
                        (crate::time::auto_per(span), false)
                    }
                };
                let per_value = if per_secs { n } else { n * per };
                let dir = if per_value > 0.0 {
                    "rising"
                } else if per_value < 0.0 {
                    "falling"
                } else {
                    "flat"
                };
                format!(
                    "{name} is {dir} {} per {} (the trend over {shown})",
                    u.format(per_value.abs()),
                    crate::time::per_label(per)
                )
            }
            "r2" => format!(
                "{name}: R² = {n:.2} — {}",
                if n >= 0.8 {
                    "a straight line fits it well"
                } else if n >= 0.4 {
                    "it roughly follows a straight line"
                } else {
                    "mostly noise: the trend says little"
                }
            ),
            "forecast" => format!(
                "{name}: {} — where the trend is {} after the newest point",
                u.format(n),
                arg.as_deref().unwrap_or("")
            ),
            _ => self.describe_plain(code, v),
        }
    }

    /// A result not tied to one series' value: in the unit of the series it uses
    /// (when they share one and it's a sum or difference), else a plain number.
    fn describe_plain(&self, code: &str, v: &Dynamic) -> String {
        if code.contains("eta(") {
            return match v.as_float() {
                Ok(secs) if secs.is_finite() => format!(
                    "in {} (around {})",
                    crate::time::fmt_duration(secs),
                    self.ds.fmt_x(
                        self.ds.x_range().map_or(0.0, |r| r.1) + secs,
                        secs.max(60.0)
                    )
                ),
                _ if v.is_unit() => "never: the trend isn't heading there".into(),
                _ => crate::script::display(v),
            };
        }
        let Some(n) = v.as_float().ok() else {
            return crate::script::display(v);
        };
        let used: Vec<&Series> = self
            .full
            .series
            .iter()
            .filter(|s| {
                code.contains(&format!("\"{}\"", s.name))
                    || regex::Regex::new(&format!(
                        r"\b{}\b",
                        regex::escape(&crate::script::ident(&s.name))
                    ))
                    .is_ok_and(|re| re.is_match(code))
            })
            .collect();
        let same_unit = used
            .first()
            .is_some_and(|f| used.iter().all(|s| s.unit == f.unit));
        let additive = !code.contains(['*', '/', '%', '<', '>', '=']);
        if same_unit && additive && !code.contains("count(") && !code.contains("r2(") {
            return used[0].unit.format(n);
        }
        crate::script::display(v)
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::HLine(value, label) => {
                // Kept in the configuration too, so it's saved with the chart.
                self.cli.hline.push(match &label {
                    Some(l) => format!("{value}:red:{l}"),
                    None => format!("{value}:red"),
                });
                self.opts.hlines.push(HLine {
                    value,
                    color: Some(Color::Red),
                    label,
                })
            }
            Action::ClearHLines => {
                self.cli.hline.clear();
                self.opts.hlines.clear()
            }
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
            Action::Toggle(n) => {
                if !self.hidden.remove(&n) {
                    self.hidden.insert(n);
                }
            }
            Action::ShowAll => self.hidden.clear(),
            Action::Only(names) => {
                let unknown: Vec<&String> = names
                    .iter()
                    .filter(|n| !self.full.series.iter().any(|s| &&s.name == n))
                    .collect();
                if !unknown.is_empty() {
                    let have: Vec<&str> = self.full.series.iter().map(|s| s.name.as_str()).collect();
                    return self.say(
                        format!("only: no series called {unknown:?} (have: {})", have.join(", ")),
                        true,
                    );
                }
                self.hidden = self
                    .full
                    .series
                    .iter()
                    .map(|s| s.name.clone())
                    .filter(|n| !names.contains(n))
                    .collect();
            }
            Action::Hide(n) => {
                self.hidden.insert(n);
            }
            Action::Chart(k) => match ChartKind::from_str(&k, true) {
                Ok(kind) => self.set_kind(kind),
                Err(_) => self.say(format!("chart: there's no chart type '{k}' — line, scatter, bar, histogram, sparkline, stat or candles"), true),
            },
            Action::Front(names) => match self.opts.panes.as_mut() {
                Some(p) => {
                    let f = p.focus;
                    p.panes[f].front = names;
                }
                None => self.opts.front = names,
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
                cli.since = Some(a.clone());
                cli.until = b.clone();
                match crate::setup::timeframe(&cli) {
                    Ok(tf) => {
                        // Kept as written ("2026-11-01", "yesterday") so profiles and
                        // recent charts reopen on the same range.
                        self.cli.last = None;
                        self.cli.since = Some(a);
                        self.cli.until = b;
                        self.set_timeframe(tf)
                    }
                    Err(e) => self.say(format!("timeframe: {e:#}"), true),
                }
            }
            Action::Flag(flag, on) => self.set_flag(flag, on),
            Action::Pan(f) => self.pan(f),
            Action::ZoomStep(zoom_in) => self.zoom_at(if zoom_in { 0.5 } else { 2.0 }, None),
            Action::Svg(path) => self.export_view(path),
            Action::Pane(a) => self.apply_pane_action(a),
            Action::Copy(text) => self.copy(text),
            Action::Param(name, value) => self.param_from_console(&name, &value),
            Action::Merge(spec) => match crate::sources::Merge::parse(&spec) {
                Ok(m) => {
                    self.cli.merge.retain(|s| {
                        crate::sources::Merge::parse(s).map_or(true, |o| o.name != m.name)
                    });
                    self.cli.merge.push(m.spec);
                    self.start_merges();
                    self.rebuild(true);
                }
                Err(e) => self.say(format!("{e:#}"), true),
            },
            Action::Unmerge(name) => {
                let before = self.cli.merge.len();
                self.cli
                    .merge
                    .retain(|s| crate::sources::Merge::parse(s).map_or(true, |o| o.name != name));
                if self.cli.merge.len() == before {
                    self.say(format!("unmerge: nothing merged as '{name}'"), true);
                }
                self.start_merges();
                self.rebuild(true);
            }
            Action::Alert(expr) => {
                let expr = expr.trim().to_string();
                match self.editing_alert.take() {
                    Some(i) if i < self.alerts.len() => self.alerts[i] = expr,
                    _ if !self.alerts.contains(&expr) => self.alerts.push(expr),
                    _ => {}
                }
                self.check_alerts();
            }
            Action::Unalert(expr) => {
                self.editing_alert = None;
                let before = self.alerts.len();
                self.alerts.retain(|a| a.trim() != expr.trim());
                if self.alerts.len() == before {
                    self.say(format!("unalert: no alert '{expr}'"), true);
                }
                self.check_alerts();
            }
            Action::NotifyTo(t) => match crate::notify::parse_target(&t) {
                Ok(t) => {
                    if !self.cli.notify.contains(&t) {
                        self.cli.notify.push(t);
                    }
                }
                Err(e) => self.say(format!("notify_to: {e}"), true),
            },
            Action::NotifyOff(t) => self.cli.notify.retain(|x| x != &t),
            Action::Load(path) => {
                let p = std::path::PathBuf::from(path.trim());
                match self.session.script.load(&p) {
                    Ok(n) => {
                        // Saved with the view (the first script, if there's room).
                        if self.cli.script.is_none() {
                            self.cli.script = std::path::absolute(&p).ok();
                        }
                        self.binds.extend(
                            self.session
                                .script
                                .binds()
                                .into_iter()
                                .filter_map(|(k, code)| Some((k.chars().next()?, code))),
                        );
                        self.update_marks();
                        self.rebuild(true);
                        self.say(
                            format!("loaded {} ({n} functions): use them in add_line(…), mark_when(…)", p.display()),
                            false,
                        );
                    }
                    Err(e) => self.say(format!("load: {e:#}"), true),
                }
            }
            Action::Title(t) => {
                self.opts.title = t.clone();
                self.cli.title = (!t.is_empty()).then_some(t);
            }
            Action::Unit(u) => {
                let u = u.trim().to_string();
                if !u.is_empty() && crate::units::lookup_unit(&u, self.cli.binary_units).is_none() {
                    self.say(format!("unit: '{u}' isn't one graft knows; shown as a suffix"), false);
                }
                self.cli.unit = (!u.is_empty()).then_some(u);
                self.rebuild(true);
            }
            Action::AddLine(name, expr) => {
                // Custom lines are worked out from this chart's own data: a pane with a
                // source of its own (a dashboard's memory pane) can't have one.
                if let Some(src) = self.focused_pane().and_then(|p| p.source.clone()) {
                    let theirs = self.pane_series_names().join(", ");
                    let main: Vec<String> = self.opts.panes.as_ref().map_or(Vec::new(), |p| {
                        p.panes
                            .iter()
                            .enumerate()
                            .filter(|(_, q)| q.source.is_none())
                            .map(|(i, _)| (i + 1).to_string())
                            .collect()
                    });
                    let go = match main.as_slice() {
                        [] => "add a pane showing this chart (pane_source(\"main\"))".to_string(),
                        [one] => format!("pick pane {one} first"),
                        many => format!("pick one of panes {} first", many.join(", ")),
                    };
                    self.say(
                        format!(
                            "add_line: this pane shows '{src}' ({theirs}); custom lines are \
                             worked out from the main chart's series — {go}"
                        ),
                        true,
                    );
                    return;
                }
                match crate::lines::CustomLine::parse(&format!("{name} = {expr}")) {
                    Ok(line) => {
                        match self.session.custom.iter().position(|l| l.name == line.name) {
                            Some(i) => self.session.custom[i] = line,
                            None => self.session.custom.push(line),
                        }
                        self.rebuild(true);
                        let first = self.session.line_errors.borrow().first().cloned();
                        if let Some(e) = first {
                            self.say(e, true);
                        }
                    }
                    Err(e) => self.say(format!("add_line: {e}"), true),
                }
            }
            Action::RemoveLine(name) => {
                let before = self.session.custom.len();
                self.session.custom.retain(|l| l.name != name);
                if self.session.custom.len() == before {
                    self.say(format!("remove_line: no custom line called '{name}'"), true);
                }
                self.rebuild(true);
            }
            Action::Undo => {
                self.step_back();
            }
            Action::Redo => {
                self.step_forward();
            }
            Action::Strategies(fee) => {
                let text = self.strategies(fee);
                self.note(text, false);
            }
            Action::Discover(fee, series) => self.start_discover(fee, series),
            Action::Undiscover => self.stop_discover(),
            Action::DiscoverPick(n) => match self.lab.as_mut() {
                Some(lab) if n >= 1 && n <= lab.tracked.len() => {
                    lab.selected = n - 1;
                    self.lab_theories = false;
                    self.place_lab_marks();
                }
                Some(lab) => {
                    let msg = format!(
                        "discover_pick: pick 1 to {} (the strategies in the panel)",
                        lab.tracked.len()
                    );
                    self.say(msg, true);
                }
                None => self.say("discover_pick: discover() starts discovering", true),
            },
            Action::DiscoverKeep(n) => match self.discover_keep(n) {
                Ok(text) => self.note(text, false),
                Err(e) => self.say(e, true),
            },
            Action::Sweep(fee) => {
                let (text, error) = match self.sweep(fee) {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                self.note(text, error);
            }
            Action::Backtest(rule, series, after) => match self.backtest(&rule, series.as_deref(), after) {
                Ok(text) => self.note(text, false),
                Err(e) => self.note(e, true),
            },
            Action::Strategy(entry, exit, series) => match self.strategy(&entry, &exit, series.as_deref()) {
                Ok(text) => self.note(text, false),
                Err(e) => self.note(e, true),
            },
            Action::MarkHere(label) => {
                let x = self.cursor.or_else(|| self.ds.x_range().map(|r| r.1));
                match x {
                    Some(x) => {
                        self.manual_marks.push(crate::render::Mark {
                            series: None,
                            x,
                            y: None,
                            label,
                            rule: None,
                            symbol: "▼".into(),
                            color: None,
                            effects: ratatui::style::Modifier::empty(),
                            badge: false,
                            bg: None,
                        });
                        self.update_marks();
                    }
                    None => self.say("mark: no data yet", true),
                }
            }
            Action::ClearMarks => {
                self.mark_rules.clear();
                self.manual_marks.clear();
                self.update_marks();
            }
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
        match key.code {
            KeyCode::Tab => return self.tab_complete(true),
            KeyCode::BackTab => return self.tab_complete(false),
            _ => self.tab_cycle = None,
        }
        let Some(input) = self.console.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.console = None;
                self.editing_rule = None;
                self.editing_alert = None;
                self.console_note = None;
                self.console_help = false;
            }
            KeyCode::Enter => {
                let code = std::mem::take(&mut input.value);
                self.console = None;
                self.console_help = false;
                self.history_pos = None;
                if !code.trim().is_empty() {
                    self.history.push(code.clone());
                    self.eval(&code);
                    // A mistake: the command comes back to fix, the cursor where it went
                    // wrong.
                    if self.console_note.as_ref().is_some_and(|n| n.error) {
                        let mut input = TextInput::new(code.clone());
                        if let Some(i) = self.error_at {
                            input.cursor = i.min(code.chars().count());
                        }
                        self.console = Some(input);
                    }
                }
                // An edit ends with this command, whatever it was: a later rule or alert
                // (from a key binding, say) mustn't replace the one that was being edited.
                self.editing_rule = None;
                self.editing_alert = None;
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
        let popup = self.popup_open();
        let t = self.on_event_inner(ev);
        // A popup opened or closed: the arrow keys start from its first button.
        if self.popup_open() != popup {
            self.popup_focus.set(0);
            self.popup_kind.set(self.popup_open());
        }
        t
    }

    fn on_event_inner(&mut self, ev: &TermEvent) -> Transition {
        // Undo and redo themselves aren't steps.
        if let TermEvent::Key(k) = ev
            && k.kind == KeyEventKind::Press
            && self.console.is_none()
        {
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            match k.code {
                KeyCode::Char('z') if ctrl => return self.step_back(),
                KeyCode::Char('y') if ctrl => return self.step_forward(),
                KeyCode::Char('Z') if ctrl => return self.step_forward(),
                // Unless you've bound u / U to something else.
                KeyCode::Char('u')
                    if !ctrl && self.no_menu_open() && !self.binds.contains_key(&'u') =>
                {
                    return self.step_back();
                }
                KeyCode::Char('U')
                    if !ctrl && self.no_menu_open() && !self.binds.contains_key(&'U') =>
                {
                    return self.step_forward();
                }
                _ => {}
            }
        }
        let before = self.snapshot();
        let t = match ev {
            TermEvent::Key(k) if k.kind == KeyEventKind::Press => self.on_key(*k),
            TermEvent::Mouse(m) => self.on_mouse(*m),
            _ => Transition::None,
        };
        self.update_tutorial();
        let after_key = self.snapshot_key();
        if !std::mem::take(&mut self.stepped) && after_key != before.key {
            self.undo.push(before);
            if self.undo.len() > UNDO_DEPTH {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        match (t, self.pending.take()) {
            (Transition::None, Some(p)) => p,
            (t, _) => t,
        }
    }

    /// Revisions of the live sources (the main one, then each --merge).
    fn source_revisions(&self) -> Vec<Option<u64>> {
        let main =
            (self.live.spec.is_live() && !self.live.eof).then_some(self.session.store.revision);
        std::iter::once(main)
            .chain(
                self.merge_subs
                    .iter()
                    .map(|m| m.is_live().then(|| m.revision())),
            )
            .collect()
    }

    /// With merged live sources, redraw in rounds: once every one of them has sent
    /// something new — or after a while, so a stalled source can't freeze the chart.
    fn round_complete(&self) -> bool {
        if self.merge_subs.is_empty() || self.cli.no_sync {
            return true;
        }
        let now = self.source_revisions();
        let all_new = now
            .iter()
            .zip(self.sync_revs.iter().chain(std::iter::repeat(&None)))
            .all(|(n, before)| match (n, before) {
                (Some(n), Some(b)) => n != b,
                // Not live (or newly live): nothing to wait for.
                _ => true,
            });
        all_new || self.last_build.elapsed() > self.sync_wait
    }

    /// Start the `--merge` sources (again, after the list changed).
    fn start_merges(&mut self) {
        self.merge_subs.clear();
        // Wait at most about two samples of the slowest source for a full round.
        let slowest = self
            .cli
            .merge
            .iter()
            .filter_map(|m| crate::sources::Merge::parse(m).ok())
            .map(|m| m.cli().every)
            .chain(std::iter::once(self.cli.every))
            .max()
            .unwrap_or(Duration::from_secs(1));
        self.sync_wait = (slowest * 2).max(Duration::from_millis(500));
        for spec in self.cli.merge.clone() {
            match crate::sources::Merge::parse(&spec) {
                Ok(m) => self
                    .merge_subs
                    .push(crate::sources::Sub::open_merge(&m, true)),
                Err(e) => self.say(format!("{e:#}"), true),
            }
        }
    }

    /// `unit(…)` from the console: values parsed without a unit get this one.
    fn apply_unit(&mut self) {
        let Some(u) = self.cli.with_params().unit else {
            return;
        };
        let u = u.as_str();
        let (dim, mult) = crate::units::lookup_unit(u, self.cli.binary_units)
            .unwrap_or_else(|| (crate::units::Dim::Other(u.to_string()), 1.0));
        for s in &mut self.full.series {
            if s.unit.dim == crate::units::Dim::None {
                s.unit = crate::units::Unit::new(dim.clone());
                if mult != 1.0 {
                    s.points.iter_mut().for_each(|p| p.1 *= mult);
                }
            }
        }
    }

    /// Whether any series has a target line to draw (set, or the automatic 0 for a
    /// clearly falling series).
    fn has_targets(&self) -> bool {
        self.ds.series.iter().any(|s| {
            self.opts
                .analysis
                .report(&self.ds, s)
                .is_some_and(|r| r.target.is_some())
        })
    }

    fn no_menu_open(&self) -> bool {
        !(self.lines_menu
            || self.range_menu
            || self.pane_menu
            || self.help
            || self.console_help
            || self.source_menu.is_some()
            || self.marks_menu.is_some()
            || self.alerts_menu
            || self.sources_menu
            || self.param_menu.is_some()
            || self.settings_menu)
    }

    fn cli_bits(&self) -> CliBits {
        CliBits {
            title: self.cli.title.clone(),
            unit: self.cli.unit.clone(),
            hline: self.cli.hline.clone(),
            notify: self.cli.notify.clone(),
        }
    }

    /// The single pane opening the pane panel makes, unchanged: the same as no panes.
    fn trivial_panes(&self) -> bool {
        self.opts.panes.as_ref().is_some_and(|p| {
            p.count() == 1
                && p.panes[0].series.is_empty()
                && p.panes[0].kind.is_none()
                && p.panes[0].source.is_none()
                && p.panes[0]
                    .lines
                    .as_ref()
                    .is_none_or(|l| *l == self.global_lines())
        })
    }

    fn close_pane_menu(&mut self) {
        self.pane_menu = false;
        if self.trivial_panes() {
            self.opts.panes = None;
        }
    }

    fn snapshot_key(&self) -> String {
        let opts = ChartOpts {
            marks: Vec::new(),
            panes: if self.trivial_panes() {
                None
            } else {
                self.opts.panes.clone()
            },
            ..self.opts.clone()
        };
        let mut hidden: Vec<&String> = self.hidden.iter().collect();
        hidden.sort();
        format!(
            "{opts:?}|{:?}|{hidden:?}|{}|{:?}|{:?}|{:?}|{:?}|{:?}",
            self.timeframe,
            self.show_stats,
            self.mark_rules,
            self.manual_marks,
            self.session.custom,
            self.alerts,
            self.cli_bits()
        )
    }

    fn snapshot(&self) -> Snapshot {
        let mut hidden: Vec<String> = self.hidden.iter().cloned().collect();
        hidden.sort();
        Snapshot {
            opts: self.opts.clone(),
            timeframe: self.timeframe,
            hidden,
            show_stats: self.show_stats,
            mark_rules: self.mark_rules.clone(),
            manual_marks: self.manual_marks.clone(),
            custom: self.session.custom.clone(),
            alerts: self.alerts.clone(),
            cli_bits: self.cli_bits(),
            key: self.snapshot_key(),
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.opts = s.opts;
        self.timeframe = s.timeframe;
        self.hidden = s.hidden.into_iter().collect();
        self.show_stats = s.show_stats;
        self.mark_rules = s.mark_rules;
        self.manual_marks = s.manual_marks;
        self.session.custom = s.custom;
        self.alerts = s.alerts;
        self.cli.title = s.cli_bits.title;
        self.cli.unit = s.cli_bits.unit;
        self.cli.hline = s.cli_bits.hline;
        self.cli.notify = s.cli_bits.notify;
        self.editing_rule = None;
        self.editing_alert = None;
        // The pane panel works on panes: it closes with them.
        self.pane_menu &= self.opts.panes.is_some();
        self.selection = None;
        self.rebuild(true);
        self.tick_sources(true);
        self.check_alerts();
    }

    /// Undo: back to the setup before the last change.
    fn step_back(&mut self) -> Transition {
        self.stepped = true;
        if let Some(t) = self.tutorial.as_mut() {
            t.undone = true;
            // (Undo keys skip the usual after-event check.)
            if t.current().check == crate::tutorial::Check::Undo {
                t.done[t.step] = true;
            }
        }
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(self.snapshot());
                self.restore(prev);
                let more = self.undo.len();
                self.say(
                    format!(
                        "undone{} · Ctrl+Y / U redoes",
                        if more > 0 {
                            format!(" ({more} more)")
                        } else {
                            String::new()
                        }
                    ),
                    false,
                );
            }
            None => self.say("nothing to undo", false),
        }
        Transition::None
    }

    fn step_forward(&mut self) -> Transition {
        self.stepped = true;
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(self.snapshot());
                self.restore(next);
                self.say("redone", false);
            }
            None => self.say("nothing to redo", false),
        }
        Transition::None
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
            Flag::Legend => self.show_legend,
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
            Flag::Legend => Cmd::Legend,
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
        // Arrow keys move through a popup's buttons; Enter presses the picked one.
        if self.popup_open() != 0 {
            match key.code {
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::BackTab => {
                    self.popup_move(key.code);
                    return Transition::None;
                }
                KeyCode::Enter => return self.popup_press(),
                _ => {}
            }
        }
        if self.lines_menu {
            self.on_lines_menu_key(key);
            return Transition::None;
        }
        if self.range_menu {
            self.on_range_menu_key(key);
            return Transition::None;
        }
        if self.source_menu.is_some() {
            self.on_source_menu_key(key);
            return Transition::None;
        }
        if self.marks_menu.is_some() {
            self.on_marks_menu_key(key);
            return Transition::None;
        }
        if self.sources_menu {
            return self.on_sources_menu_key(key);
        }
        if self.param_menu.is_some() {
            self.param_menu = None;
            return Transition::None;
        }
        if self.settings_menu {
            self.settings_menu = false;
            return Transition::None;
        }
        if self.alerts_menu {
            match key.code {
                KeyCode::Char('a') => self.edit_alert(None),
                KeyCode::Char('t') => self.notify_now("test notification from graft".into()),
                _ => self.alerts_menu = false,
            }
            return Transition::None;
        }
        if self.pane_menu && self.on_pane_key(key) {
            return Transition::None;
        }
        // The tutorial: Enter goes on, Backspace goes back.
        if self.tutorial.is_some() && !(self.help || self.console_help) {
            match key.code {
                KeyCode::Enter => return self.tutorial_hit(TutorialHit::Next),
                KeyCode::Backspace => return self.tutorial_hit(TutorialHit::Back),
                KeyCode::PageDown => {
                    self.scroll_tutorial(5);
                    return Transition::None;
                }
                KeyCode::PageUp => {
                    self.scroll_tutorial(-5);
                    return Transition::None;
                }
                _ => {}
            }
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
            // Esc never quits (that's q): it lets go of the cursor, or says how to quit.
            KeyCode::Esc => {
                // A table of results (sweep) stays until closed: Esc closes it first.
                if self
                    .console_note
                    .as_ref()
                    .is_some_and(|n| n.text.contains('\n'))
                {
                    self.console_note = None;
                } else if self.cursor.take().is_none() {
                    self.say("q quits", false);
                }
                return Transition::None;
            }
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
            KeyCode::Char('0') => Cmd::Reset,
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
                if self.pane_menu {
                    self.close_pane_menu();
                } else {
                    self.pane_menu = true;
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
            KeyCode::Char('m') => Cmd::Mouse,
            KeyCode::Char('b') => Cmd::Marker,
            KeyCode::Char('y') => Cmd::Log,
            KeyCode::Char('x') => Cmd::Split,
            KeyCode::Char('s') => Cmd::Stats,
            KeyCode::Char('E') => Cmd::Discover,
            KeyCode::Char('f') => Cmd::Forecast,
            KeyCode::Char('F') => Cmd::Format,
            KeyCode::Char('M') => Cmd::Mouse,
            KeyCode::Char('T') => Cmd::Trend,
            KeyCode::Char('D') => Cmd::SideBySide,
            KeyCode::Char('G') => Cmd::Targets,
            KeyCode::Char('r' | 'R') => Cmd::RangeMenu,
            KeyCode::Char('C') => Cmd::Copy,
            KeyCode::Char('K') => Cmd::Markers,
            KeyCode::Char('N') => Cmd::Alerts,
            KeyCode::Char('S') => Cmd::Sources,
            KeyCode::Char('i') => Cmd::Legend,
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
        // A click in the console line puts its cursor there.
        if let Some(input) = self.console.as_mut()
            && matches!(m.kind, MouseEventKind::Down(MouseButton::Left))
        {
            let r = self.console_rect.get();
            if r.contains((col, row).into()) {
                input.click(r, col);
                return Transition::None;
            }
        }
        // Right-click a pane (or the chart): its settings.
        if m.kind == MouseEventKind::Down(MouseButton::Right) && !self.help {
            let chart = self.chart_rect.get();
            if chart.contains((col, row).into()) {
                let id = self.opts.panes.as_ref().and_then(|p| {
                    p.rects(chart)
                        .into_iter()
                        .find(|(_, r)| r.contains((col, row).into()))
                        .map(|(id, _)| id)
                });
                self.open_settings(id);
                return Transition::None;
            }
        }
        if self.help {
            match m.kind {
                MouseEventKind::Down(_) => {
                    self.help = false;
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
                        Hit::SourceEdit(i) => self.edit_source(i),
                        Hit::SourceAdd => self.edit_source(Some(usize::MAX)),
                        Hit::PaneSettings(id) => {
                            self.open_settings(id);
                            Transition::None
                        }
                        Hit::SetKind(k) => {
                            self.set_kind(k);
                            Transition::None
                        }
                        Hit::CloseSettings => {
                            self.settings_menu = false;
                            Transition::None
                        }
                        Hit::ParamMenu(t, k) => {
                            self.param_menu = match self.param_menu {
                                Some(open) if open == (t, k) => None,
                                _ => Some((t, k)),
                            };
                            Transition::None
                        }
                        Hit::ParamPick(t, k, i) => {
                            self.param_menu = None;
                            if let Some(p) = self.params_of(t).into_iter().nth(k)
                                && let Some(v) = p.choices.get(i).cloned()
                                && let Err(e) = self.set_param(t, &p.name, &v)
                            {
                                self.say(e, true);
                            }
                            Transition::None
                        }
                        Hit::SourcePaneEdit(id) => {
                            self.sources_menu = false;
                            self.pane_to_edit = Some(id);
                            Transition::EditSource
                        }
                        Hit::SourcePane(id) => {
                            // Pick that pane, then choose its source as the pane panel does
                            // (in place of the menu it came from, not on top of it).
                            self.sources_menu = false;
                            self.settings_menu = false;
                            if let Some(p) = self.opts.panes.as_mut() {
                                p.focus = id.min(p.count().saturating_sub(1));
                            }
                            self.open_source_menu();
                            Transition::None
                        }
                        Hit::SourceRemove(i) => {
                            self.remove_source(i);
                            Transition::None
                        }
                        Hit::CloseSources => {
                            self.sources_menu = false;
                            Transition::None
                        }
                        Hit::InsertCommand(name) => {
                            // As if typed and completed: `last("`, `undo()`, `zoom(`.
                            let names: Vec<String> =
                                self.full.series.iter().map(|s| s.name.clone()).collect();
                            let input = self.console.get_or_insert_with(TextInput::default);
                            let (value, cursor) = (input.value.clone(), input.cursor);
                            let comps = crate::script::complete(name, name.chars().count(), &names);
                            let insert = comps
                                .items
                                .iter()
                                .find(|s| s.label == format!("{name}()"))
                                .map_or(name.to_string(), |s| s.insert.clone());
                            let chars: Vec<char> = value.chars().collect();
                            let at = cursor.min(chars.len());
                            let new: String = chars[..at]
                                .iter()
                                .copied()
                                .chain(insert.chars())
                                .chain(chars[at..].iter().copied())
                                .collect();
                            let mut t = TextInput::new(new);
                            t.cursor = at + insert.chars().count();
                            self.console = Some(t);
                            self.tab_cycle = None;
                            Transition::None
                        }
                        Hit::Complete(i) => {
                            // Tapped: as if Tab had picked it.
                            if let Some(input) = &self.console {
                                let comps = self.completions();
                                let (base, cursor) = match &self.tab_cycle {
                                    Some(t) => (t.base.clone(), t.cursor),
                                    None => (input.value.clone(), input.cursor),
                                };
                                self.put_suggestion(&base, cursor, &comps, i);
                                self.tab_cycle = None;
                            }
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
                        Hit::Candles => {
                            self.toggle_candles();
                            Transition::None
                        }
                        Hit::CloseNote => {
                            self.console_note = None;
                            Transition::None
                        }
                        Hit::Lab(h) => {
                            self.lab_hit(h);
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
                        Hit::Tutorial(h) => self.tutorial_hit(h),
                        Hit::Pane(op) => {
                            self.pane_op(op);
                            Transition::None
                        }
                        Hit::LineAction(i) => {
                            self.edit_custom_line(i);
                            Transition::None
                        }
                        Hit::DeleteLine(i) => {
                            if i < self.session.custom.len() {
                                self.session.custom.remove(i);
                                self.rebuild(true);
                            }
                            Transition::None
                        }
                        Hit::CycleMarkSymbol(i) => {
                            if let Some(r) = self.mark_rules.get_mut(i) {
                                let syms = crate::marks::SYMBOLS;
                                let at = syms
                                    .iter()
                                    .position(|s| *s == r.symbol())
                                    .map_or(0, |p| p + 1);
                                r.symbol = Some(syms[at % syms.len()].to_string());
                            }
                            self.update_marks();
                            Transition::None
                        }
                        Hit::CycleMarkColor(i) => {
                            if let Some(r) = self.mark_rules.get_mut(i) {
                                let cs = crate::marks::COLORS;
                                let at = cs
                                    .iter()
                                    .position(|c| *c == r.color.as_deref())
                                    .map_or(0, |p| p + 1);
                                r.color = cs[at % cs.len()].map(String::from);
                            }
                            self.update_marks();
                            Transition::None
                        }
                        Hit::CycleMarkEffect(i) => {
                            if let Some(r) = self.mark_rules.get_mut(i) {
                                // plain → badge → bold → … → plain (several at once: edit the rule).
                                let cycle: Vec<Option<&str>> = std::iter::once(None)
                                    .chain(crate::marks::EFFECT_CYCLE.iter().map(|e| Some(*e)))
                                    .collect();
                                let now = match r.effects.as_slice() {
                                    [] => Some(0),
                                    [one] => cycle.iter().position(|c| *c == Some(one.as_str())),
                                    _ => None,
                                };
                                let next = now.map_or(0, |p| (p + 1) % cycle.len());
                                r.effects = cycle[next].map(String::from).into_iter().collect();
                            }
                            self.update_marks();
                            Transition::None
                        }
                        Hit::ToggleMarkNotify(i) => {
                            if let Some(r) = self.mark_rules.get_mut(i) {
                                r.notify = !r.notify;
                            }
                            Transition::None
                        }
                        Hit::EditAlert(i) => {
                            self.edit_alert(i);
                            Transition::None
                        }
                        Hit::DeleteAlert(i) => {
                            if i < self.alerts.len() {
                                self.alerts.remove(i);
                                self.editing_alert = match self.editing_alert {
                                    Some(e) if e == i => None,
                                    Some(e) if e > i => Some(e - 1),
                                    other => other,
                                };
                                self.check_alerts();
                            }
                            Transition::None
                        }
                        Hit::ToggleTarget(t) => {
                            self.toggle_target(t);
                            Transition::None
                        }
                        Hit::AddTarget(kind) => {
                            self.alerts_menu = false;
                            self.console = Some(TextInput::new(format!("notify_to(\"{kind}:")));
                            self.say(
                                match kind {
                                    "ntfy" => "ntfy topic (pick something unguessable), e.g. notify_to(\"ntfy:eth-alerts-x7\") — then subscribe in the ntfy app",
                                    "webhook" => "webhook URL, e.g. notify_to(\"webhook:https://hooks.slack.com/…\")",
                                    _ => "a command; it gets $GRAFT_ALERT and $GRAFT_TITLE, e.g. notify_to(\"cmd:say $GRAFT_ALERT\")",
                                },
                                false,
                            );
                            Transition::None
                        }
                        Hit::RemoveTarget(i) => {
                            if i < self.cli.notify.len() {
                                self.cli.notify.remove(i);
                            }
                            Transition::None
                        }
                        Hit::TestNotify => {
                            self.notify_now("test notification from graft".into());
                            Transition::None
                        }
                        Hit::CloseAlerts => {
                            self.alerts_menu = false;
                            Transition::None
                        }
                        Hit::EditRule(i) => {
                            self.edit_rule(i);
                            Transition::None
                        }
                        Hit::DeleteRule(i) => {
                            self.delete_mark_row(i);
                            Transition::None
                        }
                        Hit::DeleteManual(j) => {
                            self.delete_mark_row(self.mark_rules.len() + j);
                            Transition::None
                        }
                        Hit::MarkAction(a) => {
                            self.mark_action(a);
                            Transition::None
                        }
                        Hit::PickSource(i) => {
                            let src = self
                                .source_menu
                                .as_ref()
                                .and_then(|m| m.0.get(i))
                                .and_then(|it| it.1.clone());
                            self.set_pane_source(src);
                            Transition::None
                        }
                    };
                }
                if self.lines_menu
                    || self.range_menu
                    || self.source_menu.is_some()
                    || self.marks_menu.is_some()
                    || self.alerts_menu
                {
                    // A tap outside a menu closes it.
                    self.lines_menu = false;
                    self.range_menu = false;
                    self.source_menu = None;
                    self.marks_menu = None;
                    self.alerts_menu = false;
                    return Transition::None;
                }
                // Panes: grab a border to resize, or tap a pane to focus it.
                let area = self.chart_rect.get();
                if let Some(p) = self.opts.panes.as_mut() {
                    if p.count() > 1
                        && let Some((path, a)) = p.grab_border(area, col, row)
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
                // A tap on a chart with no time axis (stat tiles, bars…) lets go of the
                // cursor, so the status line follows the newest data again.
                if self.plot_at(col, row).is_none()
                    && self.chart_rect.get().contains((col, row).into())
                {
                    self.cursor = None;
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
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if self.tutorial.is_some()
                    && self
                        .tutorial_rect
                        .get()
                        .contains(ratatui::layout::Position { x: col, y: row }) =>
            {
                self.scroll_tutorial(if m.kind == MouseEventKind::ScrollUp {
                    -2
                } else {
                    2
                });
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // A menu drawn over the chart: the scroll is meant for it, not the chart.
                let over_menu = self.lines_menu
                    || self.range_menu
                    || self.source_menu.is_some()
                    || self.marks_menu.is_some()
                    || self.alerts_menu
                    || self.sources_menu;
                if !over_menu && let Some(plot) = self.plot_at(col, row) {
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

    /// The toolbar, with the buttons under ⋯ (`more`) or without.
    fn toolbar_with(&self, more: bool) -> Vec<Button<Hit>> {
        let b = |l: &str, c: Cmd| Button::new(l, Hit::Cmd(c));
        let mut v = vec![
            b("◀", Cmd::PanLeft),
            b("▶", Cmd::PanRight),
            b("+", Cmd::ZoomIn),
            b("−", Cmd::ZoomOut),
            b("show all", Cmd::Reset).active(self.view.is_some()),
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
            b(
                &match self.cli.merge.len() {
                    0 => "sources".to_string(),
                    n => format!("sources ({})", n + 1),
                },
                Cmd::Sources,
            )
            .active(self.sources_menu),
            b("forecast", Cmd::Forecast).active(self.opts.forecast.is_some()),
            b("split", Cmd::Split).active(self.opts.split),
            b("discover", Cmd::Discover).active(self.lab.is_some()),
        ];
        if !self.undo.is_empty() {
            v.insert(0, b("↶ undo", Cmd::Undo));
        }
        // Markers, once there are some (always under ⋯).
        if self.mark_rows() > 0 {
            v.push(
                b(&format!("markers ({})", self.mark_rows()), Cmd::Markers)
                    .active(self.marks_menu.is_some()),
            );
        }
        // Alerts, once there are some: lit while one is firing.
        if !self.alerts.is_empty() {
            v.push(
                b(&format!("alerts ({})", self.alerts.len()), Cmd::Alerts)
                    .active(self.alerts_menu || !self.firing.is_empty()),
            );
        }
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
        v.push(b(if more { "less" } else { "⋯" }, Cmd::More).active(more));
        if more {
            // Under ⋯ until there are some (then they're on the toolbar itself).
            if self.mark_rows() == 0 {
                v.push(b("markers", Cmd::Markers).active(self.marks_menu.is_some()));
            }
            if self.alerts.is_empty() {
                v.push(b("alerts", Cmd::Alerts).active(self.alerts_menu));
            }
            v.extend([
                b("targets", Cmd::Targets).active(self.opts.show_targets && self.has_targets()),
                // With panes, the chart type button belongs to the picked pane.
                b(
                    self.focused_pane()
                        .and_then(|p| p.kind)
                        .unwrap_or(self.opts.kind)
                        .name(),
                    Cmd::Kind,
                ),
                b("log", Cmd::Log).active(self.opts.log),
                b("follow", Cmd::Follow).active(self.follow_pointer),
                b(if self.paused { "resume" } else { "pause" }, Cmd::Pause).active(self.paused),
                b("format", Cmd::Format),
                b("save", Cmd::Save),
                b("copy cmd", Cmd::Copy),
                b(":", Cmd::Console),
                b("svg", Cmd::Export),
                b("mouse", Cmd::Mouse).active(self.mouse),
                b("legend", Cmd::Legend).active(self.show_legend),
                b("?", Cmd::Help),
                b("quit", Cmd::Quit),
            ]);
        }
        v
    }

    fn chips(&self) -> Vec<Button<Hit>> {
        // With panes, the picked pane's series (it may have a source of its own): a
        // chip's index is what toggle_series gets.
        let names = self.chip_names();
        let shown = |name: &String| {
            !self.hidden.contains(name) && self.focused_pane().is_none_or(|p| p.shows(name))
        };
        // A candlestick chart's open/high/low/close are one chip.
        let candles = self.candle_series();
        let mut out = Vec::new();
        for (i, name) in names.iter().enumerate() {
            if let Some(c) = &candles
                && c.contains(name)
            {
                if *name == c[0] {
                    // (A candles pane draws them whether it lists them or not.)
                    let on = c.iter().all(|n| !self.hidden.contains(n));
                    out.push(
                        Button::new(
                            format!("{} candles", if on { "●" } else { "○" }),
                            Hit::Candles,
                        )
                        .active(on),
                    );
                }
                continue;
            }
            let on = shown(name);
            out.push(
                Button::new(
                    format!("{} {name}", if on { "●" } else { "○" }),
                    Hit::Series(i),
                )
                .active(on),
            );
        }
        out
    }

    /// The series the chips are for: the picked pane's, or the chart's.
    fn chip_names(&self) -> Vec<String> {
        if self.opts.panes.is_some() {
            self.pane_series_names()
        } else {
            self.ds.series.iter().map(|s| s.name.clone()).collect()
        }
    }

    /// When the picked pane (or the chart) draws candles: its open, high, low and close
    /// series — found by name, else the first four it shows.
    fn candle_series(&self) -> Option<[String; 4]> {
        if self.current_kind() != ChartKind::Candles {
            return None;
        }
        let names = self.chip_names();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        if let [Some(o), Some(h), Some(l), Some(c)] = crate::render::candle_roles_named(&refs) {
            return Some([o, h, l, c].map(|k| names[k].clone()));
        }
        let shown: Vec<&String> = names
            .iter()
            .filter(|n| self.focused_pane().is_none_or(|p| p.shows(n)))
            .collect();
        (shown.len() >= 4).then(|| [0, 1, 2, 3].map(|k| shown[k].clone()))
    }

    /// The candles chip: show or hide all four series together.
    fn toggle_candles(&mut self) {
        let Some(c) = self.candle_series() else {
            return;
        };
        let on = c.iter().all(|n| !self.hidden.contains(n));
        for n in c {
            if on {
                self.hidden.insert(n);
            } else {
                self.hidden.remove(&n);
            }
        }
    }

    pub fn draw(&self, f: &mut Frame) {
        self.hits.clear();
        self.popup_rect.set(None);
        self.plots.borrow_mut().clear();
        let area = f.area();
        // The tutorial sits above everything else.
        let tut_h = self.tutorial_height(area.width, area.height);
        let [tut_area, area] =
            Layout::vertical([Constraint::Length(tut_h), Constraint::Min(0)]).areas(area);
        if tut_h > 0 {
            self.draw_tutorial(f, tut_area);
        }
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
        let toolbar = self.toolbar();
        let tool_h = if self.mouse {
            flow_height(&toolbar, area.width)
        } else {
            0
        };
        // The legend row: live, the time, each series and its value.
        let status_h = self.legend_height(area.width);
        let (hint_line, hint_buttons) = self.hint_rows(area.width);
        let console_top = std::cell::Cell::new(area.bottom());
        let console_h = u16::from(self.console.is_some())
            + u16::from(hint_line.is_some())
            + u16::from(!hint_buttons.is_empty());
        let note = self.note_lines(area.width);
        let note_h = note.len() as u16;
        let pane_h = self.pane_panel_height(area.width);
        let lab_rows = self.lab_rows(area.width.saturating_sub(2));
        let lab_buttons = self.lab_buttons();
        let lab_h = if self.lab.is_some() {
            let want =
                lab_rows.len() as u16 + 2 + flow_height(&lab_buttons, area.width.saturating_sub(2));
            want.min(area.height * 45 / 100).max(5)
        } else {
            0
        };
        let [
            chart,
            pane_area,
            lab_area,
            stats,
            tool_area,
            note_area,
            status,
            console,
        ] = Layout::vertical([
            Constraint::Min(6),
            Constraint::Length(pane_h),
            Constraint::Length(lab_h),
            Constraint::Length(stats_h),
            Constraint::Length(tool_h),
            Constraint::Length(note_h),
            Constraint::Length(status_h),
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
            sources: Some(&self.sub_views),
        };
        self.chart_rect.set(chart);
        let opts = self.view_opts();
        f.render_widget(ChartWidget::new(&self.ds, &opts, overlay), chart);
        if self.mouse {
            self.draw_settings_buttons(f, chart);
        }
        if pane_h > 0 {
            self.draw_pane_panel(f, pane_area);
        }
        if lab_h > 0 {
            self.draw_lab(f, lab_area, lab_rows, &lab_buttons);
        }
        if self.show_stats {
            self.draw_stats(f, stats);
        }
        if self.mouse {
            crate::ui::render_buttons_quiet(&toolbar, tool_area, f.buffer_mut(), &self.hits);
        }
        if note_h > 0 {
            f.render_widget(Paragraph::new(note), note_area);
            if self
                .console_note
                .as_ref()
                .is_some_and(|n| n.text.contains('\n'))
            {
                // Tap it (or Esc) to close.
                let close = Rect {
                    x: note_area.right().saturating_sub(3),
                    width: 3.min(note_area.width),
                    height: 1,
                    ..note_area
                };
                f.render_widget(
                    Span::styled(" ✕ ", Style::default().fg(Color::White).bg(Color::DarkGray)),
                    close,
                );
                self.hits.push(note_area, Hit::CloseNote);
            }
        }
        self.draw_legend_row(f, status);
        self.draw_message(f, chart);
        if let Some(input) = &self.console {
            console_top.set(console.y);
            // Hints above the input line.
            let mut y = console.y;
            if let Some(l) = hint_line {
                f.render_widget(
                    l,
                    Rect {
                        y,
                        height: 1,
                        ..console
                    },
                );
                y += 1;
            }
            if !hint_buttons.is_empty() {
                render_buttons(
                    &hint_buttons,
                    Rect {
                        y,
                        height: 1,
                        ..console
                    },
                    f.buffer_mut(),
                    &self.hits,
                );
                y += 1;
            }
            let console = Rect {
                y,
                height: 1,
                ..console
            };
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
                if self.console_help {
                    "type a command (Tab completes) · Enter runs · Esc closes"
                } else {
                    "type help to see every command · Tab completes · Enter runs"
                },
            );
            self.console_rect.set(field);
        }
        if self.lines_menu {
            self.draw_lines_menu(f);
        }
        if self.range_menu {
            self.draw_range_menu(f);
        }
        self.draw_source_menu(f);
        self.draw_marks_menu(f);
        self.draw_alerts_menu(f);
        self.draw_sources_menu(f);
        self.draw_settings_menu(f);
        self.draw_param_menu(f);
        self.draw_popup_focus(f);
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
        if self.console_help && self.console.is_some() {
            self.draw_commands(f, console_top.get());
        }
    }

    /// `help`: every console command at once, a line per group, above the console.
    /// Tap one to put it in the console.
    fn draw_commands(&self, f: &mut Frame, bottom: u16) {
        let area = f.area();
        let w = area.width.saturating_sub(2) as usize;
        let name_style = Style::default().fg(Color::Cyan);
        let head = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        // Lay the names out: each group starts a line, its label first.
        let label_w = crate::script::GROUPS
            .iter()
            .map(|g| g.0.len())
            .max()
            .unwrap_or(0)
            + 1;
        let mut rows: Vec<Vec<(String, Style, Option<&'static str>)>> = Vec::new();
        for (group, names) in crate::script::GROUPS {
            let mut row = vec![(format!("{group:<label_w$}"), head, None)];
            let mut used = label_w;
            for name in names.iter() {
                let len = name.chars().count() + 1;
                if used + len > w && used > label_w {
                    rows.push(std::mem::take(&mut row));
                    row.push((" ".repeat(label_w), head, None));
                    used = label_w;
                }
                row.push((format!("{name} "), name_style, Some(*name)));
                used += len;
            }
            rows.push(row);
        }
        let footer: Vec<String> = [
            "tap one, or type: Tab completes · Esc closes",
            "series names are variables (a - b) · units work: 50ms 2GB 1h 5%",
        ]
        .iter()
        .flat_map(|l| wrap(l, w))
        .collect();
        let h = (rows.len() + footer.len() + 2) as u16;
        let h = h.min(bottom.saturating_sub(area.y));
        let rect = Rect {
            x: area.x,
            y: bottom.saturating_sub(h),
            width: area.width,
            height: h,
        };
        Clear.render(rect, f.buffer_mut());
        let block = Block::bordered()
            .title(" console commands ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        self.hits.push(rect, Hit::MenuBackground);
        for (i, row) in rows.iter().chain(std::iter::empty()).enumerate() {
            let y = inner.y + i as u16;
            if y >= inner.bottom() {
                break;
            }
            let mut x = inner.x;
            for (text, style, name) in row {
                let len = text.chars().count() as u16;
                f.buffer_mut().set_string(x, y, text, *style);
                if let Some(n) = name {
                    self.hits.push(
                        Rect {
                            x,
                            y,
                            width: len.saturating_sub(1).max(1),
                            height: 1,
                        },
                        Hit::InsertCommand(n),
                    );
                }
                x += len;
            }
        }
        for (k, line) in footer.iter().enumerate() {
            let y = inner.y + (rows.len() + k) as u16;
            if y < inner.bottom() {
                f.buffer_mut().set_string(
                    inner.x,
                    y,
                    line.as_str(),
                    Style::default().fg(Color::Gray),
                );
            }
        }
    }

    /// The console's output, wrapped: an error stays while you fix it, a result for
    /// a while (or until the next command).
    fn note_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(n) = &self.console_note else {
            return Vec::new();
        };
        // A table of results (sweep, strategies) stays until closed (Esc, a tap, or the
        // next command); a one-line answer for 20 seconds.
        let table = n.text.contains('\n');
        if !table && !n.error && n.at.elapsed() > Duration::from_secs(20) {
            return Vec::new();
        }
        if n.error && self.console.is_none() && n.at.elapsed() > Duration::from_secs(20) {
            return Vec::new();
        }
        let (lead, style) = if n.error {
            ("✗ ", Style::default().fg(Color::LightRed))
        } else {
            ("▸ ", Style::default().fg(Color::White))
        };
        let w = width.saturating_sub(2).max(10) as usize;
        // Several lines (a table of results): each wrapped, more of them shown.
        let rows: Vec<String> = n.text.split('\n').flat_map(|l| wrap(l, w)).collect();
        let max = if n.text.contains('\n') { 14 } else { 4 };
        let mut lines: Vec<Line> = rows
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                Line::from(vec![
                    Span::styled(
                        if i == 0 { lead } else { "  " },
                        style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(l, style),
                ])
            })
            .collect();
        lines.truncate(max);
        lines
    }

    /// The discover panel's rows (wrapped to `width`), each with what a tap on it does.
    fn lab_rows(&self, width: u16) -> Vec<(Line<'static>, Option<Hit>)> {
        let Some(lab) = &self.lab else {
            return Vec::new();
        };
        let w = width.max(10) as usize;
        let mut rows: Vec<(Line<'static>, Option<Hit>)> = Vec::new();
        let push = |text: String,
                    style: Style,
                    hit: Option<Hit>,
                    rows: &mut Vec<(Line<'static>, Option<Hit>)>| {
            for (k, l) in wrap(&text, w).into_iter().enumerate() {
                let l = if k == 0 { l } else { format!("  {l}") };
                rows.push((Line::styled(l, style), hit));
            }
        };
        let dim = Style::default().fg(Color::DarkGray);
        let plain = Style::default();
        let status = if lab.searching() {
            "searching…".to_string()
        } else {
            lab.next_in()
                .map_or(String::new(), |s| format!("next in {s}s"))
        };
        match (&lab.report, &lab.waiting) {
            (_, Some(w)) if lab.report.is_none() => {
                push(format!("{w} · {status}"), dim, None, &mut rows);
            }
            (None, _) => push(format!("first search… {status}"), dim, None, &mut rows),
            (Some(r), _) => {
                let invented = r.pool.iter().filter(|s| s.invented).count();
                push(
                    format!(
                        "search {} · {} · {} pts · {:.1}s · {} formulas, {} strategies tried · {invented}/{} best formulas invented · costs {:.1}bp a trip · {status}",
                        lab.runs,
                        r.input.name,
                        r.input.len(),
                        r.took.as_secs_f64(),
                        r.formulas,
                        r.tried,
                        r.pool.len(),
                        r.cost
                    ),
                    dim,
                    None,
                    &mut rows,
                );
            }
        }
        let Some(report) = &lab.report else {
            return rows;
        };
        if self.lab_theories {
            push(
                format!(
                    "theories: |t| ≥ {:.1} on the first 70% (stronger than luck gives from {} tests), checked on the last 30%",
                    report.bar, report.tests
                ),
                Style::default().add_modifier(Modifier::BOLD),
                None,
                &mut rows,
            );
            if report.theories.is_empty() {
                push(
                    "none yet: nothing stood out from luck".into(),
                    dim,
                    None,
                    &mut rows,
                );
            }
            for t in &report.theories {
                let style = if t.held() {
                    Style::default().fg(Color::LightGreen)
                } else {
                    Style::default().fg(Color::Gray)
                };
                push(
                    format!("• {}", t.describe(&report.input)),
                    style,
                    None,
                    &mut rows,
                );
            }
            push(
                format!(
                    "holding: first 70% {:+.1}bp, last 30% {:+.1}bp",
                    report.hold[0], report.hold[1]
                ),
                dim,
                None,
                &mut rows,
            );
            return rows;
        }
        if lab.tracked.is_empty() {
            push(
                format!(
                    "no strategy has made money after costs ({:.1}bp a round trip) on both the first 70% and the last 30% yet — it keeps looking as data comes in",
                    report.cost
                ),
                plain,
                None,
                &mut rows,
            );
            return rows;
        }
        push(
            "strategies: picked on the first 70%, checked on the last 30%, then paper-traded live · tap one for its trades".into(),
            Style::default().add_modifier(Modifier::BOLD),
            None,
            &mut rows,
        );
        for (i, t) in lab.tracked.iter().enumerate() {
            let hit = Some(Hit::Lab(LabHit::Pick(i)));
            let mut head = Style::default().fg(Color::White);
            if i == lab.selected {
                head = head.fg(Color::Yellow).add_modifier(Modifier::BOLD);
            }
            let mark = if i == lab.selected { "▸" } else { " " };
            push(
                format!(
                    "{mark}{} {}",
                    i + 1,
                    t.found.strategy.describe(&report.input)
                ),
                head,
                hit,
                &mut rows,
            );
            let live = if t.live.n == 0 {
                "live: no trades yet".to_string()
            } else {
                format!("live {}", t.live.brief())
            };
            let open = t
                .open
                .map_or(String::new(), |o| format!(" · in a trade {o:+.1}bp"));
            let color = if t.live.n == 0 {
                Color::Gray
            } else if t.live.mean > 0.0 {
                Color::LightGreen
            } else {
                Color::LightRed
            };
            push(
                format!(
                    "↳ first 70% {} · last 30% {} · {live}{open} · found in search {}",
                    t.found.picked.brief(),
                    t.found.checked.brief(),
                    t.run
                ),
                Style::default().fg(color),
                hit,
                &mut rows,
            );
        }
        rows
    }

    fn lab_buttons(&self) -> Vec<Button<Hit>> {
        let Some(lab) = &self.lab else {
            return Vec::new();
        };
        let mut v = Vec::new();
        if !lab.tracked.is_empty() && !self.lab_theories {
            v.push(Button::new(
                format!("keep {}", lab.selected + 1),
                Hit::Lab(LabHit::Keep),
            ));
        }
        v.push(Button::new(
            if self.lab_theories {
                "strategies"
            } else {
                "theories"
            },
            Hit::Lab(LabHit::View),
        ));
        v.push(Button::new("search now", Hit::Lab(LabHit::SearchNow)).active(lab.searching()));
        v.push(Button::new("stop", Hit::Lab(LabHit::Stop)));
        v
    }

    fn draw_lab(
        &self,
        f: &mut Frame,
        area: Rect,
        rows: Vec<(Line<'static>, Option<Hit>)>,
        buttons: &[Button<Hit>],
    ) {
        let fee = self.lab.as_ref().map_or(0.0, |l| l.settings.fee);
        let block = Block::bordered()
            .border_style(Style::default().fg(Color::DarkGray))
            .title(format!(" discover · fee {fee}% a side "));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let bh = flow_height(buttons, inner.width).min(inner.height);
        let body_h = inner.height.saturating_sub(bh);
        for (k, (line, hit)) in rows.into_iter().take(body_h as usize).enumerate() {
            let r = Rect {
                y: inner.y + k as u16,
                height: 1,
                ..inner
            };
            f.render_widget(Paragraph::new(line), r);
            if let Some(h) = hit {
                self.hits.push(r, h);
            }
        }
        let b_area = Rect {
            y: inner.y + body_h,
            height: bh,
            ..inner
        };
        crate::ui::render_buttons_quiet(buttons, b_area, f.buffer_mut(), &self.hits);
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
pub(crate) mod tests {
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

    pub(crate) fn screen_of(app: &App, w: u16, h: u16) -> String {
        screen_sized(app, w, h)
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
        for _ in 0..7 {
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
        assert!(
            screen(&app).contains("cursor 2026-11-01 04:00   ● node1 59.9 GB   ● node2 422.6 GB")
        );
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
        let (x, y) = find(&app, " show all ");
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
        // Keyboard: r opens the dropdown, ↓ moves the highlight, Enter picks.
        key(&mut app, KeyCode::Char('r'));
        screen(&app);
        key(&mut app, KeyCode::Down);
        screen(&app);
        key(&mut app, KeyCode::Down);
        screen(&app);
        assert!(app.timeframe.is_all(), "moving doesn't pick yet");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.timeframe.last, Some(Window::Span(21600.0)));
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
            app.console_note.as_ref().map(|m| m.text.as_str()),
            Some("356.9 GB")
        );
        key(&mut app, KeyCode::Char('D'));
        let msg = &app.console_note.as_ref().unwrap().text;
        assert!(
            msg.starts_with("node1 -1.97 GB/day, empty in 29.1 days"),
            "{msg}"
        );
        key(&mut app, KeyCode::Char('W'));
        assert_eq!(app.opts.hlines[0].value, 50e9);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "nope(");
        key(&mut app, KeyCode::Enter);
        assert!(app.console_note.as_ref().unwrap().error);
        key(&mut app, KeyCode::Esc);
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
        // Right stops at the newest data; one more goes back to everything.
        let mut last_end = None;
        for _ in 0..50 {
            if let Some(v) = app.view {
                last_end = Some(v.1);
            }
            key(&mut app, KeyCode::Right);
        }
        assert!((last_end.unwrap() - b).abs() < 1e-6);
        assert_eq!(app.view, None);
        key(&mut app, KeyCode::Up);
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
        // measure is under ⋯.
        let (x, y) = find(&app, " ⋯ ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
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
        // Every command at once, above the console; tap one to put it in.
        let s = screen(&app);
        assert!(
            s.contains("console commands") && s.contains("trends  slope r2 eta forecast"),
            "{s}"
        );
        let (x, y) = find(&app, "slope ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert_eq!(app.console.as_ref().unwrap().value, "slope(\"");
        key(&mut app, KeyCode::Esc);
        assert!(!app.console_help && app.console.is_none());
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
            s.contains("lines: avg + median") && !s.contains("node1 live"),
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
            if app.console.is_some() {
                key(app, KeyCode::Esc);
            }
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
        assert!(app.console_note.as_ref().is_some_and(|m| m.error));
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
    fn panes_with_their_own_sources() {
        // The main chart is the log; a second pane runs a command of its own.
        let mut app = app(&[
            "graft",
            LOG,
            "--pane",
            "*",
            "--pane",
            "source=load kind=stat",
        ]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut s = String::new();
        while Instant::now() < deadline {
            app.tick();
            s = screen(&app);
            if s.contains("1 min") && s.contains("15 min") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            s.contains("reclaimable.log") && s.contains("1 min") && s.contains("15 min"),
            "{s}"
        );
        // The pane panel offers sources; pick "this chart" for pane 2 again.
        key(&mut app, KeyCode::Char('w'));
        key(&mut app, KeyCode::Tab);
        assert!(screen(&app).contains("load ▾"));
        key(&mut app, KeyCode::Char('d'));
        let s = screen(&app);
        assert!(
            s.contains("data for this pane") && s.contains("Load average"),
            "{s}"
        );
        let (x, y) = find(&app, "this chart's data · reclaimable.log");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.opts.panes.as_ref().unwrap().panes[1].source.is_none());
        assert!(
            app.subs.is_empty() || {
                app.tick();
                app.subs.is_empty()
            }
        );
        // And from the console; unknown names are refused.
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "pane_source(\"nope\")");
        key(&mut app, KeyCode::Enter);
        assert!(
            app.console_note
                .as_ref()
                .is_some_and(|m| m.error && m.text.contains("no profile named 'nope'"))
        );
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "pane_source(\"processes\")");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.current_cli().pane[1], "* kind=stat source=processes");
    }

    #[test]
    fn a_dashboard_needs_no_main_source() {
        let args: Vec<String> = [
            "graft",
            "--pane",
            "source=load",
            "--pane",
            "source=processes",
            "--layout",
            "1 | 2",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let cli = <Cli as clap::Parser>::parse_from(&args);
        let new = crate::sources::promote(&cli, &args)
            .unwrap()
            .expect("promoted");
        assert!(new.contains(&"cat /proc/loadavg".to_string()), "{new:?}");
        // Its main source is now a live command: build it without waiting for the end.
        let cli = <Cli as clap::Parser>::parse_from(&new);
        let spec = crate::setup::source_spec(&cli, true).unwrap();
        let setup = crate::setup::build(cli, spec.clone(), false).unwrap();
        let app = App::new(setup, Live::new(spec));
        let p = app.opts.panes.as_ref().unwrap();
        assert_eq!(
            (p.panes[0].source.as_deref(), p.panes[1].source.as_deref()),
            (None, Some("processes"))
        );
    }

    #[test]
    fn markers_from_rules_scripts_and_by_hand() {
        let mut app = app(&[
            "graft",
            LOG,
            "-s",
            "node2",
            "--mark",
            "delta < -1.5GB => fell {delta}",
        ]);
        assert_eq!(app.opts.marks.len(), 3, "{:?}", app.opts.marks);
        let s = screen(&app);
        assert!(s.contains("▼") && s.contains("fell −1.6 GB"), "{s}");
        // From the console: another rule, then a marker by hand at the cursor.
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "mark_when(\"pct > 0.3\", \"up {pct}\")");
        key(&mut app, KeyCode::Enter);
        let rises = app
            .opts
            .marks
            .iter()
            .filter(|m| m.label.starts_with("up +"))
            .count();
        assert!(rises > 0, "{:?}", app.opts.marks);
        let x = app.ds.series[0].points[5].0;
        app.cursor = Some(x);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "mark(\"reboot\")");
        key(&mut app, KeyCode::Enter);
        assert!(
            app.opts
                .marks
                .iter()
                .any(|m| m.label == "reboot" && m.x == x && m.series.is_none())
        );
        // The readout lists markers under the cursor; rules are kept in the command line.
        assert!(screen(&app).contains("▼ reboot"));
        assert_eq!(
            app.current_cli().mark,
            ["delta < -1.5GB => fell {delta}", "pct > 0.3 => up {pct}"]
        );
        // Bad rules are reported and dropped; clear_marks() removes everything.
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "mark_when(\"value\")");
        key(&mut app, KeyCode::Enter);
        assert!(
            app.console_note
                .as_ref()
                .is_some_and(|m| m.error && m.text.contains("needs a condition"))
        );
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.current_cli().mark.len(), 2);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "clear_marks()");
        key(&mut app, KeyCode::Enter);
        assert!(app.opts.marks.is_empty() && app.current_cli().mark.is_empty());
    }

    #[test]
    fn scripts_can_add_marker_rules() {
        let path = std::env::temp_dir().join(format!("graft-marks-{}.rhai", std::process::id()));
        std::fs::write(
            &path,
            "mark_when(\"name == `node1` && delta < -500MB\", \"drop\");\n",
        )
        .unwrap();
        let app = app(&["graft", LOG, "--script", path.to_str().unwrap()]);
        let _ = std::fs::remove_file(&path);
        assert!(!app.opts.marks.is_empty());
        assert!(
            app.opts
                .marks
                .iter()
                .all(|m| m.label == "drop" && m.series.as_deref() == Some("node1"))
        );
    }

    #[test]
    fn edit_and_delete_markers_from_the_menu() {
        let mut app = app(&[
            "graft",
            LOG,
            "--mark",
            "delta < -1.5GB => fell {delta}",
            "--mark",
            "pct > 0.3 => up",
        ]);
        let before = app.opts.marks.len();
        key(&mut app, KeyCode::Char('K'));
        let s = screen(&app);
        assert!(
            s.contains("delta < -1.5GB => fell {delta}") && s.contains("▼ 3"),
            "{s}"
        );
        // Enter edits the picked rule in the console; running it replaces the rule.
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.console.as_ref().unwrap().value,
            "mark_when(\"delta < -1.5GB\", \"fell {delta}\")"
        );
        app.console
            .as_mut()
            .unwrap()
            .set("mark_when(\"delta < -500MB\", \"fell {delta}\")");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.mark_rules.len(), 2);
        assert_eq!(app.mark_rules[0].expr, "delta < -500MB");
        assert!(app.opts.marks.len() > before);
        // Esc while editing changes nothing.
        key(&mut app, KeyCode::Char('K'));
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.mark_rules[0].expr, "delta < -500MB");
        // ✕ deletes a rule.
        key(&mut app, KeyCode::Char('K'));
        let (x, y) = find(&app, "pct > 0.3 => up");
        let row = screen(&app).lines().nth(y as usize).unwrap().to_string();
        let col = row.chars().position(|c| c == '✕').unwrap() as u16;
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), col, y);
        let _ = x;
        assert_eq!(app.current_cli().mark, ["delta < -500MB => fell {delta}"]);
        assert!(app.opts.marks.iter().all(|m| m.label.starts_with("fell")));
    }

    #[test]
    fn each_pane_has_its_own_lines() {
        // Stat tiles of the median above a chart of just the live data.
        let mut app = app(&[
            "graft",
            LOG,
            "--pane",
            "* kind=stat",
            "--pane",
            "* lines=live,median",
            "--layout",
            "1 / 2",
        ]);
        key(&mut app, KeyCode::Char('w'));
        key(&mut app, KeyCode::Down); // pick the chart
        key(&mut app, KeyCode::Char('T'));
        key(&mut app, KeyCode::Char('m')); // median off
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Up); // pick the tiles
        key(&mut app, KeyCode::Char('T'));
        key(&mut app, KeyCode::Char('m')); // median on
        key(&mut app, KeyCode::Char('l')); // live off
        key(&mut app, KeyCode::Esc);
        assert_eq!(
            app.current_cli().pane,
            ["* kind=stat lines=median", "* lines=live"]
        );
        let s = screen(&app);
        assert!(
            s.contains("node1 median") && !s.contains("node1 live"),
            "{s}"
        );
    }

    #[test]
    fn a_selected_range_drives_the_other_panes() {
        let mut app = app(&[
            "graft",
            LOG,
            "-s",
            "node1",
            "--pane",
            "* kind=stat lines=median",
            "--pane",
            "* lines=live",
            "--layout",
            "1 / 2",
        ]);
        screen(&app);
        let pts = app.ds.series[0].points.clone();
        // Measure the first six hours: the tile shows the median of just that stretch.
        let (a, b) = (pts[0].0, pts[6].0);
        app.anchor = None;
        app.finish_measure(a, b);
        let s = screen(&app);
        let mut v: Vec<f64> = pts[..=6].iter().map(|p| p.1).collect();
        v.sort_by(f64::total_cmp);
        let median =
            crate::units::Unit::new(crate::units::Dim::Bytes { binary: false }).format(v[3]);
        assert!(s.contains("selected 6 hours"), "{s}");
        let (num, _) = median.split_once(' ').unwrap();
        let big = crate::render::big_text(num).unwrap();
        assert!(
            s.contains(&big[0]),
            "median {median} of the range in big digits:\n{s}"
        );
        // A tap outside the range lets go of it.
        let plot = *app.plots.borrow().last().unwrap();
        let col = plot.col_of(pts[20].0);
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            col,
            plot.rect.y + 2,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            col,
            plot.rect.y + 2,
        );
        assert!(app.selection.is_none());
        assert!(!screen(&app).contains("selected 6 hours"));
    }

    #[test]
    fn undo_redo_and_side_by_side_keeps_panes() {
        let mut app = app(&[
            "graft",
            LOG,
            "--pane",
            "node1 kind=stat",
            "--pane",
            "node2",
            "--layout",
            "1 / 2",
        ]);
        let panes = app.opts.panes.clone();
        // Side by side on and off again: the panes come back.
        key(&mut app, KeyCode::Char('D'));
        assert!(app.opts.side_by_side && app.opts.panes.is_none());
        key(&mut app, KeyCode::Char('D'));
        assert!(!app.opts.side_by_side);
        assert_eq!(app.opts.panes, panes);
        // Several changes, then undo them one by one (Ctrl+Z and u), and redo.
        key(&mut app, KeyCode::Char('y')); // log scale
        key(&mut app, KeyCode::Char('s')); // stats
        key(&mut app, KeyCode::Right); // panning isn't a step
        let ctrl_z = TermEvent::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        app.on_event(&ctrl_z);
        assert!(app.opts.log && !app.show_stats);
        key(&mut app, KeyCode::Char('u'));
        assert!(!app.opts.log);
        key(&mut app, KeyCode::Char('U'));
        assert!(app.opts.log);
        // The undo button, and the console.
        let (x, y) = find(&app, "↶ undo");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(!app.opts.log);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "redo()");
        key(&mut app, KeyCode::Enter);
        assert!(app.opts.log);
        // Back to the very start, panes and all.
        while !app.undo.is_empty() {
            app.on_event(&ctrl_z);
        }
        assert!(!app.opts.log && !app.opts.side_by_side);
        assert_eq!(app.opts.panes, panes);
    }

    #[test]
    fn show_all_and_play_forward_back_to_everything() {
        let mut app = app(&["graft", LOG]);
        screen(&app);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        assert!(app.view.is_some());
        let (x, y) = find(&app, "show all");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.view.is_none());
        // ▶ moves right; at the newest data it goes back to everything.
        key(&mut app, KeyCode::Up);
        for _ in 0..20 {
            key(&mut app, KeyCode::Right);
        }
        assert!(app.view.is_none());
    }

    #[test]
    fn targets_button_is_off_without_targets() {
        let more = |app: &mut App| {
            if !screen(app).contains(" targets ") {
                let (x, y) = find(app, " ⋯ ");
                mouse(app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
            }
        };
        // Rising data, no targets: nothing to show, so the button isn't lit.
        let mut app = app(&["graft", LOG, "-s", "node1", "--target", "none"]);
        more(&mut app);
        assert!(!app.has_targets());
        key(&mut app, KeyCode::Char('G'));
        assert!(
            app.message
                .as_ref()
                .unwrap()
                .text
                .contains("no targets set")
        );
        let mut app = super::tests::app(&["graft", LOG, "--target", "node1=50GB"]);
        more(&mut app);
        assert!(app.has_targets());
    }

    #[test]
    fn custom_lines_title_and_unit_from_the_viewer() {
        let mut app = app(&[
            "graft",
            LOG,
            "-s",
            "node1",
            "--line",
            "ema5 = ema(values(\"node1\"), 5)",
        ]);
        assert!(app.ds.series.iter().any(|s| s.name == "ema5"));
        // The lines menu lists it; ✎ opens it in the console, same name replaces it.
        key(&mut app, KeyCode::Char('T'));
        let s = screen(&app);
        assert!(
            s.contains("custom lines (Rhai)") && s.contains("ema5 = ema(values(\"node1\"), 5)"),
            "{s}"
        );
        let (x, y) = find(&app, "ema5 = ema");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert_eq!(
            app.console.as_ref().unwrap().value,
            "add_line(\"ema5\", \"ema(values(\\\"node1\\\"), 5)\")"
        );
        app.console
            .as_mut()
            .unwrap()
            .set("add_line(\"ema5\", \"sma(values(\\\"node1\\\"), 3)\")");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.current_cli().line, ["ema5 = sma(values(\"node1\"), 3)"]);
        // Errors are reported, and the line left out.
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "add_line(\"bad\", \"values(\\\"nope\\\")\")");
        key(&mut app, KeyCode::Enter);
        assert!(
            app.console_note
                .as_ref()
                .is_some_and(|m| m.error && m.text.contains("no series called \"nope\"")),
            "{:?}",
            app.console_note.as_ref().map(|m| &m.text)
        );
        // The console stays open on the mistake; Esc closes it.
        assert_eq!(
            app.console.as_ref().unwrap().value,
            "add_line(\"bad\", \"values(\\\"nope\\\")\")"
        );
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "remove_line(\"bad\"); title(\"Disks\")");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.current_cli().line.len(), 1);
        assert!(screen(&app).contains("Disks"));
        // Undo brings the bad line back.
        key(&mut app, KeyCode::Char('u'));
        assert_eq!(app.current_cli().line.len(), 2);
    }

    #[test]
    fn load_a_script_while_running_and_mouse_key() {
        let mut app = app(&["graft", LOG, "-s", "node1"]);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, r#"load("examples/indicators.rhai")"#);
        key(&mut app, KeyCode::Enter);
        assert!(
            app.message.as_ref().unwrap().text.contains("3 functions"),
            "{:?}",
            app.message.as_ref().map(|m| &m.text)
        );
        key(&mut app, KeyCode::Char(':'));
        type_str(
            &mut app,
            r#"add_line("upper", "band(values(\"node1\"), 5, 2.0)"); add_line("e", "my_ema(values(\"node1\"), 5)")"#,
        );
        key(&mut app, KeyCode::Enter);
        assert!(
            app.ds.series.iter().any(|s| s.name == "upper"),
            "{:?}",
            app.message.as_ref().map(|m| &m.text)
        );
        assert!(app.ds.series.iter().any(|s| s.name == "e"));
        // The loaded script is kept in the command line.
        assert!(
            app.current_cli()
                .script
                .unwrap()
                .ends_with("examples/indicators.rhai")
        );
        assert!(app.message.as_ref().is_none_or(|m| !m.error));
        // m turns the mouse off, says how to get it back, and turns it on again.
        assert_eq!(key(&mut app, KeyCode::Char('m')), Transition::Mouse(false));
        app.message = None;
        assert!(screen(&app).contains("m turns it back on"));
        assert_eq!(key(&mut app, KeyCode::Char('m')), Transition::Mouse(true));
    }

    #[test]
    fn hide_only_and_custom_keybinds() {
        let mut app = app(&[
            "graft",
            LOG,
            "--line",
            "avg = mean_of([\"node1\", \"node2\"])",
            "--hide",
            "node1,node2",
            "--bind",
            "z = show_all()",
        ]);
        let chips = |app: &App| screen(app);
        assert!(app.hidden.contains("node1") && app.hidden.contains("node2"));
        assert!(chips(&app).contains("○ node1") && chips(&app).contains("● avg"));
        // A key bound on the command line.
        key(&mut app, KeyCode::Char('z'));
        assert!(app.hidden.is_empty());
        // only(), toggle(), and a key bound from the console (works straight away).
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, r#"only("avg"); bind("r", "toggle(\"node2\")")"#);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.hidden.len(), 2);
        key(&mut app, KeyCode::Char('r'));
        assert!(!app.hidden.contains("node2"));
        let cli = app.current_cli();
        assert_eq!(cli.hide, ["node1"]);
        assert_eq!(cli.bind, ["z = show_all()", "r = toggle(\"node2\")"]);
        // Unknown names are reported.
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, r#"only("nope")"#);
        key(&mut app, KeyCode::Enter);
        assert!(
            app.console_note
                .as_ref()
                .is_some_and(|m| m.error && m.text.contains("no series called"))
        );
    }

    #[test]
    fn markers_notify_about_new_ones_and_the_alerts_menu() {
        let dir = std::env::temp_dir().join(format!("graft-notify-ui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (log, out) = (dir.join("live.log"), dir.join("out.txt"));
        std::fs::write(
            &log,
            "2026-11-02 10:00:00 v=150\n2026-11-02 10:01:00 v=10\n",
        )
        .unwrap();
        let notify = format!("cmd:echo \"$GRAFT_ALERT\" >> {}", out.display());
        let args = [
            "graft",
            log.to_str().unwrap(),
            "--kv",
            "-f",
            "--mark",
            "notify value > 100 => big {value}",
            "--notify",
            &notify,
        ];
        let cli = <Cli as clap::Parser>::parse_from(args);
        let spec = crate::setup::source_spec(&cli, true).unwrap();
        let setup = crate::setup::build(cli, spec.clone(), false).unwrap();
        let mut app = App::new(setup, Live::new(spec));
        let settle = |app: &mut App| {
            for _ in 0..30 {
                app.tick();
                std::thread::sleep(Duration::from_millis(20));
            }
            app.rebuild(true);
        };
        settle(&mut app);
        assert_eq!(app.opts.marks.len(), 1, "the old marker is there");
        std::thread::sleep(Duration::from_millis(200));
        assert!(!out.exists(), "history doesn't notify");
        app.started -= Duration::from_secs(3);
        // A new matching line notifies.
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "2026-11-02 10:02:00 v=170").unwrap();
        drop(f);
        settle(&mut app);
        let mut got = String::new();
        for _ in 0..50 {
            got = std::fs::read_to_string(&out).unwrap_or_default();
            if !got.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(got.starts_with("big 170"), "{got}");
        // The markers menu shows rules (toolbar) and how often each fired (menu).
        assert!(screen(&app).contains("markers (1)"));
        key(&mut app, KeyCode::Char('K'));
        let s = screen(&app);
        assert!(s.contains("▼ 2 placed") && s.contains(" notify "), "{s}");
        key(&mut app, KeyCode::Esc);
        // The alerts menu: add one, see it firing, pick where notifications go.
        key(&mut app, KeyCode::Char('N'));
        assert!(screen(&app).contains("alerts & notifications"));
        let (x, y) = find(&app, "+ add alert");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        app.console
            .as_mut()
            .unwrap()
            .set(r#"alert("last(\"v\") > 100")"#);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.alerts, ["last(\"v\") > 100"]);
        key(&mut app, KeyCode::Char('N'));
        let s = screen(&app);
        assert!(s.contains("● firing") && s.contains("alerts (1)"), "{s}");
        let (x, y) = find(&app, " desktop ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.current_cli().notify.contains(&"desktop".to_string()));
        assert_eq!(app.current_cli().alert, ["last(\"v\") > 100"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_symbol_and_colour_from_the_menu() {
        let mut app = app(&[
            "graft",
            LOG,
            "-s",
            "node2",
            "--mark",
            "delta < -1.5GB => fell",
        ]);
        assert!(
            app.opts
                .marks
                .iter()
                .all(|m| m.symbol == "▼" && m.color.is_none())
        );
        key(&mut app, KeyCode::Char('K'));
        let row_of = |app: &App| {
            let s = screen(app);
            let y = s
                .lines()
                .position(|l| l.contains("delta < -1.5GB"))
                .unwrap();
            (s.lines().nth(y).unwrap().to_string(), y as u16)
        };
        let (_, y) = row_of(&app);
        let col = |line: &str, pat: &str| {
            line.chars()
                .collect::<String>()
                .find(pat)
                .map(|b| line[..b].chars().count() as u16)
                .unwrap()
        };
        // Its buttons are on the row below: tap the symbol (▼ → ▲), the colour
        // (auto → green) and the effect (plain → badge).
        let buttons =
            |app: &App, y: u16| screen(app).lines().nth(y as usize + 1).unwrap().to_string();
        let tap = |app: &mut App, pat: &str| {
            let line = buttons(app, y);
            let x = col(&line, pat);
            mouse(app, MouseEventKind::Down(MouseButton::Left), x + 1, y + 1);
        };
        tap(&mut app, " ▼ ");
        tap(&mut app, " auto ");
        tap(&mut app, " plain ");
        assert_eq!(app.mark_rules[0].effects, ["badge"]);
        assert!(app.opts.marks.iter().all(|m| m.badge));
        tap(&mut app, " badge ");
        assert_eq!(app.mark_rules[0].effects, ["bold"]);
        assert_eq!(app.mark_rules[0].symbol(), "▲");
        assert_eq!(app.mark_rules[0].color.as_deref(), Some("green"));
        assert!(
            app.opts
                .marks
                .iter()
                .all(|m| m.symbol == "▲" && m.color == Some(Color::Green))
        );
        assert_eq!(
            app.current_cli().mark,
            ["delta < -1.5GB => fell | ▲ green bold"]
        );
        // It fits a phone: the rule, then its buttons below it.
        let small = screen_sized(&app, 46, 30);
        assert!(small.contains("plain") || small.contains("bold"), "{small}");
        key(&mut app, KeyCode::Esc);
        let s = screen(&app);
        assert!(s.contains('▲') && s.contains("fell"), "{s}");
    }

    #[test]
    fn editing_a_rule_replaces_it_and_console_clicks_move_the_cursor() {
        let mut app = app(&["graft", LOG, "--mark", "delta < -1.5GB => fell | ▲ green"]);
        key(&mut app, KeyCode::Char('K'));
        key(&mut app, KeyCode::Enter);
        // Updates while typing (a live source) don't forget it's an edit.
        app.rebuild(true);
        app.rebuild(true);
        app.console
            .as_mut()
            .unwrap()
            .set(r#"mark_when("delta < -1GB", "fell more")"#);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.mark_rules.len(), 1, "{:?}", app.mark_rules);
        let r = &app.mark_rules[0];
        assert_eq!(
            (r.expr.as_str(), r.label.as_deref()),
            ("delta < -1GB", Some("fell more"))
        );
        assert_eq!(
            (r.symbol(), r.color.as_deref()),
            ("▲", Some("green")),
            "style kept"
        );
        // Clicking in the console line moves its cursor.
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "abcdef");
        screen(&app);
        let field = app.console_rect.get();
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            field.x + 2,
            field.y,
        );
        assert_eq!(app.console.as_ref().unwrap().cursor, 2);
    }

    #[test]
    fn merged_live_sources_redraw_in_rounds() {
        let run = |sync: bool| {
            let mut args = vec![
                "graft",
                "-c",
                "echo fast=1",
                "-n",
                "200ms",
                "--kv",
                "--merge",
                "slow = -c 'sleep 0.5; echo v=2' -n 200ms --kv",
            ];
            if !sync {
                args.push("--no-sync");
            }
            let cli = <Cli as clap::Parser>::parse_from(&args);
            let spec = crate::setup::source_spec(&cli, true).unwrap();
            let setup = crate::setup::build(cli, spec.clone(), false).unwrap();
            let mut app = App::new(setup, Live::new(spec));
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(2600) {
                app.tick();
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                app.ds.series.iter().any(|s| s.name == "slow"),
                "{:?}",
                app.ds.series.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            app.builds
        };
        let (rounds, free) = (run(true), run(false));
        // The slow source answers every ~0.7 s: about 3-4 rounds, against a redraw for
        // nearly every fast sample without sync.
        // (plus a few while starting up)
        assert!(rounds <= 8, "{rounds} rebuilds in rounds");
        assert!(free >= rounds + 3, "{free} without sync vs {rounds} with");
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

    fn run(app: &mut App, code: &str) {
        // (A mistake leaves the console open on it.)
        if app.console.is_some() {
            key(app, KeyCode::Esc);
        }
        key(app, KeyCode::Char(':'));
        app.console.as_mut().unwrap().set(code);
        key(app, KeyCode::Enter);
    }

    /// A price that bounces back after every sharp drop (something to discover).
    fn bouncy_log(n: usize) -> std::path::PathBuf {
        let mut seed = 7u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 10_000) as f64 / 10_000.0 - 0.5
        };
        let mut p = vec![100.0f64];
        let mut bounce = 0;
        for i in 1..n {
            let last = p[i - 1];
            let mut step = rnd() * 0.1;
            if bounce > 0 {
                step += 0.03;
                bounce -= 1;
            } else if i > 5 && (last / p[i - 5] - 1.0) * 1e4 < -12.0 {
                bounce = 5;
            }
            p.push(last * (1.0 + step / 100.0));
        }
        let path =
            std::env::temp_dir().join(format!("graft-bouncy-{}-{n}.log", std::process::id()));
        let text: String = p.iter().map(|v| format!("price={v:.4}\n")).collect();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn wait_for_search(app: &mut App) {
        let end = Instant::now() + Duration::from_secs(60);
        while app
            .lab
            .as_ref()
            .is_some_and(|l| l.report.is_none() || l.searching())
            && Instant::now() < end
        {
            app.tick_lab();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn discovers_paper_trades_and_keeps_a_strategy() {
        let path = bouncy_log(6000);
        let mut app = app(&["graft", path.to_str().unwrap(), "--kv"]);
        run(&mut app, "discover()");
        assert!(app.lab.is_some());
        wait_for_search(&mut app);
        let lab = app.lab.as_ref().unwrap();
        assert!(!lab.tracked.is_empty(), "{:?}", lab.waiting);
        assert!(lab.tracked[0].found.checked.mean > 0.0);
        // Its trades are on the chart.
        assert!(
            app.opts
                .marks
                .iter()
                .any(|m| m.symbol == "▲" || m.symbol == "▼")
        );
        // At 46 columns: the panel, a strategy, its record, the buttons.
        let s = screen_sized(&app, 46, 44);
        if std::env::var("SHOW").is_ok() {
            eprintln!("{s}");
        }
        assert!(s.contains("discover · fee 0% a side"), "{s}");
        assert!(s.contains("first 70%") && s.contains("last 30%"), "{s}");
        assert!(s.contains("keep 1") && s.contains("theories"), "{s}");
        // The other view: theories.
        let (x, y) = find(&app, "theories");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x, y);
        assert!(app.lab_theories);
        let s = screen_sized(&app, 46, 44);
        assert!(s.contains("held") || s.contains("nothing stood out"), "{s}");
        run(&mut app, "discover_pick(1)");
        assert!(!app.lab_theories);
        // Kept: formulas become hidden lines, entries a marker rule.
        run(&mut app, "discover_keep(1)");
        let note = app
            .console_note
            .as_ref()
            .map(|n| n.text.clone())
            .unwrap_or_default();
        assert!(
            note.starts_with("kept as the marker rule \"disc1 "),
            "{note}"
        );
        assert!(
            app.session
                .custom
                .iter()
                .any(|l| l.name == "disc1_1" && l.expr.starts_with("formula("))
        );
        assert!(app.hidden.contains("disc1_1"));
        assert!(
            app.mark_rules
                .iter()
                .any(|r| r.label.as_deref().is_some_and(|l| l.starts_with("disc1 ")))
        );
        assert!(
            app.opts.marks.iter().any(|m| m.rule.is_some()),
            "the kept rule places markers"
        );
        // Saved with the chart, and the lab with it.
        let args = app.current_cli().to_args();
        assert!(args.iter().any(|a| a.contains("formula(")), "{args:?}");
        assert!(args.contains(&"--discover".to_string()), "{args:?}");
        run(&mut app, "undiscover()");
        assert!(app.lab.is_none());
        assert!(
            !app.opts
                .marks
                .iter()
                .any(|m| m.rule.is_none() && m.symbol == "•")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn discover_waits_for_enough_data() {
        let mut app = app(&["graft", LOG]);
        key(&mut app, KeyCode::Char('E'));
        assert!(app.lab.is_some());
        app.tick_lab();
        let s = screen_sized(&app, 46, 30);
        assert!(s.contains("collecting data"), "{s}");
        key(&mut app, KeyCode::Char('E'));
        assert!(app.lab.is_none());
    }

    #[test]
    fn review_fixes_for_the_viewer() {
        // An edit ends with the command it opened: a rule added later (here by a key
        // binding) is a new rule, not a replacement.
        let mut app = super::tests::app(&[
            "graft",
            LOG,
            "--mark",
            "delta < -1.5GB => fell",
            "--bind",
            "x = mark_when(\"value > 400GB\", \"big\")",
        ]);
        key(&mut app, KeyCode::Char('K'));
        key(&mut app, KeyCode::Enter);
        app.console.as_mut().unwrap().set("title(\"other\")");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('x'));
        assert_eq!(app.mark_rules.len(), 2, "{:?}", app.mark_rules);
        assert_eq!(app.mark_rules[0].expr, "delta < -1.5GB");

        // Ranges, lines and titles set in the viewer are saved with it, and undo
        // takes them back.
        let mut app = super::tests::app(&["graft", LOG]);
        run(&mut app, r#"range("2026-11-01 12:00", "2026-11-02 00:00")"#);
        let c = app.current_cli();
        assert_eq!(
            (c.since.as_deref(), c.until.as_deref(), c.last),
            (Some("2026-11-01 12:00"), Some("2026-11-02 00:00"), None)
        );
        run(&mut app, r#"hline(50GB, "limit")"#);
        assert_eq!(app.current_cli().hline.len(), 1);
        run(&mut app, r#"title("disks")"#);
        assert_eq!(app.current_cli().title.as_deref(), Some("disks"));
        run(&mut app, r#"alert("last(\"node1\") < 1GB")"#);
        assert_eq!(app.alerts.len(), 1);
        for _ in 0..3 {
            key(&mut app, KeyCode::Char('u'));
        }
        let c = app.current_cli();
        assert!(c.hline.is_empty() && c.title.is_none() && app.alerts.is_empty());
        assert_eq!(app.opts.hlines.len(), 0);

        // Opening and closing the pane panel changes nothing (not even undo).
        let mut app = super::tests::app(&["graft", LOG]);
        let steps = app.undo.len();
        key(&mut app, KeyCode::Char('w'));
        assert!(app.pane_menu && app.opts.panes.is_some());
        key(&mut app, KeyCode::Char('w'));
        assert!(app.opts.panes.is_none());
        assert_eq!(app.undo.len(), steps);
        assert!(app.current_cli().pane.is_empty());
        // Undo with the panel open leaves no panel without panes.
        key(&mut app, KeyCode::Char('w'));
        key(&mut app, KeyCode::Char('|'));
        assert!(app.opts.panes.as_ref().is_some_and(|p| p.count() == 2));
        app.on_event(&TermEvent::Key(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.pane_menu == app.opts.panes.is_some());

        // The alerts menu fits a phone: every button is there at 46 columns.
        let mut app = super::tests::app(&["graft", LOG]);
        app.alerts_menu = true;
        let s = screen_sized(&app, 46, 40);
        for b in ["+ command", "+ add alert", "done"] {
            assert!(s.contains(b), "{b}:\n{s}");
        }

        // A binding on u wins over undo.
        let mut app = super::tests::app(&["graft", LOG, "--bind", "u = title(\"bound\")"]);
        key(&mut app, KeyCode::Char('u'));
        assert_eq!(app.opts.title, "bound");
    }

    fn tutorial_app() -> App {
        let (setup, live) = crate::tutorial::start().unwrap();
        let mut app = App::new(setup, live);
        let t0 = Instant::now();
        while app.ds.series.iter().map(|s| s.points.len()).sum::<usize>() < 2700
            && t0.elapsed() < Duration::from_secs(10)
        {
            app.tick();
        }
        app.rebuild(true);
        app
    }

    #[test]
    #[ignore]
    fn write_tutorial_screens() {
        let dir = std::env::var("GRAFT_SNAPSHOTS").unwrap();
        let mut app = tutorial_app();
        let steps: usize = std::env::var("GRAFT_TUTORIAL_STEP").map_or(0, |s| s.parse().unwrap());
        for _ in 0..steps {
            key(&mut app, KeyCode::Enter);
        }
        if let Ok(code) = std::env::var("GRAFT_TUTORIAL_RUN") {
            run(&mut app, &code);
        }
        for (w, h) in [(110, 34), (46, 40)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| app.draw(f)).unwrap();
            let ans = crate::render::ansi::to_ansi(term.backend().buffer(), true);
            std::fs::write(format!("{dir}/tutorial-{steps}-{w}.ans"), ans).unwrap();
        }
    }

    #[test]
    fn the_tutorial_walks_through_everything() {
        use crate::tutorial::{Check, STEPS};
        let mut app = tutorial_app();
        assert!(screen(&app).contains("tutorial 1/"));
        let done = |app: &App| app.tutorial.as_ref().unwrap().is_done();
        let at = |app: &App| STEPS[app.tutorial.as_ref().unwrap().step].check;
        // Each task, done the way the step says.
        let mut tried = Vec::new();
        while let Some(t) = &app.tutorial {
            let step = t.current();
            let check = step.check;
            if check != Check::Read {
                assert!(!done(&app), "{check:?} done before trying it");
            }
            match check {
                Check::Read => {
                    if step.command.is_some() {
                        app.tutorial_hit(TutorialHit::Copy);
                        assert_eq!(app.take_clipboard().as_deref(), step.command);
                    }
                }
                Check::Cursor => {
                    key(&mut app, KeyCode::Char('c'));
                }
                Check::Zoomed => {
                    key(&mut app, KeyCode::Char('+'));
                }
                Check::Selected => {
                    key(&mut app, KeyCode::Char('v'));
                    (0..6).for_each(|_| {
                        key(&mut app, KeyCode::Char(','));
                    });
                    key(&mut app, KeyCode::Char('v'));
                }
                Check::Hidden => {
                    key(&mut app, KeyCode::Char('3'));
                }
                Check::Trends => {
                    key(&mut app, KeyCode::Char('T'));
                    key(&mut app, KeyCode::Char('a'));
                    key(&mut app, KeyCode::Esc);
                }
                Check::Timeframe => {
                    key(&mut app, KeyCode::Char('R'));
                    key(&mut app, KeyCode::Esc);
                    run(&mut app, r#"range("5m")"#);
                }
                Check::Stats => {
                    key(&mut app, KeyCode::Char('s'));
                }
                Check::Forecast => {
                    key(&mut app, KeyCode::Char('f'));
                }
                Check::Kind => {
                    key(&mut app, KeyCode::Char('t'));
                }
                Check::Panes => {
                    key(&mut app, KeyCode::Char('w'));
                    key(&mut app, KeyCode::Char('|'));
                    key(&mut app, KeyCode::Char('w'));
                }
                Check::HLine => run(&mut app, r#"hline(50ms, "slow")"#),
                Check::Marker => {
                    run(&mut app, r#"mark_when("value > 60ms", "spike")"#);
                    assert!(
                        app.opts
                            .marks
                            .iter()
                            .any(|m| m.series.as_deref() == Some("db")),
                        "the db spikes are marked"
                    );
                }
                Check::Alert => run(&mut app, r#"alert("api > 45ms")"#),
                Check::Line => {
                    run(&mut app, r#"add_line("smooth", "ema(values(`api`), 30)")"#);
                    assert!(app.ds.series.iter().any(|s| s.name == "smooth"));
                }
                Check::Undo => {
                    app.on_event(&TermEvent::Key(KeyEvent::new(
                        KeyCode::Char('z'),
                        KeyModifiers::CONTROL,
                    )));
                }
                Check::Copied => {
                    key(&mut app, KeyCode::Char('C'));
                }
                Check::Discover => {
                    key(&mut app, KeyCode::Char('E'));
                    assert!(app.lab.is_some());
                    let phone = screen_sized(&app, 46, 40);
                    assert!(phone.contains("discover · fee"), "{phone}");
                    // Stopped again: the rest of the tour has the room.
                    key(&mut app, KeyCode::Char('E'));
                }
            };
            assert!(done(&app), "{check:?} not ticked off");
            assert!(
                app.message.as_ref().is_none_or(|m| !m.error),
                "{check:?}: {}",
                app.message.as_ref().map_or("", |m| m.text.as_str())
            );
            tried.push(at(&app));
            // Every step fits a phone screen, buttons included.
            let phone = screen_sized(&app, 46, 40);
            assert!(
                phone.contains("next ▶") || phone.contains("finish ✓"),
                "{phone}"
            );
            key(&mut app, KeyCode::Enter);
        }
        assert_eq!(tried.len(), STEPS.len());
        // Going back keeps what's done; hide folds the panel to a line.
        let mut app = tutorial_app();
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('c'));
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Backspace);
        assert!(done(&app));
        app.tutorial_hit(TutorialHit::Fold);
        let s = screen(&app);
        assert!(s.lines().next().unwrap().contains("tutorial 2/"), "{s}");
        app.tutorial_hit(TutorialHit::Fold);
        // Long steps scroll on a phone: the examples are further down.
        let (setup, live) = crate::tutorial::start().unwrap();
        let mut phone = App::new(setup, live);
        while phone.tutorial.as_ref().unwrap().current().title != "trend lines" {
            key(&mut phone, KeyCode::Enter);
        }
        let s = screen_sized(&phone, 46, 40);
        assert!(s.contains("↓ more") && !s.contains("--trend-only"), "{s}");
        for _ in 0..4 {
            key(&mut phone, KeyCode::PageDown);
            screen_sized(&phone, 46, 40);
        }
        let s = screen_sized(&phone, 46, 40);
        assert!(s.contains("--trend-only") && s.contains("▸ try:"), "{s}");
        key(&mut phone, KeyCode::PageUp);
        key(&mut phone, KeyCode::Enter);
        assert_eq!(
            phone.tutorial.as_ref().unwrap().scroll,
            0,
            "a new step starts at the top"
        );
        // Tapping next ▶ moves on, like Enter.
        let (col, row) = find(&app, "next ▶");
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            col + 1,
            row,
        );
        assert_eq!(app.tutorial.as_ref().unwrap().step, 2);
        app.tutorial_hit(TutorialHit::End);
        assert!(app.tutorial.is_none() && !screen(&app).contains("tutorial 3/"));
    }

    #[test]
    fn every_tutorial_example_works() {
        use crate::tutorial::STEPS;
        let examples = STEPS
            .iter()
            .flat_map(|s| s.examples.iter().map(|e| e.0))
            .chain(STEPS.iter().filter_map(|s| s.command));
        for ex in examples {
            if let Some(code) = ex.strip_prefix(':') {
                // Console examples run on the demo chart without an error. (Several on a
                // line, like `:undo()  :redo()`, run one by one.)
                let mut app = tutorial_app();
                for part in code.split("  :") {
                    if part == "help" || part.starts_with("load(") || part.starts_with("save(") {
                        continue;
                    }
                    app.message = None;
                    run(&mut app, part);
                    assert!(
                        app.message.as_ref().is_none_or(|m| !m.error),
                        "{ex}: {}",
                        app.message.as_ref().unwrap().text
                    );
                }
                continue;
            }
            // Flags and command lines parse (… stands for "the rest of your command").
            let line = ex.replace(" …", "").replace("graft … ", "graft ");
            let line = if line.starts_with('-') {
                format!("graft app.log {line}")
            } else {
                line
            };
            // `tail -f app.log | graft --kv`: the graft part.
            let cmd = match line.find(" | graft ") {
                Some(i) if !line.starts_with("graft") => &line[i + 3..],
                _ => line.as_str(),
            };
            let args = crate::sources::split_args(cmd).unwrap_or_else(|e| panic!("{ex}: {e}"));
            assert_eq!(args[0], "graft", "{ex}");
            let args = crate::profile::expand(args.iter().map(Into::into).collect())
                .unwrap_or_else(|e| panic!("{ex}: {e}"));
            if let Err(e) = <Cli as clap::Parser>::try_parse_from(&args) {
                panic!("{ex}: {e}");
            }
        }
    }

    /// The tutorial covers every feature: each flag, toolbar button and console function
    /// is mentioned in some step. Adding a feature? Add it to a step in src/tutorial.rs
    /// (or, if it really doesn't belong in the tour, to the lists here with a reason).
    #[test]
    fn the_tutorial_covers_every_feature() {
        use crate::tutorial::STEPS;
        use clap::CommandFactory;
        let mut corpus = String::new();
        for s in STEPS {
            for t in [
                s.title,
                s.text,
                s.task.unwrap_or(""),
                s.command.unwrap_or(""),
            ] {
                corpus.push_str(t);
                corpus.push('\n');
            }
            for (a, b) in s.keys.iter().chain(s.examples) {
                corpus.push_str(&format!("{a}\n{b}\n"));
            }
        }
        // Buttons belong in a step's keys table, next to their keys.
        let keys_column: String = STEPS
            .iter()
            .flat_map(|s| s.keys.iter().map(|k| format!("{}\n", k.0)))
            .collect();
        let mut missing = Vec::new();

        // Flags: the long name, or the short form in an example.
        const NOT_FLAGS: &[&str] = &[
            "help",        // clap's own
            "version",     // clap's own
            "completions", // shell setup, in the README
            "man",         // packaging, in the README
        ];
        let mentioned = |pat: &str| {
            corpus.match_indices(pat).any(|(i, _)| {
                corpus[i + pat.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-'))
            })
        };
        for arg in Cli::command().get_arguments() {
            let Some(long) = arg.get_long() else {
                continue;
            };
            let short = arg
                .get_short()
                .is_some_and(|c| mentioned(&format!(" -{c}")) || mentioned(&format!("\n-{c}")));
            if !NOT_FLAGS.contains(&long) && !mentioned(&format!("--{long}")) && !short {
                missing.push(format!("flag --{long}"));
            }
        }

        // Toolbar buttons: all of them (⋯ open, with markers and alerts).
        let mut app = tutorial_app();
        app.toolbar_more = true;
        run(&mut app, r#"alert("api > 1s")"#);
        run(&mut app, r#"mark_when("value > 60ms", "x")"#);
        for b in app.toolbar() {
            let label = match b.action {
                // Its label is the current chart type.
                Hit::Cmd(Cmd::Kind) => "chart type".to_string(),
                _ => {
                    // "markers (1)" → markers, "lines: live" → lines, "all time ▾" → all time
                    let l = b.label.split(" (").next().unwrap_or_default();
                    let l = l.split(": ").next().unwrap_or_default();
                    l.trim_end_matches(" ▾").to_string()
                }
            };
            if !keys_column.contains(&label) {
                missing.push(format!("toolbar button '{}' (in a keys table)", b.label));
            }
        }

        // Console functions, as registered in src/script.rs.
        const NOT_FUNCTIONS: &[&str] = &[
            "help", // `:help` is in the tour (no brackets)
            "raw",  // another name for live()
        ];
        let src = include_str!("script.rs");
        let named = |re: &str| -> Vec<String> {
            regex::Regex::new(re)
                .unwrap()
                .captures_iter(src)
                .map(|c| c[1].to_string())
                .collect()
        };
        let mut names = named(r#"register_fn\(\s*"([a-z_0-9]+)""#);
        // Registered in loops: ("first", |s| …), ("values", false), FLAGS, range/timeframe.
        names.extend(named(r#"\("([a-z_0-9]+)", (?:\|s\||false|true)"#));
        names.extend(crate::script::FLAGS.iter().map(|f| f.0.to_string()));
        names.extend(["range".to_string(), "timeframe".to_string()]);
        names.sort();
        names.dedup();
        assert!(names.len() > 80, "only found {}: {names:?}", names.len());
        for n in names {
            // As a whole name: `at(` isn't mentioned by `values_at(`.
            let named = Regex::new(&format!(r"(?:^|[^a-z0-9_]){n}\(")).unwrap();
            if !NOT_FUNCTIONS.contains(&n.as_str()) && !named.is_match(&corpus) {
                missing.push(format!("console function {n}()"));
            }
        }

        assert!(
            missing.is_empty(),
            "not in the tutorial yet: add each to a step in src/tutorial.rs (its text, keys \
             or examples):\n  {}",
            missing.join("\n  ")
        );
    }

    #[test]
    fn console_explains_results_and_mistakes() {
        let mut app = tutorial_app();
        let out = |app: &mut App, code: &str| {
            run(app, code);
            let n = app.console_note.as_ref().expect("output");
            (n.error, n.text.clone())
        };
        // Values in the series' unit, saying which point they're from.
        let (err, t) = out(&mut app, r#"last("api")"#);
        assert!(
            !err && t.starts_with("api: ") && t.contains(" ms — the newest value ("),
            "{t}"
        );
        assert_eq!(
            out(&mut app, "api").1,
            t,
            "a series' name alone is its newest value"
        );
        // Trends in words, never scientific notation.
        let t = out(&mut app, r#"slope("api", "1h")"#).1;
        assert!(
            t.starts_with("api is rising ") && t.contains(" ms per hour"),
            "{t}"
        );
        assert_eq!(out(&mut app, r#"slope("api", 1h)"#).1, t);
        let t = out(&mut app, r#"slope("api")"#).1;
        assert!(
            !Regex::new(r"\de[-+]?\d").unwrap().is_match(&t),
            "no 4.8e-4: {t}"
        );
        assert!(out(&mut app, r#"eta("api", 60ms)"#).1.starts_with("in "));
        assert!(out(&mut app, "api - web").1.ends_with(" ms"));
        // With a cursor placed, the value there too; at() is the value there.
        key(&mut app, KeyCode::Home);
        let t = out(&mut app, r#"mean("db")"#).1;
        assert!(t.contains(" · at the cursor ("), "{t}");
        assert!(out(&mut app, r#"at("api")"#).1.contains("at the cursor"));
        assert!(
            screen(&app).contains("cursor 09:") || screen(&app).contains("cursor 1"),
            "the legend row says it's the cursor"
        );
        // `5m` is five minutes.
        // A series written plainly is its name where one is wanted: last(api).
        assert_eq!(
            out(&mut app, "last(api)").1,
            out(&mut app, r#"last("api")"#).1
        );
        assert!(out(&mut app, "slope(api, 1h)").1.contains(" per hour"));
        assert!(!out(&mut app, "mean_of([web, api])").0);
        // A line from series by name is a real line, not a flat one.
        run(&mut app, r#"add_line("total", "web + api")"#);
        let total = app
            .ds
            .series
            .iter()
            .find(|s| s.name == "total")
            .expect("total");
        assert!(total.points.len() > 100);
        assert!(
            total.points.windows(2).any(|w| w[0].1 != w[1].1),
            "it varies"
        );
        run(&mut app, r#"zoom(5m)"#);
        assert!(app.view.is_some_and(|(a, b)| (b - a - 300.0).abs() < 1.0));
        // Mistakes: plain words, a fix, and the console stays open on the command.
        for (code, says) in [
            ("lst(api)", "did you mean last(api)?"),
            (r#"lst("api")"#, r#"did you mean last("api")?"#),
            (r#"last("apu")"#, r#"did you mean "api"?"#),
            ("zoom(5x)", "5x isn't a number graft understands"),
            (r#"last("api""#, "last( is missing its closing )"),
            ("apo + 1", "did you mean api?"),
            (
                r#"slope("api", "1h", 3)"#,
                "slope() doesn't take (text, text, a number)",
            ),
            (r#"hline("x")"#, "hline needs a number — e.g. hline(50ms)"),
            (r#"mark_when("value")"#, "needs a condition"),
            (r#"chart("pie")"#, "there's no chart type 'pie'"),
        ] {
            let (err, t) = out(&mut app, code);
            assert!(err && t.contains(says), "{code}: {t}");
            assert_eq!(
                app.console.as_ref().map(|c| c.value.as_str()),
                Some(code),
                "{code}"
            );
        }
        // The cursor goes to the mistake.
        out(&mut app, "zoom(5x)");
        assert_eq!(app.console.as_ref().unwrap().cursor, 5);
    }

    #[test]
    fn console_completes_commands_and_series() {
        let mut app = tutorial_app();
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "sl");
        // The hint row: how it's used, and what Tab would put in.
        let s = screen_sized(&app, 46, 30);
        assert!(
            s.contains(r#"slope("api")  ·  slope("#) && s.contains(" slope() "),
            "{s}"
        );
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.console.as_ref().unwrap().value, r#"slope(""#);
        // Inside the quotes: the series. Tab cycles through them.
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.console.as_ref().unwrap().value, r#"slope("web")"#);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.console.as_ref().unwrap().value, r#"slope("api")"#);
        key(&mut app, KeyCode::BackTab);
        assert_eq!(app.console.as_ref().unwrap().value, r#"slope("web")"#);
        // A letter narrows it; a tap works like Tab.
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, r#"max("d"#);
        // (The suggestion in the hint row, not db in the legend row.)
        let s = screen(&app);
        let (y, line) = s
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains(" Tab ") && l.contains(" db "))
            .expect("hint row");
        let x = line[..line.find(" db ").unwrap()].chars().count() as u16;
        let y = y as u16;
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert_eq!(app.console.as_ref().unwrap().value, r#"max("db")"#);
        key(&mut app, KeyCode::Enter);
        assert!(
            app.console_note
                .as_ref()
                .is_some_and(|n| n.text.starts_with("db: "))
        );
        // Choices for other arguments: chart types, durations.
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, r#"chart("hi"#);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.console.as_ref().unwrap().value, r#"chart("histogram")"#);
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, r#"slope("api", "#);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.console.as_ref().unwrap().value, r#"slope("api", "1m")"#);
    }

    #[test]
    #[ignore]
    fn write_console_screens() {
        let dir = std::env::var("GRAFT_SNAPSHOTS").unwrap();
        let mut app = tutorial_app();
        app.tutorial = None;
        let shot = |app: &App, name: &str, w: u16, h: u16| {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| app.draw(f)).unwrap();
            let ans = crate::render::ansi::to_ansi(term.backend().buffer(), true);
            std::fs::write(format!("{dir}/console-{name}.ans"), ans).unwrap();
        };
        key(&mut app, KeyCode::Home);
        run(&mut app, r#"mean("db")"#);
        shot(&app, "result", 46, 30);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, r#"slope(""#);
        shot(&app, "complete", 46, 30);
        key(&mut app, KeyCode::Esc);
        run(&mut app, "last(api)");
        shot(&app, "error", 46, 30);
        key(&mut app, KeyCode::Esc);
        run(&mut app, "help");
        shot(&app, "help", 46, 40);
        shot(&app, "help-wide", 110, 34);
        key(&mut app, KeyCode::Esc);
        app.cli.merge.push("cb = -c 'curl -s https://api.coinbase.com/v2/prices/ETH-USD/spot' --regex 'amount\\W+(?P<p>[0-9.]+)' -n 2s".into());
        app.cli.merge.push(
            "pi = -c 'vcgencmd measure_temp' --ssh pi@raspberrypi --regex 'temp=(?P<t>[0-9.]+)'"
                .into(),
        );
        key(&mut app, KeyCode::Char('S'));
        shot(&app, "sources", 46, 30);
        shot(&app, "sources-wide", 110, 34);
    }

    #[test]
    #[ignore]
    fn write_ui_screens() {
        let dir = std::env::var("GRAFT_SNAPSHOTS").unwrap();
        let shot = |app: &App, name: &str, w: u16, h: u16| {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| app.draw(f)).unwrap();
            let ans = crate::render::ansi::to_ansi(term.backend().buffer(), true);
            std::fs::write(format!("{dir}/ui-{name}.ans"), ans).unwrap();
        };
        let mut app = tutorial_app();
        app.tutorial = None;
        app.message = None;
        shot(&app, "plain", 110, 34);
        shot(&app, "plain-phone", 46, 40);
        key(&mut app, KeyCode::Char('s'));
        run(&mut app, r#"lines("live,avg"); forecast("10m")"#);
        app.console_note = None;
        key(&mut app, KeyCode::Char('c'));
        shot(&app, "busy", 110, 34);
        shot(&app, "busy-phone", 46, 40);
        let mut app = tutorial_app();
        app.tutorial = None;
        run(&mut app, r#"layout("(1 | 2) / (3 | 4)")"#);
        run(&mut app, r#"focus_pane(1); pane("web,api kind=bar")"#);
        run(&mut app, r#"focus_pane(2); pane("api lines=live,avg")"#);
        run(&mut app, r#"focus_pane(3); pane("db kind=histogram")"#);
        run(&mut app, r#"focus_pane(4); pane("web,api,db kind=stat")"#);
        app.console_note = None;
        app.message = None;
        shot(&app, "panes", 110, 34);
        key(&mut app, KeyCode::Char('w'));
        shot(&app, "panes-panel", 110, 34);
    }

    #[test]
    fn sources_menu_lists_pane_sources() {
        let mut app = app(&["graft", LOG, "--kv", "--pane", "*", "--pane", "source=load"]);
        key(&mut app, KeyCode::Char('S'));
        let s = screen(&app);
        assert!(s.contains("main · pane 1"), "{s}");
        assert!(
            s.contains("pane 2 · load") && s.contains("cat /proc/loadavg"),
            "{s}"
        );
        // other source…: pick that pane and choose its source.
        let (x, y) = find(&app, " other source… ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.source_menu.is_some() && !app.sources_menu);
        assert_eq!(app.opts.panes.as_ref().unwrap().focus, 1);
    }

    #[test]
    fn front_draws_a_series_on_top() {
        let mut app = app(&["graft", LOG, "--pane", "*", "--pane", "node2"]);
        run(&mut app, "front(node1)");
        let p = app.opts.panes.as_ref().unwrap();
        assert_eq!(p.panes[p.focus].front, ["node1"], "the picked pane");
        assert!(
            app.current_cli()
                .pane
                .iter()
                .any(|s| s.contains("front=node1"))
        );
        run(&mut app, "front()");
        let p = app.opts.panes.as_ref().unwrap();
        assert!(p.panes[p.focus].front.is_empty());
        let mut single = super::tests::app(&["graft", LOG, "--front", "node1"]);
        assert_eq!(single.opts.front, ["node1"]);
        run(&mut single, "front(node2)");
        assert_eq!(single.current_cli().front, ["node2"]);
    }

    #[test]
    fn candles_are_one_chip_and_bring_their_series() {
        let candles = r#"echo '[{"t":60000,"o":1,"h":3,"l":0.5,"c":2,"v":7},{"t":120000,"o":2,"h":2.5,"l":1,"c":1.5,"v":9}]'"#;
        let mut app = app(&[
            "graft",
            "-c",
            candles,
            "--count",
            "1",
            "--json",
            "--time-field",
            "t",
            "--pane",
            "v kind=candles",
            "--pane",
            "v",
        ]);
        let t0 = Instant::now();
        while !app.live.eof && t0.elapsed() < Duration::from_secs(5) {
            app.tick();
        }
        app.rebuild(true);
        // The candles pane lists only v: its o/h/l/c come along, as one chip.
        let p = &app.opts.panes.as_ref().unwrap().panes[0];
        let (ds, _) = crate::render::pane_view(&app.ds, &app.opts, p);
        assert_eq!(ds.series.len(), 5);
        let labels: Vec<String> = app.chips().into_iter().map(|b| b.label).collect();
        assert!(labels.contains(&"● candles".to_string()), "{labels:?}");
        assert!(
            !labels.iter().any(|l| l == "● o" || l == "● c"),
            "{labels:?}"
        );
        let s = screen_sized(&app, 46, 30);
        assert!(
            s.contains("candles 1.5"),
            "the close, in the legend row: {s}"
        );
        // Tapping it hides (and shows) all four.
        app.toggle_candles();
        assert!(["o", "h", "l", "c"].iter().all(|n| app.hidden.contains(*n)));
        app.toggle_candles();
        assert!(app.hidden.is_empty());
        // Picking candles in a pane without them puts them in.
        run(&mut app, "focus_pane(\"down\")");
        if app.opts.panes.as_ref().unwrap().focus == 0 {
            run(&mut app, "focus_pane(\"right\")");
        }
        run(&mut app, "chart(\"candles\")");
        let p = app.opts.panes.as_ref().unwrap();
        let pane = &p.panes[p.focus];
        assert!(
            p.focus == 1 && ["o", "h", "l", "c"].iter().all(|n| pane.shows(n)),
            "{pane:?}"
        );
    }

    #[test]
    fn going_back_from_a_last_window_keeps_its_width() {
        let mut app = app(&["graft", LOG]);
        run(&mut app, "timeframe(6h)");
        let (a, b) = app.bounds();
        key(&mut app, KeyCode::Left);
        let (c, d) = app.view.expect("a window to move");
        assert_eq!(app.timeframe.last, Some(Window::Span(21600.0)), "still 6h");
        assert!((d - c - (b - a)).abs() < 1.0, "still 6 hours wide");
        assert!(d < b && c < a, "earlier than before: {a}..{b} → {c}..{d}");
        key(&mut app, KeyCode::Left);
        assert!(app.view.unwrap().1 < d, "and on back");
        // → back to the newest: the last 6 hours again, live.
        for _ in 0..20 {
            key(&mut app, KeyCode::Right);
            app.tick();
        }
        assert!(app.view.is_none() && !app.paged);
        assert_eq!(app.bounds(), (a, b));
    }

    #[test]
    fn backtests_from_the_console() {
        let mut app = app(&[
            "graft",
            LOG,
            "-s",
            "node1",
            "--mark",
            "delta < 0 => fell",
            "--mark",
            "delta > 0 => rose",
        ]);
        let note = |app: &App| app.console_note.as_ref().map(|n| (n.text.clone(), n.error));
        run(&mut app, r#"backtest("fell", 1)"#);
        let (text, error) = note(&app).unwrap();
        assert!(
            !error && text.starts_with("fell: ") && text.contains("1 point later"),
            "{text}"
        );
        run(&mut app, r#"backtest(1, "node1", "1h")"#);
        assert!(
            note(&app).unwrap().0.contains("60 min later"),
            "{:?}",
            note(&app)
        );
        run(&mut app, r#"strategy("fell", "rose")"#);
        let (text, error) = note(&app).unwrap();
        assert!(!error && text.starts_with("fell → rose on node1"), "{text}");
        // The console offers the rules by label.
        let labels = |app: &App| -> Vec<String> {
            app.completions()
                .items
                .into_iter()
                .map(|i| i.label)
                .collect()
        };
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "backtest(");
        assert_eq!(labels(&app), ["fell", "rose"]);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.console.as_ref().unwrap().value, r#"backtest("fell""#);
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, r#"strategy("fell", "r"#);
        assert_eq!(labels(&app), ["rose"]);
        key(&mut app, KeyCode::Esc);
        run(&mut app, r#"backtest("nope")"#);
        let (text, error) = note(&app).unwrap();
        assert!(error && text.contains("1 fell, 2 rose"), "{text}");
    }

    #[test]
    fn strategies_and_sweeps() {
        let mut app = app(&[
            "graft",
            LOG,
            "-s",
            "node1",
            "--param",
            "n = 2 | 4",
            "--line",
            "avg = ema(values(\"node1\"), {n})",
            "--mark",
            "cross_above(node1, avg) => up in",
            "--mark",
            "cross_below(node1, avg) => up out",
        ]);
        let note = |app: &App| app.console_note.as_ref().unwrap().text.clone();
        run(&mut app, "strategies(0.05)");
        let text = note(&app);
        assert!(text.starts_with("1 strategy on node1 · holding"), "{text}");
        assert!(
            text.contains("fee 0.05%") && text.contains("\nup: long "),
            "{text}"
        );
        assert!(
            text.contains("· short ") && text.contains("best on the first 70%"),
            "{text}"
        );
        run(&mut app, "sweep()");
        let text = note(&app);
        assert!(
            text.starts_with("sweep: 2 settings × 1 strategy × 72 ways to trade"),
            "{text}"
        );
        assert!(
            text.contains("with n=") && text.contains("last 30%"),
            "{text}"
        );
        assert!(text.contains("SL") && text.contains("TP"), "{text}");
        // The chart is as it was.
        assert!(app.ds.get("avg").is_some());
    }

    #[test]
    fn result_tables_stay_until_closed() {
        let mut app = app(&["graft", LOG]);
        app.note("a table\nof results".into(), false);
        app.console_note.as_mut().unwrap().at = Instant::now() - Duration::from_secs(120);
        assert!(
            screen(&app).contains("of results"),
            "still there two minutes on"
        );
        key(&mut app, KeyCode::Esc);
        assert!(app.console_note.is_none());
        // A tap closes it too.
        app.note("a table\nof results".into(), false);
        let (x, y) = find(&app, "of results");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x, y);
        assert!(app.console_note.is_none());
    }

    #[test]
    fn messages_dont_move_the_chart() {
        let mut app = app(&["graft", LOG]);
        screen_sized(&app, 46, 30);
        let before = app.chart_rect.get();
        app.say(
            "older data loaded — a long message that would wrap the legend row",
            false,
        );
        let s = screen_sized(&app, 46, 30);
        assert_eq!(app.chart_rect.get(), before);
        assert!(s.contains("older data loaded"), "{s}");
    }

    #[test]
    fn popups_work_with_arrow_keys() {
        let mut app = app(&["graft", LOG]);
        // The timeframe menu: arrows move the pick (the timeframe stays), Enter presses it.
        key(&mut app, KeyCode::Char('r'));
        screen(&app);
        let n = app.popup_buttons().len();
        assert!(n > 3, "{n}");
        for _ in 0..n + 2 {
            key(&mut app, KeyCode::Down);
            screen(&app);
        }
        assert!(app.timeframe.is_all(), "moving doesn't pick");
        // The last row is custom…: Enter opens the console with range(".
        let picked = app.popup_buttons()[app.popup_focus.get()].1;
        assert_eq!(picked, Hit::CustomRange);
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.console.as_ref().map(|c| c.value.as_str()),
            Some(r#"range(""#)
        );
        // The highlight is drawn.
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char('r'));
        screen(&app);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert!(
            !app.timeframe.is_all() && !app.range_menu,
            "a preset was picked"
        );
        // The sources menu too: Tab to done, Enter closes it.
        key(&mut app, KeyCode::Char('S'));
        screen(&app);
        let done = app
            .popup_buttons()
            .iter()
            .position(|b| b.1 == Hit::CloseSources)
            .unwrap();
        for _ in 0..done {
            key(&mut app, KeyCode::Tab);
        }
        key(&mut app, KeyCode::Enter);
        assert!(!app.sources_menu);
    }

    #[test]
    fn toggles_say_when_they_go_off() {
        let mut app = app(&["graft", LOG]);
        key(&mut app, KeyCode::Char('f'));
        assert!(
            app.message
                .as_ref()
                .unwrap()
                .text
                .starts_with("forecast on")
        );
        key(&mut app, KeyCode::Char('f'));
        assert_eq!(app.message.as_ref().unwrap().text, "forecast off");
    }

    #[test]
    fn dashboard_panes_keep_their_legends() {
        let mut app = app(&["graft", LOG, "--pane", "*", "--pane", "node2"]);
        let s = screen_sized(&app, 160, 40);
        // A pane with several series keeps its box (the row lists the picked pane's).
        assert!(s.contains("│node1 58.8 GB "), "{s}");
        // One chart: the legend row does it, no box.
        run(&mut app, "single()");
        assert!(!screen_sized(&app, 160, 40).contains("│node1 58.8 GB "));
    }

    #[test]
    fn expressions_without_quotes_and_lines_in_source_panes() {
        let mut app = app(&["graft", LOG]);
        // Code arguments can be written as code.
        run(&mut app, "add_line(\"total\", node1 + node2)");
        let total = app
            .ds
            .series
            .iter()
            .find(|s| s.name == "total")
            .expect("total");
        assert!(total.points.len() > 20);
        run(&mut app, "alert(node1 < 59GB)");
        assert_eq!(app.alerts, ["node1 < 59GB"]);
        run(&mut app, r#"mark_when(delta < -1GB, "drop")"#);
        assert_eq!(app.mark_rules.last().unwrap().expr, "delta < -1GB");
        // A pane with its own source can't take a custom line: it says why.
        let mut app =
            super::tests::app(&["graft", LOG, "--kv", "--pane", "*", "--pane", "source=load"]);
        run(&mut app, "focus_pane(2)");
        run(&mut app, "add_line(\"t\", node1 + node2)");
        let n = app.console_note.as_ref().unwrap();
        assert!(
            n.error && n.text.contains("this pane shows 'load'") && n.text.contains("pick pane 1"),
            "{}",
            n.text
        );
        assert!(app.session.custom.is_empty());
    }

    #[test]
    fn tutorial_examples_can_be_tapped() {
        let mut app = tutorial_app();
        // The welcome step: a command line is copied (scrolled down to it).
        screen(&app);
        for _ in 0..4 {
            key(&mut app, KeyCode::PageDown);
            screen(&app);
        }
        let (x, y) = find(&app, "graft app.log --kv");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert_eq!(app.take_clipboard().as_deref(), Some("graft app.log --kv"));
        // A console example goes into the console.
        while app.tutorial.as_ref().unwrap().current().title != "read a value" {
            key(&mut app, KeyCode::Enter);
        }
        screen(&app);
        for _ in 0..4 {
            key(&mut app, KeyCode::PageDown);
            screen(&app);
        }
        let (x, y) = find(&app, r#":at("api")"#);
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert_eq!(
            app.console.as_ref().map(|c| c.value.as_str()),
            Some(r#"at("api")"#)
        );
    }

    #[test]
    fn source_parameters_rerun_the_source() {
        let mut app = app(&[
            "graft",
            "-c",
            "echo {interval}=1",
            "--count",
            "1",
            "--kv",
            "--param",
            "interval = m1 | h1",
            "--title",
            "ETH {interval}",
            "--line",
            "per_{interval}:{interval} = m1 * 2",
        ]);
        assert_eq!(app.ds_names(), ["m1", "per_m1"]);
        assert_eq!(
            app.ds.series[1].unit.format(2.0),
            "2m1",
            "settings fill units too"
        );
        assert_eq!(app.view_opts().title, "ETH m1", "titles say the setting");
        // Where you were (a window you'd moved to, the cursor) stays across a change.
        app.view = Some((100.0, 200.0));
        app.cursor = Some(150.0);
        // The toolbar has a dropdown; picking a value runs the source again.
        let (x, y) = find(&app, "interval: m1 ▾");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert!(app.param_menu.is_some());
        let (x, y) = find(&app, "  h1");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        let t0 = Instant::now();
        while !app.live.eof && t0.elapsed() < Duration::from_secs(5) {
            app.tick();
        }
        app.rebuild(true);
        assert_eq!(app.ds_names(), ["h1"], "fresh data with the new value");
        assert_eq!(
            (app.view, app.cursor),
            (Some((100.0, 200.0)), Some(150.0)),
            "still where you were"
        );
        assert_eq!(
            app.current_cli().line,
            ["per_{interval}:{interval} = m1 * 2"],
            "saved as written"
        );
        assert_eq!(app.view_opts().title, "ETH h1");
        assert_eq!(
            app.current_cli().title.as_deref(),
            Some("ETH {interval}"),
            "saved as written"
        );
        assert_eq!(app.current_cli().param, ["interval = m1 | *h1"]);
        // The console completes the setting's name, then its values.
        key(&mut app, KeyCode::Char(':'));
        type_str(&mut app, "param(");
        let labels = |app: &App| -> Vec<String> {
            app.completions()
                .items
                .into_iter()
                .map(|i| i.label)
                .collect()
        };
        assert_eq!(labels(&app), ["interval"]);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.console.as_ref().unwrap().value, r#"param("interval""#);
        type_str(&mut app, ", ");
        assert_eq!(labels(&app), ["m1", "h1"]);
        type_str(&mut app, r#""h"#);
        assert_eq!(labels(&app), ["h1"]);
        key(&mut app, KeyCode::Esc);
        // And from the console; a value it doesn't have is refused, with the choices.
        run(&mut app, r#"param("interval", "d1")"#);
        let n = app.console_note.as_ref().unwrap();
        assert!(n.error && n.text.contains("m1, h1"), "{}", n.text);
        run(&mut app, r#"param("coin", "ETH")"#);
        assert!(
            app.console_note
                .as_ref()
                .unwrap()
                .text
                .contains("no setting called 'coin'")
        );
    }

    #[test]
    fn scrolling_back_fetches_older_data() {
        // A fake candle API: one-minute candles for the window it's asked for.
        let cmd = r#"awk -v f={from:10m} -v t={now} 'BEGIN { printf "["; n = 0; for (x = f - f % 60; x < t; x += 60) { printf "%s{\"t\":%d,\"c\":%d}", (n++ ? "," : ""), x * 1000, (x / 60) % 100 } print "]" }'"#;
        let cli = Cli::parse_from([
            "graft",
            "-c",
            cmd,
            "-n",
            "1h",
            "--json",
            "--time-field",
            "t",
            "--replace",
        ]);
        let spec = crate::setup::source_spec(&cli, true).unwrap();
        let setup = crate::setup::build(cli, spec.clone(), false).unwrap();
        let mut app = App::new(setup, Live::new(spec));
        let t0 = Instant::now();
        while app.oldest_x().is_none() && t0.elapsed() < Duration::from_secs(5) {
            app.tick();
        }
        app.rebuild(true);
        let first = app.oldest_x().expect("candles");
        let n = app.ds.series[0].points.len();
        assert!((9..=11).contains(&n), "{n} candles in the first 10 minutes");
        // Zoom in, then scroll back to the start: the window before it is fetched.
        run(&mut app, r#"zoom("5m")"#);
        for _ in 0..20 {
            run(&mut app, "pan(-0.5)");
        }
        let t0 = Instant::now();
        while app.oldest_x() == Some(first) && t0.elapsed() < Duration::from_secs(5) {
            app.tick();
        }
        let older = app.oldest_x().unwrap();
        assert!(
            first - older >= 9.0 * 60.0,
            "about 10 more minutes: {}",
            first - older
        );
        app.rebuild(true);
        let m = app.ds.series[0].points.len();
        assert!(
            m >= n + 9 && m <= n + 11,
            "{n} → {m}: added, no duplicates at the seam"
        );
        // With everything in view, ← (going back) asks for older data too.
        while app.backfill.is_some() {
            app.tick();
        }
        app.view = None;
        key(&mut app, KeyCode::Left);
        assert!(app.backfill.is_some());
        let t0 = Instant::now();
        while app.oldest_x() == Some(older) && t0.elapsed() < Duration::from_secs(5) {
            app.tick();
        }
        assert!(older - app.oldest_x().unwrap() >= 9.0 * 60.0);
    }

    #[test]
    fn pane_settings_by_right_click_or_gear() {
        let mut app = app(&["graft", LOG, "--pane", "*", "--pane", "node2"]);
        screen(&app);
        let r = app.opts.panes.as_ref().unwrap().rects(app.chart_rect.get())[1].1;
        app.on_event(&TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: r.x + r.width / 2,
            row: r.y + r.height / 2,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.settings_menu);
        assert_eq!(app.opts.panes.as_ref().unwrap().focus, 1);
        let s = screen(&app);
        assert!(
            s.contains("pane 2 settings") && s.contains("source") && s.contains("lines"),
            "{s}"
        );
        // Chart type from the menu applies to that pane.
        let (x, y) = find(&app, " histogram ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert_eq!(
            app.opts.panes.as_ref().unwrap().panes[1].kind,
            Some(ChartKind::Histogram)
        );
        key(&mut app, KeyCode::Esc);
        assert!(!app.settings_menu);
        // The ⚙ in pane 1's title opens pane 1.
        let s = screen(&app);
        let (row, line) = s
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains('⚙'))
            .unwrap();
        let col = line.chars().position(|c| c == '⚙').unwrap() as u16;
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            col,
            row as u16,
        );
        assert!(app.settings_menu && app.opts.panes.as_ref().unwrap().focus == 0);
        // Fits a phone.
        let phone = screen_sized(&app, 46, 40);
        assert!(
            phone.contains("pane 1 settings") && phone.contains(" done "),
            "{phone}"
        );
        // One chart: right-click anywhere on it.
        let mut app = app_from(&[LOG.to_string()]);
        screen(&app);
        let c = app.chart_rect.get();
        app.on_event(&TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: c.x + 10,
            row: c.y + 5,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(screen(&app).contains("chart settings"));
        let (x, y) = find(&app, " bar ");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x + 1, y);
        assert_eq!(app.opts.kind, ChartKind::Bar);
    }

    #[test]
    fn legends_can_be_hidden() {
        let mut app = app(&["graft", LOG, "--pane", "*", "--pane", "node2"]);
        assert!(screen_sized(&app, 160, 40).contains("│node1 58.8 GB "));
        key(&mut app, KeyCode::Char('i'));
        let s = screen_sized(&app, 160, 40);
        assert!(
            !s.contains("│node1 58.8 GB ") && !s.contains("● node1"),
            "{s}"
        );
        run(&mut app, "legend(true)");
        assert!(app.show_legend);
    }
}
