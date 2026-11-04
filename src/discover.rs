//! Strategy discovery: hunts for short-term edges in a price series, as it streams in.
//!
//! Nothing here knows about any market or any indicator in particular. It takes a
//! price series (and, if the chart has them, bid and ask series, plus any other series
//! as possible signals), and:
//!
//! 1. **invents formulas** (`formula.rs`): starting from the usual indicators and
//!    random formulas over the series, it keeps changing and combining the fittest. A
//!    formula is fit when the price moves one way after its extremes (its lowest or
//!    highest 10%), judged only on the first 70% of the data. The best formulas carry
//!    over to the next search, so it keeps improving as data streams in;
//! 2. tests **theories**: when a formula is in a tail of its range (lowest or highest
//!    5% / 15%), what does the price do over the next h points? Each test uses events
//!    at least h apart (so the t-statistic isn't inflated by overlap). Found on the
//!    first 70%, checked on the last 30%;
//! 3. turns the promising theories into **strategies** (enter in the theory's
//!    direction; exit after h points, or at a take-profit / stop-loss sized from the
//!    typical move), adds one filter from another formula where it helps, and trades
//!    them with costs: the fee on each side, buying at the ask and selling at the bid
//!    when there are bid and ask series. Picked on the first 70%, ranked by the last 30%.
//!
//! The viewer reruns it as data arrives and paper-trades what it found from then on:
//! the live results are the only ones the search never saw.

use crate::data::Dataset;
use crate::formula::{Expr, Maker, Rng};

/// The share of the data strategies are picked on (the rest checks them).
pub const TRAIN: f64 = 0.7;
/// Fewer points than this and there's nothing to learn from yet.
pub const MIN_POINTS: usize = 300;
/// Trades a strategy needs on the first 70% to be considered…
const MIN_TRADES: usize = 10;
/// …and the t-statistic of its returns after costs there.
const MIN_T: f64 = 1.5;

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
    /// Only the newest this many points are searched.
    pub max_points: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            fees: Fees::Flat(0.0),
            series: None,
            max_points: 20_000,
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
            if o.name == price.name || o.points.len() < 2 {
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
        }
    }

    /// `+1.20bp × 23 (t 2.1, 61% won)`.
    pub fn brief(&self) -> String {
        if self.n == 0 {
            return "no trades".into();
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
    /// The price's change that many points later (bp), on the first 70%…
    pub found: Perf,
    /// …and on the last 30%.
    pub checked: Perf,
}

impl Theory {
    /// Whether the last 30% agreed (the same direction, t of 2 or more).
    pub fn held(&self) -> bool {
        self.checked.n >= 5
            && self.checked.mean.signum() == self.found.mean.signum()
            && self.checked.t.abs() >= 2.0
    }

    pub fn describe(&self, input: &Input) -> String {
        let side = if self.cond.above { "top" } else { "bottom" };
        format!(
            "{} ({} {:.0}%) → {} later {:+.2}bp (t {:.1}, {} times) · last 30%: {:+.2}bp (t {:.1}, {}) {}",
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Exit {
    pub hold: usize,
    pub take: Option<f64>,
    pub stop: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Strategy {
    pub side: Side,
    /// All must hold to enter.
    pub conds: Vec<Cond>,
    pub exit: Exit,
}

/// A trade a strategy made: in at point `entry`, out at `exit` (`None`: still open, at
/// the newest point), and its return after costs (bp).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trade {
    pub entry: usize,
    pub exit: Option<usize>,
    pub net: f64,
}

impl Strategy {
    /// `▲ long when chg15 < -3.2bp and vol50 > 1.1bp · out after 15 points, TP 4.0bp,
    /// SL 3.0bp`.
    pub fn describe(&self, input: &Input) -> String {
        let when = self
            .conds
            .iter()
            .map(Cond::describe)
            .collect::<Vec<_>>()
            .join(" and ");
        let mut out = format!("out after {}", input.points_text(self.exit.hold));
        if let Some(t) = self.exit.take {
            out += &format!(", TP {t:.1}bp");
        }
        if let Some(s) = self.exit.stop {
            out += &format!(", SL {s:.1}bp");
        }
        format!("{} when {when} · {out}", self.side_text())
    }

    pub fn side_text(&self) -> &'static str {
        match self.side {
            Side::Long => "▲ long",
            Side::Short => "▼ short",
        }
    }

    /// The same strategy, whatever its exits: for spotting repeats.
    pub fn key(&self) -> String {
        let mut c: Vec<String> = self
            .conds
            .iter()
            .map(|c| format!("{}{}", c.feature, if c.above { ">" } else { "<" }))
            .collect();
        c.sort();
        format!("{:?} {}", self.side, c.join(" "))
    }

    /// Trades entered in [from, to): one at a time, each ending at its exit (or still
    /// open at the end of the data). `values[k]` are the values of `conds[k]`'s feature.
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
        let long = self.side == Side::Long;
        let mut out = Vec::new();
        let mut i = from;
        while i < to.min(n) {
            let enter = self.conds.iter().zip(values).all(|(c, v)| c.holds(v[i]));
            if !enter {
                i += 1;
                continue;
            }
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
                        net: fees.net(r),
                    });
                    i = j + 1;
                }
                None => {
                    // Still open at the newest point.
                    if i + 1 < n {
                        out.push(Trade {
                            entry: i,
                            exit: None,
                            net: fees.net(ret(n - 1)),
                        });
                    }
                    break;
                }
            }
        }
        out
    }
}

/// A strategy and how it did.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub strategy: Strategy,
    /// On the first 70% (where it was picked)…
    pub picked: Perf,
    /// …and on the last 30%.
    pub checked: Perf,
}

/// What a search found.
#[derive(Debug, Clone)]
pub struct Report {
    pub input: Input,
    /// Formulas tried, theories and strategies tested.
    pub formulas: usize,
    pub tests: usize,
    pub tried: usize,
    /// The |t| theories needed on the first 70% to be listed: the strongest that luck
    /// alone would give out of that many tests.
    pub bar: f64,
    /// The average spread (bp), where there are bid and ask series.
    pub spread: f64,
    /// The costs it traded with.
    pub fees: Fees,
    /// Holding over the first 70% and the last 30% (bp).
    pub hold: [f64; 2],
    /// The best theories (held ones first).
    pub theories: Vec<Theory>,
    /// The best strategies, ranked by the last 30%.
    pub strategies: Vec<Found>,
    /// The fittest formulas (to start the next search from).
    pub pool: Vec<Scored>,
    pub took: std::time::Duration,
}

/// Formulas kept from one search to the next.
const POOL: usize = 150;
/// The best of them, tried as entries and filters for strategies.
const FEATURES: usize = 40;
const TAILS: [f64; 2] = [0.05, 0.15];

/// How long a search may spend evolving formulas.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub time: std::time::Duration,
    /// At most this many new formulas.
    pub formulas: usize,
    pub seed: u64,
}

/// A formula and how well it predicts (on the first 70% only).
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
    split: usize,
    aheads: Vec<usize>,
    fwd: Vec<Vec<f64>>,
}

impl Lab<'_> {
    /// How strongly the price moves after the formula's tails, on the first 70%; `None`
    /// for formulas that can't be used (mostly missing, constant, or drifting so the
    /// last 30% isn't in the range the first 70% had).
    fn fitness(&self, e: &Expr) -> Option<f64> {
        if e.reach() * 4 > self.split || e.lopsided(self.input) {
            return None;
        }
        let v = e.eval(self.input);
        let finite = v[..self.split].iter().filter(|x| x.is_finite()).count();
        if finite * 2 < self.split {
            return None;
        }
        let q = quantiles(&v, 0, self.split, &[0.1, 0.9])?;
        if q[0] >= q[1] {
            return None;
        }
        // Drifting (like the price itself): its tails would mean something else later.
        let later: Vec<f64> = v[self.split..]
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
                    self.split.saturating_sub(h),
                    h,
                ));
                if p.n >= 12 {
                    best = best.max(p.t.abs());
                }
            }
        }
        Some(best - 0.12 * e.size() as f64)
    }
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
        if e.depth() > 6 || e.size() > 16 || !seen.insert(e.to_string()) {
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
    let by_fitness = |pop: &mut Vec<Scored>| {
        pop.sort_by(|a, b| b.fitness.total_cmp(&a.fitness));
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
            let child = match rng.below(10) {
                0..=3 => maker.mutate(&a, &mut rng),
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
pub fn search(input: Input, fees: Fees, pool: &[Expr], budget: Budget) -> Result<Report, String> {
    let started = std::time::Instant::now();
    let n = input.len();
    if n < MIN_POINTS {
        return Err(format!("{n} of the {MIN_POINTS} points it needs so far"));
    }
    let split = (n as f64 * TRAIN) as usize;
    let aheads: Vec<usize> = [5, 15, 50, 150]
        .into_iter()
        .filter(|h| h * 8 <= split)
        .collect();
    let fwd: Vec<Vec<f64>> = aheads.iter().map(|h| forward(&input.price, *h)).collect();
    let lab = Lab {
        input: &input,
        split,
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

    // Theories: each feature's tails, each horizon.
    let mut theories: Vec<Theory> = Vec::new();
    let mut tests = formulas * 2 * aheads.len();
    for (f, v) in features.iter().zip(&values) {
        let Some(q) = quantiles(
            v,
            0,
            split,
            &[TAILS[0], TAILS[1], 1.0 - TAILS[1], 1.0 - TAILS[0]],
        ) else {
            continue;
        };
        for (above, tail, threshold) in [
            (false, TAILS[0], q[0]),
            (false, TAILS[1], q[1]),
            (true, TAILS[1], q[2]),
            (true, TAILS[0], q[3]),
        ] {
            let cond = Cond {
                feature: f.clone(),
                above,
                threshold,
            };
            for (k, &h) in aheads.iter().enumerate() {
                tests += 1;
                let on = |i: usize| cond.holds(v[i]);
                let found = Perf::of(&events(on, &fwd[k], 0, split.saturating_sub(h), h));
                if found.n < 12 {
                    continue;
                }
                let checked = Perf::of(&events(on, &fwd[k], split, n, h));
                theories.push(Theory {
                    cond: cond.clone(),
                    tail,
                    ahead: h,
                    found,
                    checked,
                });
            }
        }
    }
    // The strongest |t| luck alone would give out of this many tests: theories must
    // beat it on the first 70% to be listed.
    let bar = (2.0 * (tests.max(2) as f64).ln()).sqrt().max(3.0);

    // Strategies from theories that look like something (on the first 70%).
    let index_of = |f: &Expr| {
        features
            .iter()
            .position(|g| g == f)
            .expect("a known feature")
    };
    let mut seeds: Vec<&Theory> = theories.iter().filter(|t| t.found.t.abs() >= 2.0).collect();
    seeds.sort_by(|a, b| b.found.t.abs().total_cmp(&a.found.t.abs()));
    seeds.truncate(120);
    let mut tried = 0;
    let mut candidates: Vec<Found> = Vec::new();
    let run = |s: &Strategy, from: usize, to: usize| -> Perf {
        let vals: Vec<&[f64]> = s
            .conds
            .iter()
            .map(|c| values[index_of(&c.feature)].as_slice())
            .collect();
        let nets: Vec<f64> = s
            .trades(&input, &vals, from, to, &fees)
            .into_iter()
            .filter(|t| t.exit.is_some())
            .map(|t| t.net)
            .collect();
        Perf::of(&nets)
    };
    for th in &seeds {
        let side = if th.found.mean > 0.0 {
            Side::Long
        } else {
            Side::Short
        };
        // Exits sized from the typical move over the horizon.
        let typical = th.found.sd.max(0.01);
        let h = th.ahead;
        let exits = [
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
        ];
        let mut best: Option<Found> = None;
        for exit in exits {
            let s = Strategy {
                side,
                conds: vec![th.cond.clone()],
                exit,
            };
            tried += 1;
            let picked = run(&s, 0, split);
            // It has to make money after costs where it was picked, and not by a fluke.
            if picked.n < MIN_TRADES || picked.t < MIN_T {
                continue;
            }
            if best.as_ref().is_none_or(|b| picked.t > b.picked.t) {
                best = Some(Found {
                    strategy: s,
                    picked,
                    checked: Perf::default(),
                });
            }
        }
        if let Some(b) = best
            && !candidates.iter().any(|c| c.strategy == b.strategy)
        {
            candidates.push(b);
        }
    }
    // A filter from another feature, for the best of them.
    candidates.sort_by(|a, b| b.picked.t.total_cmp(&a.picked.t));
    let terciles: Vec<Option<Vec<f64>>> = values
        .iter()
        .map(|v| quantiles(v, 0, split, &[1.0 / 3.0, 2.0 / 3.0]))
        .collect();
    let mut filtered = Vec::new();
    for base in candidates.iter().take(15) {
        let mut best: Option<Found> = None;
        for (k, f) in features.iter().enumerate() {
            if base.strategy.conds.iter().any(|c| &c.feature == f) {
                continue;
            }
            let Some(q) = &terciles[k] else { continue };
            for (above, threshold) in [(false, q[0]), (true, q[1])] {
                let mut s = base.strategy.clone();
                s.conds.push(Cond {
                    feature: f.clone(),
                    above,
                    threshold,
                });
                tried += 1;
                let picked = run(&s, 0, split);
                // Worth it only when clearly better, with enough trades left.
                if picked.n < MIN_TRADES
                    || picked.t < base.picked.t + 0.75
                    || best.as_ref().is_some_and(|b| picked.t <= b.picked.t)
                {
                    continue;
                }
                best = Some(Found {
                    strategy: s,
                    picked,
                    checked: Perf::default(),
                });
            }
        }
        filtered.extend(best);
    }
    candidates.extend(filtered);
    // Check the most promising on the last 30%, and rank by that.
    candidates.sort_by(|a, b| b.picked.t.total_cmp(&a.picked.t));
    candidates.truncate(40);
    for c in &mut candidates {
        c.checked = run(&c.strategy, split, n);
    }
    let rank = |c: &Found| {
        if c.checked.n == 0 {
            f64::MIN
        } else {
            c.checked.t.min(c.picked.t)
        }
    };
    candidates.sort_by(|a, b| rank(b).total_cmp(&rank(a)));
    // One per idea: the same entries with other exits, or other formulas that enter at
    // the same moments, are left out.
    let entries = |s: &Strategy| -> Vec<usize> {
        let vals: Vec<&[f64]> = s
            .conds
            .iter()
            .map(|c| values[index_of(&c.feature)].as_slice())
            .collect();
        s.trades(&input, &vals, 0, n, &fees)
            .iter()
            .map(|t| t.entry)
            .collect()
    };
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
    let strategies: Vec<Found> = strategies.into_iter().map(|s| s.0).collect();

    // The theories to show: strong on the first 70%, those that held first.
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
        let v = &values[index_of(&c.feature)];
        (0..split).filter(|&i| c.holds(v[i])).collect()
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
        hold: [hold(0, split), hold(split, n - 1)],
        formulas,
        tests,
        tried,
        bar,
        spread,
        fees,
        theories: theories_out,
        strategies,
        pool: evolved,
        took: started.elapsed(),
        input,
    })
}

/// A strategy's trades over `input`, entering only after point `from`.
pub fn trade(s: &Strategy, input: &Input, from: usize, fees: &Fees) -> Vec<Trade> {
    let values: Vec<Vec<f64>> = s.conds.iter().map(|c| c.feature.eval(input)).collect();
    let refs: Vec<&[f64]> = values.iter().map(Vec::as_slice).collect();
    s.trades(input, &refs, from, input.len(), fees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Series, XKind};
    use crate::units::Unit;

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
        let s = Strategy {
            side: Side::Long,
            conds: vec![],
            exit: Exit {
                hold: 5,
                take: None,
                stop: None,
            },
        };
        let t = s.trades(&inp, &[], 0, 50, &Fees::Flat(0.045));
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
        let s = Strategy {
            side: Side::Long,
            conds: vec![],
            exit: Exit {
                hold: 8,
                take: None,
                stop: None,
            },
        };
        let t = s.trades(&inp, &[], 0, 1, &f);
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
            let r = search(inp, fees, &[], b).unwrap();
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
            for s in r.strategies.iter().take(5) {
                eprintln!(
                    "  {} · {} → {}",
                    s.strategy.describe(&r.input),
                    s.picked.brief(),
                    s.checked.brief()
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
        let s = Strategy {
            side: Side::Short,
            conds: vec![],
            exit: Exit {
                hold: 3,
                take: None,
                stop: None,
            },
        };
        let t = s.trades(&inp, &[], 0, 20, &Fees::Flat(0.0));
        // Sold at 99.99, bought back at 100.01: about 2bp lost.
        assert!((t[0].net + 2.0).abs() < 0.01, "{:?}", t[0]);
    }

    #[test]
    fn finds_a_planted_edge_and_checks_it() {
        let report = search(input(bouncy(12_000)), Fees::Flat(0.0), &[], budget()).unwrap();
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
        assert_eq!(best.strategy.side, Side::Long, "{:?}", best.strategy);
        assert!(best.checked.mean > 0.0 && best.checked.t > 2.0, "{best:?}");
        assert!(report.theories.iter().any(Theory::held));
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
        let report = search(input(p), Fees::Flat(0.045), &[], budget()).unwrap();
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
