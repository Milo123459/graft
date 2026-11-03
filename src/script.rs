//! Rhai scripting: console expressions, alerts, derived series, key bindings and
//! custom parsers all share one engine with a small graph-analysis API.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use rhai::{
    AST, Array, CallFnOptions, Dynamic, Engine, EvalAltResult, ImmutableString, Map, Scope,
};

use crate::data::Dataset;
use crate::stats::Stats;
use crate::units::{Unit, lookup_unit};

type FnResult<T> = Result<T, Box<EvalAltResult>>;

/// A value produced by a script `parse(line)` function or a JSON document.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptVal {
    Num(f64),
    Str(String),
}

/// Something a script asked the viewer to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    HLine(f64, Option<String>),
    ClearHLines,
    /// Show the last N seconds (x units).
    Zoom(f64),
    ResetView,
    Show(String),
    Hide(String),
    Chart(String),
    /// Series drawn in front (empty: none).
    Front(Vec<String>),
    /// Set (`Some`) or remove (`None`) a series' forecast target.
    Target(String, Option<f64>),
    /// Forecast horizon (`"auto"`, `"7d"`), or `None` to turn forecasting off.
    Forecast(Option<String>),
    /// Timeframe (`"7d"`, `"500"`), or `None` for all data.
    Range(Option<String>),
    /// Trend lines: a comma list ("avg,median,p95", "line"), or `None` for off.
    Trend(Option<String>),
    /// Every line to show, including "live": "live,avg"; with a second list, the
    /// right-hand chart of side by side (which turns it on).
    Lines(String, Option<String>),
    /// Points in the rolling window (`None` = automatic).
    TrendWindow(Option<usize>),
    /// Timeframe as a number: seconds on a time axis, else a point count.
    RangeNumber(f64),
    /// Timeframe between two times (`until` open-ended when `None`).
    Between(String, Option<String>),
    /// An on/off toolbar button: `Some` sets it, `None` toggles.
    Flag(Flag, Option<bool>),
    /// Move the view by a fraction of its width (negative = left).
    Pan(f64),
    /// Zoom in (`true`) or out one step.
    ZoomStep(bool),
    /// Write the view to an SVG (default name when `None`).
    Svg(Option<String>),
    Help,
    Quit,
    Pane(PaneAction),
    /// Save the current view (source, format, panes, lines…) as a named profile.
    SaveProfile(String),
    /// Put text (default: the view's command line) on the clipboard.
    Copy(Option<String>),
    /// A marker here (at the cursor, or the newest data) with this label.
    MarkHere(String),
    /// Remove every marker and marker rule.
    ClearMarks,
    Undo,
    Redo,
    /// Add (or replace) a merged source: `name = args…` or a profile name.
    Merge(String),
    Unmerge(String),
    /// Load a Rhai script file while running.
    Load(String),
    /// `param("interval", "1h")`: set a source's parameter.
    Param(String, String),
    /// Add (or, when editing, replace) an alert condition.
    Alert(String),
    Unalert(String),
    /// Add / remove a notification target (`bell`, `ntfy:topic`…).
    NotifyTo(String),
    NotifyOff(String),
    /// Show only these series (hide the rest).
    Only(Vec<String>),
    Toggle(String),
    ShowAll,
    /// The chart title.
    Title(String),
    /// The unit for values that don't carry one (`USD`, `ms`, `%`…); "" for none.
    Unit(String),
    /// A custom line: name (optionally `name:unit`) and Rhai expression.
    AddLine(String, String),
    RemoveLine(String),
    /// Open the builder to change the format or source.
    Format,
}

/// Pane commands from the console / scripts.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneAction {
    /// Split the focused pane: `true` = below, `false` = to the right.
    Split(bool),
    Close,
    /// "left"/"right"/"up"/"down", or a pane number.
    Focus(String),
    Move(String),
    /// Direction and step (a share of the split, e.g. 0.1).
    Resize(String, f64),
    Even,
    /// Back to one chart.
    Single,
    /// Rearrange: `1 | 2 / 3`.
    Layout(String),
    /// Set the focused pane: `node1,node2 kind=bar lines=live,avg`.
    Set(String),
    /// Where the focused pane's data comes from: a preset/profile/recent name, or
    /// "" / "main" for the main chart.
    Source(String),
}

/// Toolbar buttons that are on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Live,
    SideBySide,
    Stats,
    Split,
    Log,
    Targets,
    Pause,
    Follow,
    Cursor,
    Mouse,
    Measure,
    Legend,
}

/// Script names of the on/off buttons. Each takes an optional bool (none = toggle).
pub const FLAGS: &[(&str, Flag)] = &[
    ("live", Flag::Live),
    ("raw", Flag::Live),
    ("side_by_side", Flag::SideBySide),
    ("stats", Flag::Stats),
    ("split", Flag::Split),
    ("log", Flag::Log),
    ("targets", Flag::Targets),
    ("pause", Flag::Pause),
    ("follow", Flag::Follow),
    ("cursor", Flag::Cursor),
    ("mouse", Flag::Mouse),
    ("measure", Flag::Measure),
    ("legend", Flag::Legend),
];

/// State shared between the host and registered functions.
#[derive(Debug, Default)]
pub struct ScriptState {
    pub data: Dataset,
    pub view: Option<(f64, f64)>,
    /// Where the viewer's cursor is (for `at()`).
    pub cursor: Option<f64>,
    pub actions: Vec<Action>,
    pub binds: Vec<(String, String)>,
    pub output: Vec<String>,
    /// Marker rules added with `mark_when` (picked up by the viewer).
    pub mark_rules: Vec<crate::marks::MarkRule>,
}

pub struct ScriptHost {
    engine: Engine,
    /// The user's --script (empty if none).
    ast: RefCell<AST>,
    state: Rc<RefCell<ScriptState>>,
    binary: bool,
}

/// Turn a series name into a valid Rhai identifier (`cpu.user` → `cpu_user`).
pub fn ident(name: &str) -> String {
    const RESERVED: &[&str] = &[
        "if",
        "else",
        "let",
        "const",
        "fn",
        "in",
        "for",
        "while",
        "loop",
        "do",
        "until",
        "return",
        "true",
        "false",
        "this",
        "switch",
        "import",
        "export",
        "as",
        "private",
        "break",
        "continue",
        "throw",
        "try",
        "catch",
        "global",
        "Fn",
        "call",
        "curry",
        "type_of",
        "print",
        "debug",
        "eval",
        // Reserved by Rhai for the future: also not usable as names.
        "var",
        "static",
        "shared",
        "goto",
        "exit",
        "match",
        "case",
        "public",
        "protected",
        "new",
        "use",
        "with",
        "module",
        "package",
        "super",
        "thread",
        "spawn",
        "go",
        "await",
        "async",
        "sync",
        "yield",
        "default",
        "void",
        "null",
        "nil",
        "is",
    ];
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    if RESERVED.contains(&s.as_str()) {
        s.push('_');
    }
    s
}

static UNIT_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?P<n>\d+(?:\.\d+)?(?:[eE][-+]?\d+)?)",
        r"(?:\s?(?P<long>KiB|MiB|GiB|TiB|PiB|kB|KB|MB|GB|TB|PB|ms|us|ns|min|day|days|week|weeks)",
        r"|(?P<short>[KMGTPBshdwm]))\b",
        r"|\b(?P<pn>\d+(?:\.\d+)?)%(?P<pct>)"
    ))
    .expect("valid unit literal regex")
});

/// Rewrite unit literals (`50GB`, `1.5 h`, `200ms`, `1d`) into plain numbers in base
/// units, leaving string literals untouched.
pub fn preprocess(src: &str, binary: bool) -> String {
    let src = &quote_bind_code(src);
    let mut out = String::with_capacity(src.len());
    let mut rest = src.as_str();
    while !rest.is_empty() {
        // Copy code up to the next string literal or comment, rewriting it; then copy
        // the literal or comment as it is (an apostrophe in `// we're` isn't a quote).
        let code_end = rest
            .char_indices()
            .find(|&(i, c)| {
                matches!(c, '"' | '`' | '\'')
                    || rest[i..].starts_with("//")
                    || rest[i..].starts_with("/*")
            })
            .map_or(rest.len(), |(i, _)| i);
        out.push_str(&rewrite_units(&rest[..code_end], binary));
        rest = &rest[code_end..];
        if rest.starts_with("//") {
            let end = rest.find('\n').unwrap_or(rest.len());
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        if rest.starts_with("/*") {
            let end = rest.find("*/").map_or(rest.len(), |i| i + 2);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        if let Some(quote) = rest.chars().next() {
            let mut end = rest.len();
            let mut escaped = false;
            for (i, c) in rest.char_indices().skip(1) {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    c if c == quote => {
                        end = i + 1;
                        break;
                    }
                    _ => {}
                }
            }
            out.push_str(&rest[..end]);
            rest = &rest[end..];
        }
    }
    out
}

/// Arguments that are code, kept as text to run later: (function, argument number).
/// Written as code, they're quoted here: `bind("r", only("eth"))` →
/// `bind("r", "only(\"eth\")")`, `add_line("total", user + sys)` →
/// `add_line("total", "user + sys")`, `alert(api > 45ms)`. (A string, a `|| …`
/// closure or a `{ … }` block work too.)
const CODE_ARGS: &[(&str, usize)] = &[
    ("bind", 1),
    ("add_line", 1),
    ("alert", 0),
    ("unalert", 0),
    ("mark_when", 0),
];

fn quote_bind_code(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        match b[i] {
            // Skip strings and comments: `bind(` inside them isn't a call.
            q @ (b'"' | b'`' | b'\'') => i = skip_string(b, i, q),
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = src[i..].find("*/").map_or(b.len(), |e| i + e + 2);
            }
            c if is_ident(c) && (i == 0 || !is_ident(b[i - 1])) => {
                let word_end = (i..b.len()).find(|&k| !is_ident(b[k])).unwrap_or(b.len());
                let word = &src[i..word_end];
                let rest = &src[word_end..];
                let Some(&(_, n)) = CODE_ARGS.iter().find(|(f, _)| *f == word) else {
                    i = word_end;
                    continue;
                };
                if !rest.trim_start().starts_with('(') {
                    i = word_end;
                    continue;
                }
                let open = word_end + (rest.len() - rest.trim_start().len());
                // Argument n: after n top-level commas; it ends at the next one or at
                // the closing paren.
                let mut start = open + 1;
                let mut found = true;
                for _ in 0..n {
                    match scan_to(b, start, b',') {
                        Some(c) => start = c + 1,
                        None => {
                            found = false;
                            break;
                        }
                    }
                }
                let end = found
                    .then(|| scan_to(b, start, b',').or_else(|| scan_to(b, start, b')')))
                    .flatten();
                let Some(end) = end else {
                    i = word_end;
                    continue;
                };
                let arg = src[start..end].trim();
                let quoted = arg.starts_with('"') || arg.starts_with('`');
                if !quoted && !arg.is_empty() {
                    let code = arg.strip_prefix("||").unwrap_or(arg).trim();
                    out.push_str(&src[copied..start]);
                    if n > 0 {
                        out.push(' ');
                    }
                    out.push('"');
                    out.push_str(
                        &code
                            .replace('\\', "\\\\")
                            .replace('"', "\\\"")
                            .replace('\n', "\\n")
                            .replace('\r', "\\r")
                            .replace('\t', "\\t"),
                    );
                    out.push('"');
                    copied = end;
                }
                i = end;
            }
            _ => i += 1,
        }
    }
    out.push_str(&src[copied..]);
    out
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Index just past the string starting at `i` (quote `q`), honouring escapes.
fn skip_string(b: &[u8], mut i: usize, q: u8) -> usize {
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == q => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// The next `target` at nesting depth 0 from `i` (skipping strings and brackets).
fn scan_to(b: &[u8], mut i: usize, target: u8) -> Option<usize> {
    let mut depth = 0i32;
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'`' | b'\'') => {
                i = skip_string(b, i, q);
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            c @ (b')' | b']' | b'}') => {
                if depth == 0 {
                    return (c == target).then_some(i);
                }
                depth -= 1;
            }
            c if c == target && depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn rewrite_units(code: &str, binary: bool) -> String {
    UNIT_LITERAL
        .replace_all(code, |c: &regex::Captures| {
            // `90%` is a percentage — unless an operand follows (`10 % 3`, remainder).
            if let Some(pn) = c.name("pn") {
                let whole = c.get(0).expect("match");
                let next = code[whole.end()..].trim_start().chars().next();
                if next.is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '(' | '_' | '"')) {
                    return whole.as_str().to_string();
                }
                let s = pn.as_str().to_string();
                return if s.contains('.') { s } else { format!("{s}.0") };
            }
            let n: f64 = c["n"].parse().unwrap_or(0.0);
            let unit = c
                .name("long")
                .or(c.name("short"))
                .map_or("", |m| m.as_str());
            let mult = match unit {
                "m" => 60.0,
                "d" | "day" | "days" => 86400.0,
                "w" | "week" | "weeks" => 604_800.0,
                u => lookup_unit(u, binary).map_or(1.0, |(_, m)| m),
            };
            let v = n * mult;
            // Keep it a float literal so arithmetic stays in floating point.
            let s = format!("{v}");
            if s.contains('.') { s } else { format!("{s}.0") }
        })
        .into_owned()
}

/// Every console function: (name, how to call it, what it does). Used to explain
/// mistakes ("slope needs …: slope(\"api\", \"1h\")").
pub const USAGE: &[(&str, &str, &str)] = &[
    // values
    ("last", r#"last("api")"#, "the newest value shown"),
    ("first", r#"first("api")"#, "the oldest value shown"),
    (
        "at",
        r#"at("api")"#,
        "the value at the cursor (or the newest)",
    ),
    ("min", r#"min("api")"#, "the smallest value shown"),
    ("max", r#"max("api")"#, "the largest value shown"),
    ("mean", r#"mean("api")"#, "the average of what's shown"),
    ("avg", r#"avg("api")"#, "the average of what's shown"),
    ("median", r#"median("api")"#, "the middle value"),
    ("p95", r#"p95("api")"#, "the 95th percentile"),
    ("percentile", r#"percentile("api", 99)"#, "any percentile"),
    (
        "stddev",
        r#"stddev("api")"#,
        "how spread out the values are",
    ),
    ("sum", r#"sum("api")"#, "all the values added up"),
    ("count", r#"count("api")"#, "the number of points"),
    ("span", r#"span("api")"#, "the time the points cover"),
    ("change", r#"change("api")"#, "last minus first"),
    ("pct_change", r#"pct_change("api")"#, "last vs first, in %"),
    ("series", "series()", "the names of all series"),
    ("values", r#"values("api")"#, "every value, as an array"),
    ("xs", r#"xs("api")"#, "every point's time, as an array"),
    (
        "values_at",
        r#"values_at("db", xs("api"))"#,
        "one series' values at another's times",
    ),
    // trends
    (
        "slope",
        r#"slope("api")  ·  slope("api", "1h")"#,
        "the trend: change per hour (or per second)",
    ),
    (
        "r2",
        r#"r2("api")"#,
        "how well a straight line fits, 0 to 1",
    ),
    (
        "eta",
        r#"eta("api", 60ms)"#,
        "seconds until the trend reaches a value",
    ),
    (
        "forecast",
        r#"forecast("api", 1h)  ·  forecast("1h")"#,
        "the trend's value ahead · draw forecasts",
    ),
    (
        "ema",
        r#"ema(values("api"), 30)"#,
        "exponential moving average of an array",
    ),
    (
        "sma",
        r#"sma(values("api"), 30)"#,
        "simple moving average of an array",
    ),
    (
        "rsi",
        r#"rsi(values("api"), 14)"#,
        "relative strength index, 0–100 (70 overbought, 30 oversold)",
    ),
    (
        "macd",
        r#"macd(values("api"))"#,
        "MACD: 12-point EMA minus 26-point EMA (signal: ema(values(\"macd\"), 9))",
    ),
    (
        "mean_of",
        r#"mean_of(["web", "api"])"#,
        "several series averaged at each moment",
    ),
    // text
    (
        "fmt",
        r#"fmt("api", 0.044)"#,
        "a number in a series' unit: 44 ms",
    ),
    ("fmt_dur", "fmt_dur(5400)", "seconds in words: 1.5 hours"),
    ("fmt_time", r#"fmt_time(last_x)"#, "a timestamp, readable"),
    ("bytes", "bytes(5e9)", "a byte count, readable: 5 GB"),
    // the view
    (
        "timeframe",
        r#"timeframe("1h")  ·  timeframe("2026-11-01", "")"#,
        "which data counts",
    ),
    (
        "range",
        r#"range("1h")  ·  range("all")"#,
        "which data counts",
    ),
    ("zoom", r#"zoom("5m")"#, "zoom to the last 5 minutes"),
    ("zoom_in", "zoom_in()", "zoom in"),
    ("zoom_out", "zoom_out()", "zoom out"),
    ("pan", r#"pan("left")  ·  pan(-0.5)"#, "move along"),
    ("reset", "reset()", "show everything"),
    (
        "chart",
        r#"chart("bar")"#,
        "line scatter bar histogram sparkline stat candles",
    ),
    (
        "front",
        r#"front("rsi14")  ·  front()"#,
        "draw these series on top of the others (none: back to normal)",
    ),
    ("lines", r#"lines("live,avg")"#, "which lines to draw"),
    (
        "trend",
        r#"trend("median")"#,
        "the trend line: avg median p95 fit off",
    ),
    ("window", "window(60)", "points in the moving average"),
    ("show", r#"show("api")"#, "show a series"),
    ("hide", r#"hide("api")"#, "hide a series"),
    ("toggle", r#"toggle("api")"#, "show or hide a series"),
    (
        "only",
        r#"only("api")  ·  only("web", "api")"#,
        "show just these",
    ),
    ("show_all", "show_all()", "show every series"),
    (
        "hline",
        r#"hline(50ms)  ·  hline(50ms, "slow")"#,
        "a reference line",
    ),
    ("clear_hlines", "clear_hlines()", "remove reference lines"),
    ("target", r#"target("api", 60ms)"#, "a forecast target"),
    ("clear_target", r#"clear_target("api")"#, "remove a target"),
    ("title", r#"title("latency")"#, "the chart's title"),
    ("unit", r#"unit("ms")"#, "the unit for bare numbers"),
    // toggles
    ("live", "live()  ·  live(false)", "the raw data on/off"),
    ("raw", "raw()  ·  raw(false)", "the raw data on/off"),
    ("side_by_side", "side_by_side()", "live left, trends right"),
    ("stats", "stats()  ·  stats(true)", "the stats table"),
    ("split", "split()", "a panel per series"),
    ("log", "log()", "log scale"),
    ("targets", "targets()", "target lines"),
    ("pause", "pause()", "pause the chart"),
    ("follow", "follow()", "the cursor follows the mouse"),
    ("cursor", "cursor()", "the cursor on/off"),
    ("mouse", "mouse()", "the mouse on/off"),
    ("measure", "measure()", "start measuring a stretch"),
    ("legend", "legend()  ·  legend(false)", "the legends on/off"),
    // panes
    ("split_right", "split_right()", "a new pane to the right"),
    ("split_down", "split_down()", "a new pane below"),
    ("close_pane", "close_pane()", "close the picked pane"),
    ("single", "single()", "back to one chart"),
    ("even_panes", "even_panes()", "equal sizes"),
    (
        "focus_pane",
        r#"focus_pane("left")  ·  focus_pane(2)"#,
        "pick a pane",
    ),
    ("move_pane", r#"move_pane("right")"#, "move the picked pane"),
    (
        "resize_pane",
        r#"resize_pane("right")  ·  resize_pane("right", 0.1)"#,
        "grow the picked pane",
    ),
    (
        "pane",
        r#"pane("db kind=histogram")"#,
        "what the picked pane shows",
    ),
    (
        "pane_source",
        r#"pane_source("cpu")"#,
        "the picked pane's source",
    ),
    ("layout", r#"layout("(1 | 2) / 3")"#, "arrange the panes"),
    // markers, alerts, lines, sources
    (
        "mark_when",
        r#"mark_when(value > 60ms, "spike")"#,
        "a marker wherever it holds",
    ),
    ("mark", r#"mark("deploy")"#, "a marker at the cursor"),
    ("clear_marks", "clear_marks()", "remove every marker"),
    ("alert", r#"alert(api > 45ms)"#, "an alert"),
    ("unalert", r#"unalert("api > 45ms")"#, "remove an alert"),
    (
        "notify_to",
        r#"notify_to("ntfy:my-topic")"#,
        "bell desktop ntfy: webhook: cmd:",
    ),
    (
        "notify_off",
        r#"notify_off("desktop")"#,
        "stop sending there",
    ),
    (
        "add_line",
        r#"add_line("total", web + api)"#,
        "a custom line, worked out at each point",
    ),
    (
        "remove_line",
        r#"remove_line("smooth")"#,
        "remove a custom line",
    ),
    (
        "merge",
        r#"merge("cpu")  ·  merge("cb = -c '…' --json")"#,
        "add a source's series",
    ),
    ("unmerge", r#"unmerge("cpu")"#, "remove a merged source"),
    (
        "param",
        r#"param("interval", "1h")"#,
        "set a source's setting (--param)",
    ),
    // keeping things
    ("undo", "undo()", "a step back"),
    ("redo", "redo()", "a step forward"),
    (
        "copy",
        "copy()  ·  copy(\"text\")",
        "copy this view's command (or text)",
    ),
    (
        "save",
        r#"save("latency")"#,
        "save as a profile: graft -p latency",
    ),
    ("svg", r#"svg()  ·  svg("out.svg")"#, "export to SVG"),
    ("load", r#"load("my.rhai")"#, "load a script"),
    ("bind", r#"bind("r", only("api"))"#, "a key of your own"),
    ("format", "format()", "open the format builder"),
    ("help", "help", "every command"),
    ("quit", "quit()", "quit"),
];

/// The console's commands by what they're for (the `help` list).
pub const GROUPS: &[(&str, &[&str])] = &[
    (
        "values",
        &[
            "last",
            "first",
            "at",
            "min",
            "max",
            "mean",
            "avg",
            "median",
            "p95",
            "percentile",
            "stddev",
            "sum",
            "count",
            "span",
            "change",
            "pct_change",
        ],
    ),
    ("trends", &["slope", "r2", "eta", "forecast"]),
    (
        "arrays",
        &[
            "values",
            "xs",
            "values_at",
            "series",
            "ema",
            "sma",
            "rsi",
            "macd",
            "mean_of",
        ],
    ),
    ("text", &["fmt", "fmt_dur", "fmt_time", "bytes"]),
    (
        "view",
        &[
            "timeframe",
            "range",
            "zoom",
            "zoom_in",
            "zoom_out",
            "pan",
            "reset",
            "chart",
            "front",
            "lines",
            "trend",
            "window",
            "title",
            "unit",
        ],
    ),
    ("series", &["show", "hide", "toggle", "only", "show_all"]),
    (
        "toggles",
        &[
            "stats",
            "split",
            "log",
            "targets",
            "live",
            "raw",
            "side_by_side",
            "pause",
            "follow",
            "cursor",
            "mouse",
            "measure",
            "legend",
        ],
    ),
    (
        "lines",
        &[
            "hline",
            "clear_hlines",
            "target",
            "clear_target",
            "add_line",
            "remove_line",
        ],
    ),
    ("markers", &["mark_when", "mark", "clear_marks"]),
    ("alerts", &["alert", "unalert", "notify_to", "notify_off"]),
    (
        "panes",
        &[
            "split_right",
            "split_down",
            "close_pane",
            "single",
            "even_panes",
            "focus_pane",
            "move_pane",
            "resize_pane",
            "pane",
            "pane_source",
            "layout",
        ],
    ),
    ("sources", &["merge", "unmerge", "param", "load", "format"]),
    (
        "keep",
        &["undo", "redo", "copy", "save", "svg", "bind", "quit"],
    ),
];

/// `last(busy)` → `last("busy")`: where a function takes a series' name, a series
/// written plainly is its name (not its newest value). Only words that are one of
/// `series` (so a script's own variables are left alone).
pub fn quote_series_names(src: &str, series: &[String]) -> String {
    if series.is_empty() {
        return src.to_string();
    }
    let by_ident: Vec<(String, &String)> = series.iter().map(|s| (ident(s), s)).collect();
    let name_of = |w: &str| by_ident.iter().find(|(id, _)| id == w).map(|(_, n)| *n);
    let b = src.as_bytes();
    // (start, end, replacement) for each argument to quote.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let bare = |start: usize, end: usize, edits: &mut Vec<(usize, usize, String)>| {
        let raw = &src[start..end];
        let t = raw.trim();
        if let Some(name) = name_of(t) {
            let lead = raw.len() - raw.trim_start().len();
            edits.push((start + lead, start + lead + t.len(), format!("{name:?}")));
        }
    };
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'`' | b'\'') => {
                i = skip_string(b, i, q);
                continue;
            }
            c if is_ident(c) && (i == 0 || !is_ident(b[i - 1])) => {
                let end = (i..b.len()).find(|&k| !is_ident(b[k])).unwrap_or(b.len());
                let word = &src[i..end];
                let first = SERIES_FIRST.contains(&word);
                let any = SERIES_ANY.contains(&word);
                let rest = &src[end..];
                if (first || any) && rest.trim_start().starts_with('(') {
                    let open = end + (rest.len() - rest.trim_start().len());
                    let mut start = open + 1;
                    loop {
                        let comma = scan_to(b, start, b',');
                        let stop = comma.or_else(|| scan_to(b, start, b')'));
                        let Some(stop) = stop else { break };
                        let arg = src[start..stop].trim();
                        if arg.starts_with('[') && arg.ends_with(']') {
                            // mean_of([web, api]): each item.
                            let lb = start + src[start..stop].find('[').expect("starts with [");
                            let mut s = lb + 1;
                            while let Some(e) = scan_to(b, s, b',').or_else(|| scan_to(b, s, b']'))
                            {
                                if e > stop {
                                    break;
                                }
                                bare(s, e, &mut edits);
                                if b[e] == b']' {
                                    break;
                                }
                                s = e + 1;
                            }
                        } else {
                            bare(start, stop, &mut edits);
                        }
                        if first || comma.is_none() {
                            break;
                        }
                        start = stop + 1;
                    }
                }
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let mut out = src.to_string();
    for (s, e, r) in edits.into_iter().rev() {
        out.replace_range(s..e, &r);
    }
    out
}

/// A usage example with the chart's own series in place of the made-up ones (api,
/// web, db), so `e.g. last("api")` reads `e.g. last("busy")` on a CPU chart.
pub fn example_for(example: &str, series: &[String]) -> String {
    if series.is_empty() || series.iter().any(|s| s == "api") {
        return example.to_string();
    }
    static DEMO: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b(api|web|db)\b").expect("valid"));
    let pick = |w: &str| {
        let k = match w {
            "api" => 0,
            "web" => 1,
            _ => 2,
        };
        &series[k % series.len()]
    };
    let mut out = String::new();
    let mut last = 0;
    for m in DEMO.find_iter(example) {
        out.push_str(&example[last..m.start()]);
        let quoted = example[..m.start()].ends_with(['"', '`']);
        let s = pick(m.as_str());
        out.push_str(&if quoted { s.clone() } else { ident(s) });
        last = m.end();
    }
    out.push_str(&example[last..]);
    out
}

/// How `name` is called, if it's a console function.
pub fn usage(name: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    USAGE.iter().find(|u| u.0 == name)
}

/// Functions whose first argument is a series name (`last("api")`).
const SERIES_FIRST: &[&str] = &[
    "last",
    "first",
    "at",
    "min",
    "max",
    "mean",
    "avg",
    "median",
    "p95",
    "percentile",
    "stddev",
    "sum",
    "count",
    "span",
    "change",
    "pct_change",
    "values",
    "xs",
    "values_at",
    "slope",
    "r2",
    "eta",
    "forecast",
    "fmt",
    "show",
    "hide",
    "toggle",
    "target",
    "clear_target",
];
/// Functions where every text argument is a series name (`only("a", "b")`, `mean_of([…])`).
const SERIES_ANY: &[&str] = &["only", "mean_of", "front"];

/// The choices for a text argument that isn't a series.
fn arg_choices(func: &str, arg: usize) -> &'static [&'static str] {
    match (func, arg) {
        ("slope", 1) => &["1m", "1h", "1d", "1w"],
        (_, 1..) => &[],
        (func, _) => arg_choices_first(func),
    }
}

fn arg_choices_first(func: &str) -> &'static [&'static str] {
    match func {
        "chart" => &[
            "line",
            "scatter",
            "bar",
            "histogram",
            "sparkline",
            "stat",
            "candles",
        ],
        "lines" => &["live", "avg", "median", "p95", "line"],
        "trend" => &["avg", "median", "p95", "fit", "off"],
        "notify_to" | "notify_off" => &["bell", "desktop", "ntfy:", "webhook:", "cmd:"],
        "timeframe" | "range" | "zoom" => &["5m", "15m", "1h", "6h", "1d", "7d", "all"],
        "pan" | "focus_pane" | "move_pane" | "resize_pane" => &["left", "right", "up", "down"],
        "forecast" => &["auto", "1h", "1d", "7d", "off"],
        _ => &[],
    }
}

/// One suggestion: what Tab puts in place of the word being typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub insert: String,
    /// What the hint row shows.
    pub label: String,
}

/// What the console can offer at the cursor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Completions {
    /// Char offset where the word being completed starts (it's replaced up to the cursor).
    pub start: usize,
    pub items: Vec<Suggestion>,
    /// The call the cursor is in: how it's used, and what it does.
    pub signature: Option<(&'static str, &'static str)>,
}

/// How many arguments `func` takes at least (its shortest usage form), if known.
fn min_args(func: &str) -> Option<usize> {
    let u = usage(func)?;
    u.1.split("  ·  ")
        .filter_map(|form| {
            let inner = form.trim().strip_prefix(func)?.strip_prefix('(')?;
            let inner = inner.strip_suffix(')')?;
            if inner.trim().is_empty() {
                return Some(0);
            }
            let b = inner.as_bytes();
            let mut n = 1;
            let mut i = 0;
            while let Some(c) = scan_to(b, i, b',') {
                n += 1;
                i = c + 1;
            }
            Some(n)
        })
        .min()
}

/// What a marker rule can use besides series and functions.
const MARK_VARS: &[&str] = &["value", "prev", "delta", "pct", "rate", "time", "name"];

/// Suggestions for console input `text` with the cursor at char `cursor`.
pub fn complete(text: &str, cursor: usize, series: &[String]) -> Completions {
    complete_with(text, cursor, series, &[])
}

/// The source settings (`--param`) the console can offer: name and choices.
pub type ParamChoices = [(String, Vec<String>)];

/// `complete`, also offering the source's settings in `param("name", "value")`.
pub fn complete_with(
    text: &str,
    cursor: usize,
    series: &[String],
    params: &ParamChoices,
) -> Completions {
    complete_in(text, cursor, series, &[], false, params)
}

/// `extra`: more variables to offer (inside a marker rule); `in_code`: completing the
/// text of an expression argument.
fn complete_in(
    text: &str,
    cursor: usize,
    series: &[String],
    extra: &[&str],
    in_code: bool,
    params: &ParamChoices,
) -> Completions {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    // Walk up to the cursor: are we in a string, and in which calls?
    let mut in_str: Option<usize> = None;
    let mut calls: Vec<(String, usize, usize)> = Vec::new(); // (function, args seen, its `(`)
    let mut i = 0;
    while i < cursor {
        let c = chars[i];
        match (in_str, c) {
            (Some(_), '\\') => i += 1,
            (Some(_), '"') => in_str = None,
            (Some(_), _) => {}
            (None, '"') => in_str = Some(i),
            (None, '(') => {
                let mut j = i;
                while j > 0 && (chars[j - 1].is_alphanumeric() || chars[j - 1] == '_') {
                    j -= 1;
                }
                calls.push((chars[j..i].iter().collect(), 0, i));
            }
            (None, ')') => {
                calls.pop();
            }
            (None, ',') => {
                if let Some(c) = calls.last_mut() {
                    c.1 += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let open = calls.last().map(|c| c.2);
    let call = calls.last().map(|c| (c.0.clone(), c.1));
    // The choices for argument `arg` of `func`: series, a fixed list, or the source's
    // settings (`param("interval", "1h")`: names, then that one's values).
    let choices = |func: &str, arg: usize| -> Vec<String> {
        if SERIES_ANY.contains(&func) || SERIES_FIRST.contains(&func) && arg == 0 {
            return series.to_vec();
        }
        if func == "param" {
            if arg == 0 {
                return params.iter().map(|p| p.0.clone()).collect();
            }
            // The first argument as written: `"interval"`.
            let first: String = open
                .map(|o| {
                    chars[o + 1..cursor]
                        .iter()
                        .take_while(|c| **c != ',')
                        .collect()
                })
                .unwrap_or_default();
            let first = first.trim().trim_matches('"');
            return params
                .iter()
                .find(|p| p.0 == first)
                .map(|p| p.1.clone())
                .unwrap_or_default();
        }
        arg_choices(func, arg)
            .iter()
            .map(|s| s.to_string())
            .collect()
    };
    let signature = call
        .as_ref()
        .and_then(|(f, _)| usage(f))
        .map(|u| (u.1, u.2));
    let mut out = Completions {
        start: cursor,
        items: Vec::new(),
        signature,
    };
    let starts = |a: &str, b: &str| a.to_lowercase().starts_with(&b.to_lowercase());
    let contains = |a: &str, b: &str| a.to_lowercase().contains(&b.to_lowercase());
    // Prefix matches first, then other matches.
    let ranked = |options: Vec<String>, word: &str| -> Vec<String> {
        let mut v: Vec<String> = options
            .iter()
            .filter(|o| starts(o, word))
            .cloned()
            .collect();
        v.extend(
            options
                .iter()
                .filter(|o| !starts(o, word) && contains(o, word))
                .cloned(),
        );
        v
    };
    let close = |at: usize| chars.get(at) != Some(&'"');
    if let Some(q) = in_str {
        let (func, arg) = call.clone().unwrap_or_default();
        // Text that's code (add_line's expression, an alert, a marker rule): complete
        // it as code, with `backticks` for any text inside so the quotes still hold.
        if CODE_ARGS.contains(&(func.as_str(), arg)) {
            let inner: String = chars[q + 1..cursor]
                .iter()
                .map(|&c| if c == '`' { '"' } else { c })
                .collect();
            let vars: &[&str] = if func == "mark_when" { MARK_VARS } else { &[] };
            let mut c = complete_in(&inner, inner.chars().count(), series, vars, true, params);
            c.start += q + 1;
            for item in &mut c.items {
                item.insert = item.insert.replace('"', "`");
                item.label = item.label.replace('"', "`");
            }
            c.signature = c.signature.or(out.signature);
            return c;
        }
        // In text: a series name, or one of the argument's choices.
        let word: String = chars[q + 1..cursor].iter().collect();
        let (func, arg) = (func, arg);
        let options: Vec<String> = choices(&func, arg);
        out.start = q + 1;
        // The last argument (of the shortest form): close the call too.
        let last =
            min_args(&func).is_some_and(|n| arg + 1 >= n) && !SERIES_ANY.contains(&func.as_str());
        let paren = if last && !chars[cursor..].contains(&')') {
            ")"
        } else {
            ""
        };
        out.items = ranked(options, &word)
            .into_iter()
            .filter(|o| *o != word || close(cursor))
            .map(|o| Suggestion {
                insert: if close(cursor) && !o.ends_with(':') {
                    format!("{o}\"{paren}")
                } else {
                    o.clone()
                },
                label: o,
            })
            .collect();
        return out;
    }
    // A word being typed: commands and series.
    let mut start = cursor;
    while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
        start -= 1;
    }
    let word: String = chars[start..cursor].iter().collect();
    if word.is_empty() {
        // Right after `last(`: the series, quoted.
        if let Some((func, arg)) = &call
            && chars[..cursor]
                .iter()
                .rev()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| matches!(c, '(' | ',' | '['))
        {
            let options: Vec<String> = choices(func, *arg);
            let last = min_args(func).is_some_and(|n| *arg + 1 >= n)
                && !SERIES_ANY.contains(&func.as_str());
            let paren = if last && !chars[cursor..].contains(&')') {
                ")"
            } else {
                ""
            };
            out.items = options
                .into_iter()
                .map(|o| Suggestion {
                    insert: if o.ends_with(':') {
                        format!("\"{o}")
                    } else if !paren.is_empty() {
                        format!("\"{o}\"{paren}")
                    } else {
                        format!("\"{o}\"")
                    },
                    label: o,
                })
                .collect();
        }
        return out;
    }
    if word.starts_with(|c: char| c.is_ascii_digit()) {
        return out;
    }
    out.start = start;
    let funcs: Vec<String> = USAGE.iter().map(|u| u.0.to_string()).collect();
    let mut idents: Vec<String> = series.iter().map(|s| ident(s)).collect();
    // A marker rule (quoted, or written as code): its own variables too.
    let in_rule = call
        .as_ref()
        .is_some_and(|(f, a)| f == "mark_when" && *a == 0);
    let vars: &[&str] = if in_rule { MARK_VARS } else { extra };
    idents.extend(vars.iter().map(|v| v.to_string()));
    for name in ranked(funcs, &word) {
        let example = usage(&name).map_or("", |u| u.1);
        // `last("` for one that takes text first, `undo()` for one that takes nothing.
        let insert = if example.starts_with(&format!("{name}(\"")) {
            format!("{name}(\"")
        } else if example.starts_with(&format!("{name}()")) {
            format!("{name}()")
        } else if example.starts_with(&format!("{name}(")) {
            format!("{name}(")
        } else {
            name.clone()
        };
        out.items.push(Suggestion {
            insert,
            label: format!("{name}()"),
        });
    }
    // Inside a call or an expression, series (and rule variables) usually are what's
    // being typed: they go first.
    let names: Vec<Suggestion> = ranked(idents, &word)
        .into_iter()
        .map(|id| Suggestion {
            insert: id.clone(),
            label: id,
        })
        .collect();
    if call.is_some() || in_code {
        out.items.splice(0..0, names);
    } else {
        out.items.extend(names);
    }
    // Typed in full already: nothing to add.
    out.items.retain(|s| s.insert != word);
    out
}

/// Letters to change to turn `a` into `b` (for "did you mean").
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != *cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The closest of `options` to `word`, if it's close enough to be a typo.
pub fn did_you_mean<'a>(word: &str, options: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    options
        .into_iter()
        .map(|o| (edit_distance(&word.to_lowercase(), &o.to_lowercase()), o))
        .filter(|(d, o)| {
            *d <= (o.chars().count() / 3).max(1) || o.starts_with(word) && word.len() >= 2
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, o)| o)
}

/// A friendly explanation of a Rhai error for `code`, and where in it the problem is
/// (a character offset), when known. `series` are the chart's series names.
pub fn explain_error(raw: &str, code: &str, series: &[String]) -> (String, Option<usize>) {
    // From inside a custom line, marker rule or alert: explain the error in its own
    // code, keep saying where it came from.
    static WRAPPED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(line [^:]+|mark '[^']*'|alert '[^']*'): (?:Runtime error: )?(.*)$")
            .expect("valid")
    });
    if let Some(c) = WRAPPED.captures(raw) {
        let (inner, _) = explain_error(&c[2], "", series);
        return (format!("{}: {inner}", &c[1]), None);
    }
    static POS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s*\(line (\d+), position (\d+)\)\s*$").expect("valid"));
    // Where: Rhai counts lines and columns from 1.
    let at = POS.captures(raw).and_then(|c| {
        let (line, col): (usize, usize) = (c[1].parse().ok()?, c[2].parse().ok()?);
        let before: usize = code
            .split('\n')
            .take(line - 1)
            .map(|l| l.chars().count() + 1)
            .sum();
        Some(before + col.saturating_sub(1))
    });
    let msg = POS.replace(raw, "").to_string();
    let msg = msg
        .strip_prefix("Runtime error: ")
        .unwrap_or(&msg)
        .to_string();
    let quoted = |names: &[String]| {
        names
            .iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let how = |name: &str| {
        usage(name).map_or(String::new(), |u| {
            format!(" — e.g. {}", example_for(u.1, series))
        })
    };
    // Function not found: name (types)
    if let Some(rest) = msg.strip_prefix("Function not found: ") {
        let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
        let name = name.trim();
        if usage(name).is_none() {
            let names = USAGE.iter().map(|u| u.0);
            return match did_you_mean(name, names) {
                Some(m) => {
                    // Their own call with the name fixed: lst(busy) → last(busy).
                    let theirs = code.find(&format!("{name}(")).and_then(|p| {
                        let open = p + name.len();
                        let close = scan_to(code.as_bytes(), open + 1, b')')?;
                        Some(format!("{m}{}", &code[open..=close]))
                    });
                    let fix = theirs.unwrap_or_else(|| {
                        usage(m).map_or(format!("{m}()"), |u| example_for(u.1, series))
                    });
                    (format!("there's no {name}() — did you mean {fix}?"), at)
                }
                None => (
                    format!("there's no {name}() — type help to see every command"),
                    at,
                ),
            };
        }
        // A series given as a value (`last(api)`): it needs to be its name, in quotes.
        let takes_name = usage(name).is_some_and(|u| u.1.starts_with(&format!("{name}(\"")));
        if takes_name && (args.starts_with("(f64") || args.starts_with("(i64")) {
            let fix = series
                .iter()
                .find(|s| code.contains(&format!("{name}({}", ident(s))));
            let example = match fix {
                Some(s) => format!("{name}(\"{s}\")"),
                None => usage(name).map_or(String::new(), |u| u.1.to_string()),
            };
            return (
                format!("{name}() needs the series' name in quotes: {example}"),
                at,
            );
        }
        // Rhai's type names, as a person would say them.
        let kinds: Vec<&str> = args
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(", ")
            .filter(|a| !a.trim().is_empty())
            .map(|a| match a.trim() {
                t if t.contains("str") || t.contains("String") => "text",
                "i64" | "f64" => "a number",
                "bool" => "true/false",
                t if t.contains("Array") => "a list",
                "()" => "nothing",
                _ => "something else",
            })
            .collect();
        let given = match kinds.len() {
            0 => "no arguments".to_string(),
            _ => format!("({})", kinds.join(", ")),
        };
        return (format!("{name}() doesn't take {given}{}", how(name)), at);
    }
    if let Some(var) = msg.strip_prefix("Variable not found: ") {
        let var = var.trim();
        let idents: Vec<String> = series.iter().map(|s| ident(s)).collect();
        let options = idents
            .iter()
            .map(String::as_str)
            .chain(USAGE.iter().map(|u| u.0));
        let hint = match did_you_mean(var, options) {
            Some(m) if usage(m).is_some() => format!(" — did you mean {m}()?{}", how(m)),
            Some(m) => format!(" — did you mean {m}?"),
            None if series.is_empty() => String::new(),
            None => format!(" — the series here are {}", quoted(series)),
        };
        return (
            format!("there's no series or variable called {var}{hint}"),
            at,
        );
    }
    // A number with letters it doesn't know (`5x`): units it does know, or a string.
    if let Some(i) = at {
        let chars: Vec<char> = code.chars().collect();
        let word = |c: char| c.is_alphanumeric() || c == '.';
        let mut start = i.min(chars.len());
        while start > 0 && word(chars[start - 1]) {
            start -= 1;
        }
        let token: String = chars[start..].iter().take_while(|c| word(**c)).collect();
        if token.starts_with(|c: char| c.is_ascii_digit())
            && token.contains(|c: char| c.is_alphabetic())
        {
            return (
                format!(
                    "{token} isn't a number graft understands — units it knows: ms s min h d w, \
                     KB MB GB, % (or write it as text: \"{token}\")"
                ),
                Some(start),
            );
        }
    }
    if let Some(f) = msg
        .strip_prefix("Expecting ')' to close the arguments list of this function call '")
        .and_then(|r| r.strip_suffix('\''))
    {
        return (format!("{f}( is missing its closing ){}", how(f)), at);
    }
    if let Some(f) = msg
        .strip_prefix("Expecting ',' to separate the arguments to function call '")
        .and_then(|r| r.strip_suffix('\''))
    {
        return (format!("{f}(…): expected a , or ) here{}", how(f)), at);
    }
    if msg.starts_with("Unterminated string") || msg.contains("Open string is not terminated") {
        return ("a \" is missing: text goes between two quotes".into(), at);
    }
    // Ours already say what's wrong (unknown series 'apu' …): add a did-you-mean.
    if let Some(name) = msg
        .strip_prefix("unknown series '")
        .and_then(|r| r.split_once('\''))
        .map(|r| r.0)
    {
        let hint = did_you_mean(name, series.iter().map(String::as_str)).map_or(
            format!(" — the series here are {}", quoted(series)),
            |m| format!(" — did you mean \"{m}\"?"),
        );
        return (format!("there's no series called \"{name}\"{hint}"), at);
    }
    // A rule that isn't a condition.
    if let Some(t) = msg
        .strip_prefix("returned ")
        .and_then(|r| r.strip_suffix(", expected true/false"))
    {
        let what = match t {
            "f64" | "i64" => "a number",
            "string" | "ImmutableString" => "text",
            "()" => "nothing",
            _ => t,
        };
        return (
            format!(
                "it gives {what}, but this needs a condition (true or false), e.g. value > 60ms"
            ),
            at,
        );
    }
    // Ours start with the function's name: add how to call it.
    let first: String = msg
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    // (Unless it already lists what's allowed.)
    if usage(&first).is_some() && !msg.contains("e.g.") && !msg.contains(" — ") {
        return (format!("{msg}{}", how(&first)), at);
    }
    (msg, at)
}

fn dyn_to_f64(d: &Dynamic) -> Option<f64> {
    d.as_float()
        .ok()
        .or_else(|| d.as_int().ok().map(|i| i as f64))
}

/// Format a script result for display.
pub fn display(d: &Dynamic) -> String {
    if d.is_unit() {
        "ok".into()
    } else if let Ok(f) = d.as_float() {
        crate::units::fmt_compact(f)
    } else {
        d.to_string()
    }
}

impl ScriptHost {
    pub fn new(script: Option<&Path>, binary: bool) -> Result<Self> {
        let state = Rc::new(RefCell::new(ScriptState::default()));
        let mut engine = Engine::new();
        engine.set_max_operations(50_000_000);
        // Room for real indicator code (the defaults are tuned for one-liners).
        engine.set_max_expr_depths(256, 256);
        register_api(&mut engine, &state, binary);
        let ast = match script {
            Some(path) => {
                let src = std::fs::read_to_string(path)
                    .with_context(|| format!("reading script {}", path.display()))?;
                let ast = engine
                    .compile(preprocess(&src, binary))
                    .map_err(|e| anyhow!("{}: {e}", path.display()))?;
                // Run top-level statements once (registers key bindings etc.).
                engine
                    .run_ast(&ast)
                    .map_err(|e| anyhow!("{}: {e}", path.display()))?;
                ast
            }
            None => AST::empty(),
        };
        Ok(Self {
            engine,
            ast: RefCell::new(ast),
            state,
            binary,
        })
    }

    /// Load another script while running: its functions join the ones already
    /// loaded, and its top-level code runs (key bindings, `mark_when` rules…).
    pub fn load(&self, path: &Path) -> Result<usize> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("reading script {}", path.display()))?;
        let ast = self
            .engine
            .compile(preprocess(&src, self.binary))
            .map_err(|e| anyhow!("{}: {e}", path.display()))?;
        // Named functions (closures are compiled to anonymous ones).
        let functions = ast
            .iter_functions()
            .filter(|f| !f.name.starts_with("anon$"))
            .count();
        let merged = self.ast.borrow().merge(&ast);
        self.engine
            .run_ast(&merged.clone_functions_only().merge(&ast))
            .map_err(|e| anyhow!("{}: {e}", path.display()))?;
        *self.ast.borrow_mut() = merged.clone_functions_only();
        Ok(functions)
    }

    pub fn has_fn(&self, name: &str) -> bool {
        self.ast.borrow().iter_functions().any(|f| f.name == name)
    }

    /// Replace the data the API functions see.
    pub fn set_data(&self, data: Dataset, view: Option<(f64, f64)>) {
        let mut st = self.state.borrow_mut();
        st.data = data;
        st.view = view;
    }

    pub fn set_cursor(&self, x: Option<f64>) {
        self.state.borrow_mut().cursor = x;
    }

    pub fn take_actions(&self) -> Vec<Action> {
        std::mem::take(&mut self.state.borrow_mut().actions)
    }

    pub fn take_output(&self) -> Vec<String> {
        std::mem::take(&mut self.state.borrow_mut().output)
    }

    /// Marker rules added since last asked (`mark_when` in the script or console).
    pub fn take_mark_rules(&self) -> Vec<crate::marks::MarkRule> {
        std::mem::take(&mut self.state.borrow_mut().mark_rules)
    }

    /// Whether `mark_when` added rules that haven't been picked up yet.
    pub fn has_new_mark_rules(&self) -> bool {
        !self.state.borrow().mark_rules.is_empty()
    }

    /// Evaluate a compiled expression with variables already in `scope`.
    pub fn eval_ast_with_scope(&self, scope: &mut Scope, ast: &AST) -> Result<Dynamic> {
        self.engine
            .eval_ast_with_scope(scope, ast)
            .map_err(|e| anyhow!("{e}"))
    }

    pub fn binds(&self) -> Vec<(String, String)> {
        self.state.borrow().binds.clone()
    }

    /// Compile an expression (unit literals allowed) that may call script functions.
    pub fn compile_expr(&self, src: &str) -> Result<AST> {
        let names: Vec<String> = self
            .state
            .borrow()
            .data
            .series
            .iter()
            .map(|s| s.name.clone())
            .collect();
        let src = quote_series_names(src, &names);
        let ast = self
            .engine
            .compile(preprocess(&src, self.binary))
            .map_err(|e| anyhow!("{e}"))?;
        Ok(self.ast.borrow().clone_functions_only().merge(&ast))
    }

    /// Evaluate a compiled expression with the given variables, expecting a number.
    pub fn eval_number<K: AsRef<str>>(&self, ast: &AST, vars: &[(K, f64)]) -> Result<f64> {
        let mut scope = Scope::new();
        for (k, v) in vars {
            scope.push(k.as_ref(), *v);
        }
        let d: Dynamic = self
            .engine
            .eval_ast_with_scope(&mut scope, ast)
            .map_err(|e| anyhow!("{e}"))?;
        dyn_to_f64(&d)
            .ok_or_else(|| anyhow!("expression returned {} instead of a number", d.type_name()))
    }

    /// Scope with each series' latest (in-view) value bound to its identifier.
    fn series_scope(&self) -> Scope<'static> {
        let st = self.state.borrow();
        let mut scope = Scope::new();
        for s in &st.data.series {
            if let Some(&(_, y)) = s.in_view(st.view).last() {
                scope.push(ident(&s.name), y);
            }
        }
        scope
    }

    /// Evaluate console input. Series names are variables holding their latest value.
    pub fn eval(&self, src: &str) -> Result<Dynamic> {
        let ast = self.compile_expr(src)?;
        let mut scope = self.series_scope();
        self.engine
            .eval_ast_with_scope(&mut scope, &ast)
            .map_err(|e| anyhow!("{e}"))
    }

    /// Evaluate an alert condition.
    pub fn eval_bool(&self, src: &str) -> Result<bool> {
        let d = self.eval(src)?;
        d.as_bool()
            .map_err(|t| anyhow!("alert '{src}' returned {t}, expected true/false"))
    }

    /// Call the script's `parse(line)` function.
    pub fn parse_line(&self, line: &str) -> Result<Option<Vec<(String, ScriptVal)>>> {
        let opts = CallFnOptions::new().eval_ast(false);
        let d: Dynamic = self
            .engine
            .call_fn_with_options(
                opts,
                &mut Scope::new(),
                &self.ast.borrow(),
                "parse",
                (line.to_string(),),
            )
            .map_err(|e| anyhow!("parse(): {e}"))?;
        if d.is_unit() {
            return Ok(None);
        }
        let map = d
            .try_cast::<Map>()
            .ok_or_else(|| anyhow!("parse() must return a map or ()"))?;
        let mut out = Vec::new();
        flatten_map(&map, "", &mut out);
        Ok(Some(out))
    }
}

fn flatten_map(map: &Map, prefix: &str, out: &mut Vec<(String, ScriptVal)>) {
    for (k, v) in map {
        let key = if prefix.is_empty() {
            k.to_string()
        } else {
            format!("{prefix}.{k}")
        };
        if let Some(n) = dyn_to_f64(v) {
            out.push((key, ScriptVal::Num(n)));
        } else if let Ok(b) = v.as_bool() {
            out.push((key, ScriptVal::Num(f64::from(u8::from(b)))));
        } else if let Some(inner) = v.read_lock::<Map>() {
            flatten_map(&inner, &key, out);
        } else if !v.is_unit() {
            out.push((key, ScriptVal::Str(v.to_string())));
        }
    }
}

type State = Rc<RefCell<ScriptState>>;
type StatFn = fn(&Stats) -> f64;

/// A series' in-view points with its unit.
struct ViewPoints {
    points: Vec<(f64, f64)>,
    unit: Unit,
}

/// Points of a series within the current view.
fn points(state: &State, name: &str) -> FnResult<ViewPoints> {
    let st = state.borrow();
    let s = st.data.get(name).ok_or_else(|| {
        let names: Vec<&str> = st.data.series.iter().map(|s| s.name.as_str()).collect();
        format!("unknown series '{name}' (have: {})", names.join(", "))
    })?;
    Ok(ViewPoints {
        points: s.in_view(st.view).to_vec(),
        unit: s.unit.clone(),
    })
}

fn stats(state: &State, name: &str) -> FnResult<Stats> {
    let pts = points(state, name)?.points;
    Ok(Stats::compute(&pts).ok_or_else(|| format!("series '{name}' has no points in view"))?)
}

fn register_api(engine: &mut Engine, state: &State, binary: bool) {
    {
        let st = state.clone();
        engine.on_print(move |s| st.borrow_mut().output.push(s.to_string()));
        let st = state.clone();
        engine.on_debug(move |s, _, _| st.borrow_mut().output.push(s.to_string()));
    }

    let st = state.clone();
    engine.register_fn("series", move || -> Array {
        st.borrow()
            .data
            .series
            .iter()
            .map(|s| Dynamic::from(s.name.clone()))
            .collect()
    });

    let stat_fns: [(&str, StatFn); 15] = [
        ("first", |s| s.first),
        ("last", |s| s.last),
        ("min", |s| s.min),
        ("max", |s| s.max),
        ("mean", |s| s.mean),
        ("avg", |s| s.mean),
        ("median", |s| s.median),
        ("stddev", |s| s.stddev),
        ("p95", |s| s.p95),
        ("sum", |s| s.sum),
        ("r2", |s| s.r2),
        ("slope", |s| s.slope),
        ("change", |s| s.last - s.first),
        ("pct_change", |s| {
            if s.first == 0.0 {
                f64::NAN
            } else {
                (s.last - s.first) / s.first.abs() * 100.0
            }
        }),
        ("span", |s| s.x_last - s.x_first),
    ];
    for (fname, f) in stat_fns {
        let st = state.clone();
        engine.register_fn(fname, move |name: &str| -> FnResult<f64> {
            stats(&st, name).map(|s| f(&s))
        });
    }

    let st = state.clone();
    engine.register_fn("count", move |name: &str| -> FnResult<i64> {
        Ok(points(&st, name)?.points.len() as i64)
    });

    let st = state.clone();
    // (90 or 90.0: whole numbers are ints in Rhai.)
    engine.register_fn(
        "percentile",
        move |name: &str, q: Dynamic| -> FnResult<f64> {
            let q =
                dyn_to_f64(&q).ok_or("percentile needs a number, e.g. percentile(\"x\", 90)")?;
            let mut ys: Vec<f64> = points(&st, name)?.points.iter().map(|p| p.1).collect();
            ys.sort_by(f64::total_cmp);
            Ok(crate::stats::percentile(&ys, q / 100.0))
        },
    );

    // at(name): the value at the cursor (the point nearest it), else the newest.
    let st = state.clone();
    engine.register_fn("at", move |name: &str| -> FnResult<f64> {
        let cursor = st.borrow().cursor;
        let pts = points(&st, name)?.points;
        let p = match cursor {
            Some(x) => {
                let i = pts.partition_point(|p| p.0 < x);
                [i.checked_sub(1), Some(i)]
                    .into_iter()
                    .flatten()
                    .filter_map(|i| pts.get(i))
                    .min_by(|a, b| (a.0 - x).abs().total_cmp(&(b.0 - x).abs()))
            }
            None => pts.last(),
        };
        p.map(|p| p.1)
            .ok_or_else(|| format!("series '{name}' has no points in view").into())
    });

    // slope per duration: slope("node1", "1d") or slope("node1", 1d)
    let st = state.clone();
    engine.register_fn("slope", move |name: &str, per: Dynamic| -> FnResult<f64> {
        let per = match per.clone().into_immutable_string() {
            Ok(p) => crate::time::parse_duration_secs(&p).map_err(|e| e.to_string())?,
            Err(_) => dyn_to_f64(&per).ok_or("slope's second argument is a duration: \"1h\"")?,
        };
        Ok(stats(&st, name)?.slope * per)
    });

    // eta(name, target) -> seconds until the trend reaches target, or ()
    let st = state.clone();
    engine.register_fn(
        "eta",
        move |name: &str, target: Dynamic| -> FnResult<Dynamic> {
            let target = dyn_to_f64(&target).ok_or("eta target must be a number")?;
            Ok(stats(&st, name)?
                .eta(target)
                .map_or(Dynamic::UNIT, Dynamic::from_float))
        },
    );

    // forecast(name, ahead) -> trend value `ahead` seconds after the last point
    let st = state.clone();
    engine.register_fn(
        "forecast",
        move |name: &str, ahead: Dynamic| -> FnResult<f64> {
            let ahead = dyn_to_f64(&ahead).ok_or("forecast offset must be a number")?;
            let s = stats(&st, name)?;
            Ok(s.trend_at(s.x_last + ahead))
        },
    );

    // Array helpers for custom lines.
    let floats = |a: &Array| -> Vec<f64> { a.iter().filter_map(dyn_to_f64).collect() };
    let to_array = |v: Vec<f64>| -> Array { v.into_iter().map(Dynamic::from_float).collect() };
    engine.register_fn("ema", move |a: Array, n: i64| -> FnResult<Array> {
        if n < 1 {
            return Err("ema(values, n): n must be at least 1".into());
        }
        Ok(to_array(crate::lines::ema(&floats(&a), n as usize)))
    });
    engine.register_fn("rsi", move |a: Array, n: i64| -> FnResult<Array> {
        if n < 1 {
            return Err("rsi(values, n): n must be at least 1".into());
        }
        Ok(to_array(crate::lines::rsi(&floats(&a), n as usize)))
    });
    engine.register_fn("macd", move |a: Array| -> FnResult<Array> {
        Ok(to_array(crate::lines::macd(&floats(&a))))
    });
    engine.register_fn("sma", move |a: Array, n: i64| -> FnResult<Array> {
        if n < 1 {
            return Err("sma(values, n): n must be at least 1".into());
        }
        Ok(to_array(crate::lines::sma(&floats(&a), n as usize)))
    });
    let st = state.clone();
    engine.register_fn("mean_of", move |names: Array| -> FnResult<Array> {
        // The average of several series, on the first one's times (each where it has data).
        let names: Vec<String> = names.iter().map(|d| d.to_string()).collect();
        if names.is_empty() {
            return Err("mean_of([\"a\", \"b\"]) needs at least one series".into());
        }
        // Sources that haven't sent anything yet are left out (until they do).
        let others: Vec<Vec<(f64, f64)>> = names
            .iter()
            .filter_map(|n| points(&st, n).ok().map(|p| p.points))
            .filter(|p| !p.is_empty())
            .collect();
        let Some(base) = others.first().cloned() else {
            // None at all: say which names are unknown.
            points(&st, &names[0])?;
            return Ok(Array::new());
        };
        Ok(base
            .iter()
            .map(|&(x, _)| {
                let vals: Vec<f64> = others
                    .iter()
                    .filter_map(|p| crate::render::value_near(p, x))
                    .collect();
                if vals.is_empty() {
                    Dynamic::UNIT
                } else {
                    Dynamic::from_float(vals.iter().sum::<f64>() / vals.len() as f64)
                }
            })
            .collect())
    });
    let st = state.clone();
    engine.register_fn(
        "values_at",
        move |name: &str, xs: Array| -> FnResult<Array> {
            // Another series' values at these times (interpolated; () outside its range).
            let pts = points(&st, name)?.points;
            Ok(xs
                .iter()
                .map(|x| {
                    dyn_to_f64(x)
                        .and_then(|x| crate::render::value_near(&pts, x))
                        .map_or(Dynamic::UNIT, Dynamic::from_float)
                })
                .collect())
        },
    );

    for (fname, take_x) in [("values", false), ("xs", true)] {
        let st = state.clone();
        engine.register_fn(fname, move |name: &str| -> FnResult<Array> {
            Ok(points(&st, name)?
                .points
                .iter()
                .map(|p| Dynamic::from_float(if take_x { p.0 } else { p.1 }))
                .collect())
        });
    }

    // fmt(name, value) -> value formatted in the series' unit
    let st = state.clone();
    engine.register_fn("fmt", move |name: &str, v: Dynamic| -> FnResult<String> {
        let v = dyn_to_f64(&v).ok_or("fmt value must be a number")?;
        Ok(points(&st, name)?.unit.format(v))
    });
    engine.register_fn("fmt_dur", |secs: Dynamic| -> FnResult<String> {
        Ok(crate::time::fmt_duration(
            dyn_to_f64(&secs).ok_or("fmt_dur needs a number")?,
        ))
    });
    engine.register_fn("fmt_time", |x: Dynamic| -> FnResult<String> {
        Ok(crate::time::format_time_full(
            dyn_to_f64(&x).ok_or("fmt_time needs a number")?,
        ))
    });
    engine.register_fn("bytes", move |v: Dynamic| -> FnResult<String> {
        Ok(Unit::new(crate::units::Dim::Bytes { binary })
            .format(dyn_to_f64(&v).ok_or("bytes needs a number")?))
    });

    // Viewer actions.
    let push = |state: &State, a: Action| state.borrow_mut().actions.push(a);
    let st = state.clone();
    engine.register_fn("hline", move |v: Dynamic| -> FnResult<()> {
        push(
            &st,
            Action::HLine(dyn_to_f64(&v).ok_or("hline needs a number")?, None),
        );
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("hline", move |v: Dynamic, label: &str| -> FnResult<()> {
        push(
            &st,
            Action::HLine(
                dyn_to_f64(&v).ok_or("hline needs a number")?,
                Some(label.to_string()),
            ),
        );
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("clear_hlines", move || push(&st, Action::ClearHLines));
    let st = state.clone();
    engine.register_fn("zoom", move |d: Dynamic| -> FnResult<()> {
        let secs = match d.clone().into_immutable_string() {
            Ok(s) => crate::time::parse_duration_secs(&s).map_err(|e| e.to_string())?,
            Err(_) => dyn_to_f64(&d).ok_or("zoom needs a duration like \"6h\" or a number")?,
        };
        push(&st, Action::Zoom(secs));
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("reset", move || push(&st, Action::ResetView));
    let st = state.clone();
    engine.register_fn("show", move |n: &str| push(&st, Action::Show(n.into())));
    let st = state.clone();
    engine.register_fn("hide", move |n: &str| push(&st, Action::Hide(n.into())));
    let st = state.clone();
    engine.register_fn("toggle", move |n: &str| push(&st, Action::Toggle(n.into())));
    let st = state.clone();
    engine.register_fn("show_all", move || push(&st, Action::ShowAll));
    // only("eth"), only("eth", "ema30"), only(["eth", "ema30"])…
    let st = state.clone();
    engine.register_fn("only", move |names: Array| {
        push(
            &st,
            Action::Only(names.iter().map(|d| d.to_string()).collect()),
        )
    });
    let st = state.clone();
    engine.register_fn("only", move |a: &str| {
        push(&st, Action::Only(vec![a.into()]))
    });
    let st = state.clone();
    engine.register_fn("only", move |a: &str, b: &str| {
        push(&st, Action::Only(vec![a.into(), b.into()]))
    });
    let st = state.clone();
    engine.register_fn("only", move |a: &str, b: &str, c: &str| {
        push(&st, Action::Only(vec![a.into(), b.into(), c.into()]))
    });
    let st = state.clone();
    engine.register_fn("chart", move |k: &str| push(&st, Action::Chart(k.into())));
    for n in 0..=4 {
        let st = state.clone();
        let f = move |names: Vec<String>| push(&st, Action::Front(names));
        match n {
            0 => engine.register_fn("front", move || f(Vec::new())),
            1 => engine.register_fn("front", move |a: &str| f(vec![a.into()])),
            2 => engine.register_fn("front", move |a: &str, b: &str| f(vec![a.into(), b.into()])),
            3 => engine.register_fn("front", move |a: &str, b: &str, c: &str| {
                f(vec![a.into(), b.into(), c.into()])
            }),
            _ => engine.register_fn("front", move |a: &str, b: &str, c: &str, d: &str| {
                f(vec![a.into(), b.into(), c.into(), d.into()])
            }),
        };
    }
    let st = state.clone();
    engine.register_fn("target", move |n: &str, v: Dynamic| -> FnResult<()> {
        push(
            &st,
            Action::Target(
                n.into(),
                Some(dyn_to_f64(&v).ok_or("target needs a number, e.g. target(\"node1\", 50GB)")?),
            ),
        );
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("trend", move |t: &str| {
        push(
            &st,
            Action::Trend((!matches!(t, "off" | "none" | "false")).then(|| t.to_string())),
        );
    });
    for &(name, flag) in FLAGS {
        let st = state.clone();
        engine.register_fn(name, move |on: bool| {
            push(&st, Action::Flag(flag, Some(on)))
        });
        let st = state.clone();
        engine.register_fn(name, move || push(&st, Action::Flag(flag, None)));
    }
    let st = state.clone();
    engine.register_fn("lines", move |l: &str| {
        push(&st, Action::Lines(l.to_string(), None))
    });
    let st = state.clone();
    engine.register_fn("lines", move |l: &str, r: &str| {
        push(&st, Action::Lines(l.to_string(), Some(r.to_string())))
    });
    let st = state.clone();
    engine.register_fn("window", move |w: Dynamic| -> FnResult<()> {
        let n = match w.clone().into_immutable_string() {
            Ok(s) if s == "auto" => None,
            Ok(s) => Some(
                s.trim()
                    .parse::<usize>()
                    .map_err(|_| "window needs a number of points, or \"auto\"")?,
            ),
            Err(_) => Some(
                dyn_to_f64(&w)
                    .ok_or("window needs a number of points, or \"auto\"")?
                    .round()
                    .max(1.0) as usize,
            ),
        };
        push(&st, Action::TrendWindow(n));
        Ok(())
    });
    // `timeframe` is the toolbar button's name; `range` the original one.
    for name in ["range", "timeframe"] {
        let st = state.clone();
        engine.register_fn(name, move |r: Dynamic| -> FnResult<()> {
            match r.clone().into_immutable_string() {
                Ok(s) => push(
                    &st,
                    Action::Range(
                        (!matches!(s.as_str(), "all" | "off" | "none")).then(|| s.to_string()),
                    ),
                ),
                Err(_) => push(
                    &st,
                    Action::RangeNumber(
                        dyn_to_f64(&r).ok_or("timeframe needs e.g. 1h, \"7d\", 500 or \"all\"")?,
                    ),
                ),
            }
            Ok(())
        });
        let st = state.clone();
        engine.register_fn(name, move |a: &str, b: &str| {
            push(
                &st,
                Action::Between(a.to_string(), (!b.is_empty()).then(|| b.to_string())),
            )
        });
    }
    let st = state.clone();
    engine.register_fn("pan", move |by: Dynamic| -> FnResult<()> {
        let f = match by.clone().into_immutable_string() {
            Ok(s) if s == "left" => -0.2,
            Ok(s) if s == "right" => 0.2,
            Ok(_) => return Err("pan needs \"left\", \"right\" or a fraction of the view (-0.5 = half a screen left)".into()),
            Err(_) => dyn_to_f64(&by).ok_or("pan needs \"left\" or \"right\"")?,
        };
        push(&st, Action::Pan(f));
        Ok(())
    });
    let pane = |st: &State, a: PaneAction| st.borrow_mut().actions.push(Action::Pane(a));
    let st = state.clone();
    engine.register_fn("split_right", move || pane(&st, PaneAction::Split(false)));
    let st = state.clone();
    engine.register_fn("split_down", move || pane(&st, PaneAction::Split(true)));
    let st = state.clone();
    engine.register_fn("close_pane", move || pane(&st, PaneAction::Close));
    let st = state.clone();
    engine.register_fn("focus_pane", move |to: Dynamic| -> FnResult<()> {
        let to = match to.clone().into_immutable_string() {
            Ok(s) => s.to_string(),
            Err(_) => to
                .as_int()
                .map_err(
                    |_| "focus_pane needs \"left\", \"right\", \"up\", \"down\" or a pane number",
                )?
                .to_string(),
        };
        pane(&st, PaneAction::Focus(to));
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("move_pane", move |d: &str| {
        pane(&st, PaneAction::Move(d.into()))
    });
    let st = state.clone();
    engine.register_fn("resize_pane", move |d: &str| {
        pane(&st, PaneAction::Resize(d.into(), 0.05))
    });
    let st = state.clone();
    engine.register_fn("resize_pane", move |d: &str, by: Dynamic| -> FnResult<()> {
        let v = dyn_to_f64(&by).ok_or("resize_pane(\"right\", 10) moves the edge 10%")?;
        pane(&st, PaneAction::Resize(d.into(), v / 100.0));
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("even_panes", move || pane(&st, PaneAction::Even));
    let st = state.clone();
    engine.register_fn("single", move || pane(&st, PaneAction::Single));
    let st = state.clone();
    engine.register_fn("layout", move |l: &str| {
        pane(&st, PaneAction::Layout(l.into()))
    });
    let st = state.clone();
    engine.register_fn("pane", move |spec: &str| {
        pane(&st, PaneAction::Set(spec.into()))
    });
    let st = state.clone();
    engine.register_fn("pane_source", move |src: &str| {
        pane(&st, PaneAction::Source(src.into()))
    });
    let st = state.clone();
    engine.register_fn("zoom_in", move || push(&st, Action::ZoomStep(true)));
    let st = state.clone();
    engine.register_fn("zoom_out", move || push(&st, Action::ZoomStep(false)));
    let st = state.clone();
    engine.register_fn("svg", move || push(&st, Action::Svg(None)));
    let st = state.clone();
    engine.register_fn("svg", move |path: &str| {
        push(&st, Action::Svg(Some(path.to_string())))
    });
    let st = state.clone();
    engine.register_fn("save", move |name: &str| {
        push(&st, Action::SaveProfile(name.into()))
    });
    // mark_when(expr, label, true): also notify for each new marker.
    let st = state.clone();
    engine.register_fn(
        "mark_when",
        move |expr: &str, label: &str, notify: bool| -> FnResult<()> {
            let mut rule = crate::marks::MarkRule::parse(&format!("{expr} => {label}"))?;
            rule.notify = notify;
            st.borrow_mut().mark_rules.push(rule);
            Ok(())
        },
    );
    for with_label in [false, true] {
        let st = state.clone();
        let f = move |expr: &str, label: &str| -> FnResult<()> {
            let spec = if with_label {
                format!("{expr} => {label}")
            } else {
                expr.to_string()
            };
            let rule = crate::marks::MarkRule::parse(&spec)?;
            st.borrow_mut().mark_rules.push(rule);
            Ok(())
        };
        if with_label {
            engine.register_fn("mark_when", f);
        } else {
            engine.register_fn("mark_when", move |expr: &str| f(expr, ""));
        }
    }
    let st = state.clone();
    engine.register_fn("mark", move |label: &str| {
        push(&st, Action::MarkHere(label.into()))
    });
    let st = state.clone();
    engine.register_fn("clear_marks", move || push(&st, Action::ClearMarks));
    let st = state.clone();
    engine.register_fn("merge", move |spec: &str| {
        push(&st, Action::Merge(spec.into()))
    });
    let st = state.clone();
    engine.register_fn("unmerge", move |name: &str| {
        push(&st, Action::Unmerge(name.into()))
    });
    let st = state.clone();
    engine.register_fn("alert", move |e: &str| push(&st, Action::Alert(e.into())));
    let st = state.clone();
    engine.register_fn("unalert", move |e: &str| {
        push(&st, Action::Unalert(e.into()))
    });
    let st = state.clone();
    engine.register_fn("notify_to", move |t: &str| {
        push(&st, Action::NotifyTo(t.into()))
    });
    let st = state.clone();
    engine.register_fn("notify_off", move |t: &str| {
        push(&st, Action::NotifyOff(t.into()))
    });
    let st = state.clone();
    engine.register_fn("load", move |path: &str| {
        push(&st, Action::Load(path.into()))
    });
    let st = state.clone();
    engine.register_fn("param", move |name: &str, value: &str| {
        push(&st, Action::Param(name.into(), value.into()))
    });
    let st = state.clone();
    engine.register_fn("title", move |t: &str| push(&st, Action::Title(t.into())));
    let st = state.clone();
    engine.register_fn("unit", move |u: &str| push(&st, Action::Unit(u.into())));
    let st = state.clone();
    engine.register_fn("add_line", move |name: &str, expr: &str| {
        push(&st, Action::AddLine(name.into(), expr.into()))
    });
    let st = state.clone();
    engine.register_fn("remove_line", move |name: &str| {
        push(&st, Action::RemoveLine(name.into()))
    });
    let st = state.clone();
    engine.register_fn("undo", move || push(&st, Action::Undo));
    let st = state.clone();
    engine.register_fn("redo", move || push(&st, Action::Redo));
    let st = state.clone();
    engine.register_fn("copy", move || push(&st, Action::Copy(None)));
    let st = state.clone();
    engine.register_fn("copy", move |t: Dynamic| {
        push(&st, Action::Copy(Some(t.to_string())))
    });
    let st = state.clone();
    engine.register_fn("help", move || push(&st, Action::Help));
    let st = state.clone();
    engine.register_fn("quit", move || push(&st, Action::Quit));
    let st = state.clone();
    engine.register_fn("format", move || push(&st, Action::Format));
    let st = state.clone();
    engine.register_fn("clear_target", move |n: &str| {
        push(&st, Action::Target(n.into(), None))
    });
    let st = state.clone();
    engine.register_fn("forecast", move |h: &str| {
        push(
            &st,
            Action::Forecast((!matches!(h, "off" | "none" | "false")).then(|| h.to_string())),
        );
    });
    let st = state.clone();
    engine.register_fn("forecast", move |on: bool| {
        push(&st, Action::Forecast(on.then(|| "auto".to_string())))
    });
    let st = state.clone();
    engine.register_fn(
        "bind",
        move |key: ImmutableString, code: ImmutableString| {
            st.borrow_mut()
                .binds
                .push((key.to_string(), code.to_string()));
        },
    );
}

#[cfg(test)]
mod tests {

    /// Every console function has a usage line (for errors and hints) and a place in
    /// the `help` list.
    #[test]
    fn every_function_has_usage_and_a_group() {
        let src = include_str!("script.rs");
        let names = |re: &str| -> Vec<String> {
            Regex::new(re)
                .unwrap()
                .captures_iter(src)
                .map(|c| c[1].to_string())
                .collect()
        };
        let mut all = names(r#"register_fn\(\s*"([a-z_0-9]+)""#);
        all.extend(names(r#"\("([a-z_0-9]+)", (?:\|s\||false|true)"#));
        all.extend(FLAGS.iter().map(|f| f.0.to_string()));
        all.extend(["range".to_string(), "timeframe".to_string()]);
        for n in &all {
            assert!(usage(n).is_some(), "{n}() needs a line in USAGE");
            let groups = GROUPS.iter().filter(|g| g.1.contains(&n.as_str())).count();
            assert!(
                n == "help" || groups == 1,
                "{n}() is in {groups} GROUPS (needs 1)"
            );
        }
        for u in USAGE {
            assert!(
                all.iter().any(|n| n == u.0),
                "USAGE has {} but there's no such function",
                u.0
            );
        }
    }

    #[test]
    fn completions() {
        let s: Vec<String> = ["web", "api", "db"].iter().map(|s| s.to_string()).collect();
        let items = |t: &str| -> Vec<String> {
            complete(t, t.chars().count(), &s)
                .items
                .into_iter()
                .map(|i| i.insert)
                .collect()
        };
        assert_eq!(items("und"), ["undo()"]);
        assert_eq!(items("perc")[0], "percentile(\"");
        assert_eq!(items("zoom_"), ["zoom_in()", "zoom_out()"]);
        assert_eq!(items("ap"), ["api"]);
        // The last argument closes the call; one with more to come doesn't.
        assert_eq!(items("last(\"")[..3], ["web\")", "api\")", "db\")"]);
        assert_eq!(items("percentile(\"d"), ["db\""]);
        assert_eq!(items("only(\"web\", \"a"), ["api\""]);
        assert_eq!(items("last("), ["\"web\")", "\"api\")", "\"db\")"]);
        assert_eq!(items("notify_to(\"n"), ["ntfy:"]);
        assert!(
            items("title(\"").is_empty(),
            "free text: nothing to suggest"
        );
        assert!(items("5").is_empty());
        // Inside an expression argument, quoted or not: series first, text in backticks.
        let s2: Vec<String> = ["user", "sys", "busy"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let items2 = |t: &str| -> Vec<String> {
            complete(t, t.chars().count(), &s2)
                .items
                .into_iter()
                .map(|i| i.insert)
                .collect()
        };
        assert_eq!(items2(r#"add_line("total", "us"#)[0], "user");
        assert_eq!(items2(r#"add_line("total", us"#)[0], "user");
        assert_eq!(items2(r#"add_line("total", "user + s"#)[0], "sys");
        assert_eq!(items2(r#"add_line("t", "ema(values(`b"#), ["busy`)"]);
        assert!(items2(r#"add_line("t", "va"#).contains(&"values(`".to_string()));
        assert_eq!(items2(r#"mark_when("del"#), ["delta"]);
        assert_eq!(items2(r#"alert("sy"#)[0], "sys");
        let c = complete("slope(\"api\", ", 13, &s);
        assert_eq!(
            c.signature.map(|s| s.0),
            Some(r#"slope("api")  ·  slope("api", "1h")"#)
        );
    }

    use super::*;
    use crate::data::{Series, XKind};
    use crate::units::Dim;

    fn host_with_data() -> ScriptHost {
        let host = ScriptHost::new(None, false).unwrap();
        let unit = Unit::new(Dim::Bytes { binary: false });
        let s = Series {
            name: "node1".into(),
            unit,
            points: vec![(0.0, 60e9), (86400.0, 58e9), (172_800.0, 56e9)],
        };
        host.set_data(
            Dataset {
                x_kind: XKind::Time,
                series: vec![s],
            },
            None,
        );
        host
    }

    #[test]
    fn bind_takes_code() {
        assert_eq!(
            quote_bind_code(r#"bind("r", only("eth", "ema30"))"#),
            r#"bind("r", "only(\"eth\", \"ema30\")")"#
        );
        assert_eq!(
            quote_bind_code(r#"bind("r", "hide(\"x\")")"#),
            r#"bind("r", "hide(\"x\")")"#
        );
        assert_eq!(
            quote_bind_code(r#"bind("x", { hide("a"); zoom(6h) }); let y = 1;"#),
            r#"bind("x", "{ hide(\"a\"); zoom(6h) }"); let y = 1;"#
        );
        assert_eq!(
            quote_bind_code(r#"bind("c", || toggle("cb"))"#),
            r#"bind("c", "toggle(\"cb\")")"#
        );
        // Not calls: inside strings and comments, or other names.
        let s = r#"print("bind(\"r\", x)"); // bind("r", y)
rebind("a", b)"#;
        assert_eq!(quote_bind_code(s), s);
        // A bound key really runs it.
        let host = ScriptHost::new(None, false).unwrap();
        let _ = host.eval(r#"bind("r", hide("node1"))"#).unwrap();
        assert_eq!(
            host.binds(),
            [("r".to_string(), r#"hide("node1")"#.to_string())]
        );
        assert!(
            host.take_actions().is_empty(),
            "binding doesn't run the code"
        );
    }

    #[test]
    fn comments_percent_and_multiline_binds() {
        // An apostrophe in a comment doesn't hide the code after it from unit rewriting.
        assert_eq!(
            preprocess("// we're over\nx > 5GB", false),
            "// we're over\nx > 5000000000.0"
        );
        assert_eq!(
            preprocess("/* it's */ x > 1KB", false),
            "/* it's */ x > 1000.0"
        );
        // Percentages, but not the remainder operator.
        assert_eq!(preprocess("value > 90%", false), "value > 90.0");
        assert_eq!(preprocess("10 % 3 == 1", false), "10 % 3 == 1");
        assert_eq!(preprocess("(90%)", false), "(90.0)");
        // A bind() block over several lines.
        let host = ScriptHost::new(None, false).unwrap();
        let _ = host
            .eval("bind(\"x\", {\n  hide(\"a\");\n  zoom(6h)\n})")
            .unwrap();
        assert_eq!(host.binds()[0].1, "{\n  hide(\"a\");\n  zoom(6h)\n}");
    }

    #[test]
    fn preprocess_unit_literals() {
        assert_eq!(preprocess("node1 < 50GB", false), "node1 < 50000000000.0");
        assert_eq!(preprocess("x * 1d + 2 h", false), "x * 86400.0 + 2 h");
        assert_eq!(preprocess("t > 1.5s", false), "t > 1.5");
        assert_eq!(
            preprocess(r#"zoom("6h") + node1"#, false),
            r#"zoom("6h") + node1"#
        );
        assert_eq!(preprocess("1G", true), "1073741824.0");
    }

    #[test]
    fn idents() {
        assert_eq!(ident("cpu.user"), "cpu_user");
        assert_eq!(ident("Active(anon)"), "Active_anon_");
        assert_eq!(ident("1min"), "_1min");
        assert_eq!(ident("in"), "in_");
    }

    #[test]
    fn api_functions() {
        let h = host_with_data();
        assert_eq!(h.eval("last(\"node1\")").unwrap().as_float().unwrap(), 56e9);
        assert!(h.eval_bool("node1 < 57GB").unwrap());
        assert!(h.eval_bool("slope(\"node1\") * 1d < -1.9GB").unwrap());
        assert_eq!(
            h.eval("fmt(\"node1\", slope(\"node1\", \"1d\"))")
                .unwrap()
                .to_string(),
            "-2 GB"
        );
        // 56 GB at 2 GB/day → 28 days.
        assert_eq!(
            h.eval("fmt_dur(eta(\"node1\", 0))").unwrap().to_string(),
            "28 days"
        );
        assert!(h.eval("last(\"nope\")").is_err());
        assert_eq!(h.eval("count(\"node1\") + 1").unwrap().as_int().unwrap(), 4);
    }

    #[test]
    fn actions_and_print() {
        let h = host_with_data();
        assert!(
            h.eval("hline(50GB, \"limit\"); zoom(\"6h\"); print(\"hi\")")
                .unwrap()
                .is_unit()
        );
        assert_eq!(
            h.take_actions(),
            vec![
                Action::HLine(50e9, Some("limit".into())),
                Action::Zoom(21600.0)
            ]
        );
        assert_eq!(h.take_output(), vec!["hi"]);
    }

    #[test]
    fn derived_expression() {
        let h = ScriptHost::new(None, false).unwrap();
        let ast = h.compile_expr("a - b / 2").unwrap();
        assert_eq!(
            h.eval_number(&ast, &[("a", 10.0), ("b", 4.0)]).unwrap(),
            8.0
        );
    }
}
