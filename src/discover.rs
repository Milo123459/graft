//! Strategy discovery: hunts for short-term edges in a price series, as it streams in.
//!
//! Nothing here knows about any market or any indicator in particular. It takes a
//! price series (and, if the chart has them, bid and ask series, plus any other series
//! as possible signals), and:
//!
//! 0. **lays the search out from the data** ([`Layout`]): the horizons it looks at
//!    (from the shortest where the typical move is several times what a round trip
//!    costs), the tails of each formula's range it tests (wider for longer horizons, so
//!    there are enough events), and how it validates: it learns on the first half and
//!    walks forward over the second, in as many stretches as the longest horizon
//!    allows;
//! 1. **invents formulas** (`formula.rs`): starting from the usual indicators and
//!    random formulas over the series, it keeps changing and combining the fittest. A
//!    formula is fit when the price moves one way after its extremes (its lowest or
//!    highest 10%), judged only on the learning half. The best formulas carry over to
//!    the next search, so it keeps improving as data streams in;
//! 2. tests **theories**: when a formula is in a tail of its range, what does the price
//!    do over the next h points? Each test uses events at least h apart (so the
//!    t-statistic isn't inflated by overlap). Found on the learning half, then checked
//!    walking forward: on each later stretch, with the tail's threshold re-fitted on
//!    everything before it;
//! 3. turns the promising theories into **strategies** (enter in the theory's
//!    direction; exit after h points, or at a take-profit / stop-loss sized from the
//!    typical move), adds one filter from another formula where it helps, and trades
//!    them with costs: the fee on each side, buying at the ask and selling at the bid
//!    when there are bid and ask series. Picked on the learning half, checked walking
//!    forward like the theories, ranked by both and by how many stretches it made
//!    money in.
//!
//! A strategy's thresholds are ranks, not fixed numbers: "in the lowest 5% of its range
//! so far". Each walk-forward step re-fits them on the data before it, as trading it
//! from then on would; what's paper-traded is fitted on all of it.
//!
//! The viewer reruns it as data arrives and paper-trades what it found from then on:
//! the live results are the only ones the search never saw.

use crate::data::Dataset;
use crate::formula::{Expr, Maker, Rng};

/// Fewer points than this and there's nothing to learn from yet.
pub const MIN_POINTS: usize = 300;
/// Trades a strategy needs on the learning half to be considered…
const MIN_TRADES: usize = 10;
/// …and the t-statistic of its returns after costs there…
const MIN_T: f64 = 1.5;
/// …and of how much better than holding the same way it did.
const MIN_EDGE_T: f64 = 1.0;

/// What trading costs, besides the spread (buying at the ask, selling at the bid).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fees {
    /// A % of the trade's value on each buy and each sell (0.045: Hyperliquid's taker
    /// fee).
    Flat(f64),
    /// Profit sharing, as on Veranta's Upside Perps, at this leverage: nothing on a
    /// losing trade; on a winning one you keep 75% of the profit up to 250% ROI, 80% up
    /// to 500%, 90% up to 1,500%, 95% beyond (ROI = the move × leverage), and the
    /// take-profit can't go past 2,500% ROI. Liquidated at -85% ROI. `staked`: the
    /// protocol's share is 25% smaller (staking AVNT).
    Share { leverage: f64, staked: bool },
}

impl Fees {
    /// `0.045` (a flat % a side), `upside:250` or `upside:250:staked`.
    pub fn parse(text: &str) -> Result<Fees, String> {
        let t = text.trim().to_lowercase();
        if let Ok(v) = t.trim_end_matches('%').parse::<f64>() {
            return if v >= 0.0 {
                Ok(Fees::Flat(v))
            } else {
                Err("the fee is a % per buy and per sell, e.g. 0.045".into())
            };
        }
        let mut parts = t.split([':', ' ']).filter(|p| !p.is_empty());
        if parts.next() == Some("upside") {
            let lev = parts
                .next()
                .and_then(|l| l.trim_end_matches('x').parse::<f64>().ok())
                .filter(|l| *l >= 1.0)
                .ok_or("upside:LEVERAGE, e.g. upside:250 (1x to 500x for BTC)")?;
            let staked = parts.next() == Some("staked");
            return Ok(Fees::Share {
                leverage: lev,
                staked,
            });
        }
        Err(format!(
            "'{text}': a fee in % a side (0.045) or upside:LEVERAGE (upside:250, upside:250:staked)"
        ))
    }

    /// As --discover takes it.
    pub fn to_arg(self) -> String {
        match self {
            Fees::Flat(f) => f.to_string(),
            Fees::Share { leverage, staked } => {
                format!("upside:{leverage}{}", if staked { ":staked" } else { "" })
            }
        }
    }

    /// `fee 0.045% a side`, `Upside 250x`.
    pub fn short(&self) -> String {
        match self {
            Fees::Flat(f) => format!("fee {f}% a side"),
            Fees::Share { leverage, staked } => {
                format!("Upside {leverage}x{}", if *staked { " staked" } else { "" })
            }
        }
    }

    /// The share of a profit of `roi` % that you keep.
    pub fn keep(roi: f64, staked: bool) -> f64 {
        let keep = if roi <= 250.0 {
            0.75
        } else if roi <= 500.0 {
            0.80
        } else if roi <= 1500.0 {
            0.90
        } else {
            0.95
        };
        if staked {
            1.0 - (1.0 - keep) * 0.75
        } else {
            keep
        }
    }

    /// A trade's result after costs, from its move after the spread (bp of the
    /// position's value).
    pub fn net(&self, gross: f64) -> f64 {
        match *self {
            Fees::Flat(f) => gross - 2.0 * f * 100.0,
            Fees::Share { leverage, staked } => {
                if gross <= 0.0 {
                    // Nothing on losses; at most the margin is lost.
                    gross.max(-1e4 / leverage)
                } else {
                    gross * Self::keep(gross * leverage / 100.0, staked)
                }
            }
        }
    }

    /// The move (bp, against you) that liquidates, if there's leverage.
    pub fn liquidation(&self) -> Option<f64> {
        match self {
            Fees::Flat(_) => None,
            Fees::Share { leverage, .. } => Some(0.85 * 1e4 / leverage),
        }
    }

    /// The most a trade can make (the take-profit's limit, bp).
    pub fn max_take(&self) -> Option<f64> {
        match self {
            Fees::Flat(_) => None,
            Fees::Share { leverage, .. } => Some(25.0 * 1e4 / leverage),
        }
    }

    /// The position's return on its margin (%) for a result in bp.
    pub fn roi(&self, bp: f64) -> Option<f64> {
        match self {
            Fees::Flat(_) => None,
            Fees::Share { leverage, .. } => Some(bp * leverage / 100.0),
        }
    }

    /// What it costs, for the panel.
    pub fn explain(&self, spread: f64) -> String {
        match self {
            Fees::Flat(f) => format!(
                "a round trip costs {:.1}bp ({:.1}bp fees + {spread:.1}bp spread)",
                2.0 * f * 100.0 + spread,
                2.0 * f * 100.0
            ),
            Fees::Share { leverage, staked } => format!(
                "{spread:.1}bp spread; no fee on losses; {}% of profits taken (less above 250% ROI = a {:.0}bp move); liquidated at a {:.1}bp move against",
                if *staked { 18.75 } else { 25.0 },
                250.0 / leverage * 100.0,
                0.85 * 1e4 / leverage
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub fees: Fees,
    /// The price series (`None`: close, price or mid, else the first).
    pub series: Option<String>,
    /// Only the newest this many points are searched (the lab sizes it from how fast
    /// its searches go; everything by default).
    pub max_points: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            fees: Fees::Flat(0.0),
            series: None,
            max_points: usize::MAX,
        }
    }
}

/// Another series, lined up with the price's points.
#[derive(Debug, Clone)]
pub struct Other {
    pub name: String,
    pub values: Vec<f64>,
    /// On the price's scale (an average, a band…): compared to the price as a gap.
    pub price_like: bool,
}

/// The data a search runs on.
#[derive(Debug, Clone)]
pub struct Input {
    pub name: String,
    pub x: Vec<f64>,
    pub price: Vec<f64>,
    /// Bid and ask, lined up with the price (series named …bid… and …ask…).
    pub book: Option<(String, Vec<f64>, String, Vec<f64>)>,
    /// Half the average spread (a fraction of the price): what buying and selling cost
    /// where there's no bid or ask (history with prices only).
    pub half_spread: f64,
    pub others: Vec<Other>,
    /// Whether x is time (so lengths can be told in seconds).
    pub time: bool,
}

/// The value of points (sorted by x) at each of `xs`: the one at or just before it.
pub fn line_up(points: &[(f64, f64)], xs: &[f64]) -> Vec<f64> {
    let mut j = 0;
    xs.iter()
        .map(|&x| {
            while j < points.len() && points[j].0 <= x {
                j += 1;
            }
            if j == 0 { f64::NAN } else { points[j - 1].1 }
        })
        .collect()
}

/// The series a search trades: the one asked for, else close / price / mid, else the
/// first.
pub fn price_series<'a>(ds: &'a Dataset, name: Option<&str>) -> Option<&'a crate::data::Series> {
    if let Some(n) = name {
        return ds
            .series
            .iter()
            .find(|s| s.name == n)
            .or_else(|| ds.series.iter().find(|s| s.name.eq_ignore_ascii_case(n)));
    }
    ["close", "price", "mid", "c"]
        .iter()
        .find_map(|n| ds.series.iter().find(|s| s.name.eq_ignore_ascii_case(n)))
        .or_else(|| ds.series.iter().find(|s| s.points.len() > 1))
}

impl Input {
    pub fn from_dataset(ds: &Dataset, s: &Settings) -> Result<Input, String> {
        let price = price_series(ds, s.series.as_deref()).ok_or_else(|| match &s.series {
            Some(n) => format!("there's no series called \"{n}\""),
            None => "there's no data yet".to_string(),
        })?;
        let start = price.points.len().saturating_sub(s.max_points);
        let pts: Vec<(f64, f64)> = price.points[start..]
            .iter()
            .copied()
            .filter(|p| p.1.is_finite() && p.1 > 0.0)
            .collect();
        let x: Vec<f64> = pts.iter().map(|p| p.0).collect();
        let values: Vec<f64> = pts.iter().map(|p| p.1).collect();
        let median = {
            let mut v = values.clone();
            v.sort_by(f64::total_cmp);
            v.get(v.len() / 2).copied().unwrap_or(0.0)
        };
        let mut others = Vec::new();
        let (mut bid, mut ask) = (None, None);
        for o in &ds.series {
            // Its own kept formulas (disc1_1…) are already among its formulas.
            let kept = o.name.strip_prefix("disc").is_some_and(|r| {
                r.split_once('_').is_some_and(|(a, b)| {
                    a.chars().all(|c| c.is_ascii_digit()) && b.chars().all(|c| c.is_ascii_digit())
                })
            });
            if o.name == price.name || o.points.len() < 2 || kept {
                continue;
            }
            let lined = line_up(&o.points, &x);
            let finite: Vec<f64> = lined.iter().copied().filter(|v| v.is_finite()).collect();
            if finite.is_empty() {
                continue;
            }
            let (lo, hi) = finite
                .iter()
                .fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
            let near = |v: f64| median > 0.0 && (v / median - 1.0).abs() < 0.2;
            let price_like = near(lo) && near(hi);
            let lower = o.name.to_lowercase();
            let last = lower
                .rsplit(['.', '_'])
                .next()
                .unwrap_or(&lower)
                .to_string();
            // Bid and ask, however little of the data they cover (history from
            // elsewhere may have only prices): where they're missing, the usual spread.
            if price_like && bid.is_none() && last.contains("bid") {
                bid = Some((o.name.clone(), lined));
                continue;
            }
            if price_like && ask.is_none() && last.contains("ask") {
                ask = Some((o.name.clone(), lined));
                continue;
            }
            // Signals have to be there for most of the data.
            if finite.len() < lined.len() / 2 || lo == hi {
                continue;
            }
            // A clock (a timestamp field): never falls and is huge.
            let rising = finite.windows(2).filter(|w| w[1] >= w[0]).count();
            if rising + 1 >= finite.len() && lo > 1e8 {
                continue;
            }
            others.push(Other {
                name: o.name.clone(),
                values: lined,
                price_like,
            });
        }
        let book = match (bid, ask) {
            (Some((bn, b)), Some((an, a))) => Some((bn, b, an, a)),
            (b, a) => {
                // Only one side: just another series.
                for (n, v) in [b, a].into_iter().flatten() {
                    others.push(Other {
                        name: n,
                        values: v,
                        price_like: true,
                    });
                }
                None
            }
        };
        let half_spread = match &book {
            Some((_, b, _, a)) => {
                let s: Vec<f64> = b
                    .iter()
                    .zip(a)
                    .filter(|(b, a)| b.is_finite() && a.is_finite() && **b > 0.0)
                    .map(|(b, a)| a / b - 1.0)
                    .collect();
                if s.is_empty() {
                    0.0
                } else {
                    s.iter().sum::<f64>() / s.len() as f64 / 2.0
                }
            }
            None => 0.0,
        };
        Ok(Input {
            name: price.name.clone(),
            x,
            price: values,
            book,
            half_spread,
            others,
            time: ds.x_kind == crate::data::XKind::Time,
        })
    }

    pub fn len(&self) -> usize {
        self.price.len()
    }

    /// The typical gap between points (seconds on a time axis).
    pub fn step(&self) -> f64 {
        let mut d: Vec<f64> = self.x.windows(2).map(|w| w[1] - w[0]).collect();
        d.sort_by(f64::total_cmp);
        d.get(d.len() / 2).copied().unwrap_or(1.0)
    }

    /// `15 points (3s)`.
    pub fn points_text(&self, n: usize) -> String {
        let pts = format!("{n} point{}", if n == 1 { "" } else { "s" });
        if self.time {
            format!(
                "{pts} ({})",
                crate::time::fmt_duration(n as f64 * self.step())
            )
        } else {
            pts
        }
    }

    /// What you pay to buy, and get when you sell, at point i.
    fn buy_at(&self, i: usize) -> f64 {
        match &self.book {
            Some((_, _, _, ask)) if ask[i].is_finite() => ask[i],
            _ => self.price[i] * (1.0 + self.half_spread),
        }
    }

    fn sell_at(&self, i: usize) -> f64 {
        match &self.book {
            Some((_, bid, _, _)) if bid[i].is_finite() => bid[i],
            _ => self.price[i] * (1.0 - self.half_spread),
        }
    }

    fn other(&self, name: &str) -> Option<&Other> {
        self.others.iter().find(|o| o.name == name)
    }
}

// ───────────── formulas over the input ─────────────

impl crate::formula::Columns for Input {
    fn column(&self, name: &str) -> Option<&[f64]> {
        if name == self.name {
            return Some(&self.price);
        }
        if let Some((bn, b, an, a)) = &self.book {
            if name == bn {
                return Some(b);
            }
            if name == an {
                return Some(a);
            }
        }
        self.other(name).map(|o| o.values.as_slice())
    }

    fn rows(&self) -> usize {
        self.len()
    }
}

impl Input {
    /// Every series a formula can use: the price first.
    pub fn columns(&self) -> Vec<String> {
        let mut v = vec![self.name.clone()];
        if let Some((b, _, a, _)) = &self.book {
            v.push(b.clone());
            v.push(a.clone());
        }
        v.extend(self.others.iter().map(|o| o.name.clone()));
        v
    }

    /// The usual indicators, as formulas (where the search starts).
    pub fn classics(&self) -> Vec<Expr> {
        let others: Vec<(String, bool)> = self
            .others
            .iter()
            .map(|o| (o.name.clone(), o.price_like))
            .collect();
        crate::formula::classics(
            &self.name,
            self.book
                .as_ref()
                .map(|(b, _, a, _)| (b.as_str(), a.as_str())),
            &others,
            self.len(),
        )
    }
}

/// `-3.2`, `0.0041`, `1.2e6`: three significant digits.
pub fn fmt_num(v: f64) -> String {
    let a = v.abs();
    if a == 0.0 {
        "0".into()
    } else if a >= 1e5 || a < 1e-3 {
        format!("{v:.2e}")
    } else {
        let digits = (2 - a.log10().floor() as i32).clamp(0, 6) as usize;
        format!("{v:.digits$}")
    }
}

// ───────────── statistics ─────────────

/// How a set of returns (bp each) did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Perf {
    pub n: usize,
    /// Average per trade (bp).
    pub mean: f64,
    pub sd: f64,
    /// t-statistic of the mean (how sure it isn't luck: above 2 is a start).
    pub t: f64,
    /// Share of winners (0–1).
    pub win: f64,
    /// All of them added up (bp).
    pub total: f64,
    /// Largest fall of the running total from its high (bp).
    pub drawdown: f64,
    /// The best and the worst one (bp).
    pub best: f64,
    pub worst: f64,
}

impl Perf {
    pub fn of(r: &[f64]) -> Perf {
        let n = r.len();
        if n == 0 {
            return Perf::default();
        }
        let mean = r.iter().sum::<f64>() / n as f64;
        let var = if n > 1 {
            r.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1) as f64
        } else {
            0.0
        };
        let sd = var.sqrt();
        let t = if sd > 0.0 {
            mean / (sd / (n as f64).sqrt())
        } else {
            0.0
        };
        let (mut run, mut peak, mut dd) = (0.0f64, 0.0f64, 0.0f64);
        for v in r {
            run += v;
            peak = peak.max(run);
            dd = dd.max(peak - run);
        }
        Perf {
            n,
            mean,
            sd,
            t,
            win: r.iter().filter(|v| **v > 0.0).count() as f64 / n as f64,
            total: run,
            drawdown: dd,
            best: r.iter().copied().fold(f64::MIN, f64::max),
            worst: r.iter().copied().fold(f64::MAX, f64::min),
        }
    }

    /// `max DD 12.3bp (31% of margin) · worst −3.2bp (−8%) · best +8.1bp (+20%)`: what
    /// it risked — with leverage, also as a share of the margin a trade puts up.
    pub fn risk(&self, fees: &Fees) -> String {
        if self.n == 0 {
            return "no trades".into();
        }
        let roi = |bp: f64, signed: bool| {
            fees.roi(bp).map_or(String::new(), |r| {
                if signed {
                    format!(" ({r:+.0}%)")
                } else {
                    format!(" ({r:.0}% of margin)")
                }
            })
        };
        format!(
            "max DD {:.1}bp{} · worst {:+.1}bp{} · best {:+.1}bp{}",
            self.drawdown,
            roi(self.drawdown, false),
            self.worst,
            roi(self.worst, true),
            self.best,
            roi(self.best, true)
        )
    }

    /// `+1.20bp × 23 (t 2.1, 61% won)` (no t for fewer than 5: it means nothing yet).
    pub fn brief(&self) -> String {
        if self.n == 0 {
            return "no trades".into();
        }
        if self.n < 5 {
            return format!(
                "{:+.2}bp × {} ({:.0}% won)",
                self.mean,
                self.n,
                self.win * 100.0
            );
        }
        format!(
            "{:+.2}bp × {} (t {:.1}, {:.0}% won)",
            self.mean,
            self.n,
            self.t,
            self.win * 100.0
        )
    }
}

/// The value at quantile q (0–1) of the finite values in v[from..to].
fn quantiles(v: &[f64], from: usize, to: usize, qs: &[f64]) -> Option<Vec<f64>> {
    let mut s: Vec<f64> = v[from..to]
        .iter()
        .copied()
        .filter(|x| x.is_finite())
        .collect();
    if s.len() < 20 {
        return None;
    }
    s.sort_by(f64::total_cmp);
    Some(
        qs.iter()
            .map(|q| s[((s.len() - 1) as f64 * q).round() as usize])
            .collect(),
    )
}

// ───────────── theories ─────────────

/// One side of a feature's range.
#[derive(Debug, Clone, PartialEq)]
pub struct Cond {
    pub feature: Expr,
    /// `true`: the feature is above the threshold; `false`: below.
    pub above: bool,
    pub threshold: f64,
}

impl Cond {
    fn holds(&self, v: f64) -> bool {
        v.is_finite()
            && if self.above {
                v > self.threshold
            } else {
                v < self.threshold
            }
    }

    /// `pct(price, 15) < -3.20`.
    pub fn describe(&self) -> String {
        format!(
            "{} {} {}",
            self.feature,
            if self.above { ">" } else { "<" },
            fmt_num(self.threshold)
        )
    }
}

/// When a feature is in a tail of its range, the price moves this way afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct Theory {
    pub cond: Cond,
    /// How far the tail reaches into the range (0.05: its last 5%).
    pub tail: f64,
    /// Points ahead.
    pub ahead: usize,
    /// The price's change that many points later (bp), on the learning half…
    pub found: Perf,
    /// …and walking forward over the rest.
    pub checked: Perf,
}

impl Theory {
    /// Whether the walk-forward agreed (the same direction, t of 2 or more).
    pub fn held(&self) -> bool {
        self.checked.n >= 5
            && self.checked.mean.signum() == self.found.mean.signum()
            && self.checked.t.abs() >= 2.0
    }

    pub fn describe(&self, input: &Input) -> String {
        let side = if self.cond.above { "top" } else { "bottom" };
        format!(
            "{} ({} {:.0}%) → {} later {:+.2}bp (t {:.1}, {} times) · walking forward: {:+.2}bp (t {:.1}, {}) {}",
            self.cond.describe(),
            side,
            self.tail * 100.0,
            input.points_text(self.ahead),
            self.found.mean,
            self.found.t,
            self.found.n,
            self.checked.mean,
            self.checked.t,
            self.checked.n,
            if self.held() { "✓ held" } else { "✗ faded" }
        )
    }
}

/// The price's change (bp) from each point to h points later.
fn forward(p: &[f64], h: usize) -> Vec<f64> {
    (0..p.len())
        .map(|i| {
            if i + h < p.len() {
                (p[i + h] / p[i] - 1.0) * 1e4
            } else {
                f64::NAN
            }
        })
        .collect()
}

/// The returns after each point in [from, to) where `on` holds, taking points at least
/// `gap` apart (so no two overlap).
fn events(on: impl Fn(usize) -> bool, fwd: &[f64], from: usize, to: usize, gap: usize) -> Vec<f64> {
    let mut out = Vec::new();
    let mut i = from;
    while i < to {
        if on(i) && fwd[i].is_finite() {
            out.push(fwd[i]);
            i += gap.max(1);
        } else {
            i += 1;
        }
    }
    out
}

// ───────────── strategies ─────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Long,
    Short,
}

/// How a trade ends: after `hold` points at the latest, or sooner at the take-profit or
/// stop-loss (bp from the entry, after the spread; `None`: none).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Exit {
    pub hold: usize,
    pub take: Option<f64>,
    pub stop: Option<f64>,
}

/// When to trade: long when all `long` conditions hold, short when all `short` ones do
/// (either can be empty: a one-way strategy), never when an `except` one holds (a
/// condition it learned it loses in).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Strategy {
    pub long: Vec<Cond>,
    pub short: Vec<Cond>,
    pub except: Vec<Cond>,
    pub exit: Exit,
}

/// A trade a strategy made: in at point `entry`, out at `exit` (`None`: still open, at
/// the newest point), its move after the spread and its result after costs (bp).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trade {
    pub entry: usize,
    pub exit: Option<usize>,
    pub side: Side,
    pub gross: f64,
    pub net: f64,
}

fn join_conds(c: &[Cond]) -> String {
    c.iter()
        .map(Cond::describe)
        .collect::<Vec<_>>()
        .join(" and ")
}

impl Strategy {
    pub fn one_way(side: Side, conds: Vec<Cond>, exit: Exit) -> Strategy {
        let (long, short) = match side {
            Side::Long => (conds, Vec::new()),
            Side::Short => (Vec::new(), conds),
        };
        Strategy {
            long,
            short,
            except: Vec::new(),
            exit,
        }
    }

    /// Every condition, in the order `trades` takes their values: long, short, except.
    pub fn conds(&self) -> impl Iterator<Item = &Cond> {
        self.long.iter().chain(&self.short).chain(&self.except)
    }

    /// `▲ long when z(price, 15) > 2.2 · ▼ short when z(price, 15) < -2.2 · except when
    /// std(pct(price, 1), 50) > 3.1 · out after 15 points, TP 4.0bp, SL 3.0bp`.
    pub fn describe(&self, input: &Input) -> String {
        let mut parts = Vec::new();
        if !self.long.is_empty() {
            parts.push(format!("▲ long when {}", join_conds(&self.long)));
        }
        if !self.short.is_empty() {
            parts.push(format!("▼ short when {}", join_conds(&self.short)));
        }
        if !self.except.is_empty() {
            parts.push(format!("except when {}", join_conds(&self.except)));
        }
        let mut out = format!("out after {}", input.points_text(self.exit.hold));
        if let Some(t) = self.exit.take {
            out += &format!(", TP {t:.1}bp");
        }
        if let Some(s) = self.exit.stop {
            out += &format!(", SL {s:.1}bp");
        }
        parts.push(out);
        parts.join(" · ")
    }

    /// The same strategy, whatever its exits: for spotting repeats.
    pub fn key(&self) -> String {
        let list = |c: &[Cond]| {
            let mut v: Vec<String> = c
                .iter()
                .map(|c| format!("{}{}", c.feature, if c.above { ">" } else { "<" }))
                .collect();
            v.sort();
            v.join(" ")
        };
        format!(
            "L[{}] S[{}] X[{}]",
            list(&self.long),
            list(&self.short),
            list(&self.except)
        )
    }

    /// Trades entered in [from, to): one at a time, each ending at its exit (or still
    /// open at the end of the data). `values` are the conditions' features, in the
    /// order of `conds()`.
    pub fn trades(
        &self,
        input: &Input,
        values: &[&[f64]],
        from: usize,
        to: usize,
        fees: &Fees,
    ) -> Vec<Trade> {
        let liquidation = fees.liquidation();
        let max_take = fees.max_take();
        let n = input.len();
        let (nl, ns) = (self.long.len(), self.short.len());
        let (lv, rest) = values.split_at(nl.min(values.len()));
        let (sv, xv) = rest.split_at(ns.min(rest.len()));
        let all = |c: &[Cond], v: &[&[f64]], i: usize| {
            !c.is_empty() && c.iter().zip(v).all(|(c, v)| c.holds(v[i]))
        };
        let mut out = Vec::new();
        let mut i = from;
        while i < to.min(n) {
            let side = if self.except.iter().zip(xv).any(|(c, v)| c.holds(v[i])) {
                None
            } else if all(&self.long, lv, i) {
                Some(Side::Long)
            } else if all(&self.short, sv, i) {
                Some(Side::Short)
            } else {
                None
            };
            let Some(side) = side else {
                i += 1;
                continue;
            };
            let long = side == Side::Long;
            let entry = if long {
                input.buy_at(i)
            } else {
                input.sell_at(i)
            };
            let ret = |j: usize| {
                let exit = if long {
                    input.sell_at(j)
                } else {
                    input.buy_at(j)
                };
                let r = (exit / entry - 1.0) * 1e4;
                if long { r } else { -r }
            };
            let last = (i + self.exit.hold).min(n - 1);
            let mut closed = None;
            for j in i + 1..=last {
                let r = ret(j);
                let hit = self.exit.take.is_some_and(|t| r >= t)
                    || self.exit.stop.is_some_and(|s| r <= -s)
                    || liquidation.is_some_and(|l| r <= -l)
                    || max_take.is_some_and(|m| r >= m)
                    || j == i + self.exit.hold;
                if hit {
                    closed = Some((j, r));
                    break;
                }
            }
            match closed {
                Some((j, r)) => {
                    out.push(Trade {
                        entry: i,
                        exit: Some(j),
                        side,
                        gross: r,
                        net: fees.net(r),
                    });
                    i = j + 1;
                }
                None => {
                    // Still open at the newest point.
                    if i + 1 < n {
                        let r = ret(n - 1);
                        out.push(Trade {
                            entry: i,
                            exit: None,
                            side,
                            gross: r,
                            net: fees.net(r),
                        });
                    }
                    break;
                }
            }
        }
        out
    }
}

/// What it learned a strategy loses in: entering when `cond` holds, its trades did
/// this badly (`removed`) and the rest this well (`kept`).
#[derive(Debug, Clone, PartialEq)]
pub struct Lesson {
    pub cond: Cond,
    pub removed: Perf,
    pub kept: Perf,
}

/// The condition (a feature above or below a threshold) that best tells a strategy's
/// losing trades from its winners: leaving out the trades entered while it held must
/// clearly help what's left (t up by `gain`), keep at least half of them, and leave
/// out trades that lost on average.
pub fn adverse(
    trades: &[Trade],
    features: &[Expr],
    values: &[Vec<f64>],
    skip: &[&Expr],
    gain: f64,
) -> Option<Lesson> {
    let closed: Vec<&Trade> = trades.iter().filter(|t| t.exit.is_some()).collect();
    if closed.len() < 8 {
        return None;
    }
    let base = Perf::of(&closed.iter().map(|t| t.net).collect::<Vec<_>>());
    let mut best: Option<Lesson> = None;
    for (f, v) in features.iter().zip(values) {
        if skip.contains(&f) {
            continue;
        }
        let at: Vec<(f64, f64)> = closed
            .iter()
            .map(|t| (v[t.entry], t.net))
            .filter(|p| p.0.is_finite())
            .collect();
        if at.len() < closed.len() * 3 / 4 {
            continue;
        }
        let mut xs: Vec<f64> = at.iter().map(|p| p.0).collect();
        xs.sort_by(f64::total_cmp);
        let mut tried_th = Vec::new();
        for q in 1..10 {
            let th = xs[xs.len() * q / 10];
            if tried_th.contains(&th) {
                continue;
            }
            tried_th.push(th);
            for above in [true, false] {
                let bad = |x: f64| if above { x > th } else { x < th };
                let (out, kept): (Pairs, Pairs) = at.iter().partition(|p| bad(p.0));
                if kept.len() * 2 < closed.len() || out.len() < 3 {
                    continue;
                }
                let removed = Perf::of(&out.iter().map(|p| p.1).collect::<Vec<_>>());
                let kept = Perf::of(&kept.iter().map(|p| p.1).collect::<Vec<_>>());
                if removed.mean >= 0.0 || kept.t < base.t + gain {
                    continue;
                }
                if best.as_ref().is_none_or(|b| kept.t > b.kept.t) {
                    best = Some(Lesson {
                        cond: Cond {
                            feature: f.clone(),
                            above,
                            threshold: th,
                        },
                        removed,
                        kept,
                    });
                }
            }
        }
    }
    best
}

/// Points of (a feature's value, a trade's result).
type Pairs<'a> = Vec<&'a (f64, f64)>;

/// Why a strategy isn't paying (from its trades where it wasn't picked), and a fixed
/// version when it found a condition it loses in.
#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    pub key: String,
    pub why: String,
    pub fix: Option<Found>,
}

/// A strategy and how it did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Found {
    pub strategy: Strategy,
    /// On the learning half (where it was picked)…
    pub picked: Perf,
    /// …and walking forward over the rest: all of it, and each stretch.
    pub checked: Perf,
    pub steps: Vec<Perf>,
    /// Its edge over holding (each trade's move less what holding the same side for as
    /// long gave, bp before costs): on the learning half, and walking forward.
    pub picked_edge: Perf,
    pub edge: Perf,
}

impl Found {
    /// The walk-forward stretches it made money in, of those it traded in.
    pub fn consistency(&self) -> Option<(usize, usize)> {
        let of = self.steps.iter().filter(|p| p.n > 0).count();
        let up = self
            .steps
            .iter()
            .filter(|p| p.n > 0 && p.mean > 0.0)
            .count();
        (of > 0).then_some((up, of))
    }
}

/// What a search found.
#[derive(Debug, Clone)]
pub struct Report {
    pub input: Input,
    /// Formulas tried, theories and strategies tested.
    pub formulas: usize,
    pub tests: usize,
    pub tried: usize,
    /// How it used the data: what it learned on, how it checked, the horizons and
    /// tails.
    pub layout: Layout,
    /// The |t| theories needed on the learning half to be listed: the strongest that
    /// luck alone would give out of that many tests.
    pub bar: f64,
    /// The average spread (bp), where there are bid and ask series.
    pub spread: f64,
    /// The costs it traded with.
    pub fees: Fees,
    /// Holding over the learning half and the rest (bp).
    pub hold: [f64; 2],
    /// The best theories (held ones first).
    pub theories: Vec<Theory>,
    /// The best strategies, ranked by the weaker of the learning half and the
    /// walk-forward.
    pub strategies: Vec<Found>,
    /// The fittest formulas (to start the next search from).
    pub pool: Vec<Scored>,
    /// What it made of the strategies being paper-traded.
    pub reviews: Vec<Review>,
    pub took: std::time::Duration,
}

/// What each part of a formula costs its fitness (in t): a bigger formula has to
/// predict better to be kept. Small, since the walk-forward catches formulas that only
/// fit the past, so deep formulas can win when they're really better.
const SIZE_COST: f64 = 0.04;
/// Formulas kept from one search to the next.
const POOL: usize = 150;
/// The best of them, tried as entries and filters for strategies.
const FEATURES: usize = 40;

// ───────────── how a search lays out the data ─────────────

/// How a search uses the data: worked out from the data (its length, how much the
/// price moves, what trading costs) each time it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// Formulas are invented, theories found and strategies picked on [0, learn): the
    /// first half. For an edge that's really there, t grows with √trades, so the
    /// weaker of the picked and checked t (what strategies are ranked by) is largest
    /// when both halves are the same size.
    pub learn: usize,
    /// Then they're checked walking forward: on each stretch [cuts[k], cuts[k + 1]),
    /// with their thresholds re-fitted on everything before it. As many stretches
    /// (up to 5) as leave each one 30 spans of the longest horizon.
    pub cuts: Vec<usize>,
    /// Horizons (points ahead): from the shortest where the typical move is 4× what a
    /// round trip costs, each about 3× the last; at most 1/60 of the learning half.
    pub aheads: Vec<usize>,
    /// The typical move (bp, one standard deviation) over each horizon.
    pub moves: Vec<f64>,
    /// The tails of a formula's range tried at each horizon (0.05: its lowest or
    /// highest 5%): enough of the learning half that events h apart come up about 20
    /// times per tail, between 2% and 10%, and 3× that.
    pub tails: Vec<[f64; 2]>,
    /// What a round trip costs (bp): the spread and fees, or with profit sharing the
    /// move whose kept share pays the spread.
    pub cost: f64,
    /// No horizon the data allows moves enough to pay for that (so the longest are
    /// used, and edges have to be large).
    pub short_of_costs: bool,
}

impl Layout {
    pub fn of(input: &Input, fees: &Fees) -> Layout {
        let n = input.len();
        let learn = n / 2;
        let p = &input.price;
        let spread = input.half_spread * 2.0 * 1e4;
        let cost = match *fees {
            Fees::Flat(f) => spread + 2.0 * f * 100.0,
            Fees::Share { staked, .. } => spread / Fees::keep(0.0, staked),
        };
        // The typical move over h points, from spans that don't overlap.
        let move_over = |h: usize| -> f64 {
            let r: Vec<f64> = (0..learn.saturating_sub(h))
                .step_by(h.max(1))
                .map(|i| (p[i + h] / p[i] - 1.0) * 1e4)
                .filter(|r| r.is_finite())
                .collect();
            Perf::of(&r).sd
        };
        let longest = (learn / 60).max(2);
        // 2, 3, 4, 6, 9, 13, 20, 30, 45 … up to the longest the data allows.
        let mut ladder = vec![2usize];
        while let Some(&h) = ladder.last() {
            let next = (h * 3).div_ceil(2);
            if next > longest {
                break;
            }
            ladder.push(next);
        }
        let moves: Vec<f64> = ladder.iter().map(|&h| move_over(h)).collect();
        let first = ladder
            .iter()
            .zip(&moves)
            .position(|(_, m)| *m >= 4.0 * cost);
        let short_of_costs = first.is_none();
        let start = first.unwrap_or(ladder.len().saturating_sub(1));
        // About 3× apart (every third rung) from the start; closer together when the
        // data runs out first, and shorter ones only when there aren't 4 above it.
        let room = ladder.len() - start;
        let mut picked: Vec<usize> = if room >= 10 {
            (start..ladder.len()).step_by(3).take(4).collect()
        } else {
            let mut v: Vec<usize> = (0..4).map(|k| start + (room - 1) * k / 3).collect();
            v.dedup();
            v
        };
        let mut down = start;
        while picked.len() < 4 && down > 0 {
            down -= 1;
            picked.insert(0, down);
        }
        let aheads: Vec<usize> = picked.iter().map(|&k| ladder[k]).collect();
        let moves: Vec<f64> = picked.iter().map(|&k| moves[k]).collect();
        let tails = aheads
            .iter()
            .map(|&h| {
                let t = (20.0 * h as f64 / learn.max(1) as f64).clamp(0.02, 0.1);
                [t, (3.0 * t).min(0.3)]
            })
            .collect();
        let h_max = aheads.last().copied().unwrap_or(2);
        let stretches = ((n - learn) / (30 * h_max)).clamp(1, 5);
        let cuts = (0..=stretches)
            .map(|k| learn + (n - learn) * k / stretches)
            .collect();
        Layout {
            learn,
            cuts,
            aheads,
            moves,
            tails,
            cost,
            short_of_costs,
        }
    }

    /// The walk-forward stretches, as (from, to).
    pub fn stretches(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.cuts.windows(2).map(|w| (w[0], w[1]))
    }

    /// `learned on the first half (12,000 points), checked walking forward over 4
    /// stretches · horizons 5s 15s 45s 2m · tails 2–30%`.
    pub fn describe(&self, input: &Input) -> String {
        let hs: Vec<String> = self.aheads.iter().map(|h| input.points_text(*h)).collect();
        let lo = self.tails.iter().map(|t| t[0]).fold(1.0, f64::min);
        let hi = self.tails.iter().map(|t| t[1]).fold(0.0, f64::max);
        format!(
            "learned on the first half ({} points), checked walking forward over {} stretch{} · horizons {}{} · tails {:.0}–{:.0}%",
            self.learn,
            self.cuts.len() - 1,
            if self.cuts.len() == 2 { "" } else { "es" },
            hs.join(" "),
            if self.short_of_costs {
                " (none moves 4× the costs yet)"
            } else {
                ""
            },
            lo * 100.0,
            hi * 100.0
        )
    }
}

/// Sorted finite values of features over [0, to), kept for re-fitting thresholds.
#[derive(Default)]
struct Ranks {
    sorted: std::collections::HashMap<(String, usize), Vec<f64>>,
}

impl Ranks {
    fn of(&mut self, feature: &Expr, v: &[f64], to: usize) -> &[f64] {
        self.sorted
            .entry((feature.to_string(), to))
            .or_insert_with(|| {
                let mut s: Vec<f64> = v[..to.min(v.len())]
                    .iter()
                    .copied()
                    .filter(|x| x.is_finite())
                    .collect();
                s.sort_by(f64::total_cmp);
                s
            })
    }

    /// `c` with its threshold at the same rank among the values before `to` as it had
    /// among those before `learn`.
    fn refit(&mut self, c: &Cond, v: &[f64], learn: usize, to: usize) -> Cond {
        if to == learn {
            return c.clone();
        }
        let base = self.of(&c.feature, v, learn);
        if base.is_empty() {
            return c.clone();
        }
        let q = base.partition_point(|x| *x < c.threshold) as f64 / base.len() as f64;
        let now = self.of(&c.feature, v, to);
        if now.is_empty() {
            return c.clone();
        }
        let at = ((now.len() - 1) as f64 * q).round() as usize;
        Cond {
            threshold: now[at.min(now.len() - 1)],
            ..c.clone()
        }
    }
}

/// How long a search may spend evolving formulas.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub time: std::time::Duration,
    /// At most this many new formulas.
    pub formulas: usize,
    pub seed: u64,
}

/// A formula and how well it predicts (on the learning half only).
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub expr: Expr,
    /// The best |t| of the price's change after its tails, less a little for its size.
    pub fitness: f64,
    /// Not one of the usual indicators: the search made it up.
    pub invented: bool,
}

/// The parts of a search that look at the data.
struct Lab<'a> {
    input: &'a Input,
    learn: usize,
    aheads: Vec<usize>,
    fwd: Vec<Vec<f64>>,
}

impl Lab<'_> {
    /// How strongly the price moves after the formula's tails, on the learning half;
    /// `None` for formulas that can't be used (mostly missing, constant, or drifting so
    /// the rest isn't in the range the learning half had).
    fn fitness(&self, e: &Expr) -> Option<f64> {
        if e.reach() * 4 > self.learn || e.lopsided(self.input) || e.ill_posed(self.input) {
            return None;
        }
        let v = e.eval(self.input);
        // The price in disguise (an average of it, a lagged copy…): its tails are price
        // levels ("below 77,189"), which only fit the past.
        if level_like(&v, &self.input.price, self.learn) {
            return None;
        }
        let finite = v[..self.learn].iter().filter(|x| x.is_finite()).count();
        if finite * 2 < self.learn {
            return None;
        }
        let q = quantiles(&v, 0, self.learn, &[0.1, 0.9])?;
        if q[0] >= q[1] {
            return None;
        }
        // Drifting (like the price itself): its tails would mean something else later.
        let later: Vec<f64> = v[self.learn..]
            .iter()
            .copied()
            .filter(|x| x.is_finite())
            .collect();
        let outside = later.iter().filter(|x| **x < q[0] || **x > q[1]).count();
        if later.is_empty() || outside as f64 > later.len() as f64 * 0.4 {
            return None;
        }
        let mut best = 0.0f64;
        for (k, &h) in self.aheads.iter().enumerate() {
            for (above, th) in [(false, q[0]), (true, q[1])] {
                let on = |i: usize| {
                    let x = v[i];
                    x.is_finite() && if above { x > th } else { x < th }
                };
                let p = Perf::of(&events(
                    on,
                    &self.fwd[k],
                    0,
                    self.learn.saturating_sub(h),
                    h,
                ));
                if p.n >= 12 {
                    best = best.max(p.t.abs());
                }
            }
        }
        Some(best - SIZE_COST * e.size() as f64)
    }
}

/// With profit sharing: targets just past the ROI tiers (keep 90% past 500% ROI, 95%
/// past 1,500%), held up to 10× the horizon, out at liquidation otherwise — wins kept
/// almost whole, losses free of fees.
fn tier_exits(fees: &Fees, h: usize) -> Vec<Exit> {
    let Fees::Share { leverage, .. } = *fees else {
        return Vec::new();
    };
    [500.0, 1500.0]
        .into_iter()
        .map(|roi| Exit {
            hold: 10 * h,
            take: Some(roi / leverage * 100.0 * 1.01),
            stop: None,
        })
        .collect()
}

/// Whether `v` moves with the price's level (correlation above 0.9 over [0, to)).
fn level_like(v: &[f64], price: &[f64], to: usize) -> bool {
    let (mut n, mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let base = price.first().copied().unwrap_or(0.0);
    let vb = v.iter().copied().find(|x| x.is_finite()).unwrap_or(0.0);
    for i in 0..to.min(v.len()) {
        if !v[i].is_finite() {
            continue;
        }
        let (x, y) = (v[i] - vb, price[i] - base);
        n += 1.0;
        sx += x;
        sy += y;
        sxx += x * x;
        syy += y * y;
        sxy += x * y;
    }
    if n < 10.0 {
        return false;
    }
    let cov = sxy / n - sx / n * sy / n;
    let vx = sxx / n - (sx / n).powi(2);
    let vy = syy / n - (sy / n).powi(2);
    vx > 0.0 && vy > 0.0 && (cov / (vx * vy).sqrt()).abs() > 0.9
}

/// The share of the smaller of two sorted lists of points that's in the other (within
/// 3 points: signals a moment apart are the same idea).
fn overlap(a: &[usize], b: &[usize]) -> f64 {
    let small = a.len().min(b.len());
    if small == 0 {
        return 0.0;
    }
    let (mut i, mut j, mut both) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i].abs_diff(b[j]) <= 3 {
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

/// Evolve formulas: start from `pool` (the last search's best), the usual indicators
/// and random ones; then keep making changed and combined copies of the fittest.
fn evolve(lab: &Lab, pool: &[Expr], budget: Budget) -> (Vec<Scored>, usize) {
    let started = std::time::Instant::now();
    let input = lab.input;
    let classics = input.classics();
    let maker = Maker::new(input.columns(), input.len());
    let mut rng = Rng::new(budget.seed);
    let mut seen = std::collections::HashSet::new();
    let mut pop: Vec<Scored> = Vec::new();
    let mut tried = 0;
    let mut add = |e: Expr, pop: &mut Vec<Scored>, tried: &mut usize| {
        // Classics as they are; anything new has to make sense (its parts go together).
        if e.depth() > 9
            || e.size() > 30
            || !(e.sensible() || classics.contains(&e))
            || !seen.insert(e.to_string())
        {
            return;
        }
        *tried += 1;
        if let Some(fitness) = lab.fitness(&e) {
            let invented = !classics.contains(&e);
            pop.push(Scored {
                expr: e,
                fitness,
                invented,
            });
        }
    };
    // Kept formulas only for the series this data still has.
    let cols = input.columns();
    for e in pool
        .iter()
        .chain(&classics)
        .filter(|e| e.columns().iter().all(|c| cols.contains(c)))
    {
        add(e.clone(), &mut pop, &mut tried);
    }
    for _ in 0..POOL {
        add(maker.random(&mut rng), &mut pop, &mut tried);
    }
    // The fittest, but varied: at most 3 of a shape (the same operations with other
    // windows or series) and a sixth of the pool with the same outer operation, so one
    // idea (RSI of RSI…) can't crowd out the rest.
    let by_fitness = |pop: &mut Vec<Scored>| {
        pop.sort_by(|a, b| b.fitness.total_cmp(&a.fitness));
        let mut shapes: std::collections::HashMap<String, usize> = Default::default();
        let mut heads: std::collections::HashMap<String, usize> = Default::default();
        pop.retain(|s| {
            let shape = shapes.entry(s.expr.shape()).or_default();
            let head = heads.entry(s.expr.head()).or_default();
            if *shape >= 3 || *head >= POOL / 6 {
                return false;
            }
            *shape += 1;
            *head += 1;
            true
        });
        pop.truncate(POOL);
    };
    by_fitness(&mut pop);
    while started.elapsed() < budget.time && tried < budget.formulas && !pop.is_empty() {
        let pick = |rng: &mut Rng, pop: &[Scored]| -> Expr {
            // The fitter of three at random.
            (0..3)
                .map(|_| &pop[rng.below(pop.len())])
                .max_by(|a, b| a.fitness.total_cmp(&b.fitness))
                .expect("three")
                .expr
                .clone()
        };
        for _ in 0..40 {
            let a = pick(&mut rng, &pop);
            let child = match rng.below(12) {
                0..=3 => maker.mutate(&a, &mut rng),
                // Built on two that work.
                8 | 9 => {
                    let b = pick(&mut rng, &pop);
                    maker.compose(&a, &b, &mut rng)
                }
                4..=7 => {
                    let b = pick(&mut rng, &pop);
                    let c = maker.cross(&a, &b, &mut rng);
                    if rng.chance(0.3) {
                        maker.mutate(&c, &mut rng)
                    } else {
                        c
                    }
                }
                // Fresh ideas now and then.
                _ => maker.random(&mut rng),
            };
            add(child, &mut pop, &mut tried);
        }
        by_fitness(&mut pop);
    }
    (pop, tried)
}

/// Search `input` for theories and strategies, evolving formulas from `pool` for at
/// most `budget`.
/// What the lab brings to a search: the strategies it's paper-trading (to review), and
/// the ones that failed live (not to be picked again).
#[derive(Debug, Clone, Default)]
pub struct Feedback {
    pub review: Vec<Strategy>,
    pub failed: Vec<String>,
}

pub fn search(
    input: Input,
    fees: Fees,
    pool: &[Expr],
    budget: Budget,
    feedback: &Feedback,
) -> Result<Report, String> {
    let started = std::time::Instant::now();
    let n = input.len();
    if n < MIN_POINTS {
        return Err(format!("{n} of the {MIN_POINTS} points it needs so far"));
    }
    let layout = Layout::of(&input, &fees);
    let learn = layout.learn;
    let aheads = layout.aheads.clone();
    let fwd: Vec<Vec<f64>> = aheads.iter().map(|h| forward(&input.price, *h)).collect();
    let lab = Lab {
        input: &input,
        learn,
        aheads: aheads.clone(),
        fwd,
    };
    let (evolved, formulas) = evolve(&lab, pool, budget);
    let fwd = &lab.fwd;
    // The fittest formulas, and the usual ones (as filters: volatility, trend…).
    let mut features: Vec<Expr> = evolved
        .iter()
        .take(FEATURES)
        .map(|s| s.expr.clone())
        .collect();
    for c in input.classics() {
        if !features.contains(&c) {
            features.push(c);
        }
    }
    let values: Vec<Vec<f64>> = features.iter().map(|f| f.eval(&input)).collect();
    let spread = input.half_spread * 2.0 * 1e4;
    let ranks: std::cell::RefCell<Ranks> = Default::default();

    // Theories: each feature's tails, each horizon (with its own tails).
    let mut theories: Vec<Theory> = Vec::new();
    let mut tests = formulas * 2 * aheads.len();
    for (f, v) in features.iter().zip(&values) {
        for (k, &h) in aheads.iter().enumerate() {
            let [t1, t2] = layout.tails[k];
            let Some(q) = quantiles(v, 0, learn, &[t1, t2, 1.0 - t2, 1.0 - t1]) else {
                continue;
            };
            for (above, tail, threshold) in [
                (false, t1, q[0]),
                (false, t2, q[1]),
                (true, t2, q[2]),
                (true, t1, q[3]),
            ] {
                let cond = Cond {
                    feature: f.clone(),
                    above,
                    threshold,
                };
                tests += 1;
                let on = |i: usize| cond.holds(v[i]);
                let found = Perf::of(&events(on, &fwd[k], 0, learn.saturating_sub(h), h));
                if found.n < 12 {
                    continue;
                }
                // Walking forward: each stretch with the threshold re-fitted on
                // everything before it.
                let mut later = Vec::new();
                for (from, to) in layout.stretches() {
                    let c = ranks.borrow_mut().refit(&cond, v, learn, from);
                    later.extend(events(|i| c.holds(v[i]), &fwd[k], from, to, h));
                }
                theories.push(Theory {
                    cond: cond.clone(),
                    tail,
                    ahead: h,
                    found,
                    checked: Perf::of(&later),
                });
            }
        }
    }
    // The strongest |t| luck alone would give out of this many tests: theories must
    // beat it on the learning half to be listed.
    let bar = (2.0 * (tests.max(2) as f64).ln()).sqrt().max(3.0);

    // Strategies from theories that look like something (on the learning half).
    let index_of = |f: &Expr| features.iter().position(|g| g == f);
    // A strategy's features, computed here if it isn't one of ours (one under review).
    let extra: std::cell::RefCell<Vec<(Expr, Vec<f64>)>> = Default::default();
    let vals_of = |s: &Strategy| -> Vec<Vec<f64>> {
        s.conds()
            .map(|c| match index_of(&c.feature) {
                Some(k) => values[k].clone(),
                None => {
                    let mut ex = extra.borrow_mut();
                    if let Some(v) = ex.iter().find(|e| e.0 == c.feature) {
                        return v.1.clone();
                    }
                    let v = c.feature.eval(&input);
                    ex.push((c.feature.clone(), v.clone()));
                    v
                }
            })
            .collect()
    };
    let trades_of = |s: &Strategy, from: usize, to: usize| -> Vec<Trade> {
        let vals = vals_of(s);
        let refs: Vec<&[f64]> = vals.iter().map(Vec::as_slice).collect();
        s.trades(&input, &refs, from, to, &fees)
    };
    let perf = |t: &[Trade]| -> Perf {
        Perf::of(
            &t.iter()
                .filter(|t| t.exit.is_some())
                .map(|t| t.net)
                .collect::<Vec<_>>(),
        )
    };
    // How much better than holding: each trade's move less what the price's drift over
    // [from, to) gave the same side in the same time. A strategy that only rides the
    // trend (long while it rises) has no edge here, however much it made.
    let edges = |t: &[Trade], from: usize, to: usize| -> Vec<f64> {
        let p = &input.price;
        let last = to.min(n).saturating_sub(1);
        let drift = if last > from {
            (p[last] / p[from] - 1.0) * 1e4 / (last - from) as f64
        } else {
            0.0
        };
        t.iter()
            .filter_map(|t| {
                let held = (t.exit? - t.entry) as f64 * drift;
                Some(match t.side {
                    Side::Long => t.gross - held,
                    Side::Short => t.gross + held,
                })
            })
            .collect()
    };
    let beat = |t: &[Trade], from: usize, to: usize| Perf::of(&edges(t, from, to));
    // A strategy with its thresholds re-fitted on [0, to): at the same ranks.
    let refit = |s: &Strategy, to: usize| -> Strategy {
        let vals = vals_of(s);
        let mut r = ranks.borrow_mut();
        let mut v = vals.iter();
        let mut fit = |cs: &[Cond]| -> Vec<Cond> {
            cs.iter()
                .map(|c| r.refit(c, v.next().expect("a value per condition"), learn, to))
                .collect()
        };
        let long = fit(&s.long);
        let short = fit(&s.short);
        let except = fit(&s.except);
        Strategy {
            long,
            short,
            except,
            exit: s.exit,
        }
    };
    // Walking forward: each stretch traded with the thresholds re-fitted on everything
    // before it. All of it, and each stretch.
    // Also its edge over holding, each stretch against its own drift.
    let walk = |s: &Strategy| -> (Perf, Vec<Perf>, Perf) {
        let mut all = Vec::new();
        let mut each = Vec::new();
        let mut over = Vec::new();
        for (from, to) in layout.stretches() {
            let t = trades_of(&refit(s, from), from, to);
            each.push(perf(&t));
            over.extend(edges(&t, from, to));
            all.extend(t);
        }
        (perf(&all), each, Perf::of(&over))
    };
    let mut seeds: Vec<&Theory> = theories.iter().filter(|t| t.found.t.abs() >= 2.0).collect();
    seeds.sort_by(|a, b| b.found.t.abs().total_cmp(&a.found.t.abs()));
    seeds.truncate(120);
    let mut tried = 0;
    let mut candidates: Vec<Found> = Vec::new();
    // Exits sized from the typical move over the horizon.
    let exits = |typical: f64, h: usize| {
        let typical = typical.max(0.01);
        [
            Exit {
                hold: h,
                take: None,
                stop: None,
            },
            Exit {
                hold: 2 * h,
                take: Some(typical),
                stop: Some(typical),
            },
            Exit {
                hold: 3 * h,
                take: Some(2.0 * typical),
                stop: Some(typical),
            },
            Exit {
                hold: h,
                take: Some(0.5 * typical),
                stop: Some(typical),
            },
            // Small losses, big wins (when wins are what's charged for).
            Exit {
                hold: 4 * h,
                take: Some(3.0 * typical),
                stop: Some(0.5 * typical),
            },
        ]
        .into_iter()
        .chain(tier_exits(&fees, h))
        .collect::<Vec<_>>()
    };
    let mut best_exit = |make: &dyn Fn(Exit) -> Strategy, typical: f64, h: usize| {
        let mut best: Option<Found> = None;
        for exit in exits(typical, h) {
            let s = make(exit);
            tried += 1;
            let t = trades_of(&s, 0, learn);
            let picked = perf(&t);
            // It has to make money after costs where it was picked, and not by a fluke…
            if picked.n < MIN_TRADES || picked.t < MIN_T {
                continue;
            }
            // …and do better than holding the same way for as long: not just ride the
            // trend.
            let edge = beat(&t, 0, learn);
            if edge.mean <= 0.0 || edge.t < MIN_EDGE_T {
                continue;
            }
            let score = picked.t.min(edge.t);
            if best
                .as_ref()
                .is_none_or(|b| score > b.picked.t.min(b.picked_edge.t))
            {
                best = Some(Found {
                    strategy: s,
                    picked,
                    picked_edge: edge,
                    ..Found::default()
                });
            }
        }
        best
    };
    for th in &seeds {
        let side = if th.found.mean > 0.0 {
            Side::Long
        } else {
            Side::Short
        };
        // One way: in the theory's direction.
        let one = best_exit(
            &|exit| Strategy::one_way(side, vec![th.cond.clone()], exit),
            th.found.sd,
            th.ahead,
        );
        // Both ways: the same formula's other extreme, when the price moves the other
        // way after it (momentum or reversion either way).
        let mirror = theories.iter().find(|m| {
            m.cond.feature == th.cond.feature
                && m.cond.above != th.cond.above
                && m.tail == th.tail
                && m.ahead == th.ahead
                && m.found.mean.signum() == -th.found.mean.signum()
                && m.found.t.abs() >= 1.5
        });
        let both = mirror.and_then(|m| {
            let (up, down): (&Theory, &Theory) = if side == Side::Long { (th, m) } else { (m, th) };
            best_exit(
                &|exit| Strategy {
                    long: vec![up.cond.clone()],
                    short: vec![down.cond.clone()],
                    except: Vec::new(),
                    exit,
                },
                (th.found.sd + m.found.sd) / 2.0,
                th.ahead,
            )
        });
        for b in [one, both].into_iter().flatten() {
            if !candidates.iter().any(|c| c.strategy == b.strategy) {
                candidates.push(b);
            }
        }
    }
    // What the best of them lose in: the condition that tells their losing trades from
    // their winners on the learning half (anything the formulas measure), left out.
    candidates.sort_by(|a, b| b.picked.t.total_cmp(&a.picked.t));
    let mut lessons = Vec::new();
    for base in candidates.iter().take(15) {
        tried += 1;
        let t = trades_of(&base.strategy, 0, learn);
        let base_edge = beat(&t, 0, learn);
        let own: Vec<&Expr> = base.strategy.conds().map(|c| &c.feature).collect();
        if let Some(l) = adverse(&t, &features, &values, &own, 0.75) {
            let mut s = base.strategy.clone();
            s.except.push(l.cond.clone());
            let t = trades_of(&s, 0, learn);
            let picked = perf(&t);
            let edge = beat(&t, 0, learn);
            if picked.n >= MIN_TRADES && picked.t > base.picked.t && edge.mean >= base_edge.mean {
                lessons.push(Found {
                    strategy: s,
                    picked,
                    picked_edge: edge,
                    ..Found::default()
                });
            }
        }
    }
    candidates.extend(lessons);
    // Not the ones that already failed live.
    candidates.retain(|c| !feedback.failed.contains(&c.strategy.key()));
    // Check the most promising walking forward, and rank by that too.
    candidates.sort_by(|a, b| b.picked.t.total_cmp(&a.picked.t));
    candidates.truncate(40);
    for c in &mut candidates {
        (c.checked, c.steps, c.edge) = walk(&c.strategy);
    }
    let rank = |c: &Found| {
        if c.checked.n == 0 {
            return f64::MIN;
        }
        // The weakest of: where it was picked, walking forward, and against holding.
        let base = c.checked.t.min(c.picked.t).min(c.edge.t);
        // Up in fewer than half the stretches it traded in: one lucky stretch.
        if c.consistency()
            .is_some_and(|(up, of)| of >= 2 && up * 2 < of)
        {
            base - 1.0
        } else {
            base
        }
    };
    candidates.sort_by(|a, b| rank(b).total_cmp(&rank(a)));
    // One per idea: the same entries with other exits, or other formulas that enter at
    // the same moments, are left out.
    let entries =
        |s: &Strategy| -> Vec<usize> { trades_of(s, 0, n).iter().map(|t| t.entry).collect() };
    let mut strategies: Vec<(Found, Vec<usize>)> = Vec::new();
    for c in candidates {
        if strategies.len() >= 12 {
            break;
        }
        if strategies
            .iter()
            .any(|s| s.0.strategy.key() == c.strategy.key())
        {
            continue;
        }
        let at = entries(&c.strategy);
        if strategies.iter().any(|s| overlap(&s.1, &at) > 0.7) {
            continue;
        }
        strategies.push((c, at));
    }
    // What's paper-traded from now on: the thresholds re-fitted on all of the data.
    let strategies: Vec<Found> = strategies
        .into_iter()
        .map(|(mut f, _)| {
            f.strategy = refit(&f.strategy, n);
            f
        })
        .collect();

    // Reviews of what's being paper-traded: after the learning half (where it wasn't picked
    // — and for an older one, also the newest data), why it isn't paying, and a fix
    // when there's a condition it loses in.
    let reviews: Vec<Review> = feedback
        .review
        .iter()
        .map(|s| {
            let all = trades_of(s, 0, n);
            let later: Vec<Trade> = all.iter().filter(|t| t.entry >= learn).copied().collect();
            let closed: Vec<&Trade> = later.iter().filter(|t| t.exit.is_some()).collect();
            let gross = Perf::of(&closed.iter().map(|t| t.gross).collect::<Vec<_>>());
            let net = perf(&later);
            let before = perf(&all.iter().filter(|t| t.entry < learn).copied().collect::<Vec<_>>());
            let edge = beat(&later, learn, n);
            let own: Vec<&Expr> = s.conds().map(|c| &c.feature).collect();
            // A lesson from every trade it made, so there are enough to learn from.
            let lesson = adverse(&all, &features, &values, &own, 0.5);
            let fix = lesson.as_ref().and_then(|l| {
                let mut f = s.clone();
                f.except.push(l.cond.clone());
                let t = trades_of(&f, 0, learn);
                let (checked, steps, edge) = walk(&f);
                (checked.n >= 3 && checked.mean > net.mean && edge.mean > 0.0).then_some(
                    Found {
                        strategy: f,
                        picked: perf(&t),
                        picked_edge: beat(&t, 0, learn),
                        checked,
                        steps,
                        edge,
                    },
                )
            });
            let why = if closed.len() < 5 {
                "too few trades on the newest data to tell yet".to_string()
            } else if net.mean > 0.0 && net.t >= 1.0 {
                format!(
                    "working: {:+.2}bp a trade on the newest data, {:+.2}bp before costs",
                    net.mean, gross.mean
                )
            } else if gross.mean > 0.0 && edge.mean <= 0.0 {
                format!(
                    "it only rode the trend: {:+.2}bp a trade before costs, but holding the same way as long did {:+.2}bp better",
                    gross.mean, -edge.mean
                )
            } else if gross.mean > 0.0 {
                format!(
                    "right about the direction ({:+.2}bp a trade before costs) but costs take {:.2}bp of it",
                    gross.mean,
                    gross.mean - net.mean
                )
            } else if before.mean > 0.0 {
                format!(
                    "the edge faded: {:+.2}bp a trade on the older data, {:+.2}bp before costs on the newest — it fitted the past",
                    before.mean, gross.mean
                )
            } else {
                format!("it doesn't predict here: {:+.2}bp a trade before costs", gross.mean)
            };
            let why = match &lesson {
                Some(l) => format!(
                    "{why}; it loses when {} ({} trades, {:+.2}bp each; the rest {:+.2}bp)",
                    l.cond.describe(),
                    l.removed.n,
                    l.removed.mean,
                    l.kept.mean
                ),
                None => why,
            };
            Review {
                key: s.key(),
                why,
                fix,
            }
        })
        .collect();

    // The theories to show: strong on the learning half, those that held first.
    let mut shown: Vec<Theory> = theories
        .into_iter()
        .filter(|t| t.found.t.abs() >= bar)
        .collect();
    shown.sort_by(|a, b| {
        b.held()
            .cmp(&a.held())
            .then(b.found.t.abs().total_cmp(&a.found.t.abs()))
    });
    // One per idea: not the same formula's side twice, nor two that pick the same
    // moments (`slope(price, 8)` and `gap(slope(price, 8), price)`).
    let fires = |c: &Cond| -> Vec<usize> {
        let v = &values[index_of(&c.feature).expect("a theory's own feature")];
        (0..learn).filter(|&i| c.holds(v[i])).collect()
    };
    let mut theories_out: Vec<(Theory, Vec<usize>)> = Vec::new();
    for t in shown {
        if theories_out.len() >= 12 {
            break;
        }
        if theories_out
            .iter()
            .any(|o| o.0.cond.feature == t.cond.feature && o.0.cond.above == t.cond.above)
        {
            continue;
        }
        let at = fires(&t.cond);
        if theories_out.iter().any(|o| overlap(&o.1, &at) > 0.7) {
            continue;
        }
        theories_out.push((t, at));
    }
    let theories_out: Vec<Theory> = theories_out.into_iter().map(|t| t.0).collect();

    let hold = |a: usize, b: usize| (input.price[b] / input.price[a] - 1.0) * 1e4;
    Ok(Report {
        hold: [hold(0, learn), hold(learn, n - 1)],
        layout,
        formulas,
        tests,
        tried,
        bar,
        spread,
        fees,
        theories: theories_out,
        strategies,
        reviews,
        pool: evolved,
        took: started.elapsed(),
        input,
    })
}

/// A strategy's trades over `input`, entering only after point `from`.
pub fn trade(s: &Strategy, input: &Input, from: usize, fees: &Fees) -> Vec<Trade> {
    let values: Vec<Vec<f64>> = s.conds().map(|c| c.feature.eval(input)).collect();
    let refs: Vec<&[f64]> = values.iter().map(Vec::as_slice).collect();
    s.trades(input, &refs, from, input.len(), fees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Series, XKind};
    use crate::units::Unit;

    /// A condition that always holds (with values of 1).
    fn always() -> Cond {
        Cond {
            feature: Expr::Num(1.0),
            above: true,
            threshold: 0.0,
        }
    }

    fn budget() -> Budget {
        Budget {
            time: std::time::Duration::from_secs(30),
            formulas: 1500,
            seed: 1,
        }
    }

    fn input(price: Vec<f64>) -> Input {
        Input {
            name: "price".into(),
            x: (0..price.len()).map(|i| i as f64).collect(),
            price,
            book: None,
            half_spread: 0.0,
            others: Vec::new(),
            time: false,
        }
    }

    /// A random walk where every sharp drop (over 5 points) is followed by a bounce.
    fn bouncy(n: usize) -> Vec<f64> {
        let mut seed = 7u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 10_000) as f64 / 10_000.0 - 0.5
        };
        let mut p = vec![100.0];
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
        p
    }

    #[test]
    fn indicators_wait_for_their_window() {
        let v = [1.0, 2.0, 3.0, 2.0, 1.0];
        let c = crate::lines::change(&v, 2);
        assert!(c[1].is_nan());
        assert!((c[2] - 20000.0).abs() < 1e-9);
        let pos = crate::lines::range_pos(&v, 3);
        assert_eq!(pos[2], 100.0);
        assert_eq!(pos[4], 0.0);
        let up = crate::lines::updown(&v, 2);
        assert_eq!(&up[2..], [100.0, 50.0, 0.0]);
        let z = crate::lines::zscore(&[5.0, 5.0, 5.0, 8.0], 3);
        assert_eq!(z[2], 0.0);
        assert!(z[3] > 1.0);
        assert!(crate::lines::volatility(&v, 2)[2].is_finite());
    }

    #[test]
    fn costs_come_off_every_trade() {
        let inp = input(vec![100.0; 50]);
        let s = Strategy::one_way(
            Side::Long,
            vec![always()],
            Exit {
                hold: 5,
                take: None,
                stop: None,
            },
        );
        let t = s.trades(&inp, &[&vec![1.0; inp.len()]], 0, 50, &Fees::Flat(0.045));
        assert!(t.iter().all(|t| (t.net + 9.0).abs() < 1e-9), "{t:?}");
        // Back to back: the next starts after the last ends.
        assert_eq!(t[1].entry, t[0].exit.unwrap() + 1);
    }

    #[test]
    fn profit_sharing_takes_nothing_from_losses_and_less_from_big_wins() {
        let f = Fees::parse("upside:250").unwrap();
        assert_eq!(
            f,
            Fees::Share {
                leverage: 250.0,
                staked: false
            }
        );
        assert_eq!(Fees::parse("0.045").unwrap(), Fees::Flat(0.045));
        assert!(Fees::parse("upside").is_err());
        // A loss is just the loss.
        assert_eq!(f.net(-5.0), -5.0);
        // +2% at 250x is 500% ROI: you keep 80% (the docs' example); at 50x, 100%: 75%.
        assert!((f.net(200.0) - 160.0).abs() < 1e-9);
        let f50 = Fees::parse("upside:50x").unwrap();
        assert!((f50.net(200.0) - 150.0).abs() < 1e-9);
        // Staked: the 25% share is a quarter smaller.
        let staked = Fees::parse("upside:250:staked").unwrap();
        assert!((staked.net(10.0) - 10.0 * (1.0 - 0.25 * 0.75)).abs() < 1e-9);
        // Liquidated at -85% ROI: a 34bp move at 250x; never more than the margin.
        assert!((f.liquidation().unwrap() - 34.0).abs() < 1e-9);
        assert_eq!(f.net(-900.0), -40.0);
        // A trade that falls through the liquidation level is closed there.
        let mut p = vec![100.0; 10];
        p[3] = 99.5; // -50bp
        let inp = input(p);
        let s = Strategy::one_way(
            Side::Long,
            vec![always()],
            Exit {
                hold: 8,
                take: None,
                stop: None,
            },
        );
        let t = s.trades(&inp, &[&vec![1.0; inp.len()]], 0, 1, &f);
        assert_eq!(t[0].exit, Some(3));
        assert!((t[0].net + 40.0).abs() < 1e-9, "{t:?}");
    }

    /// `GRAFT_PRICES=file` (lines: time price) — what it finds on real prices, with
    /// each fee model. `cargo test --release real_prices -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs GRAFT_PRICES"]
    fn real_prices() {
        let path = std::env::var("GRAFT_PRICES").expect("GRAFT_PRICES");
        let text = std::fs::read_to_string(path).unwrap();
        let (x, price): (Vec<f64>, Vec<f64>) = text
            .lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                Some((
                    w.next()?.parse::<f64>().ok()? / 1000.0,
                    w.next()?.parse::<f64>().ok()?,
                ))
            })
            .unzip();
        let spread: f64 = std::env::var("GRAFT_SPREAD_BP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.6);
        for fees in std::env::var("GRAFT_FEES")
            .unwrap_or("0,0.045,upside:50,upside:250,upside:500".into())
            .split(',')
        {
            let fees = Fees::parse(fees).unwrap();
            let inp = Input {
                name: "price".into(),
                x: x.clone(),
                price: price.clone(),
                book: None,
                half_spread: spread / 2.0 / 1e4,
                others: Vec::new(),
                time: true,
            };
            let mut b = budget();
            b.formulas = 6000;
            let r = search(inp, fees, &[], b, &Feedback::default()).unwrap();
            eprintln!(
                "\n=== {} · {} pts · {} formulas · bar {:.1} · hold {:+.1}bp / {:+.1}bp · {:?}",
                fees.short(),
                r.input.len(),
                r.formulas,
                r.bar,
                r.hold[0],
                r.hold[1],
                r.took
            );
            eprintln!(
                "{} · moves {:?} · cost {:.2}bp",
                r.layout.describe(&r.input),
                r.layout.moves,
                r.layout.cost
            );
            for s in r.strategies.iter().take(5) {
                eprintln!(
                    "  {} · {} → {} · beats holding {}",
                    s.strategy.describe(&r.input),
                    s.picked.brief(),
                    s.checked.brief(),
                    s.edge.brief()
                );
            }
            if fees == Fees::Flat(0.0) {
                for t in r.theories.iter().take(8) {
                    eprintln!("  T {}", t.describe(&r.input));
                }
            }
        }
    }

    #[test]
    fn history_without_a_book_pays_the_usual_spread() {
        // Prices for 400 points; bid and ask only for the last 40 (the live part).
        let s = |name: &str, from: usize, v: f64| Series {
            name: name.into(),
            unit: Unit::default(),
            points: (from..400).map(|i| (i as f64, v)).collect(),
        };
        let ds = Dataset {
            series: vec![
                s("price", 0, 100.0),
                s("bid", 360, 99.99),
                s("ask", 360, 100.01),
            ],
            x_kind: XKind::Number,
        };
        let inp = Input::from_dataset(&ds, &Settings::default()).unwrap();
        assert!(inp.book.is_some(), "kept, though it covers a tenth");
        assert!((inp.buy_at(0) - 100.01).abs() < 1e-4, "{}", inp.buy_at(0));
        assert!((inp.sell_at(0) - 99.99).abs() < 1e-4);
        assert_eq!(inp.buy_at(399), 100.01);
    }

    #[test]
    fn buys_at_the_ask_and_sells_at_the_bid() {
        let mut inp = input(vec![100.0; 20]);
        inp.book = Some((
            "bid".into(),
            vec![99.99; 20],
            "ask".into(),
            vec![100.01; 20],
        ));
        let s = Strategy::one_way(
            Side::Short,
            vec![always()],
            Exit {
                hold: 3,
                take: None,
                stop: None,
            },
        );
        let t = s.trades(&inp, &[&vec![1.0; inp.len()]], 0, 20, &Fees::Flat(0.0));
        // Sold at 99.99, bought back at 100.01: about 2bp lost.
        assert!((t[0].net + 2.0).abs() < 0.01, "{:?}", t[0]);
    }

    #[test]
    fn finds_a_planted_edge_and_checks_it() {
        let report = search(
            input(bouncy(12_000)),
            Fees::Flat(0.0),
            &[],
            budget(),
            &Feedback::default(),
        )
        .unwrap();
        let best = &report.strategies[0];
        for s in &report.strategies {
            eprintln!(
                "{} · {} → {}",
                s.strategy.describe(&report.input),
                s.picked.brief(),
                s.checked.brief()
            );
        }
        for t in &report.theories {
            eprintln!("{}", t.describe(&report.input));
        }
        for f in report.pool.iter().take(10) {
            eprintln!(
                "{:.2} {} {}",
                f.fitness,
                if f.invented { "new" } else { "   " },
                f.expr
            );
        }
        eprintln!("{} formulas, {:?}", report.formulas, report.took);
        assert!(!best.strategy.long.is_empty(), "{:?}", best.strategy);
        assert!(best.checked.mean > 0.0 && best.checked.t > 2.0, "{best:?}");
        assert!(report.theories.iter().any(Theory::held));
    }

    #[test]
    fn the_layout_comes_from_the_data() {
        let p = bouncy(12_000);
        let free = Layout::of(&input(p.clone()), &Fees::Flat(0.0));
        assert_eq!(free.learn, 6_000);
        assert_eq!(free.aheads[0], 2, "nothing to pay: from the shortest");
        assert!(
            free.aheads.windows(2).all(|w| w[1] >= w[0] * 2),
            "{:?}",
            free.aheads
        );
        assert!(*free.aheads.last().unwrap() <= 6_000 / 60);
        assert!(free.cuts.len() > 2, "several stretches: {:?}", free.cuts);
        assert_eq!(*free.cuts.last().unwrap(), 12_000);
        // Paying more: longer horizons, where the typical move covers it.
        let dear = Layout::of(&input(p.clone()), &Fees::Flat(0.01));
        let sum = |l: &Layout| l.aheads.iter().sum::<usize>();
        assert!(
            sum(&dear) > sum(&free),
            "{:?} {:?}",
            dear.aheads,
            free.aheads
        );
        assert!(
            dear.aheads
                .iter()
                .zip(&dear.moves)
                .any(|(h, m)| *h > 2 && *m >= 4.0 * dear.cost),
            "{:?} {:?} {}",
            dear.aheads,
            dear.moves,
            dear.cost
        );
        // Longer horizons get wider tails (enough events), within 2–30%.
        assert!(free.tails.windows(2).all(|w| w[1][0] >= w[0][0]));
        assert!(free.tails.iter().all(|t| t[0] >= 0.02 && t[1] <= 0.3));
        // Less data: fewer stretches, shorter horizons.
        let small = Layout::of(&input(p[..600].to_vec()), &Fees::Flat(0.0));
        assert!(small.cuts.len() <= free.cuts.len());
        assert!(small.aheads.last() <= free.aheads.last());
        // Way too dear: says so.
        let silly = Layout::of(&input(p), &Fees::Flat(5.0));
        assert!(silly.short_of_costs);
        assert!(silly.describe(&input(bouncy(600))).contains("4× the costs"));
    }

    #[test]
    fn thresholds_are_refitted_at_the_same_rank() {
        // A feature that drifts up: its lowest 10% so far moves up with it.
        let v: Vec<f64> = (0..1000).map(|i| i as f64).collect();
        let f = Expr::Col("price".into());
        let c = Cond {
            feature: f,
            above: false,
            threshold: 50.0,
        };
        let mut r = Ranks::default();
        let same = r.refit(&c, &v, 500, 500);
        assert_eq!(same.threshold, 50.0);
        let later = r.refit(&c, &v, 500, 1000);
        assert!(
            (later.threshold - 100.0).abs() <= 1.0,
            "{}",
            later.threshold
        );
    }

    #[test]
    fn records_say_what_they_risked() {
        // +3, −2, −4 (down 6 from the high of 3), +5.
        let p = Perf::of(&[3.0, -2.0, -4.0, 5.0]);
        assert_eq!((p.drawdown, p.worst, p.best), (6.0, -4.0, 5.0));
        assert_eq!(
            p.risk(&Fees::Flat(0.0)),
            "max DD 6.0bp · worst -4.0bp · best +5.0bp"
        );
        // At 250x: 6bp is 15% of the margin.
        let up = Fees::parse("upside:250").unwrap();
        assert_eq!(
            p.risk(&up),
            "max DD 6.0bp (15% of margin) · worst -4.0bp (-10%) · best +5.0bp (+12%)"
        );
        assert_eq!(Perf::of(&[]).risk(&up), "no trades");
    }

    #[test]
    fn riding_the_trend_is_no_edge() {
        // A steady rise with noise and nothing else: long-and-hold makes money, but no
        // formula can time it better than holding.
        let mut seed = 11u64;
        let mut p = vec![100.0];
        for i in 1..8000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let r = ((seed >> 33) % 1000) as f64 / 1000.0 - 0.5;
            p.push(p[i - 1] * (1.0 + 0.00005 + r * 0.0004));
        }
        let report = search(
            input(p),
            Fees::Flat(0.0),
            &[],
            budget(),
            &Feedback::default(),
        )
        .unwrap();
        assert!(report.hold[1] > 50.0, "it rises: {:?}", report.hold);
        for s in &report.strategies {
            eprintln!(
                "{} · {} → {} · edge {}",
                s.strategy.describe(&report.input),
                s.picked.brief(),
                s.checked.brief(),
                s.edge.brief()
            );
            // Nothing that does worse than holding is kept (without this check: 100%
            // won, +26bp a trade, t 489 — and 1bp a trade worse than just holding)…
            assert!(s.picked_edge.mean > 0.0, "{s:?}");
            // …and nothing beats it where it wasn't picked by more than luck.
            assert!(s.edge.t < 2.0, "{s:?}");
        }
    }

    #[test]
    fn noise_has_no_edge_after_costs() {
        let mut seed = 3u64;
        let mut p = vec![100.0];
        for i in 1..8000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let r = ((seed >> 33) % 1000) as f64 / 1000.0 - 0.5;
            p.push(p[i - 1] * (1.0 + r * 0.0004));
        }
        let report = search(
            input(p),
            Fees::Flat(0.045),
            &[],
            budget(),
            &Feedback::default(),
        )
        .unwrap();
        // Paying 9bp a round trip on noise: nothing makes money where it was picked.
        assert!(report.strategies.is_empty(), "{:?}", report.strategies);
        // Thousands of formulas tested on noise: the bar for a theory is that high.
        assert!(report.bar > 4.0, "{}", report.bar);
        assert!(
            report.theories.iter().filter(|t| t.held()).count() <= 1,
            "{:?}",
            report.theories
        );
    }

    #[test]
    fn reads_bid_ask_and_signals_from_a_dataset() {
        let pts = |f: &dyn Fn(f64) -> f64| -> Vec<(f64, f64)> {
            (0..400).map(|i| (i as f64, f(i as f64))).collect()
        };
        let s = |name: &str, points| Series {
            name: name.into(),
            unit: Unit::default(),
            points,
        };
        let ds = Dataset {
            series: vec![
                s("feed.bestBidPrice", pts(&|i| 99.0 + (i * 0.1).sin())),
                s("price", pts(&|i| 100.0 + (i * 0.1).sin())),
                s("feed.bestAskPrice", pts(&|i| 101.0 + (i * 0.1).sin())),
                s("confidence", pts(&|i| 5.0 + (i * 0.37).cos())),
                s("timestampUs", pts(&|i| 1.7e15 + i)),
            ],
            x_kind: XKind::Number,
        };
        let inp = Input::from_dataset(&ds, &Settings::default()).unwrap();
        assert_eq!(inp.name, "price");
        assert!(inp.book.is_some());
        let names: Vec<&str> = inp.others.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["confidence"], "the clock is left out");
        let spread = crate::formula::parse("gap(feed.bestAskPrice, feed.bestBidPrice)").unwrap();
        assert!(inp.classics().contains(&spread));
    }
}
