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
    /// What it's for (blank lines separate paragraphs).
    pub text: &'static str,
    /// What to do, with keys and buttons.
    pub task: Option<&'static str>,
    pub check: Check,
    /// A command for the copy button (recipes).
    pub command: Option<&'static str>,
}

const fn step(title: &'static str, text: &'static str, task: &'static str, check: Check) -> Step {
    Step {
        title,
        text,
        task: Some(task),
        check,
        command: None,
    }
}

const fn read(title: &'static str, text: &'static str) -> Step {
    Step {
        title,
        text,
        task: None,
        check: Check::Read,
        command: None,
    }
}

const fn recipe(title: &'static str, text: &'static str, command: &'static str) -> Step {
    Step {
        title,
        text,
        task: None,
        check: Check::Read,
        command: Some(command),
    }
}

pub const STEPS: &[Step] = &[
    read(
        "welcome to graft",
        "graft turns text into live charts. This one is reading a made-up service log, a line a second: \
         `10:41:07 web=23ms api=48ms db=12ms`. Logs, commands, CSV, JSON, a pipe — anything that prints numbers works.\n\
         Enter or next ▶ moves on, ◀ back goes back. Everything you try here is real: keep what you like.",
    ),
    step(
        "read a value",
        "The cursor shows every series' value at one moment, with the time.",
        "tap the chart (or press c). Drag it, or step with , and .",
        Check::Cursor,
    ),
    step(
        "zoom in",
        "Zoom into a stretch to see detail; the y axis fits what's shown. ◀ ▶ (or ← →) move along.",
        "scroll on the chart, or tap + (or press +). show all (or 0) zooms back out",
        Check::Zoomed,
    ),
    step(
        "stats",
        "A table per series: last, min, max, average, the trend per minute/hour/day and a forecast. Next, you'll narrow it down to a stretch.",
        "tap stats (or press s)",
        Check::Stats,
    ),
    step(
        "measure a stretch",
        "Pick a stretch to get stats for just that part: min, max, average, change. Other panes follow it too.",
        "tap measure (or press v), then tap a start and an end — or drag across. Esc clears it",
        Check::Selected,
    ),
    step(
        "hide a series",
        "The chips under the chart are the series. Hide the noisy ones to look at the rest.",
        "tap a chip like ● db (or press 1, 2, 3). a shows them all again",
        Check::Hidden,
    ),
    step(
        "trend lines",
        "Raw data is jumpy. An average, median or p95 line shows where it's really going; the raw data dims underneath.",
        "tap lines (or press T) and turn on average or median",
        Check::Trends,
    ),
    step(
        "timeframe",
        "Look at just the last few minutes (or days). Stats, trends and forecasts use the same range.",
        "tap all time ▾ (or press R) and pick last 5m. [ and ] step through",
        Check::Timeframe,
    ),
    step(
        "forecast",
        "api is slowly getting slower. A forecast continues each trend forward; a target (--target 60ms) says when it gets there.",
        "tap forecast (or press f)",
        Check::Forecast,
    ),
    step(
        "chart types",
        "Line, scatter, bar (latest values), histogram (the spread), sparklines and big-number stat tiles.",
        "press t a few times (or ⋯ → chart type). t again comes back to line",
        Check::Kind,
    ),
    step(
        "panes",
        "Several charts in one view, zellij-style: each pane has its own series, chart type and lines — even its own source.",
        "tap panes (or press w), then | to split side by side. Pick a pane and set its chart type; done (or w) closes the panel",
        Check::Panes,
    ),
    step(
        "the console",
        "Every button is a command too, so you can type it, bind it to a key (--bind) or put it in a script.",
        "press : and type  hline(50ms, \"slow\")  then Enter",
        Check::HLine,
    ),
    step(
        "markers",
        "Mark the moments that matter with a rule, checked on every point: value, prev, delta, pct, name… \
         The database spikes now and then — mark it. K lists your markers, with symbol, colour and notify.",
        "press : and type  mark_when(\"value > 60ms\", \"spike\")",
        Check::Marker,
    ),
    step(
        "alerts",
        "An alert is a condition on the whole chart. When it starts holding you're told: bell, desktop, or your phone (ntfy). N opens the alerts menu.",
        "press : and type  alert(\"api > 45ms\")",
        Check::Alert,
    ),
    step(
        "your own lines",
        "Write a line in Rhai over the whole data: moving averages, sums, ratios. ema, sma, values, mean_of are built in \
         (`backticks` are strings too, handy inside quotes).",
        "press : and type  add_line(\"smooth\", \"ema(values(`api`), 30)\")",
        Check::Line,
    ),
    step(
        "undo",
        "Tried something you don't like? Undo goes back a step, all the way to how it started.",
        "press Ctrl+Z (or u, or tap ↶ undo). Ctrl+Y or U redoes",
        Check::Undo,
    ),
    step(
        "keep it",
        "C copies the command that opens exactly this view. save(\"name\") in the console keeps it as a profile: graft -p name. \
         graft on its own lists your recent charts.",
        "press C",
        Check::Copied,
    ),
    read(
        "more to find",
        "? lists every key. ⋯ has the rest: log scale, scatter, export to SVG/PNG, pause, the format editor (F) to change how lines are read.\n\
         The next pages are recipes for real use. copy puts a command on your clipboard.",
    ),
    recipe(
        "recipe: your machine",
        "Built-in presets: cpu, memory, load, network, disk, ping, temperature — and a dashboard of four.",
        "graft -p dashboard",
    ),
    recipe(
        "recipe: a Pi from your phone",
        "Run the command on another machine over ssh, and get a push on your phone (the ntfy app, topic my-pi) when it runs hot.",
        "graft -p temperature --ssh pi@raspberrypi --alert 'temp > 70' --notify ntfy:my-pi",
    ),
    recipe(
        "recipe: errors per minute",
        "Count matching lines in a log as it grows — one series per pattern.",
        "graft -f app.log --grep 'errors=ERROR|FATAL' --grep 'warnings=WARN'",
    ),
    recipe(
        "recipe: anything a command prints",
        "Run a command every few seconds and pull numbers out with a regex (named groups become series).",
        "graft -c 'curl -s https://api.coinbase.com/v2/prices/BTC-USD/spot' --regex 'amount\\W+(?P<btc>[0-9.]+)' -n 5s -u USD",
    ),
    recipe(
        "recipe: pipe anything",
        "Pipe output in and graft suggests formats that fit — pick one and it remembers. Or say it yourself with --format.",
        "ping 1.1.1.1 | graft --format 'time={latency}'",
    ),
    recipe(
        "recipe: combine feeds",
        "Merge sources into one chart and build on them: two exchanges' prices, their mean, an EMA, and a marker when it crosses.",
        "graft --merge 'coinbase = -c \"curl -s https://api.coinbase.com/v2/prices/ETH-USD/spot\" --regex \"amount\\W+(?P<p>[0-9.]+)\" -n 2s -u USD' \
         --merge 'binance = -c \"curl -s https://api.binance.com/api/v3/ticker/price?symbol=ETHUSDT\" --regex \"price\\W+(?P<p>[0-9.]+)\" -n 2s -u USD' \
         --line 'eth = mean_of([\"coinbase\", \"binance\"])' --line 'ema30 = ema(values(\"eth\"), 30)' \
         --mark 'name == \"eth\" && prev < ema30_prev && value >= ema30 => above EMA30 | ▲ green' --hide coinbase,binance --title ETH",
    ),
    read(
        "that's the tour",
        "q quits. graft --tutorial runs this again, and ? lists every key. Have fun graphing!",
    ),
];

/// Where you are in the tour.
#[derive(Debug, Clone, Default)]
pub struct Tutorial {
    pub step: usize,
    /// Steps done (their task tried), kept when you go back.
    pub done: Vec<bool>,
    /// Folded down to one line, to see more of the chart.
    pub folded: bool,
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
            self.undone = false;
            self.copied = false;
            true
        } else {
            false
        }
    }

    pub fn back(&mut self) {
        self.step = self.step.saturating_sub(1);
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
