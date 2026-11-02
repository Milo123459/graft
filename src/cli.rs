//! Command-line interface definition.

use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Parser, ValueEnum};
use clap_complete::Shell;

use crate::time::parse_duration;

const EXAMPLES: &str = "\
Examples:
  graft reclaimable.log                         auto-detect timestamps and `key: value` pairs
  graft reclaimable.log --stats --once          print the chart, stats and trend, then exit
  graft app.log -r '(?P<time>\\S+ \\S+) .* took (?P<latency>\\d+ms)'
  graft -c 'cat /proc/meminfo' -n 1s --binary-units -s MemFree,Cached
  graft -c 'cat /proc/net/dev' --kv --rate --unit B -s 'eth*'
  graft data.csv --csv --resample 1h:max --smooth 3
  graft --explain -r '(?P<node>node\\d): (?P<value>\\S+ GB)' -g node reclaimable.log
  graft reclaimable.log --alert 'node1 < 50GB' --once --no-chart && echo ok
  graft -p meminfo                              run a saved (or built-in) profile

Interactive keys: press ? inside the viewer.";

/// Graph anything: logs, command output, CSV or JSON — right in your terminal.
#[derive(Parser, Debug, Clone)]
#[command(name = "graft", version, about, after_help = EXAMPLES, args_override_self = true)]
pub struct Cli {
    /// Input file. Use "-" (or pipe data in) to read stdin.
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

    /// Parse `key: value` / `key=value` pairs (the default when nothing else is chosen).
    #[arg(long, group = "parser", help_heading = "Parsing")]
    pub kv: bool,

    /// Parse CSV (header row auto-detected).
    #[arg(long, group = "parser", help_heading = "Parsing")]
    pub csv: bool,

    /// Parse JSON objects (NDJSON or multi-line documents); nested keys become `a.b.c`.
    #[arg(long, group = "parser", help_heading = "Parsing")]
    pub json: bool,

    /// Whitespace-separated columns to use (1-based), e.g. `1,3`. Header rows are detected.
    #[arg(
        long,
        value_name = "N,..",
        value_delimiter = ',',
        group = "parser",
        help_heading = "Parsing"
    )]
    pub columns: Option<Vec<usize>>,

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

    /// Add a derived series using a Rhai expression, e.g. `used = MemTotal - MemAvailable`.
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

    // ───────────── Chart ─────────────
    /// Chart type.
    #[arg(short = 'k', long, value_enum, default_value_t = ChartKind::Line, help_heading = "Chart")]
    pub kind: ChartKind,

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

    /// Give each series its own panel and y axis (useful when scales differ a lot).
    #[arg(long, help_heading = "Chart")]
    pub split: bool,

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

    /// Express trends per this duration (default: chosen from the data span).
    #[arg(long, value_name = "DURATION", help_heading = "Analysis")]
    pub per: Option<String>,

    /// Forecast when each series reaches this value (default: 0 for falling series).
    #[arg(
        long,
        value_name = "VALUE",
        allow_hyphen_values = true,
        help_heading = "Analysis"
    )]
    pub target: Option<String>,

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
    #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "50", help_heading = "Output")]
    pub explain: Option<usize>,

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

/// How much data to retain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Window {
    /// Seconds (or x units) back from the newest point.
    Span(f64),
    /// The newest N points of each series.
    Points(usize),
}

fn parse_window(s: &str) -> Result<Window, String> {
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
}
