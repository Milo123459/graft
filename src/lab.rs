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
}

impl Tracked {
    /// Its record where it wasn't picked (walking forward, then live), as one t-statistic.
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
            pool: Vec::new(),
            refreshed: None,
            failed: Vec::new(),
            benched: Vec::new(),
            lessons: Vec::new(),
            points: FIRST_POINTS,
            pool_size: FIRST_POOL,
            tried: 0,
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
        let settings = Settings {
            max_points: self.points.min(self.settings.max_points),
            ..self.settings.clone()
        };
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
        self.pool = report.pool.iter().map(|s| s.expr.clone()).collect();
        // How many points the next search can take: formula evaluations go through the
        // points at this rate.
        let secs = report.took.as_secs_f64().max(1e-3);
        let rate = report.formulas as f64 * report.input.len() as f64 / secs;
        self.points = ((rate * SEARCH_TIME.as_secs_f64() / ENOUGH_FORMULAS) as usize)
            .max(discover::MIN_POINTS * 10);
        // Keep as many formulas as the next search can score again in a third of its
        // time: no fixed number, it grows with the machine.
        let n = report.input.len().max(1) as f64;
        self.pool_size = ((rate * SEARCH_TIME.as_secs_f64() / 3.0 / n) as usize).max(150);
        self.tried += report.formulas;
        let since = report.input.x.last().copied().unwrap_or(f64::NEG_INFINITY);
        let selected_key = self
            .tracked
            .get(self.selected)
            .map(|t| t.found.strategy.key());
        let new = |found: Found, note: Option<String>, run: usize| Tracked {
            found,
            since,
            run,
            live: Perf::default(),
            open: None,
            trades: Vec::new(),
            why: None,
            note,
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
            if let Some(fix) = &r.fix
                && !self.failed.contains(&fix.strategy.key())
                && !self
                    .tracked
                    .iter()
                    .any(|t| t.found.strategy.key() == fix.strategy.key())
            {
                let lesson = fix
                    .strategy
                    .except
                    .last()
                    .map_or(String::new(), discover::Cond::describe);
                let note = format!("learned from #{}: not when {lesson}", k + 1);
                self.tracked.push(new(fix.clone(), Some(note), self.runs));
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
            if f.checked.n < 3 || f.checked.mean <= 0.0 || f.edge.mean <= 0.0 {
                continue;
            }
            self.tracked.push(new(f.clone(), None, self.runs));
        }
        self.report = Some(report);
        self.refreshed = None;
        self.refresh(ds);
        self.rank(selected_key.clone());
        // The same trades as a better one: only the better one stays.
        let mut k = 1;
        while k < self.tracked.len() {
            let entries = |t: &Tracked| t.trades.iter().map(|p| p.entry).collect::<Vec<_>>();
            let mine = entries(&self.tracked[k]);
            let same = (0..k).any(|j| overlap_x(&entries(&self.tracked[j]), &mine) > 0.7);
            // (Never the one on the chart — others with the same rules can go.)
            if same && k != self.selected {
                self.tracked.remove(k);
                if self.selected > k {
                    self.selected -= 1;
                }
            } else {
                k += 1;
            }
        }
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

    /// Strategies that keep losing live go, each with what it learned (and it isn't
    /// picked again).
    fn retire(&mut self, now: f64) {
        let keep = self
            .tracked
            .get(self.selected)
            .map(|t| t.found.strategy.key());
        let mut k = 0;
        while k < self.tracked.len() {
            let t = &self.tracked[k];
            let losing = t.live.n >= RETIRE_AFTER && t.live.mean < 0.0 && t.live.t < -0.5;
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
                    "retired {what}: live {}{}",
                    t.live.brief(),
                    t_why.map(|w| format!(" — {w}")).unwrap_or_default()
                ),
            );
            self.lessons.truncate(5);
            // Still watched, with a record from now: it comes back if it pays.
            let mut t = t;
            t.since = now;
            t.live = Perf::default();
            t.open = None;
            self.benched.insert(0, t);
            self.benched.truncate(BENCH);
        }
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
        if self.tracked.is_empty() && self.benched.is_empty() {
            return None;
        }
        // Paper trades over everything loaded (whatever the searches take).
        let all = Settings {
            max_points: usize::MAX,
            ..self.settings.clone()
        };
        let input = Input::from_dataset(ds, &all).ok()?;
        let newest = (input.x.last().copied().unwrap_or(0.0), input.len());
        if let Some((at, x, len)) = self.refreshed
            && ((x, len) == newest || at.elapsed() < Duration::from_secs(1))
        {
            return None;
        }
        self.refreshed = Some((Instant::now(), newest.0, newest.1));
        let mut news = None;
        let fees = self.settings.fees;
        for (k, t) in self.tracked.iter_mut().enumerate() {
            let before = t.trades.iter().filter(|r| r.live).count();
            paper(t, &input, &fees);
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
        // The retired ones too: a strategy that only works in some markets comes back
        // when it starts paying again.
        for t in &mut self.benched {
            paper(t, &input, &fees);
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
                if p.live { "live".into() } else { String::new() },
                if long { "▲" } else { "▼" },
                color,
            ));
            if let Some(x) = p.exit {
                let net = p.net;
                out.push(mark(
                    x,
                    p.y_out,
                    format!("{net:+.1}bp"),
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
