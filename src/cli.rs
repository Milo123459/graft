//! Command-line interface definition.

use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Parser, ValueEnum};
use clap_complete::Shell;

use crate::time::parse_duration;

const EXAMPLES: &str = "\
Examples:
  graft                                         quick start: ready-made charts, or build your own
  graft reclaimable.log                         no format given: opens the builder with suggestions
  graft reclaimable.log -F '{time} node1: {node1} node2: {node2}'   graph it with this format
  graft reclaimable.log -F '{key}: {value}' --stats --once           print chart + stats, then exit
  graft reclaimable.log --kv --forecast=7d --target node1=50GB --fit-window 12h
  graft app.log -F '{time} {_} took {latency}'  `{_}` skips text; units like `12ms` are understood
  graft app.log -r '(?P<time>\\S+ \\S+) .* took (?P<latency>\\d+ms)'
  graft -c 'cat /proc/meminfo' -n 1s --binary-units -s MemFree,Cached
  graft -c 'df -B1' -F '{key} {_} {used} {avail} {_}' -s '*.avail'
  graft data.csv --csv --resample 1h:max --smooth 3
  graft reclaimable.log --kv --alert 'node1 < 50GB' --once --no-chart && echo ok
  graft -p cpu                                  run a preset (cpu, memory, load, network, disk,
                                                temperature, ping, processes) or a saved profile

Format templates (-F): {name} captures a number (with optional unit) as series `name`;
{time} or {time:%d/%m %H:%M} a timestamp; {x} a numeric x; {key} names the series
(one per distinct value); {_} skips any text; {name:int}, {name:word} and {name:/regex/}
change what is matched; spaces match any run of whitespace; {{ and }} are literal braces.

Interactive: mouse/touch works everywhere (tap, drag to measure, scroll to zoom);
press ? inside the viewer for keys.";

/// Graph anything: logs, command output, CSV or JSON — right in your terminal.
#[derive(Parser, Debug, Clone, PartialEq)]
#[command(name = "graft", version, about, after_help = EXAMPLES, args_override_self = true)]
pub struct Cli {
    /// Input file. Use "-" (or pipe data in) to read stdin. Omit to open the builder.
    #[arg(value_name = "FILE")]
    pub input: Option<PathBuf>,

    // ───────────── Input ─────────────
    /// Run a shell command repeatedly and graph its output (like `watch`).
    #[arg(
        short = 'c',
        long,
        value_name = "COMMAND",
        help_heading = "Input",
        conflicts_with = "input"
    )]
    pub cmd: Option<String>,

    /// Interval between --cmd runs.
    #[arg(short = 'n', long, value_name = "DURATION", default_value = "1s", value_parser = parse_duration, help_heading = "Input")]
    pub every: Duration,

    /// Stop after this many --cmd samples (non-interactive default: 1).
    #[arg(long, value_name = "N", help_heading = "Input")]
    pub count: Option<u64>,

    /// Keep reading the file as it grows (like `tail -f`).
    #[arg(short = 'f', long, help_heading = "Input")]
    pub follow: bool,

    /// Only keep recent data: a duration (`10m`, `6h`) or a number of points (`500`).
    #[arg(short = 'w', long, value_name = "WINDOW", value_parser = parse_window, help_heading = "Input")]
    pub window: Option<Window>,

    // ───────────── Parsing ─────────────
    /// Parse lines with a format template, e.g. `{time} node1: {node1} node2: {node2}`
    /// (see "Format templates" below).
    #[arg(
        short = 'F',
        long,
        value_name = "TEMPLATE",
        group = "parser",
        help_heading = "Parsing"
    )]
    pub format: Option<String>,

    /// Parse lines with a regex. Named groups become series; special names:
    /// `time` (timestamp), `x` (numeric x), `key`/`series` (series name), `value`, `unit`.
    #[arg(
        short = 'r',
        long,
        value_name = "REGEX",
        group = "parser",
        help_heading = "Parsing"
    )]
    pub regex: Option<String>,

    /// Parse `key: value` / `key=value` pairs.
    #[arg(long, group = "parser", help_heading = "Parsing")]
    pub kv: bool,

    /// Guess the format: `key: value` pairs if there are any, else numeric columns.
    /// (A format is never guessed unless you ask for it.)
    #[arg(long, group = "parser", help_heading = "Parsing")]
    pub auto: bool,

    /// Parse CSV (header row auto-detected).
    #[arg(long, group = "parser", help_heading = "Parsing")]
    pub csv: bool,

    /// Parse JSON objects (NDJSON or multi-line documents); nested keys become `a.b.c`.
    #[arg(long, group = "parser", help_heading = "Parsing")]
    pub json: bool,

    /// Whitespace-separated columns: `1,3` (1-based) or `all` numeric columns.
    #[arg(
        long,
        value_name = "N,..|all",
        group = "parser",
        help_heading = "Parsing"
    )]
    pub columns: Option<String>,

    /// Parse each line with the `parse(line)` function from --script (returns a map).
    #[arg(long, group = "parser", requires = "script", help_heading = "Parsing")]
    pub parse_fn: bool,

    /// Names for --columns / CSV columns, e.g. `load1,load5,load15`.
    #[arg(
        long,
        value_name = "NAME,..",
        value_delimiter = ',',
        help_heading = "Parsing"
    )]
    pub names: Vec<String>,

    /// Field delimiter for --csv / --columns.
    #[arg(short = 'd', long, value_name = "CHAR", help_heading = "Parsing")]
    pub delimiter: Option<char>,

    /// Regex capture whose value names the series (one line per distinct value).
    #[arg(short = 'g', long, value_name = "CAPTURE", help_heading = "Parsing")]
    pub group_by: Option<String>,

    /// CSV column / JSON key holding the timestamp.
    #[arg(long, value_name = "FIELD", help_heading = "Parsing")]
    pub time_field: Option<String>,

    /// strftime format for timestamps, e.g. `%d/%m/%Y %H:%M` (default: auto-detect).
    #[arg(short = 't', long, value_name = "FORMAT", help_heading = "Parsing")]
    pub time_format: Option<String>,

    /// What to use for the x axis.
    #[arg(short = 'X', long, value_enum, default_value_t = XMode::Auto, help_heading = "Parsing")]
    pub x: XMode,

    /// Treat kB/MB/GB/K/M/G as 1024-based (like /proc/meminfo, `df -h`, `free -h`).
    #[arg(short = 'B', long, help_heading = "Parsing")]
    pub binary_units: bool,

    /// Unit for values that have none, e.g. `B`, `ms`, `%`.
    #[arg(short = 'u', long, value_name = "UNIT", help_heading = "Parsing")]
    pub unit: Option<String>,

    // ───────────── Series ─────────────
    /// Only show these series (comma separated, `*` globs allowed).
    #[arg(
        short = 's',
        long,
        value_name = "NAME,..",
        value_delimiter = ',',
        help_heading = "Series"
    )]
    pub select: Vec<String>,

    /// Hide these series (comma separated, `*` globs allowed).
    #[arg(
        short = 'e',
        long,
        value_name = "NAME,..",
        value_delimiter = ',',
        help_heading = "Series"
    )]
    pub exclude: Vec<String>,

    /// Add a derived series using a Rhai expression, e.g. `used = MemTotal - MemAvailable`
    /// or with a unit, `busy:% = user / total * 100`. Computed after --rate/--delta.
    #[arg(long = "series", value_name = "NAME=EXPR", action = ArgAction::Append, help_heading = "Series")]
    pub derived: Vec<String>,

    /// Convert counters into per-second rates.
    #[arg(long, help_heading = "Series")]
    pub rate: bool,

    /// Show the change between consecutive points.
    #[arg(long, help_heading = "Series")]
    pub delta: bool,

    /// Bucket points: `1h` (mean) or `1h:max` (mean|min|max|sum|last|count).
    #[arg(long, value_name = "WIDTH[:AGG]", value_parser = parse_resample, help_heading = "Series")]
    pub resample: Option<Resample>,

    /// Moving average over N points.
    #[arg(long, value_name = "N", help_heading = "Series")]
    pub smooth: Option<usize>,

    /// Timeframe: only use the most recent part of the data — a duration (`7d`, `12h`)
    /// or a number of points. Unlike --window this is a view: the viewer can widen it again.
    #[arg(short = 'L', long, value_name = "DURATION|N", value_parser = parse_window, help_heading = "Series")]
    pub last: Option<Window>,

    /// Timeframe start: a timestamp like `2026-10-15` or `2026-10-15 08:00` (or a number for numeric x).
    #[arg(long, value_name = "TIME", help_heading = "Series")]
    pub since: Option<String>,

    /// Timeframe end (same forms as --since).
    #[arg(long, value_name = "TIME", help_heading = "Series")]
    pub until: Option<String>,

    // ───────────── Chart ─────────────
    /// Chart type.
    #[arg(short = 'k', long, value_enum, default_value_t = ChartKind::Line, help_heading = "Chart")]
    pub kind: ChartKind,

    /// Give each series its own panel and y axis (useful when scales differ a lot).
    #[arg(long, help_heading = "Chart")]
    pub split: bool,

    /// Point marker for line/scatter charts.
    #[arg(short = 'm', long, value_enum, default_value_t = MarkerKind::Braille, help_heading = "Chart")]
    pub marker: MarkerKind,

    /// Chart title.
    #[arg(long, help_heading = "Chart")]
    pub title: Option<String>,

    /// Series colours, e.g. `cyan,#ff8800,203`.
    #[arg(
        long,
        value_name = "COLOR,..",
        value_delimiter = ',',
        help_heading = "Chart"
    )]
    pub colors: Vec<String>,

    /// Legend position.
    #[arg(long, value_enum, default_value_t = LegendPos::TopRight, help_heading = "Chart")]
    pub legend: LegendPos,

    /// Logarithmic y axis.
    #[arg(long, help_heading = "Chart")]
    pub log: bool,

    /// Lower y bound (units allowed, e.g. `0`, `50GB`).
    #[arg(
        long,
        value_name = "VALUE",
        allow_hyphen_values = true,
        help_heading = "Chart"
    )]
    pub ymin: Option<String>,

    /// Upper y bound (units allowed).
    #[arg(
        long,
        value_name = "VALUE",
        allow_hyphen_values = true,
        help_heading = "Chart"
    )]
    pub ymax: Option<String>,

    /// Horizontal reference line: `VALUE[:COLOR[:LABEL]]`, e.g. `50GB:red:limit`. Repeatable.
    #[arg(long, value_name = "SPEC", action = ArgAction::Append, help_heading = "Chart")]
    pub hline: Vec<String>,

    /// Number of histogram bins.
    #[arg(long, default_value_t = 20, help_heading = "Chart")]
    pub bins: usize,

    /// strftime format for time-axis labels (default: chosen from the visible span).
    #[arg(long, value_name = "FORMAT", help_heading = "Chart")]
    pub x_format: Option<String>,

    /// Output width in columns (default: terminal width).
    #[arg(long, help_heading = "Chart")]
    pub width: Option<u16>,

    /// Output height in rows (default: fit the terminal).
    #[arg(long, help_heading = "Chart")]
    pub height: Option<u16>,

    /// Disable colours (also honours NO_COLOR).
    #[arg(long, env = "NO_COLOR", value_parser = clap::builder::FalseyValueParser::new(), help_heading = "Chart")]
    pub no_color: bool,

    // ───────────── Analysis ─────────────
    /// Print statistics and trends (slope, R², forecast) for each series.
    #[arg(short = 'S', long, help_heading = "Analysis")]
    pub stats: bool,

    /// Stats output: `text` (table + sentences) or `json`.
    #[arg(long, value_enum, default_value_t = StatsFormat::Text, help_heading = "Analysis")]
    pub stats_format: StatsFormat,

    /// One line per series from a template, e.g. `{name}: {trend}/{per}, {eta}`. Fields:
    /// name n first last min max mean median stddev p95 change trend per r2 target eta eta_at
    /// direction model; append `:raw` for unformatted numbers (`{last:raw}`).
    #[arg(long, value_name = "TEMPLATE", help_heading = "Analysis")]
    pub stats_template: Option<String>,

    /// Express trends per this duration (default: chosen from the data span).
    #[arg(long, value_name = "DURATION", help_heading = "Analysis")]
    pub per: Option<String>,

    /// Trend model used for trends and forecasts.
    #[arg(long, value_enum, default_value_t = FitModel::Linear, help_heading = "Analysis")]
    pub fit: FitModel,

    /// Fit trends to recent data only: a duration (`12h`) or a number of points.
    #[arg(long, value_name = "WINDOW", value_parser = parse_window, help_heading = "Analysis")]
    pub fit_window: Option<Window>,

    /// Forecast target(s): `50GB` for every series, `node1=50GB` for one, `none` to disable.
    /// Comma separated or repeated. Default: 0 for series that are falling.
    #[arg(
        long,
        value_name = "[NAME=]VALUE",
        value_delimiter = ',',
        allow_hyphen_values = true,
        help_heading = "Analysis"
    )]
    pub target: Vec<String>,

    /// Draw the trend projected into the future, e.g. `--forecast` or `--forecast=7d`.
    #[arg(long, value_name = "HORIZON", num_args = 0..=1, require_equals = true, default_missing_value = "auto", help_heading = "Analysis")]
    pub forecast: Option<String>,

    /// Trend lines, any combination: avg (moving average), median, p95 (rolling 95th
    /// percentile), fit (the straight trend line). `--trend` alone = fit;
    /// `--trend=avg,p95`. The raw data is drawn in a darker shade underneath.
    #[arg(long, value_enum, value_name = "LINES", num_args = 0..=1, require_equals = true, value_delimiter = ',', default_missing_value = "fit", help_heading = "Analysis")]
    pub trend: Vec<TrendStyle>,

    /// Show only the trend lines, not the raw data (implies --trend).
    #[arg(long, help_heading = "Analysis")]
    pub trend_only: bool,

    /// Points in the moving average / median / p95 (default: 10% of the data, at least 10).
    #[arg(long, value_name = "N", help_heading = "Analysis")]
    pub trend_window: Option<usize>,

    /// Two charts side by side: the live data, and its trends.
    #[arg(long, help_heading = "Chart")]
    pub side_by_side: bool,

    /// Don't draw target lines (targets still drive forecasts and stats).
    #[arg(long, help_heading = "Analysis")]
    pub no_targets: bool,

    /// Exit with status 3 if a Rhai condition holds, e.g. `node1 < 50GB` or `slope("node2") < 0`.
    #[arg(long, value_name = "EXPR", action = ArgAction::Append, help_heading = "Analysis")]
    pub alert: Vec<String>,

    // ───────────── Output ─────────────
    /// Print the chart once and exit instead of opening the interactive viewer.
    #[arg(short = '1', long, help_heading = "Output")]
    pub once: bool,

    /// Don't draw a chart (useful with --stats / --alert / --export).
    #[arg(long, help_heading = "Output")]
    pub no_chart: bool,

    /// Export to a file; format from extension: .svg, .png, .csv, .json. Repeatable.
    #[arg(short = 'o', long, value_name = "FILE", action = ArgAction::Append, help_heading = "Output")]
    pub export: Vec<PathBuf>,

    /// Show how each input line was parsed (first N lines, default 50) and exit.
    #[arg(long, value_name = "N", num_args = 0..=1, require_equals = true, default_missing_value = "50", help_heading = "Output")]
    pub explain: Option<usize>,

    // ───────────── Interactive ─────────────
    /// Open the interactive builder (source, format suggestions, live preview) first.
    #[arg(short = 'b', long, help_heading = "Interactive")]
    pub build: bool,

    /// Don't capture the mouse (keeps the terminal's own text selection working).
    #[arg(long, help_heading = "Interactive")]
    pub no_mouse: bool,

    // ───────────── Scripting ─────────────
    /// Rhai script defining functions, key bindings (`bind("k", "code")`) or `parse(line)`.
    #[arg(long, value_name = "FILE", help_heading = "Scripting")]
    pub script: Option<PathBuf>,

    // ───────────── Profiles & misc ─────────────
    /// Load arguments from a saved profile (see --list-profiles). CLI flags override it.
    #[arg(short = 'p', long, value_name = "NAME", help_heading = "Profiles")]
    pub profile: Option<String>,

    /// Save the current arguments as a named profile.
    #[arg(long, value_name = "NAME", help_heading = "Profiles")]
    pub save_profile: Option<String>,

    /// List saved and built-in profiles.
    #[arg(long, help_heading = "Profiles")]
    pub list_profiles: bool,

    /// Print a shell completion script.
    #[arg(long, value_name = "SHELL", help_heading = "Misc")]
    pub completions: Option<Shell>,

    /// Print a man page (roff).
    #[arg(long, help_heading = "Misc")]
    pub man: bool,
}

impl Cli {
    /// A default configuration (as if run with no arguments).
    pub fn empty() -> Self {
        Self::parse_from(["graft"])
    }

    /// The arguments that reproduce this configuration (only non-default values),
    /// excluding one-shot actions like --save-profile or --completions.
    pub fn to_args(&self) -> Vec<String> {
        let d = Self::empty();
        let mut a: Vec<String> = Vec::new();
        let mut opt = |flag: &str, v: Option<String>| {
            if let Some(v) = v {
                a.push(flag.into());
                a.push(v);
            }
        };
        opt("--cmd", self.cmd.clone());
        opt(
            "--every",
            (self.every != d.every).then(|| fmt_secs(self.every.as_secs_f64())),
        );
        opt("--count", self.count.map(|c| c.to_string()));
        opt("--window", self.window.map(fmt_window));
        opt("--format", self.format.clone());
        opt("--regex", self.regex.clone());
        opt("--columns", self.columns.clone());
        opt(
            "--names",
            (!self.names.is_empty()).then(|| self.names.join(",")),
        );
        opt("--delimiter", self.delimiter.map(String::from));
        opt("--group-by", self.group_by.clone());
        opt("--time-field", self.time_field.clone());
        opt("--time-format", self.time_format.clone());
        opt("--x", (self.x != d.x).then(|| value_name(self.x)));
        opt("--unit", self.unit.clone());
        opt(
            "--select",
            (!self.select.is_empty()).then(|| self.select.join(",")),
        );
        opt(
            "--exclude",
            (!self.exclude.is_empty()).then(|| self.exclude.join(",")),
        );
        for s in &self.derived {
            opt("--series", Some(s.clone()));
        }
        opt(
            "--resample",
            self.resample
                .map(|r| format!("{}:{}", fmt_secs(r.width), value_name(r.agg))),
        );
        opt("--smooth", self.smooth.map(|n| n.to_string()));
        opt("--last", self.last.map(fmt_window));
        opt("--trend-window", self.trend_window.map(|n| n.to_string()));
        opt("--since", self.since.clone());
        opt("--until", self.until.clone());
        opt(
            "--kind",
            (self.kind != d.kind).then(|| value_name(self.kind)),
        );
        opt(
            "--marker",
            (self.marker != d.marker).then(|| value_name(self.marker)),
        );
        opt("--title", self.title.clone());
        opt(
            "--colors",
            (!self.colors.is_empty()).then(|| self.colors.join(",")),
        );
        opt(
            "--legend",
            (self.legend != d.legend).then(|| value_name(self.legend)),
        );
        opt("--ymin", self.ymin.clone());
        opt("--ymax", self.ymax.clone());
        for h in &self.hline {
            opt("--hline", Some(h.clone()));
        }
        opt(
            "--bins",
            (self.bins != d.bins).then(|| self.bins.to_string()),
        );
        opt("--x-format", self.x_format.clone());
        opt("--width", self.width.map(|w| w.to_string()));
        opt("--height", self.height.map(|h| h.to_string()));
        opt(
            "--stats-format",
            (self.stats_format != d.stats_format).then(|| value_name(self.stats_format)),
        );
        opt("--stats-template", self.stats_template.clone());
        opt("--per", self.per.clone());
        opt("--fit", (self.fit != d.fit).then(|| value_name(self.fit)));
        opt("--fit-window", self.fit_window.map(fmt_window));
        opt(
            "--target",
            (!self.target.is_empty()).then(|| self.target.join(",")),
        );
        for e in &self.alert {
            opt("--alert", Some(e.clone()));
        }
        for e in &self.export {
            opt("--export", Some(e.display().to_string()));
        }
        opt(
            "--script",
            self.script.as_ref().map(|p| p.display().to_string()),
        );
        let flags = [
            (self.follow, "--follow"),
            (self.kv, "--kv"),
            (self.auto, "--auto"),
            (self.csv, "--csv"),
            (self.json, "--json"),
            (self.parse_fn, "--parse-fn"),
            (self.binary_units, "--binary-units"),
            (self.rate, "--rate"),
            (self.delta, "--delta"),
            (self.split, "--split"),
            (self.log, "--log"),
            (self.no_color, "--no-color"),
            (self.stats, "--stats"),
            (self.once, "--once"),
            (self.no_chart, "--no-chart"),
            (self.no_mouse, "--no-mouse"),
            (self.trend_only, "--trend-only"),
            (self.side_by_side, "--side-by-side"),
            (self.no_targets, "--no-targets"),
        ];
        a.extend(flags.iter().filter(|f| f.0).map(|f| f.1.to_string()));
        // Optional-value flags need `=`.
        if let Some(f) = &self.forecast {
            a.push(if f == "auto" {
                "--forecast".into()
            } else {
                format!("--forecast={f}")
            });
        }
        if !self.trend.is_empty() {
            a.push(format!(
                "--trend={}",
                self.trend
                    .iter()
                    .map(|t| value_name(*t))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        if let Some(n) = self.explain {
            a.push(format!("--explain={n}"));
        }
        if let Some(input) = &self.input {
            let s = input.display().to_string();
            if s.starts_with('-') && s != "-" {
                a.push("--".into());
            }
            a.push(s);
        }
        a
    }

    /// The command line as a copy-pasteable shell string.
    pub fn command_line(&self) -> String {
        std::iter::once("graft".to_string())
            .chain(self.to_args().iter().map(|a| shell_quote(a)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Clear every parser choice (so exactly one can be set).
    pub fn clear_parser(&mut self) {
        self.format = None;
        self.regex = None;
        self.kv = false;
        self.csv = false;
        self.json = false;
        self.columns = None;
        self.parse_fn = false;
        self.auto = false;
    }

    /// Whether a way of parsing the input was chosen (one is always required).
    pub fn has_parser(&self) -> bool {
        self.format.is_some()
            || self.regex.is_some()
            || self.kv
            || self.auto
            || self.csv
            || self.json
            || self.columns.is_some()
            || self.parse_fn
    }
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./,:=@%+".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub fn fit_name(m: FitModel) -> String {
    value_name(m)
}

fn value_name<T: ValueEnum>(v: T) -> String {
    v.to_possible_value()
        .map(|p| p.get_name().to_string())
        .unwrap_or_default()
}

fn fmt_secs(s: f64) -> String {
    crate::time::fmt_duration_compact(s)
}

fn fmt_window(w: Window) -> String {
    match w {
        Window::Points(n) => n.to_string(),
        Window::Span(s) => fmt_secs(s),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum XMode {
    /// Parsed timestamp, else --cmd sample time, else line number.
    Auto,
    /// Require a timestamp on every line.
    Time,
    /// Line (or sample) number.
    Index,
    /// Time each line was read.
    Arrival,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ChartKind {
    Line,
    Scatter,
    /// Latest value of each series.
    Bar,
    /// Distribution of values.
    Histogram,
    /// One compact sparkline per series.
    Sparkline,
}

impl ChartKind {
    pub fn next(self) -> Self {
        match self {
            Self::Line => Self::Scatter,
            Self::Scatter => Self::Bar,
            Self::Bar => Self::Histogram,
            Self::Histogram => Self::Sparkline,
            Self::Sparkline => Self::Line,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Scatter => "scatter",
            Self::Bar => "bar",
            Self::Histogram => "histogram",
            Self::Sparkline => "sparkline",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MarkerKind {
    Braille,
    Dot,
    Block,
    HalfBlock,
    Bar,
}

impl MarkerKind {
    pub fn next(self) -> Self {
        match self {
            Self::Braille => Self::Dot,
            Self::Dot => Self::Block,
            Self::Block => Self::HalfBlock,
            Self::HalfBlock => Self::Bar,
            Self::Bar => Self::Braille,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LegendPos {
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
    Top,
    Bottom,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TrendStyle {
    /// Moving average.
    #[value(alias = "average", alias = "mean")]
    Avg,
    /// Moving median: like the average but ignores spikes.
    Median,
    /// Rolling 95th percentile: the slow end (good for latency).
    P95,
    /// The fitted trend line (linear or exponential, see --fit), as used for forecasts.
    #[value(alias = "line", alias = "linear")]
    Fit,
}

impl TrendStyle {
    pub const ALL: [TrendStyle; 4] = [
        TrendStyle::Avg,
        TrendStyle::Median,
        TrendStyle::P95,
        TrendStyle::Fit,
    ];

    /// Short name for legends and buttons.
    pub fn label(self) -> &'static str {
        match self {
            TrendStyle::Avg => "avg",
            TrendStyle::Median => "median",
            TrendStyle::P95 => "p95",
            TrendStyle::Fit => "trend",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StatsFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FitModel {
    /// Constant change per unit of time (y = a + b·x).
    Linear,
    /// Constant percentage change per unit of time (y = a·e^(b·x)); positive values only.
    Exp,
}

/// How much data to retain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Window {
    /// Seconds (or x units) back from the newest point.
    Span(f64),
    /// The newest N points of each series.
    Points(usize),
}

impl Window {
    /// The trailing part of `points` (sorted by x) inside this window.
    pub fn apply<'a>(&self, points: &'a [(f64, f64)]) -> &'a [(f64, f64)] {
        match *self {
            Window::Points(n) => &points[points.len().saturating_sub(n)..],
            Window::Span(span) => match points.last() {
                Some(&(last, _)) => &points[points.partition_point(|p| p.0 < last - span)..],
                None => points,
            },
        }
    }
}

pub fn parse_window(s: &str) -> Result<Window, String> {
    if let Ok(n) = s.parse::<usize>() {
        return Ok(Window::Points(n));
    }
    crate::time::parse_duration_secs(s)
        .map(Window::Span)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Agg {
    Mean,
    Min,
    Max,
    Sum,
    Last,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resample {
    pub width: f64,
    pub agg: Agg,
}

fn parse_resample(s: &str) -> Result<Resample, String> {
    let (w, agg) = s.split_once(':').unwrap_or((s, "mean"));
    let width = crate::time::parse_duration_secs(w).map_err(|e| e.to_string())?;
    if width <= 0.0 {
        return Err("resample width must be positive".into());
    }
    let agg = Agg::from_str(agg, true).map_err(|_| format!("unknown aggregation '{agg}'"))?;
    Ok(Resample { width, agg })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn later_flags_override_earlier_ones() {
        let cli = Cli::try_parse_from(["graft", "-k", "bar", "-k", "line", "x.log"]).unwrap();
        assert_eq!(cli.kind, ChartKind::Line);
    }

    #[test]
    fn parses_window_and_resample() {
        assert_eq!(parse_window("500").unwrap(), Window::Points(500));
        assert_eq!(parse_window("10m").unwrap(), Window::Span(600.0));
        let r = parse_resample("1h:max").unwrap();
        assert_eq!((r.width, r.agg), (3600.0, Agg::Max));
    }

    #[test]
    fn optional_values_do_not_swallow_the_input() {
        let cli = Cli::try_parse_from(["graft", "--forecast", "x.log", "--explain"]).unwrap();
        assert_eq!(cli.forecast.as_deref(), Some("auto"));
        assert_eq!(cli.explain, Some(50));
        assert_eq!(cli.input, Some("x.log".into()));
    }

    #[test]
    fn to_args_round_trips() {
        let cases: &[&[&str]] = &[
            &["graft"],
            &[
                "graft",
                "x.log",
                "-F",
                "{time} a: {a}",
                "--stats",
                "--split",
                "--forecast=7d",
                "--target",
                "a=5GB,none",
            ],
            &[
                "graft",
                "-c",
                "cat /proc/meminfo",
                "-n",
                "500ms",
                "-B",
                "-s",
                "Mem*,Cached",
                "--window",
                "10m",
                "--kind",
                "bar",
            ],
            &[
                "graft",
                "--resample",
                "1h:max",
                "--fit",
                "exp",
                "--fit-window",
                "300",
                "--x",
                "index",
                "--",
                "-odd-name.log",
            ],
            &[
                "graft",
                "d.csv",
                "--csv",
                "--series",
                "t = a + b",
                "--hline",
                "5:red:x",
                "--explain=3",
                "--forecast",
                "--no-mouse",
                "--trend=avg,p95",
                "--trend-only",
                "--side-by-side",
                "--trend-window",
                "9",
            ],
        ];
        for args in cases {
            let cli = Cli::try_parse_from(*args).unwrap();
            let again =
                Cli::try_parse_from(std::iter::once("graft".to_string()).chain(cli.to_args()))
                    .unwrap();
            assert_eq!(cli, again, "{args:?} -> {:?}", cli.to_args());
        }
    }

    #[test]
    fn command_line_quotes() {
        let cli = Cli::try_parse_from(["graft", "-c", "cat /proc/meminfo", "--stats"]).unwrap();
        assert_eq!(
            cli.command_line(),
            "graft --cmd 'cat /proc/meminfo' --stats"
        );
    }

    #[test]
    fn window_apply() {
        let pts = [(0.0, 1.0), (10.0, 2.0), (20.0, 3.0)];
        assert_eq!(Window::Span(10.0).apply(&pts), &pts[1..]);
        assert_eq!(Window::Points(1).apply(&pts), &pts[2..]);
    }
}
