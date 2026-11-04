//! The viewer's discovery lab: reruns the search (`discover.rs`) in the background as
//! data streams in, and paper-trades what it found from the moment it found it.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::data::Dataset;
use crate::discover::{self, Budget, Found, Input, Perf, Report, Settings, Side};
use crate::formula::Expr;

/// How long each search may evolve formulas.
const SEARCH_TIME: Duration = Duration::from_secs(3);
/// Strategies paper-traded at once.
const TRACKED: usize = 8;

/// A strategy being paper-traded since it was found.
#[derive(Debug, Clone)]
pub struct Tracked {
    pub found: Found,
    /// The newest x when it was found: live trades are the ones entered after it.
    pub since: f64,
    /// Which search found it (from 1).
    pub run: usize,
    pub live: Perf,
    /// The trade it's in now (its return so far, bp).
    pub open: Option<f64>,
    /// Every trade over the data: (entry x, exit x, return bp, live).
    pub trades: Vec<(f64, Option<f64>, f64, bool)>,
    /// The price at each trade's entry and exit (for drawing them).
    pub prices: Vec<(f64, Option<f64>)>,
}

impl Tracked {
    /// Its record where it wasn't picked (the last 30%, then live), as one t-statistic.
    pub fn score(&self) -> f64 {
        let (c, l) = (&self.found.checked, &self.live);
        let n = (c.n + l.n) as f64;
        if n < 2.0 {
            return f64::MIN;
        }
        let mean = (c.total + l.total) / n;
        let var = (c.sd.powi(2) * c.n as f64 + l.sd.powi(2) * l.n as f64) / n;
        if var <= 0.0 {
            return mean.signum() * 10.0;
        }
        mean / (var.sqrt() / n.sqrt())
    }
}

pub struct Lab {
    pub settings: Settings,
    worker: Option<Receiver<Result<Report, String>>>,
    last_run: Option<Instant>,
    /// Seconds between searches.
    pub every: Duration,
    pub runs: usize,
    pub report: Option<Report>,
    /// Why there's no report yet (too few points…).
    pub waiting: Option<String>,
    pub tracked: Vec<Tracked>,
    /// The strategy whose trades are on the chart (index into `tracked`).
    pub selected: usize,
    /// The formulas to start the next search from.
    pool: Vec<Expr>,
    /// When the live results were last brought up to date, and for which data.
    refreshed: Option<(Instant, f64, usize)>,
}

impl Lab {
    pub fn new(settings: Settings) -> Lab {
        Lab {
            settings,
            worker: None,
            last_run: None,
            every: Duration::from_secs(20),
            runs: 0,
            report: None,
            waiting: None,
            tracked: Vec::new(),
            selected: 0,
            pool: Vec::new(),
            refreshed: None,
        }
    }

    pub fn searching(&self) -> bool {
        self.worker.is_some()
    }

    /// Seconds until the next search.
    pub fn next_in(&self) -> Option<u64> {
        let last = self.last_run?;
        Some(self.every.saturating_sub(last.elapsed()).as_secs())
    }

    /// Search again now (on the next tick).
    pub fn rerun(&mut self) {
        self.last_run = None;
    }

    /// Start a search when one is due, take in a finished one, and bring the paper
    /// trades up to date. Returns news to tell (a new live trade of the best strategy).
    pub fn tick(&mut self, ds: &Dataset) -> Option<String> {
        if let Some(rx) = &self.worker {
            match rx.try_recv() {
                Ok(result) => {
                    self.worker = None;
                    self.take(result, ds);
                }
                Err(TryRecvError::Disconnected) => self.worker = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let due = self.last_run.is_none_or(|t| t.elapsed() >= self.every);
        if self.worker.is_none() && due {
            self.last_run = Some(Instant::now());
            match Input::from_dataset(ds, &self.settings) {
                Ok(input) if input.len() >= discover::MIN_POINTS => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let fee = self.settings.fee;
                    let pool = self.pool.clone();
                    self.runs += 1;
                    let budget = Budget {
                        time: SEARCH_TIME,
                        formulas: 20_000,
                        seed: 0x9e37_79b9_7f4a_7c15 ^ (self.runs as u64) ^ input.len() as u64,
                    };
                    std::thread::spawn(move || {
                        let _ = tx.send(discover::search(input, fee, &pool, budget));
                    });
                    self.worker = Some(rx);
                }
                Ok(input) => {
                    self.waiting = Some(format!(
                        "collecting data: {} of the {} points it needs",
                        input.len(),
                        discover::MIN_POINTS
                    ));
                    // Check again soon.
                    self.last_run = Some(Instant::now() - self.every + Duration::from_secs(2));
                }
                Err(e) => self.waiting = Some(e),
            }
        }
        self.refresh(ds)
    }

    fn take(&mut self, result: Result<Report, String>, ds: &Dataset) {
        let report = match result {
            Ok(r) => r,
            Err(e) => {
                self.waiting = Some(e);
                return;
            }
        };
        self.waiting = None;
        self.pool = report.pool.iter().map(|s| s.expr.clone()).collect();
        let since = report.input.x.last().copied().unwrap_or(f64::NEG_INFINITY);
        let selected_key = self
            .tracked
            .get(self.selected)
            .map(|t| t.found.strategy.key());
        for f in report.strategies.iter().take(4) {
            // Found again: its record on the newest data, but it keeps its live record.
            if let Some(t) = self
                .tracked
                .iter_mut()
                .find(|t| t.found.strategy == f.strategy)
            {
                t.found = f.clone();
                continue;
            }
            // Only strategies that held up where they weren't picked.
            if f.checked.n < 3 || f.checked.mean <= 0.0 {
                continue;
            }
            self.tracked.push(Tracked {
                found: f.clone(),
                since,
                run: self.runs,
                live: Perf::default(),
                open: None,
                trades: Vec::new(),
                prices: Vec::new(),
            });
        }
        self.report = Some(report);
        self.refreshed = None;
        self.refresh(ds);
        self.rank(selected_key);
        // Too many: the worst go (never the one on the chart).
        while self.tracked.len() > TRACKED {
            let last = self.tracked.len() - 1;
            let drop = if self.selected == last {
                last - 1
            } else {
                last
            };
            self.tracked.remove(drop);
            if self.selected > drop {
                self.selected -= 1;
            }
        }
    }

    /// Best first; the selection follows its strategy.
    fn rank(&mut self, keep: Option<String>) {
        self.tracked.sort_by(|a, b| b.score().total_cmp(&a.score()));
        self.selected = keep
            .and_then(|k| {
                self.tracked
                    .iter()
                    .position(|t| t.found.strategy.key() == k)
            })
            .unwrap_or(0);
    }

    /// Paper trades on the newest data (at most once a second).
    fn refresh(&mut self, ds: &Dataset) -> Option<String> {
        if self.tracked.is_empty() {
            return None;
        }
        let input = Input::from_dataset(ds, &self.settings).ok()?;
        let newest = (input.x.last().copied().unwrap_or(0.0), input.len());
        if let Some((at, x, len)) = self.refreshed
            && ((x, len) == newest || at.elapsed() < Duration::from_secs(1))
        {
            return None;
        }
        self.refreshed = Some((Instant::now(), newest.0, newest.1));
        let mut news = None;
        for (k, t) in self.tracked.iter_mut().enumerate() {
            let trades = discover::trade(&t.found.strategy, &input, 0, self.settings.fee);
            let before = t.trades.iter().filter(|r| r.3).count();
            t.prices = trades
                .iter()
                .map(|r| (input.price[r.entry], r.exit.map(|e| input.price[e])))
                .collect();
            t.trades = trades
                .iter()
                .map(|r| {
                    (
                        input.x[r.entry],
                        r.exit.map(|e| input.x[e]),
                        r.net,
                        input.x[r.entry] > t.since,
                    )
                })
                .collect();
            let live: Vec<f64> = t
                .trades
                .iter()
                .filter(|r| r.3 && r.1.is_some())
                .map(|r| r.2)
                .collect();
            t.live = Perf::of(&live);
            t.open = t
                .trades
                .last()
                .filter(|r| r.1.is_none() && r.3)
                .map(|r| r.2);
            let now = t.trades.iter().filter(|r| r.3).count();
            if k == 0 && now > before && t.trades.last().is_some_and(|r| r.1.is_none()) {
                let side = match t.found.strategy.side {
                    Side::Long => "▲ long",
                    Side::Short => "▼ short",
                };
                news = Some(format!(
                    "discover: the best strategy went {side} at {}",
                    crate::discover::fmt_num(*input.price.last().unwrap_or(&0.0))
                ));
            }
        }
        news
    }

    /// The selected strategy's trades, as markers: ▲/▼ in, • out with its result.
    pub fn marks(&self) -> Vec<crate::render::Mark> {
        use ratatui::style::{Color, Modifier};
        let Some(t) = self.tracked.get(self.selected) else {
            return Vec::new();
        };
        let Some(report) = &self.report else {
            return Vec::new();
        };
        let series = Some(report.input.name.clone());
        let long = t.found.strategy.side == Side::Long;
        let mark = |x: f64, y: Option<f64>, label: String, symbol: &str, color: Color| {
            crate::render::Mark {
                series: series.clone(),
                x,
                y,
                label,
                rule: None,
                symbol: symbol.into(),
                color: Some(color),
                effects: Modifier::empty(),
                badge: false,
                bg: None,
            }
        };
        let mut out = Vec::new();
        for (k, &(entry, exit, net, live)) in t.trades.iter().enumerate() {
            let (y_in, y_out) = t.prices.get(k).copied().unwrap_or((f64::NAN, None));
            let color = if long {
                Color::LightGreen
            } else {
                Color::LightRed
            };
            out.push(mark(
                entry,
                Some(y_in).filter(|v| v.is_finite()),
                if live { "live".into() } else { String::new() },
                if long { "▲" } else { "▼" },
                color,
            ));
            if let Some(x) = exit {
                out.push(mark(
                    x,
                    y_out,
                    format!("{net:+.1}bp"),
                    "•",
                    if net > 0.0 { Color::Green } else { Color::Red },
                ));
            }
        }
        out
    }
}
