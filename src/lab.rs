//! The viewer's discovery lab: reruns the search (`discover.rs`) in the background as
//! data streams in, and paper-trades what it found from the moment it found it.
//!
//! It learns from what it trades: each search reviews the strategies being
//! paper-traded (why one isn't paying: costs, an edge that faded, a condition it loses
//! in) and tries a fixed version where it found one. A strategy that keeps losing live
//! is retired with its lesson, and isn't picked again.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::data::Dataset;
use crate::discover::{self, Budget, Feedback, Found, Input, Perf, Report, Settings, Side};
use crate::formula::Expr;

/// How long each search may evolve formulas.
const SEARCH_TIME: Duration = Duration::from_secs(3);
/// Formulas a search should get through in its time: it searches as many of the
/// newest points as leave time for that (measured on the last search), so a fast
/// machine searches all the data and a slow one the newest part.
const ENOUGH_FORMULAS: f64 = 2000.0;
/// What the first search tries, before it knows how fast searches go.
const FIRST_POINTS: usize = 50_000;
const FIRST_POOL: usize = 1000;
/// Retired strategies still watched (the newest).
const BENCH: usize = 24;
/// Strategies paper-traded at once.
const TRACKED: usize = 8;
/// Live trades before a losing strategy is retired.
const RETIRE_AFTER: usize = 8;
/// How many trades' worth the usual spread counts for in a strategy's score
/// (`Tracked::score`).
const PRIOR_TRADES: f64 = 30.0;
/// Repairs of repairs at most: each one fits the same data a bit closer.
const MAX_REPAIRS: usize = 3;

/// One of a tracked strategy's trades, for the record and the chart.
#[derive(Debug, Clone, Copy)]
pub struct PaperTrade {
    pub entry: f64,
    pub exit: Option<f64>,
    pub side: Side,
    pub net: f64,
    /// Entered after the strategy was found.
    pub live: bool,
    /// The price at the entry and the exit.
    pub y_in: f64,
    pub y_out: Option<f64>,
}

impl PaperTrade {
    /// What its entry says on the chart: "open" while it's still in it, else nothing
    /// (one strategy is only ever in one trade, so only one entry can say it).
    pub fn label(&self) -> &'static str {
        if self.exit.is_none() { "open" } else { "" }
    }

    /// What its exit says: the result, and "paper" when it was entered after the
    /// strategy was found (paper-traded, not replayed from the data it was found on).
    pub fn exit_label(&self) -> String {
        let paper = if self.live { " paper" } else { "" };
        format!("{:+.1}bp{paper}", self.net)
    }
}

/// One strategy's numbers, to set beside the others': where it wasn't picked
/// (walking forward, then live), all together.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Compared {
    /// Its place in `tracked` (the panel's ranking).
    pub index: usize,
    /// Walking forward plus live, added up (bp).
    pub profit: f64,
    pub live: Perf,
    /// The larger of its max drawdowns walking forward and live (bp).
    pub drawdown: f64,
    /// An average trade (bp), how sure that is (t), and the share that won.
    pub mean: f64,
    pub t: f64,
    pub won: f64,
    pub trades: usize,
    pub worst: f64,
    pub best: f64,
    /// Beats holding by, a trade walking forward (bp before costs).
    pub edge: f64,
    /// The trade it's in now (bp so far).
    pub open: Option<f64>,
}

impl Tracked {
    pub fn compared(&self, index: usize) -> Compared {
        let (c, l) = (&self.found.checked, &self.live);
        let n = c.n + l.n;
        let both = [c, l];
        let some = || both.iter().filter(|p| p.n > 0);
        let profit = c.total + l.total;
        let mean = if n > 0 { profit / n as f64 } else { 0.0 };
        // Pooled spread: each part's own, and how far its average is from the whole's.
        let var = if n > 1 {
            some()
                .map(|p| p.n as f64 * (p.sd.powi(2) + (p.mean - mean).powi(2)))
                .sum::<f64>()
                / n as f64
        } else {
            0.0
        };
        let t = if var > 0.0 {
            mean / (var.sqrt() / (n as f64).sqrt())
        } else {
            0.0
        };
        Compared {
            index,
            profit,
            live: *l,
            drawdown: c.drawdown.max(l.drawdown),
            mean,
            t,
            won: if n > 0 {
                (c.win * c.n as f64 + l.win * l.n as f64) / n as f64
            } else {
                0.0
            },
            trades: n,
            worst: some().map(|p| p.worst).fold(0.0, f64::min),
            best: some().map(|p| p.best).fold(0.0, f64::max),
            edge: self.found.edge.mean,
            open: self.open,
        }
    }

    /// Its running total after each trade it got out of (exit x, bp), over all the
    /// data loaded.
    pub fn equity(&self) -> Vec<(f64, f64)> {
        let mut total = 0.0;
        self.trades
            .iter()
            .filter_map(|p| {
                let x = p.exit?;
                total += p.net;
                Some((x, total))
            })
            .collect()
    }
}

/// The strategies side by side, sorted by `by` (the best first: the most made, the
/// smallest drawdown, the mildest worst trade…; ties in the panel's order).
pub fn compare(tracked: &[Tracked], by: crate::cli::LabSort) -> Vec<Compared> {
    use crate::cli::LabSort as S;
    let mut v: Vec<Compared> = tracked
        .iter()
        .enumerate()
        .map(|(i, t)| t.compared(i))
        .collect();
    let key = |c: &Compared| -> f64 {
        // No trades yet: nothing to compare, so last.
        if c.trades == 0 && by != S::Rank {
            return f64::NEG_INFINITY;
        }
        match by {
            S::Rank => -(c.index as f64),
            S::Profit => c.profit,
            S::Drawdown => -c.drawdown,
            S::Live => c.live.total,
            S::Mean => c.mean,
            S::T => c.t,
            S::Won => c.won,
            S::Trades => c.trades as f64,
            S::Edge => c.edge,
            S::Worst => c.worst,
        }
    };
    v.sort_by(|a, b| key(b).total_cmp(&key(a)).then(a.index.cmp(&b.index)));
    v
}

/// How many of the price's newest points have every series that starts later than it
/// (a live order book, say, with no history behind it), when that's a stretch worth
/// searching on its own: at least 600 points, and not nearly all of them anyway.
pub fn recent_window(ds: &Dataset, s: &Settings) -> Option<usize> {
    let price = discover::price_series(ds, s.series.as_deref())?;
    let first = price.points.first()?.0;
    let total = price.points.len();
    let span = price.points.last()?.0 - first;
    // Series that start well after the price (a tenth of its span in).
    let late = ds
        .series
        .iter()
        .filter(|o| o.name != price.name && o.points.len() >= 2)
        .filter_map(|o| o.points.first().map(|p| p.0))
        .filter(|x| *x > first + span * 0.1)
        .max_by(f64::total_cmp)?;
    let n = total - price.points.partition_point(|p| p.0 < late);
    (n >= 2 * discover::MIN_POINTS && n * 10 < total * 9).then_some(n)
}

/// What the strategies' trades usually vary by (bp): their walking-forward spread,
/// weighted by how many trades each had.
pub fn typical_sd(tracked: &[Tracked]) -> f64 {
    let (sum, n) = tracked
        .iter()
        .map(|t| &t.found.checked)
        .filter(|c| c.n >= 2)
        .fold((0.0, 0.0), |(s, n), c| {
            (s + c.sd.powi(2) * c.n as f64, n + c.n as f64)
        });
    if n > 0.0 { (sum / n).sqrt() } else { 0.0 }
}

/// Where the newest stretch (`recent_window`) starts, as an x.
pub fn newest_from(ds: &Dataset, s: &Settings) -> Option<f64> {
    let n = recent_window(ds, s)?;
    let price = discover::price_series(ds, s.series.as_deref())?;
    price
        .points
        .get(price.points.len().checked_sub(n)?)
        .map(|p| p.0)
}

/// `paper` for each, side by side on the cores: each reads every formula over all the
/// data loaded (a day of seconds), on the thread drawing the screen.
fn paper_all(mut ts: Vec<&mut Tracked>, input: &Input, fees: &discover::Fees) {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(ts.len())
        .max(1);
    if threads == 1 {
        for t in ts {
            paper(t, input, fees);
        }
        return;
    }
    let chunk = ts.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for part in ts.chunks_mut(chunk) {
            scope.spawn(move || {
                for t in part {
                    paper(t, input, fees);
                }
            });
        }
    });
}

/// Its paper trades over `input`, and its live record (the ones entered after
/// `since`).
fn paper(t: &mut Tracked, input: &Input, fees: &discover::Fees) {
    let trades = discover::trade(&t.found.strategy, input, 0, fees);
    t.trades = trades
        .iter()
        .map(|r| PaperTrade {
            entry: input.x[r.entry],
            exit: r.exit.map(|e| input.x[e]),
            side: r.side,
            net: r.net,
            live: input.x[r.entry] > t.since,
            y_in: input.price[r.entry],
            y_out: r.exit.map(|e| input.price[e]),
        })
        .collect();
    let live: Vec<f64> = t
        .trades
        .iter()
        .filter(|r| r.live && r.exit.is_some())
        .map(|r| r.net)
        .collect();
    t.live = Perf::of(&live);
    t.open = t
        .trades
        .last()
        .filter(|r| r.exit.is_none() && r.live)
        .map(|r| r.net);
}

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
    /// Every trade over the data.
    pub trades: Vec<PaperTrade>,
    /// Why it isn't paying (or that it is), from the latest search's review.
    pub why: Option<String>,
    /// How it came about, when it's a fix of another.
    pub note: Option<String>,
    /// Its record on the newest stretch (the market's own data), from the latest
    /// review, once it's traded there enough to tell.
    pub newest: Option<Perf>,
    /// How many repairs it's been through (a fix of a fix of …).
    pub repairs: usize,
    /// Which one it is, for good (from 1): repairs share their rules' `key`, and the
    /// list is re-ranked, so the selection follows this.
    pub id: usize,
}

impl Tracked {
    /// Trading live, and not yet the 8 live trades it's judged on.
    pub fn proving(&self) -> bool {
        self.live.n > 0 && self.live.n < RETIRE_AFTER
    }

    /// Its record where it wasn't picked (walking forward, then live), as one
    /// t-statistic — with its spread blended with `typical_sd` (what trades usually
    /// vary by), worth PRIOR_TRADES trades. 19 wins out of 19 have almost no spread, so
    /// a huge t, but that's what luck gives one of the many formulas tried: it mustn't
    /// outrank 400 trades that paid, whose own spread is what counts.
    pub fn score(&self, typical_sd: f64) -> f64 {
        let (c, l) = (&self.found.checked, &self.live);
        let n = (c.n + l.n) as f64;
        if n < 2.0 {
            return f64::MIN;
        }
        let mean = (c.total + l.total) / n;
        let var = (c.sd.powi(2) * c.n as f64 + l.sd.powi(2) * l.n as f64) / n;
        let var = (var * n + typical_sd.powi(2) * PRIOR_TRADES) / (n + PRIOR_TRADES);
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
    /// The id the next strategy tracked gets.
    next_id: usize,
    /// Goes up whenever the paper trades are brought up to date: what's drawn from them
    /// (the markers) only needs redoing then.
    pub changes: u64,
    /// The formulas to start the next search from.
    pool: Vec<Expr>,
    /// When the live results were last brought up to date, and for which data.
    refreshed: Option<(Instant, f64, usize)>,
    /// Strategies that failed live (keys): not picked again…
    failed: Vec<String>,
    /// …but still paper-traded, from when they were retired: one that pays again
    /// comes back.
    pub benched: Vec<Tracked>,
    /// What it learned retiring them, newest first.
    pub lessons: Vec<String>,
    /// How many of the newest points the next search takes…
    pub points: usize,
    /// …and how many formulas it keeps for the one after.
    pub pool_size: usize,
    /// Formulas tried in all, since it started.
    pub tried: usize,
    /// The search running is on the newest stretch only (`recent_window`)…
    searching_recent: bool,
    /// …and the one reported was.
    pub recent: bool,
    pub since: Instant,
}

impl Lab {
    pub fn new(settings: Settings) -> Lab {
        Lab {
            settings,
            worker: None,
            last_run: None,
            // One after another: it keeps inventing while it runs.
            every: Duration::from_secs(1),
            runs: 0,
            report: None,
            waiting: None,
            tracked: Vec::new(),
            selected: 0,
            next_id: 1,
            changes: 0,
            pool: Vec::new(),
            refreshed: None,
            failed: Vec::new(),
            benched: Vec::new(),
            lessons: Vec::new(),
            points: FIRST_POINTS,
            pool_size: FIRST_POOL,
            tried: 0,
            searching_recent: false,
            recent: false,
            since: Instant::now(),
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
            // Every other search: only the newest stretch where series that start late
            // (a live order book with no history) have data, so they get used too.
            let recent = (self.runs % 2 == 1)
                .then(|| recent_window(ds, &self.settings))
                .flatten();
            self.searching_recent = recent.is_some();
            let settings = Settings {
                max_points: self
                    .points
                    .min(self.settings.max_points)
                    .min(recent.unwrap_or(usize::MAX)),
                ..self.settings.clone()
            };
            match Input::from_dataset(ds, &settings) {
                Ok(input) if input.len() >= discover::MIN_POINTS => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let fees = self.settings.fees;
                    let pool = self.pool.clone();
                    let feedback = Feedback {
                        review: self
                            .tracked
                            .iter()
                            .map(|t| t.found.strategy.clone())
                            .collect(),
                        failed: self.failed.clone(),
                        newest_from: newest_from(ds, &self.settings),
                    };
                    self.runs += 1;
                    let budget = Budget {
                        time: SEARCH_TIME,
                        formulas: usize::MAX,
                        pool: self.pool_size,
                        seed: 0x9e37_79b9_7f4a_7c15 ^ (self.runs as u64) ^ input.len() as u64,
                    };
                    std::thread::spawn(move || {
                        let _ = tx.send(discover::search(input, fees, &pool, budget, &feedback));
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
        self.recent = self.searching_recent;
        self.pool = report.pool.iter().map(|s| s.expr.clone()).collect();
        // How many points the next search can take: formula evaluations go through the
        // points at this rate.
        let secs = report.took.as_secs_f64().max(1e-3);
        let rate = report.formulas as f64 * report.input.len() as f64 / secs;
        self.points = ((rate * SEARCH_TIME.as_secs_f64() / ENOUGH_FORMULAS) as usize)
            .max(discover::MIN_POINTS * 10);
        // Keep as many formulas as the next search can score again in a third of its
        // time: no fixed number, it grows with the machine.
        // (From full searches: a short one on the newest stretch would make it too many
        // for the next full one.)
        if !self.recent {
            let n = report.input.len().max(1) as f64;
            self.pool_size = ((rate * SEARCH_TIME.as_secs_f64() / 3.0 / n) as usize).max(150);
        }
        self.tried += report.formulas;
        let since = report.input.x.last().copied().unwrap_or(f64::NEG_INFINITY);
        let selected_id = self.selected_id();
        let new = |found: Found, note: Option<String>, run: usize| Tracked {
            found,
            since,
            run,
            live: Perf::default(),
            open: None,
            trades: Vec::new(),
            why: None,
            note,
            newest: None,
            repairs: 0,
            id: 0,
        };
        // What the review made of each one, and fixes to try.
        for r in &report.reviews {
            let Some(k) = self
                .tracked
                .iter()
                .position(|t| t.found.strategy.key() == r.key)
            else {
                continue;
            };
            self.tracked[k].why = Some(r.why.clone());
            self.tracked[k].newest = r.newest;
            // A repair of a repair too, a few times over: each has to clear the same
            // bar as a new strategy and beat the one it came from where it's judged.
            if let Some(fix) = &r.fix
                && self.tracked[k].repairs < MAX_REPAIRS
                && !self.failed.contains(&fix.strategy.key())
                && !self
                    .tracked
                    .iter()
                    .chain(&self.benched)
                    .any(|t| t.found.strategy == fix.strategy)
            {
                let what = r.fix_note.clone().unwrap_or_default();
                let note = format!("learned from #{}: {what}", k + 1);
                let repairs = self.tracked[k].repairs + 1;
                let mut t = new(fix.clone(), Some(note), self.runs);
                t.repairs = repairs;
                self.track(t);
            }
        }
        for f in report.strategies.iter().take(4) {
            // Found again (the same rules and holding time; the thresholds, stop and
            // take-profit maybe re-fitted to newer data): its record on the newest data.
            // It keeps trading as it was, so its live record stays what it did.
            if let Some(t) = self.tracked.iter_mut().find(|t| {
                t.found.strategy.key() == f.strategy.key()
                    && t.found.strategy.exit.hold == f.strategy.exit.hold
            }) {
                t.found.picked = f.picked;
                t.found.checked = f.checked;
                t.found.steps = f.steps.clone();
                t.found.picked_edge = f.picked_edge;
                t.found.edge = f.edge;
                continue;
            }
            // Only strategies that held up where they weren't picked, and did better
            // than holding there.
            if f.checked.n < 5 || f.checked.mean <= 0.0 || f.edge.mean <= 0.0 {
                continue;
            }
            self.track(new(f.clone(), None, self.runs));
        }
        self.report = Some(report);
        self.refreshed = None;
        self.refresh(ds);
        self.rank(selected_id);
        // The same trades as a better one: only the better one stays.
        let mut k = 1;
        while k < self.tracked.len() {
            let entries = |t: &Tracked| t.trades.iter().map(|p| p.entry).collect::<Vec<_>>();
            let mine = entries(&self.tracked[k]);
            let same = (0..k).any(|j| overlap_x(&entries(&self.tracked[j]), &mine) > 0.7);
            // (Never the one on the chart, nor one proving itself live — others with
            // the same rules can go.)
            if same && k != self.selected && !self.tracked[k].proving() {
                self.tracked.remove(k);
                if self.selected > k {
                    self.selected -= 1;
                }
            } else {
                k += 1;
            }
        }
        self.rank(selected_id);
        self.trim();
    }

    /// Down to TRACKED strategies.
    fn trim(&mut self) {
        // Too many: the worst go — but not one that's trading live and hasn't had
        // its 8 live trades yet (then losing retires it, or it competes like the rest),
        // so newcomers that look good on paper can't push out the live test. Never
        // the one on the chart.
        while self.tracked.len() > TRACKED {
            let worst = |protected: bool| {
                (0..self.tracked.len())
                    .rev()
                    .find(|&k| k != self.selected && self.tracked[k].proving() == protected)
            };
            let Some(drop) = worst(false).or_else(|| worst(true)) else {
                break;
            };
            self.tracked.remove(drop);
            if self.selected > drop {
                self.selected -= 1;
            }
        }
    }

    /// Start tracking a strategy, with an id of its own.
    fn track(&mut self, mut t: Tracked) {
        t.id = self.next_id;
        self.next_id += 1;
        self.tracked.push(t);
    }

    /// The id of the strategy on the chart.
    pub fn selected_id(&self) -> Option<usize> {
        self.tracked.get(self.selected).map(|t| t.id)
    }

    /// Select strategy `id` again wherever it is now (else the best).
    fn reselect(&mut self, id: Option<usize>) {
        self.selected = id
            .and_then(|id| self.tracked.iter().position(|t| t.id == id))
            .unwrap_or(0);
    }

    /// Best first; the selection follows its strategy.
    fn rank(&mut self, keep: Option<usize>) {
        let sd = typical_sd(&self.tracked);
        self.tracked
            .sort_by(|a, b| b.score(sd).total_cmp(&a.score(sd)));
        self.reselect(keep);
    }

    /// Strategies that keep losing live go, each with what it learned (and it isn't
    /// picked again).
    fn retire(&mut self, now: f64) {
        let keep = self.selected_id();
        let mut k = 0;
        while k < self.tracked.len() {
            let t = &self.tracked[k];
            let loses = |p: &Perf| p.n >= RETIRE_AFTER && p.mean < 0.0 && p.t < -0.5;
            // Live, or on the newest stretch (the market it's trading, recorded): it
            // needn't lose 8 live trades first to show it doesn't work there.
            let losing = loses(&t.live) || t.newest.as_ref().is_some_and(loses);
            if !losing {
                k += 1;
                continue;
            }
            let t = self.tracked.remove(k);
            let input = self.report.as_ref().map(|r| &r.input);
            let t_why = t.why.clone();
            let what = input.map_or(t.found.strategy.key(), |i| t.found.strategy.describe(i));
            self.failed.push(t.found.strategy.key());
            self.lessons.insert(
                0,
                format!(
                    "retired {what}: {}{}",
                    match t.newest {
                        Some(p) if !(t.live.n >= RETIRE_AFTER && t.live.mean < 0.0) => {
                            format!("on the newest stretch {}", p.brief())
                        }
                        _ => format!("live {}", t.live.brief()),
                    },
                    t_why.map(|w| format!(" — {w}")).unwrap_or_default()
                ),
            );
            self.lessons.truncate(5);
            // Still watched, with a record from now: it comes back if it pays.
            let mut t = t;
            t.since = now;
            t.live = Perf::default();
            t.open = None;
            t.newest = None;
            self.benched.insert(0, t);
            self.benched.truncate(BENCH);
        }
        self.reselect(keep);
    }

    /// Paper trades on the newest data (at most once a second).
    fn refresh(&mut self, ds: &Dataset) -> Option<String> {
        if self.tracked.is_empty() && self.benched.is_empty() {
            return None;
        }
        // Nothing new, or done less than a second ago: nothing to do. (Asked every
        // frame, so before reading all the data in.)
        let price = discover::price_series(ds, self.settings.series.as_deref())?;
        let seen = (price.points.last()?.0, price.points.len());
        if let Some((at, x, len)) = self.refreshed
            && ((x, len) == seen || at.elapsed() < Duration::from_secs(1))
        {
            return None;
        }
        self.refreshed = Some((Instant::now(), seen.0, seen.1));
        // Paper trades over everything loaded (whatever the searches take).
        let all = Settings {
            max_points: usize::MAX,
            ..self.settings.clone()
        };
        let input = Input::from_dataset(ds, &all).ok()?;
        let newest = (input.x.last().copied().unwrap_or(0.0), input.len());
        self.changes += 1;
        let mut news = None;
        let fees = self.settings.fees;
        let before: Vec<usize> = self
            .tracked
            .iter()
            .map(|t| t.trades.iter().filter(|r| r.live).count())
            .collect();
        // The retired ones too: a strategy that only works in some markets comes back
        // when it starts paying again.
        paper_all(
            self.tracked
                .iter_mut()
                .chain(self.benched.iter_mut())
                .collect(),
            &input,
            &fees,
        );
        for (k, t) in self.tracked.iter().enumerate() {
            let before = before[k];
            let now = t.trades.iter().filter(|r| r.live).count();
            if k == 0
                && now > before
                && let Some(last) = t.trades.last().filter(|r| r.exit.is_none())
            {
                let side = match last.side {
                    Side::Long => "▲ long",
                    Side::Short => "▼ short",
                };
                news = Some(format!(
                    "discover: the best strategy went {side} at {}",
                    crate::discover::fmt_num(last.y_in)
                ));
            }
        }
        let mut k = 0;
        while k < self.benched.len() {
            let t = &self.benched[k];
            if t.live.n >= RETIRE_AFTER && t.live.mean > 0.0 && t.live.t >= 1.0 {
                let mut t = self.benched.remove(k);
                let key = t.found.strategy.key();
                self.failed.retain(|f| *f != key);
                let back = format!("back: {} since it was retired", t.live.brief());
                let what = self
                    .report
                    .as_ref()
                    .map_or(key.clone(), |r| t.found.strategy.describe(&r.input));
                self.lessons.insert(0, format!("{back} — {what}"));
                self.lessons.truncate(5);
                t.note = Some(back);
                // A new live record from here.
                t.since = newest.0;
                paper(&mut t, &input, &fees);
                self.tracked.push(t);
            } else {
                k += 1;
            }
        }
        self.retire(newest.0);
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
        let lane = Some(format!("strategy {}", self.selected + 1));
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
                lane: lane.clone(),
            }
        };
        let mut out = Vec::new();
        for p in &t.trades {
            let long = p.side == Side::Long;
            let color = if long {
                Color::LightGreen
            } else {
                Color::LightRed
            };
            out.push(mark(
                p.entry,
                Some(p.y_in).filter(|v| v.is_finite()),
                p.label().into(),
                if long { "▲" } else { "▼" },
                color,
            ));
            if let Some(x) = p.exit {
                let net = p.net;
                out.push(mark(
                    x,
                    p.y_out,
                    p.exit_label(),
                    "•",
                    if net > 0.0 { Color::Green } else { Color::Red },
                ));
            }
        }
        out
    }
}

/// The share of the smaller of two sorted lists of times found in the other (within a
/// few points' time: the same moments).
fn overlap_x(a: &[f64], b: &[f64]) -> f64 {
    let small = a.len().min(b.len());
    if small == 0 {
        return 0.0;
    }
    let step = {
        let mut d: Vec<f64> = a.windows(2).map(|w| w[1] - w[0]).collect();
        d.sort_by(f64::total_cmp);
        d.get(d.len() / 2).copied().unwrap_or(1.0)
    };
    let tol = step.clamp(1e-9, 5.0) * 3.0;
    let (mut i, mut j, mut both) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if (a[i] - b[j]).abs() <= tol {
            both += 1;
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    both as f64 / small as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Series, XKind};
    use crate::discover::{Cond, Exit, Found, Strategy};
    use crate::formula::Expr;

    /// A price that falls for `down` points, then rises, with a little noise.
    fn prices(down: usize, n: usize) -> Dataset {
        let mut p = 100.0;
        let points = (0..n)
            .map(|i| {
                let wiggle = if i % 2 == 0 { 1.0002 } else { 0.9999 };
                p *= if i < down { 0.9995 } else { 1.0005 } * wiggle;
                (i as f64, p)
            })
            .collect();
        Dataset {
            x_kind: XKind::Number,
            series: vec![Series {
                name: "price".into(),
                unit: Default::default(),
                points,
            }],
        }
    }

    #[test]
    fn losing_on_the_newest_stretch_retires_it_without_waiting_for_live_trades() {
        let mut lab = Lab::new(Settings::default());
        let t = |newest: Option<Perf>| Tracked {
            found: Found::default(),
            since: 0.0,
            run: 1,
            live: Perf::default(),
            open: None,
            trades: Vec::new(),
            why: None,
            note: None,
            newest,
            repairs: 0,
            id: 0,
        };
        let losing = Perf::of(&[-1.0, -0.5, -0.8, -1.2, 0.3, -0.9, -0.7, -1.1, -0.4]);
        let fine = Perf::of(&[1.0, 0.5, -0.8, 1.2, 0.3, 0.9, -0.7, 1.1, 0.4]);
        lab.tracked = vec![t(Some(fine)), t(Some(losing)), t(None)];
        lab.retire(0.0);
        assert_eq!(lab.tracked.len(), 2);
        assert!(lab.tracked.iter().all(|t| t.newest != Some(losing)));
        assert!(
            lab.lessons[0].contains("on the newest stretch"),
            "{:?}",
            lab.lessons
        );
    }

    fn meh() -> Perf {
        Perf::of(&[1.0, -1.0, 0.5, -0.4, 0.2, 0.3, -0.2, 0.1, 0.4, -0.3])
    }

    #[test]
    fn the_selection_stays_on_the_strategy_picked_when_others_share_its_rules() {
        // A repair with another exit has the same rules (`key`) as the one it came
        // from: re-ranking must keep the one tapped, not the first with those rules.
        let mut lab = Lab::new(Settings::default());
        lab.track(tracked(meh(), Perf::default()));
        lab.track(tracked(
            Perf::of(&[2.0, 1.5, 1.8, 2.2, 1.9, 2.1, 1.7, 2.0, 1.6, 2.3]),
            Perf::default(),
        ));
        lab.selected = 0;
        let id = lab.selected_id();
        lab.rank(id);
        assert_eq!(lab.selected, 1, "the better one went first");
        assert_eq!(lab.selected_id(), id);
        lab.retire(0.0);
        assert_eq!(lab.selected_id(), id);
    }

    fn tracked(checked: Perf, live: Perf) -> Tracked {
        Tracked {
            found: Found {
                checked,
                ..Found::default()
            },
            since: 0.0,
            run: 1,
            live,
            open: None,
            trades: Vec::new(),
            why: None,
            note: None,
            newest: None,
            repairs: 0,
            id: 0,
        }
    }

    #[test]
    fn a_few_lucky_wins_dont_outrank_hundreds_of_trades_that_paid() {
        // 19 out of 19, with little spread; 404 trades, +0.63bp each, a wide spread.
        let lucky = Perf::of(&(0..19).map(|i| 1.4 + 0.01 * i as f64).collect::<Vec<_>>());
        let many = Perf::of(
            &(0..404)
                .map(|i| if i % 5 < 2 { 4.0 } else { -1.6 })
                .collect::<Vec<_>>(),
        );
        assert!(lucky.t > many.t, "{} {}", lucky.t, many.t);
        let (a, b) = (
            tracked(lucky, Perf::default()),
            tracked(many, Perf::default()),
        );
        let sd = typical_sd(&[a.clone(), b.clone()]);
        assert!(b.score(sd) > a.score(sd), "{} {}", b.score(sd), a.score(sd));
        // And among themselves, a better record still ranks first.
        assert!(b.score(sd) > tracked(meh(), Perf::default()).score(sd));
    }

    #[test]
    fn newcomers_dont_push_out_one_proving_itself_live() {
        let mut lab = Lab::new(Settings::default());
        let good = Perf::of(&[1.0, 2.0, 1.5, 0.5, 1.2, 2.2, 0.8, 1.1, 1.9, 1.4]);
        let meh = Perf::of(&[1.0, -1.0, 0.5, -0.4, 0.2, 0.3, -0.2, 0.1, 0.4, -0.3]);
        // Ranked last, but 3 trades into its live test.
        let live = tracked(meh, Perf::of(&[0.5, -0.2, 0.7]));
        lab.tracked = (0..TRACKED)
            .map(|_| tracked(good, Perf::default()))
            .collect();
        lab.tracked.push(live);
        lab.trim();
        assert_eq!(lab.tracked.len(), TRACKED);
        assert!(lab.tracked.iter().any(Tracked::proving));
        // Once it's had its 8 live trades, it competes like the rest.
        let done = tracked(meh, Perf::of(&[0.5, -0.2, 0.7, 0.1, -0.3, 0.2, 0.1, -0.1]));
        lab.tracked.push(done);
        lab.trim();
        assert_eq!(lab.tracked.iter().filter(|t| t.live.n == 8).count(), 0);
    }

    #[test]
    fn entries_say_open_while_in_and_exits_say_paper_once_found() {
        let t = |exit: Option<f64>, live| PaperTrade {
            entry: 0.0,
            exit,
            side: Side::Long,
            net: 0.0,
            live,
            y_in: 1.0,
            y_out: None,
        };
        assert_eq!(t(Some(1.0), false).label(), "");
        assert_eq!(t(Some(1.0), true).label(), "");
        assert_eq!(t(None, true).label(), "open");
        assert_eq!(t(Some(1.0), false).exit_label(), "+0.0bp");
        assert_eq!(t(Some(1.0), true).exit_label(), "+0.0bp paper");
    }

    #[test]
    fn the_newest_stretch_is_where_late_series_have_data() {
        // 10,000 seconds of price; a recorded book that starts later.
        let with_book = |from: usize| {
            let mut ds = prices(0, 10_000);
            let book = (from..10_000)
                .map(|x| (x as f64, (x as f64 * 0.37).sin()))
                .collect();
            ds.series.push(Series {
                name: "imbalance".into(),
                unit: Default::default(),
                points: book,
            });
            ds
        };
        let s = Settings::default();
        assert_eq!(recent_window(&with_book(8_000), &s), Some(2_000));
        // Too little of it yet (under 600 points), or nearly everything anyway.
        assert_eq!(recent_window(&with_book(9_700), &s), None);
        assert_eq!(recent_window(&with_book(500), &s), None);
        assert_eq!(recent_window(&prices(0, 10_000), &s), None);
        // The lab takes turns: all of it, then the newest stretch, where the book is an
        // input too.
        let ds = with_book(8_000);
        let mut lab = Lab::new(Settings::default());
        let mut seen = Vec::new();
        let end = Instant::now() + Duration::from_secs(60);
        while seen.len() < 2 && Instant::now() < end {
            let before = lab.report.as_ref().map(|r| r.took);
            lab.tick(&ds);
            if let Some(r) = &lab.report
                && Some(r.took) != before
            {
                let book = r.input.others.iter().any(|o| o.name == "imbalance");
                seen.push((lab.recent, r.input.len(), book));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(seen, [(false, 10_000, false), (true, 2_000, true)]);
    }

    #[test]
    fn a_retired_strategy_comes_back_when_it_pays_again() {
        // Always long, out after 5 points: it loses while the price falls.
        let always = Cond {
            feature: Expr::Num(1.0),
            above: true,
            threshold: 0.0,
        };
        let s = Strategy::one_way(
            Side::Long,
            vec![always],
            Exit {
                hold: 5,
                take: None,
                stop: None,
            },
        );
        let mut lab = Lab::new(Settings::default());
        lab.tracked.push(Tracked {
            found: Found {
                strategy: s.clone(),
                ..Found::default()
            },
            since: 0.0,
            run: 1,
            live: Perf::default(),
            open: None,
            trades: Vec::new(),
            why: None,
            note: None,
            newest: None,
            repairs: 0,
            id: 1,
        });
        lab.refresh(&prices(400, 400));
        assert!(lab.tracked.is_empty(), "retired after losing live");
        assert_eq!(lab.benched.len(), 1);
        assert!(
            lab.failed.contains(&s.key()),
            "not picked again by a search"
        );
        // The price turns: still watched, it makes money again, and comes back.
        lab.refreshed = None;
        lab.refresh(&prices(400, 600));
        assert_eq!(
            lab.tracked.len(),
            1,
            "{:?}",
            lab.benched.first().map(|t| t.live)
        );
        assert!(lab.benched.is_empty() && !lab.failed.contains(&s.key()));
        let t = &lab.tracked[0];
        assert!(
            t.note.as_deref().is_some_and(|n| n.starts_with("back: ")),
            "{:?}",
            t.note
        );
        assert!(lab.lessons[0].starts_with("back: "), "{:?}", lab.lessons);
        // Its live record starts again from its return.
        assert_eq!(t.live.n, 0);
    }
}
