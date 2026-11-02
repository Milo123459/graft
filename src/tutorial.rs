//! `graft --tutorial`: a guided tour of the viewer on a live demo feed.
//!
//! The feed is a made-up service log, one line a second (with 15 minutes of history):
//! `2026-11-02 10:41:07 web=23.1ms api=48.2ms db=12.0ms`. The database spikes now and
//! then and the API slowly gets slower, so there's something to find, mark, alert on
//! and forecast. Each step says what a feature is for, asks you to try it, and ticks
//! itself off when you have; the last steps are recipes for real use.

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::cli::Cli;
use crate::data::LineMeta;
use crate::interactive::Live;
use crate::setup::Setup;
use crate::source::{Event, SourceSpec};

/// Seconds of history the feed starts with.
const HISTORY: i64 = 15 * 60;

/// What a step asks you to do: checked against the viewer after every event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// Nothing to do: read and go on.
    Read,
    Cursor,
    Zoomed,
    Selected,
    Hidden,
    Trends,
    Timeframe,
    Stats,
    Forecast,
    Kind,
    Panes,
    HLine,
    Marker,
    Alert,
    Line,
    Undo,
    Copied,
}

pub struct Step {
    pub title: &'static str,
    /// What it's for and how it works (`\n` separates paragraphs).
    pub text: &'static str,
    /// Its keys and buttons: (key, what it does).
    pub keys: &'static [(&'static str, &'static str)],
    /// Examples: (flag, console command or command line, what it does).
    pub examples: &'static [(&'static str, &'static str)],
    /// What to try now.
    pub task: Option<&'static str>,
    pub check: Check,
    /// A command for the copy button (recipes).
    pub command: Option<&'static str>,
}

const BASE: Step = Step {
    title: "",
    text: "",
    keys: &[],
    examples: &[],
    task: None,
    check: Check::Read,
    command: None,
};

pub const STEPS: &[Step] = &[
    Step {
        title: "welcome to graft",
        text: r#"graft turns text into live charts. This one is reading a made-up service log, a line a second, with 15 minutes of history:
  2026-11-02 10:41:07 web=23ms api=48ms db=12ms
It found three series (web, api, db), their unit (ms) and the time on each line. Logs, commands, CSV, JSON, a pipe: anything that prints numbers works.
Each step explains a feature, its keys and some examples, then asks you to try it, and ticks itself off when you have. Everything you do here is real: keep what you like."#,
        keys: &[
            (
                "Enter / next ▶",
                "the next step (whether or not you tried it)",
            ),
            ("Backspace / ◀ back", "the step before"),
            ("PgUp PgDn / scroll", "scroll this panel when there's more"),
            ("hide", "fold this panel to one line (tap it to unfold)"),
        ],
        examples: &[
            (
                "graft app.log",
                "a file: graft suggests formats that fit, you pick one",
            ),
            (
                "graft app.log --kv",
                "or say the format: key=value pairs, like this demo",
            ),
            (
                "tail -f app.log | graft --kv",
                "pipe anything in; it updates as lines arrive",
            ),
            (
                "graft -c 'uptime' -n 5s --kv",
                "run a command every 5 seconds and chart it",
            ),
        ],
        ..BASE
    },
    Step {
        title: "the screen",
        text: r#"From the top: the chart, with a legend of each series' latest value; the series chips (● web ● api ● db); the toolbar, where every button also has a key; and the status line: live or paused, the time of the newest data, and messages.
graft works the same with a mouse, a keyboard or a phone over ssh: tap where you'd click."#,
        keys: &[
            ("?", "every key on one screen"),
            (":", "the console: every button is a command too"),
            (
                "space / p",
                "pause the chart (data is still read; resume catches up)",
            ),
            (
                "m",
                "mouse on/off (off lets you select text in your terminal)",
            ),
            ("⋯", "more buttons: chart type, log scale, export…"),
            ("q", "quit (Esc never quits: it clears things)"),
        ],
        examples: &[
            ("graft app.log --kv --no-mouse", "start with the mouse off"),
            (
                "graft app.log --kv --once",
                "print the chart once and exit (scripts, CI)",
            ),
        ],
        ..BASE
    },
    Step {
        title: "read a value",
        text: r#"The cursor is a vertical line: the status line shows its time and every series' value there. It stays where you put it as new data arrives. Past the newest point it reads the forecast (a later step)."#,
        keys: &[
            (
                "tap / c / cursor",
                "put the cursor down (c again removes it)",
            ),
            ("drag", "scrub through the data"),
            (", .  Shift ← →", "step one point at a time"),
            ("Home End", "the first / newest point"),
            ("o", "the cursor follows the mouse, no clicking"),
            ("Esc", "let go of the cursor"),
        ],
        examples: &[
            (r#":last("api")"#, "the newest value, in the console"),
            (r#":max("db")"#, "the biggest value on screen"),
        ],
        task: Some("tap the chart (or press c), then try , and ."),
        check: Check::Cursor,
        ..BASE
    },
    Step {
        title: "zoom in",
        text: r#"Zoom into a stretch to see detail: the y axis rescales to what's on screen, so small changes become visible. New data keeps arriving; while you're zoomed in the view stays put."#,
        keys: &[
            ("scroll", "zoom in/out around the pointer (Shift: move)"),
            ("+ − / ↑ ↓", "zoom in / out"),
            ("◀ ▶ / ← → / h l", "move along (when zoomed in)"),
            ("H L", "move half a screen"),
            ("show all / 0 / r", "everything again"),
        ],
        examples: &[
            (r#":zoom("5m")"#, "just the last 5 minutes"),
            (":reset()", "the same as show all"),
        ],
        task: Some("scroll on the chart, or tap + (or press +). show all (or 0) zooms back out"),
        check: Check::Zoomed,
        ..BASE
    },
    Step {
        title: "stats",
        text: r#"A table, one row per series: last, min, max, average, and the trend: how fast it's changing (per second, minute, hour or day, whichever reads best), how well a straight line fits (R², 0 = noise, 1 = perfect), and when it'll reach a target.
It covers the timeframe, or a measured stretch: the next step."#,
        keys: &[("s / stats", "the table on/off")],
        examples: &[
            (
                "graft app.log --kv --stats --once",
                "the same table printed, for scripts",
            ),
            (
                "--stats-template '{name}: {last}, {trend}/{per}'",
                "your own one-line summary",
            ),
            ("--stats-format json", "machine-readable"),
            (
                r#":slope("api", "1h")"#,
                "the trend as a number: change per hour",
            ),
        ],
        task: Some("tap stats (or press s)"),
        check: Check::Stats,
        ..BASE
    },
    Step {
        title: "measure a stretch",
        text: r#"Pick a stretch of time and the stats cover just that: how bad was that spike, what was the average during the deploy? Stat tiles, bar charts and other panes follow it too, and so do console functions like mean()."#,
        keys: &[
            ("v / measure", "then tap the start and the end"),
            ("drag", "or drag across the chart"),
            ("Shift+click", "from the cursor to here"),
            ("Esc", "clear it"),
        ],
        examples: &[
            (r#":mean("db")"#, "the average over the measured stretch"),
            (
                r#":pct_change("api")"#,
                "how much it changed across it, in %",
            ),
        ],
        task: Some("tap measure (or press v), then tap a start and an end — or drag across"),
        check: Check::Selected,
        ..BASE
    },
    Step {
        title: "hide a series",
        text: r#"Each chip is a series: ● shown, ○ hidden. Hiding one rescales the y axis to what's left, handy when one series dwarfs the rest (db's spikes here). Hidden series are still read, so showing them again loses nothing."#,
        keys: &[
            ("tap a chip / 1–9", "show or hide that series"),
            ("a", "show them all"),
        ],
        examples: &[
            ("--hide db", "start with it hidden"),
            ("--select web,api", "only read these (globs work: 'node*')"),
            (
                r#":only("api")"#,
                "just one; show_all() brings the rest back",
            ),
            (r#"--bind 'x = toggle("db")'"#, "your own key for it"),
        ],
        task: Some("tap a chip like ● db (or press 1, 2, 3). a shows them all again"),
        check: Check::Hidden,
        ..BASE
    },
    Step {
        title: "trend lines",
        text: r#"Raw data is jumpy; a trend line shows where it's really going. average is a moving average (spikes pull it up), median the moving middle value (ignores spikes), p95 the rolling 95th percentile: the bad-but-normal case, how latency is usually judged.
Each uses the last N points, the window (10% of the data, at least 10). With any on, the raw data dims underneath; turn live off to see only the trends."#,
        keys: &[
            ("T / lines", "the lines menu"),
            ("l a m p", "in it: live, average, median, p95 on/off"),
            (
                "+ −",
                "in it: a wider / narrower window (smoother / quicker)",
            ),
        ],
        examples: &[
            ("--trend=avg,p95", "start with these lines"),
            (
                "--trend-only --trend-window 60",
                "only the trends, over 60 points",
            ),
            (r#":lines("live,median")"#, "in the console"),
        ],
        task: Some("tap lines (or press T) and turn on average or median (a / m)"),
        check: Check::Trends,
        ..BASE
    },
    Step {
        title: "timeframe",
        text: r#"The timeframe is the part of the data that counts: the chart, stats, trends and forecasts all use it. Unlike zooming it's a setting, kept when you save the chart. The rest of the data is still there: all time brings it back."#,
        keys: &[
            (
                "R / all time ▾",
                "the timeframe menu: last 5m, 1h, 1d… or custom",
            ),
            ("[ ]", "a narrower / wider timeframe"),
        ],
        examples: &[
            (
                "--last 1h",
                "start on the last hour (or --last 500: points)",
            ),
            (
                "--since 2026-11-01 --until '2026-11-02 12:00'",
                "between two times",
            ),
            (
                r#":range("5m")"#,
                "in the console; range(\"all\") for everything",
            ),
            (
                "--window 10m",
                "different: only keep 10 minutes in memory (long runs)",
            ),
        ],
        task: Some("tap all time ▾ (or press R) and pick last 5m"),
        check: Check::Timeframe,
        ..BASE
    },
    Step {
        title: "forecast",
        text: r#"A forecast continues each trend past the newest point, dashed. Move the cursor into it to read future values. The trend is a straight line (same change per hour); --fit exp makes it the same % per hour, for things that grow.
api is slowly getting slower here. Give it a target and the stats say when it gets there."#,
        keys: &[
            ("f / forecast", "forecast on/off"),
            ("G / targets", "target lines on/off"),
        ],
        examples: &[
            (
                "--forecast=1h",
                "an hour ahead (default: a sensible distance)",
            ),
            ("--target api=60ms", "when will api reach 60 ms?"),
            (r#":eta("api", 60ms)"#, "seconds until then, in the console"),
            ("--fit-window 10m", "fit only the last 10 minutes"),
        ],
        task: Some("tap forecast (or press f)"),
        check: Check::Forecast,
        ..BASE
    },
    Step {
        title: "chart types",
        text: r#"line and scatter show change over time. bar shows the latest value of each series, for comparing; histogram how the values are spread (a latency distribution); sparkline a small chart per series; stat big-number tiles with change and a sparkline, for dashboards."#,
        keys: &[
            ("t", "the next chart type (⋯ → chart type)"),
            ("b", "point style: braille, dots, blocks, bars"),
            ("y", "log scale, for values spanning 10× or more"),
            ("x / split", "a panel per series, each with its own y axis"),
        ],
        examples: &[
            ("--kind histogram", "start as a histogram"),
            ("--kind stat --select api", "one big number"),
            (r#":chart("bar")"#, "in the console"),
        ],
        task: Some("press t a few times; t again comes back to line"),
        check: Check::Kind,
        ..BASE
    },
    Step {
        title: "panes",
        text: r#"Panes put several charts in one view, like zellij. Each pane has its own series, chart type and lines, and can show another source (source=cpu). The outlined pane is the one the buttons change. Drag a border to resize."#,
        keys: &[
            ("w / panes", "the pane panel (w again closes it)"),
            ("| or v  _ or s", "in it: split side by side / stacked"),
            ("Tab  Alt+arrows", "pick a pane"),
            ("t  1–9", "in it: the pane's chart type / series"),
            ("x  =", "in it: close the pane / even sizes"),
        ],
        examples: &[
            ("--pane 'web,api' --pane 'db kind=histogram'", "two panes"),
            (
                "--layout '(1 | 2) / 3'",
                "how they're arranged ('1 |70% 2' for sizes)",
            ),
            ("graft -p dashboard", "four built-in presets in panes"),
        ],
        task: Some(
            "tap panes (or press w), then | to split side by side. Then w to close the panel",
        ),
        check: Check::Panes,
        ..BASE
    },
    Step {
        title: "the console",
        text: r#"The console runs Rhai, a small scripting language. Series names are variables holding their latest value, units work in numbers (50ms, 2GB, 1h, 5%), and every button has a command. Type help in it for the list."#,
        keys: &[
            (":", "open the console"),
            ("Enter  Esc", "run / close"),
            ("↑ ↓", "earlier commands"),
        ],
        examples: &[
            (":api - web", "the difference of the latest values"),
            (r#":mean("db")"#, "the average of what's shown"),
            (":stats(true)", "same as the button"),
            (
                "--script my.rhai",
                "your own functions and keys, loaded at start",
            ),
            (r#":load("my.rhai")"#, "…or while it's running"),
        ],
        task: Some(r#"press : and type  hline(50ms, "slow")  then Enter"#),
        check: Check::HLine,
        ..BASE
    },
    Step {
        title: "markers",
        text: r#"A marker rule is checked at every point of every series. It can use value, prev (the point before), delta (value − prev), pct (% change), rate (per second), name (the series) and time. Where it holds, a ▼ marks the spot with your label ({delta} and friends fill in).
The database spikes now and then: mark it."#,
        keys: &[
            (
                "K / markers",
                "the markers menu: your rules and how often each fired",
            ),
            (
                "e x",
                "in it: edit / delete a rule (symbol, colour, notify too)",
            ),
            ("a n", "in it: add a rule / mark now, by hand"),
        ],
        examples: &[
            (
                "--mark 'delta < -1GB => dropped {delta}'",
                "a big drop between two samples",
            ),
            (
                "--mark 'pct > 50 => jumped | ▲ red'",
                "with a symbol and colour",
            ),
            (
                r#"--mark 'name == "db" && value > 60ms => slow'"#,
                "one series only",
            ),
        ],
        task: Some(r#"press : and type  mark_when("value > 60ms", "spike")"#),
        check: Check::Marker,
        ..BASE
    },
    Step {
        title: "alerts",
        text: r#"An alert is one condition on the whole chart, checked as data arrives. It fires when it starts holding (once, not every second) and the alerts button lights up while it does. You can be told by the terminal bell, a desktop notification, your phone (the ntfy app), a webhook (Slack, Discord) or a command."#,
        keys: &[(
            "N / alerts",
            "add or edit alerts, choose where they go, send a test",
        )],
        examples: &[
            (
                "--alert 'api > 45ms' --notify ntfy:my-topic",
                "a push to your phone",
            ),
            (
                r#"--alert 'slope("db", "1h") > 0'"#,
                "rising over the last hour",
            ),
            (
                "graft … --alert 'disk > 90%' --once",
                "in cron: exit status 3 when it holds",
            ),
        ],
        task: Some(r#"press : and type  alert("api > 45ms")"#),
        check: Check::Alert,
        ..BASE
    },
    Step {
        title: "your own lines",
        text: r#"A custom line is worked out with Rhai over all the data and drawn like any series: hide it, mark it, alert on it. Return an array (one value per point), [x, y] pairs, or a number for a flat line. Built in: ema, sma, values, xs, values_at, mean_of. `backticks` are strings too, handy inside quotes."#,
        keys: &[(
            "T → + custom line",
            "add, edit or delete lines in the lines menu",
        )],
        examples: &[
            (
                r#"--line 'smooth = ema(values("api"), 30)'"#,
                "an exponential moving average",
            ),
            (
                r#"--line 'both = mean_of(["web", "api"])'"#,
                "two series averaged at each moment",
            ),
            (
                r#"--line 'avg = mean("api")'"#,
                "a flat line at the average",
            ),
        ],
        task: Some(r#"press : and type  add_line("smooth", "ema(values(`api`), 30)")"#),
        check: Check::Line,
        ..BASE
    },
    Step {
        title: "undo",
        text: r#"Every change (buttons, keys, console commands) is a step you can undo, up to 100 of them. Where you're looking (zoom, cursor) isn't a change, so undo never jumps the view around."#,
        keys: &[
            ("Ctrl+Z / u / ↶ undo", "a step back"),
            ("Ctrl+Y / U", "redo"),
        ],
        examples: &[(":undo()  :redo()", "in the console, or bound to a key")],
        task: Some("press Ctrl+Z (or u, or tap ↶ undo)"),
        check: Check::Undo,
        ..BASE
    },
    Step {
        title: "keep it",
        text: r#"graft remembers each chart you open: graft on its own lists them, tap one to pick up where you left off. Save a view under a name to open it any time, or copy the command that recreates it exactly, for a script or a friend."#,
        keys: &[
            ("C", "copy this view's command to your clipboard"),
            ("e", "export the chart to SVG"),
        ],
        examples: &[
            (
                r#":save("api")"#,
                "keep it as a profile, then: graft -p api",
            ),
            (
                "graft -p cpu --last 1h",
                "a profile (or preset) plus more flags",
            ),
            (
                "graft app.log --kv -o chart.svg",
                "export without the viewer (also .csv .json)",
            ),
        ],
        task: Some("press C"),
        check: Check::Copied,
        ..BASE
    },
    Step {
        title: "more to find",
        text: r#"F opens the format builder: change how lines are read, with live suggestions and a preview. Presets chart your machine in one word. --ssh runs anything on another machine, --merge combines sources into one chart.
The next pages are recipes for real use: copy puts one on your clipboard."#,
        keys: &[
            ("F", "the format builder"),
            ("?", "every key"),
            (":help", "every console command"),
        ],
        examples: &[
            (
                "graft -p cpu",
                "presets: cpu memory load network disk ping temperature",
            ),
            (r#"--bind 'r = only("api")'"#, "your own key"),
            (
                "graft -c 'df -k' --ssh pi@raspberrypi …",
                "a command on your Pi",
            ),
        ],
        ..BASE
    },
    Step {
        title: "recipe: your machine",
        text: "Built-in presets: cpu, memory, load, network, disk, ping, temperature, and a dashboard of four in panes.",
        command: Some("graft -p dashboard"),
        ..BASE
    },
    Step {
        title: "recipe: a Pi from your phone",
        text: "Run the command on another machine over ssh, and get a push on your phone (the ntfy app, topic my-pi) when it runs hot.",
        command: Some(
            "graft -p temperature --ssh pi@raspberrypi --alert 'temp > 70' --notify ntfy:my-pi",
        ),
        ..BASE
    },
    Step {
        title: "recipe: errors per minute",
        text: "Count matching lines in a log as it grows: one series per pattern, per minute (--grep-every for another bucket).",
        command: Some("graft -f app.log --grep 'errors=ERROR|FATAL' --grep 'warnings=WARN'"),
        ..BASE
    },
    Step {
        title: "recipe: anything a command prints",
        text: "Run a command every few seconds and pull numbers out with a regex: named groups become series.",
        command: Some(
            r#"graft -c 'curl -s https://api.coinbase.com/v2/prices/BTC-USD/spot' --regex 'amount\W+(?P<btc>[0-9.]+)' -n 5s -u USD"#,
        ),
        ..BASE
    },
    Step {
        title: "recipe: pipe anything",
        text: "Pipe output in and graft suggests formats that fit: pick one and it remembers. Or say it yourself with --format, where {name} captures a number.",
        command: Some("ping 1.1.1.1 | graft --format 'time={latency}'"),
        ..BASE
    },
    Step {
        title: "recipe: combine feeds",
        text: "Merge sources into one chart and build on them: two exchanges' prices, their mean, an EMA, and a marker when it crosses.",
        command: Some(
            r#"graft --merge 'coinbase = -c "curl -s https://api.coinbase.com/v2/prices/ETH-USD/spot" --regex "amount\W+(?P<p>[0-9.]+)" -n 2s -u USD' --merge 'binance = -c "curl -s https://api.binance.com/api/v3/ticker/price?symbol=ETHUSDT" --regex "price\W+(?P<p>[0-9.]+)" -n 2s -u USD' --line 'eth = mean_of(["coinbase", "binance"])' --line 'ema30 = ema(values("eth"), 30)' --mark 'name == "eth" && prev < ema30_prev && value >= ema30 => above EMA30 | ▲ green' --hide coinbase,binance --title ETH"#,
        ),
        ..BASE
    },
    Step {
        title: "that's the tour",
        text: "q quits. graft --tutorial runs this again, ? lists every key and :help every console command. Have fun graphing!",
        ..BASE
    },
];

/// Where you are in the tour.
#[derive(Debug, Clone, Default)]
pub struct Tutorial {
    pub step: usize,
    /// Steps done (their task tried), kept when you go back.
    pub done: Vec<bool>,
    /// Folded down to one line, to see more of the chart.
    pub folded: bool,
    /// Rows the panel is scrolled down by (long steps).
    pub scroll: u16,
    /// Things noticed as they happen (not visible in the state afterwards).
    pub undone: bool,
    pub copied: bool,
}

impl Tutorial {
    pub fn new() -> Self {
        Tutorial {
            done: vec![false; STEPS.len()],
            ..Self::default()
        }
    }

    pub fn current(&self) -> &'static Step {
        &STEPS[self.step]
    }

    pub fn is_done(&self) -> bool {
        self.current().check == Check::Read || self.done[self.step]
    }

    pub fn next(&mut self) -> bool {
        if self.step + 1 < STEPS.len() {
            self.step += 1;
            self.scroll = 0;
            self.undone = false;
            self.copied = false;
            true
        } else {
            false
        }
    }

    pub fn back(&mut self) {
        self.step = self.step.saturating_sub(1);
        self.scroll = 0;
    }
}

/// The arguments the tutorial chart runs with.
pub fn cli() -> Cli {
    <Cli as clap::Parser>::parse_from(["graft", "--kv", "--title", "graft tutorial", "--tutorial"])
}

/// The viewer's setup and its (live, made-up) source.
pub fn start() -> Result<(Setup, Live)> {
    let spec = SourceSpec::Stdin;
    let setup = crate::setup::build(cli(), spec.clone(), false)?;
    Ok((setup, Live::from_rx(spec, feed())))
}

/// Small, fast, good-enough randomness (no dependency needed for a demo).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Roughly normal, mean 0, sd 1.
    fn noise(&mut self) -> f64 {
        (0..4).map(|_| self.next()).sum::<f64>() - 2.0
    }
}

/// The demo source's state: a spike in the database lasts a few seconds.
struct Feed {
    rng: Rng,
    spike: u32,
}

impl Feed {
    /// The line for second `t` (seconds since the start of the history).
    fn line(&mut self, t: i64, when: chrono::DateTime<chrono::Local>) -> String {
        let r = &mut self.rng;
        let web = 22.0 + 4.0 * (t as f64 / 70.0).sin() + 1.5 * r.noise();
        // Slowly slower: +1 ms every 2 minutes.
        let api = 36.0 + t as f64 / 120.0 + 3.0 * r.noise();
        if self.spike == 0 && r.next() < 1.0 / 100.0 {
            self.spike = 2 + (r.next() * 4.0) as u32;
        }
        let db = if self.spike > 0 {
            self.spike -= 1;
            70.0 + 35.0 * r.next()
        } else {
            12.0 + 1.5 * r.noise()
        };
        format!(
            "{} web={:.1}ms api={:.1}ms db={:.1}ms",
            when.format("%Y-%m-%d %H:%M:%S"),
            web.max(1.0),
            api.max(1.0),
            db.max(1.0)
        )
    }
}

fn feed() -> Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let start = chrono::Local::now() - chrono::Duration::seconds(HISTORY);
        let mut f = Feed {
            rng: Rng(0x9e37_79b9_7f4a_7c15),
            spike: 0,
        };
        let send = |line: String| {
            tx.send(Event::Line(
                line,
                LineMeta {
                    arrival: crate::time::now_secs(),
                    sample: None,
                },
            ))
            .is_ok()
        };
        for t in 0..HISTORY {
            if !send(f.line(t, start + chrono::Duration::seconds(t))) {
                return;
            }
        }
        let mut t = HISTORY;
        loop {
            thread::sleep(Duration::from_secs(1));
            if !send(f.line(t, chrono::Local::now())) {
                return;
            }
            t += 1;
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_feed_parses_and_has_spikes() {
        let mut f = Feed {
            rng: Rng(0x9e37_79b9_7f4a_7c15),
            spike: 0,
        };
        let start = chrono::Local::now();
        let lines: Vec<String> = (0..HISTORY)
            .map(|t| f.line(t, start + chrono::Duration::seconds(t)))
            .collect();
        assert!(lines[0].contains(" web=") && lines[0].ends_with("ms"));
        let spikes = lines
            .iter()
            .filter(|l| {
                let db: f64 = l
                    .split("db=")
                    .nth(1)
                    .unwrap()
                    .trim_end_matches("ms")
                    .parse()
                    .unwrap();
                db > 60.0
            })
            .count();
        assert!(spikes > 5, "{spikes} spiky seconds");
        // Every step's text fits the panel's limits.
        assert!(
            STEPS
                .iter()
                .all(|s| !s.title.is_empty() && s.title.len() < 40)
        );
    }
}
