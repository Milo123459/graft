//! Backtests over markers: what a series did after each marker a rule placed
//! (`backtest("golden cross", 10)`), and trades from one rule's markers to another's
//! (`strategy("golden cross", "death cross")`). Not trading-specific: "what happened
//! to latency an hour after each deploy marker" works the same way.

use crate::data::{Dataset, Series};
use crate::render::Mark;

/// A marker rule, by its number in the markers menu (from 1) or by its label/condition.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleRef {
    Number(usize),
    Named(String),
}

impl std::fmt::Display for RuleRef {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            RuleRef::Number(n) => write!(f, "number {n}"),
            RuleRef::Named(t) => write!(f, "\"{t}\""),
        }
    }
}

impl RuleRef {
    pub fn from_dynamic(d: &rhai::Dynamic) -> RuleRef {
        match d.as_int() {
            Ok(n) => RuleRef::Number(n.max(0) as usize),
            Err(_) => RuleRef::Named(d.to_string()),
        }
    }

    /// Which of `rules` it means: the number, else the rule whose label starts with
    /// the text, else whose label or condition contains it.
    pub fn find(&self, rules: &[crate::marks::MarkRule]) -> Option<usize> {
        match self {
            RuleRef::Number(n) => n.checked_sub(1).filter(|i| *i < rules.len()),
            RuleRef::Named(text) => {
                let t = text.to_lowercase();
                let label =
                    |r: &crate::marks::MarkRule| r.label.clone().unwrap_or_default().to_lowercase();
                rules
                    .iter()
                    .position(|r| label(r).starts_with(&t))
                    .or_else(|| {
                        rules.iter().position(|r| {
                            label(r).contains(&t) || r.expr.to_lowercase().contains(&t)
                        })
                    })
            }
        }
    }
}

/// How far after a marker to look: a number of points, or a length of time (x units).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum After {
    Points(usize),
    Span(f64),
}

/// The value of `s` at `x`: its point there, else the one just before.
fn value_at(s: &Series, x: f64) -> Option<(usize, f64)> {
    let i = s.points.partition_point(|p| p.0 <= x);
    let i = i.checked_sub(1)?;
    Some((i, s.points[i].1))
}

/// The value `after` a point (`None` past the end of the data).
fn value_after(s: &Series, i: usize, after: After) -> Option<f64> {
    match after {
        After::Points(n) => s.points.get(i + n).map(|p| p.1),
        After::Span(d) => {
            let x = s.points[i].0 + d;
            let j = s.points.partition_point(|p| p.0 < x);
            s.points.get(j).map(|p| p.1)
        }
    }
}

fn pct(from: f64, to: f64) -> Option<f64> {
    (from != 0.0).then(|| (to - from) / from.abs() * 100.0)
}

/// What followed a rule's markers.
#[derive(Debug, Clone, PartialEq)]
pub struct Followed {
    pub markers: usize,
    /// % change of the series after each marker (those with enough data after them).
    pub changes: Vec<f64>,
}

impl Followed {
    pub fn summary(&self, rule: &str, series: &str, after: &str) -> String {
        if self.markers == 0 {
            return format!("{rule}: no markers yet");
        }
        if self.changes.is_empty() {
            return format!(
                "{rule}: {} markers, none with {after} of {series} after them yet",
                self.markers
            );
        }
        let n = self.changes.len() as f64;
        let mean = self.changes.iter().sum::<f64>() / n;
        let mut sorted = self.changes.clone();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2];
        let up = self.changes.iter().filter(|c| **c > 0.0).count() as f64 / n * 100.0;
        format!(
            "{rule}: {} times · {series} {after} later: {mean:+.2}% on average (median {median:+.2}%), up {up:.0}% of the time · best {:+.2}%, worst {:+.2}%",
            self.changes.len(),
            sorted[sorted.len() - 1],
            sorted[0],
        )
    }
}

/// What `series` did `after` each of `marks`.
pub fn followed(marks: &[&Mark], s: &Series, after: After) -> Followed {
    let changes = marks
        .iter()
        .filter_map(|m| {
            let (i, v0) = value_at(s, m.x)?;
            pct(v0, value_after(s, i, after)?)
        })
        .collect();
    Followed {
        markers: marks.len(),
        changes,
    }
}

/// Trades from each entry marker to the next exit marker after it.
#[derive(Debug, Clone, PartialEq)]
pub struct Trades {
    /// % return of each closed trade.
    pub closed: Vec<f64>,
    /// The trade still open at the end of the data, and how it's doing.
    pub open: Option<f64>,
    /// Largest fall of the running total from its high, in %.
    pub drawdown: f64,
    /// Share of the time (between the first entry and the end) spent in a trade.
    pub in_market: f64,
}

impl Trades {
    /// From (time, % return) of each closed trade: the drawdown in time order.
    pub fn from_closed(mut closed: Vec<(f64, f64)>, open: Option<f64>, in_market: f64) -> Trades {
        closed.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (mut equity, mut peak, mut drawdown) = (1.0f64, 1.0f64, 0.0f64);
        for &(_, r) in &closed {
            equity *= 1.0 + r / 100.0;
            peak = peak.max(equity);
            drawdown = drawdown.max((peak - equity) / peak * 100.0);
        }
        Trades {
            closed: closed.into_iter().map(|c| c.1).collect(),
            open,
            drawdown,
            in_market,
        }
    }

    /// The closed trades' compounded return, in %.
    pub fn total(&self) -> f64 {
        (self
            .closed
            .iter()
            .fold(1.0, |acc, r| acc * (1.0 + r / 100.0))
            - 1.0)
            * 100.0
    }

    pub fn summary(&self, entry: &str, exit: &str, series: &str) -> String {
        let open = self
            .open
            .map(|o| format!(" · one still open, {o:+.2}%"))
            .unwrap_or_default();
        if self.closed.is_empty() {
            return format!("{entry} → {exit} on {series}: no finished trades yet{open}");
        }
        let total = self
            .closed
            .iter()
            .fold(1.0, |acc, r| acc * (1.0 + r / 100.0))
            - 1.0;
        let n = self.closed.len();
        let wins = self.closed.iter().filter(|r| **r > 0.0).count();
        let mean = self.closed.iter().sum::<f64>() / n as f64;
        let best = self.closed.iter().copied().fold(f64::MIN, f64::max);
        let worst = self.closed.iter().copied().fold(f64::MAX, f64::min);
        format!(
            "{entry} → {exit} on {series}: {n} trades, {:+.2}% in all · won {wins} ({:.0}%), {mean:+.2}% each on average, best {best:+.2}%, worst {worst:+.2}% · max drawdown {:.2}% · in a trade {:.0}% of the time{open}",
            total * 100.0,
            wins as f64 / n as f64 * 100.0,
            self.drawdown,
            self.in_market * 100.0,
        )
    }
}

/// Long (buy at entries, sell at exits) or short (sell at exits… i.e. the other way:
/// in at the exit rule's markers, out at the entry rule's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Long,
    Short,
}

/// How trades are run: the side, a stop-loss and a take-profit (% from the entry
/// price), and the fee (% per buy and per sell).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rules {
    pub side: Side,
    pub stop: Option<f64>,
    pub take: Option<f64>,
    pub fee: f64,
}

impl Rules {
    pub fn long(fee: f64) -> Rules {
        Rules {
            side: Side::Long,
            stop: None,
            take: None,
            fee,
        }
    }

    /// `long · SL 1% · TP 3%` (`no SL` / `no TP` when there's none).
    pub fn describe(&self) -> String {
        let side = match self.side {
            Side::Long => "long",
            Side::Short => "short",
        };
        let stop = self
            .stop
            .map_or("no SL".to_string(), |v| format!("SL {v}%"));
        let take = self
            .take
            .map_or("no TP".to_string(), |v| format!("TP {v}%"));
        format!("{side} · {stop} · {take}")
    }
}

/// The prices trades run on: the close, and the high and low if there are any (so a
/// stop or take-profit is hit within a candle, not only at its close).
#[derive(Debug, Clone, Copy)]
pub struct Prices<'a> {
    pub close: &'a Series,
    pub high: Option<&'a Series>,
    pub low: Option<&'a Series>,
}

impl<'a> Prices<'a> {
    /// The close (`series_for`), with series called high and low if there are.
    pub fn of(ds: &'a Dataset, close: &'a Series) -> Prices<'a> {
        let find = |names: [&str; 2]| {
            ds.series
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(names[0]))
                .or_else(|| {
                    ds.series
                        .iter()
                        .find(|s| s.name.eq_ignore_ascii_case(names[1]))
                })
        };
        Prices {
            close,
            high: find(["high", "h"]),
            low: find(["low", "l"]),
        }
    }
}

/// Run trades in `window` (from, to): in at each entry (when not already in), out at
/// the next exit, the stop or the take-profit, whichever comes first. A trade still open
/// at the end of the window is closed there, unless the window runs to the end of the
/// data (then it's reported as open).
pub fn simulate(
    entries: &[f64],
    exits: &[f64],
    p: &Prices,
    r: &Rules,
    window: (f64, f64),
) -> Trades {
    let s = p.close;
    let (ins, outs) = match r.side {
        Side::Long => (entries, exits),
        Side::Short => (exits, entries),
    };
    let mut ins: Vec<f64> = ins
        .iter()
        .copied()
        .filter(|x| *x >= window.0 && *x < window.1)
        .collect();
    let mut outs: Vec<f64> = outs.to_vec();
    ins.sort_by(f64::total_cmp);
    outs.sort_by(f64::total_cmp);
    let last_x = s.points.last().map_or(window.1, |p| p.0);
    let to_end = window.1 >= last_x;
    let short = r.side == Side::Short;
    let ret =
        |buy: f64, sell: f64| pct(buy, sell).map(|v| if short { -v } else { v } - 2.0 * r.fee);
    let (mut closed, mut open, mut time_in) = (Vec::new(), None, 0.0);
    let mut from = f64::NEG_INFINITY;
    for &x in &ins {
        if x <= from {
            continue; // already in a trade
        }
        let Some((i, entry)) = value_at(s, x) else {
            continue;
        };
        let exit_at = outs
            .iter()
            .copied()
            .find(|e| *e > x)
            .filter(|e| *e <= window.1);
        let stop_px = r.stop.map(|v| {
            if short {
                entry * (1.0 + v / 100.0)
            } else {
                entry * (1.0 - v / 100.0)
            }
        });
        let take_px = r.take.map(|v| {
            if short {
                entry * (1.0 - v / 100.0)
            } else {
                entry * (1.0 + v / 100.0)
            }
        });
        // Walk the candles after the entry until the exit (or the window's end).
        let until = exit_at.unwrap_or(window.1.min(last_x));
        let mut done: Option<(f64, f64)> = None;
        for k in i + 1..s.points.len() {
            let (t, c) = s.points[k];
            if t > until {
                break;
            }
            let at = |o: Option<&Series>| o.and_then(|o| value_at(o, t)).map_or(c, |v| v.1);
            let (hi, lo) = (at(p.high).max(c), at(p.low).min(c));
            // Both in one candle: the stop is assumed to come first.
            let stopped = stop_px.filter(|&px| if short { hi >= px } else { lo <= px });
            let took = take_px.filter(|&px| if short { lo <= px } else { hi >= px });
            if let Some(px) = stopped.or(took) {
                done = Some((t, px));
                break;
            }
        }
        let close_at = |t: f64| value_at(s, t).map(|v| (t, v.1));
        let finish = done
            .or_else(|| exit_at.and_then(close_at))
            .or_else(|| (!to_end).then(|| close_at(window.1.min(last_x))).flatten());
        match finish {
            Some((t, px)) => {
                if let Some(v) = ret(entry, px) {
                    closed.push((t, v));
                }
                time_in += t - x;
                from = t;
            }
            None => {
                if let Some(&(end, now)) = s.points.last() {
                    open = ret(entry, now);
                    time_in += end - x;
                }
                break;
            }
        }
    }
    let span = match ins.first() {
        Some(a) if window.1.min(last_x) > *a => window.1.min(last_x) - a,
        _ => 0.0,
    };
    Trades::from_closed(closed, open, if span > 0.0 { time_in / span } else { 0.0 })
}

/// Buy at each entry (when not already in), sell at the next exit after it.
/// `fee`: % taken on each buy and each sell (0.05 = 0.05%).
pub fn trades(entries: &[&Mark], exits: &[&Mark], s: &Series, fee: f64) -> Trades {
    let xs = |m: &[&Mark]| m.iter().map(|m| m.x).collect::<Vec<_>>();
    let p = Prices {
        close: s,
        high: None,
        low: None,
    };
    simulate(
        &xs(entries),
        &xs(exits),
        &p,
        &Rules::long(fee),
        (f64::NEG_INFINITY, f64::INFINITY),
    )
}

/// Stops and take-profits worth trying (%; `None`: none).
pub const STOPS: [Option<f64>; 6] = [None, Some(0.5), Some(1.0), Some(2.0), Some(3.0), Some(5.0)];
pub const TAKES: [Option<f64>; 6] = [None, Some(1.0), Some(2.0), Some(3.0), Some(5.0), Some(8.0)];

/// The best way to trade a strategy: each side with each stop and take-profit, picked
/// on the first half of the time, then checked walking forward over the second — in
/// stretches, each traded the way that was best on everything before it — so a stop
/// that only fits the past shows up there.
#[derive(Debug, Clone)]
pub struct Best {
    /// Picked on the first half…
    pub rules: Rules,
    /// …where it did this.
    pub picked_on: Trades,
    /// Walking forward over the second half (each stretch re-picked).
    pub after: Trades,
    /// How many stretches, and how many of them were traded the way `rules` says.
    pub stretches: usize,
    pub same: usize,
    /// The best way on all of it: how to trade it from now on.
    pub now: Rules,
    /// Every way's result on the first half (for the median).
    pub all: Vec<f64>,
}

/// Every way to trade (side × stop × take-profit).
fn ways(fee: f64) -> impl Iterator<Item = Rules> {
    [Side::Long, Side::Short].into_iter().flat_map(move |side| {
        STOPS.into_iter().flat_map(move |stop| {
            TAKES.into_iter().map(move |take| Rules {
                side,
                stop,
                take,
                fee,
            })
        })
    })
}

/// The way that made the most in `window`, its trades, and every way's result.
fn pick(
    entries: &[f64],
    exits: &[f64],
    p: &Prices,
    fee: f64,
    window: (f64, f64),
) -> (Rules, Trades, Vec<f64>) {
    let mut found: Option<(Rules, Trades)> = None;
    let mut all = Vec::new();
    for r in ways(fee) {
        let t = simulate(entries, exits, p, &r, window);
        all.push(t.total());
        if found.as_ref().is_none_or(|b| t.total() > b.1.total()) {
            found = Some((r, t));
        }
    }
    let (r, t) = found.expect("there are ways to trade");
    (r, t, all)
}

pub fn best(entries: &[f64], exits: &[f64], p: &Prices, fee: f64) -> Option<Best> {
    let (first, last) = (p.close.points.first()?.0, p.close.points.last()?.0);
    let split = learned_until(first, last);
    let (rules, picked_on, all) = pick(entries, exits, p, fee, (first, split));
    // As many stretches (up to 5) as leave each about 5 signals to trade.
    let signals = entries
        .iter()
        .filter(|x| **x >= split)
        .count()
        .max(exits.iter().filter(|x| **x >= split).count());
    let stretches = (signals / 5).clamp(1, 5);
    let cut = |k: usize| split + (last - split) * k as f64 / stretches as f64;
    let (mut closed, mut open, mut time_in, mut same) = (Vec::new(), None, 0.0, 0);
    for k in 0..stretches {
        let (from, to) = (cut(k), cut(k + 1));
        let (r, ..) = pick(entries, exits, p, fee, (first, from));
        same += usize::from(r == rules);
        // The last runs to the end of the data (a trade there may still be open).
        let to = if k + 1 == stretches {
            f64::INFINITY
        } else {
            to
        };
        let t = simulate(entries, exits, p, &r, (from, to));
        let at = closed.len();
        closed.extend(
            t.closed
                .iter()
                .enumerate()
                .map(|(i, c)| ((at + i) as f64, *c)),
        );
        time_in += t.in_market / stretches as f64;
        open = t.open;
    }
    let (now, ..) = pick(entries, exits, p, fee, (first, f64::INFINITY));
    Some(Best {
        rules,
        picked_on,
        after: Trades::from_closed(closed, open, time_in),
        stretches,
        same,
        now,
        all,
    })
}

/// Where the first half ends: for an edge that's really there, the weaker of the two
/// halves' results is clearest when they're the same size.
pub fn learned_until(first: f64, last: f64) -> f64 {
    first + (last - first) / 2.0
}

/// What holding `s` from `from` to `to` returned, in %.
pub fn hold_in(s: &Series, from: f64, to: f64) -> Option<f64> {
    let a = s.points.iter().find(|p| p.0 >= from)?.1;
    let b = s.points.iter().rev().find(|p| p.0 <= to)?.1;
    pct(a, b)
}

/// The strategies among marker rules: pairs labelled `NAME in` and `NAME out` (or
/// buy/sell, entry/exit), as (name, entry rule, exit rule).
pub fn pairs(labels: &[String]) -> Vec<(String, usize, usize)> {
    const ENDS: [(&str, &str); 3] = [(" in", " out"), (" buy", " sell"), (" entry", " exit")];
    let mut out = Vec::new();
    for (i, l) in labels.iter().enumerate() {
        let l = l.to_lowercase();
        for (a, b) in ENDS {
            if let Some(name) = l.strip_suffix(a) {
                let exit = format!("{name}{b}");
                if let Some(j) = labels.iter().position(|o| o.to_lowercase() == exit) {
                    out.push((name.to_string(), i, j));
                }
            }
        }
    }
    out
}

/// The series a backtest measures: the one named; else a close price (a series called
/// close or c, e.g. candles'); else the one the markers are on.
pub fn series_for<'a>(ds: &'a Dataset, name: Option<&str>, marks: &[&Mark]) -> Option<&'a Series> {
    if let Some(n) = name {
        return ds.get(n);
    }
    // A series called close (e.g. close:USD = c) before a raw `c` field.
    if let Some(s) = ds
        .series
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case("close"))
    {
        return Some(s);
    }
    let names: Vec<&str> = ds.series.iter().map(|s| s.name.as_str()).collect();
    if let Some(k) = crate::render::candle_roles_named(&names)[3] {
        return ds.series.get(k);
    }
    let on = marks.iter().find_map(|m| m.series.clone())?;
    ds.get(&on)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::Unit;

    fn mark(x: f64) -> Mark {
        Mark {
            series: Some("price".into()),
            x,
            y: None,
            label: String::new(),
            rule: Some(0),
            symbol: "▲".into(),
            color: None,
            effects: ratatui::style::Modifier::empty(),
            badge: false,
            bg: None,
            lane: None,
        }
    }

    fn price() -> Series {
        Series {
            name: "price".into(),
            unit: Unit::default(),
            // 100, 110, 99, 120, 90, 100
            points: [100.0, 110.0, 99.0, 120.0, 90.0, 100.0]
                .iter()
                .enumerate()
                .map(|(i, &v)| (i as f64 * 60.0, v))
                .collect(),
        }
    }

    #[test]
    fn what_followed_each_marker() {
        let s = price();
        let (a, b, c) = (mark(0.0), mark(120.0), mark(300.0));
        let f = followed(&[&a, &b, &c], &s, After::Points(1));
        // 100 → 110, 99 → 120; the last has nothing after it.
        assert_eq!(f.markers, 3);
        assert_eq!(f.changes.len(), 2);
        assert!((f.changes[0] - 10.0).abs() < 1e-9);
        assert!((f.changes[1] - 21.212_121).abs() < 1e-4);
        let by_time = followed(&[&a], &s, After::Span(120.0));
        assert!(
            (by_time.changes[0] + 1.0).abs() < 1e-9,
            "100 → 99 two minutes on"
        );
        let text = f.summary("dip", "price", "1 point");
        assert!(
            text.contains("2 times") && text.contains("up 100%"),
            "{text}"
        );
    }

    #[test]
    fn shorts_stops_and_take_profits() {
        let s = price(); // 100, 110, 99, 120, 90, 100 a minute apart
        let p = Prices {
            close: &s,
            high: None,
            low: None,
        };
        let all = (f64::NEG_INFINITY, f64::INFINITY);
        let rules = |side, stop, take| Rules {
            side,
            stop,
            take,
            fee: 0.0,
        };
        // Short: in at the exit rule's marker (120), out at the next entry's (90): +25%.
        let t = simulate(&[240.0], &[180.0], &p, &rules(Side::Short, None, None), all);
        assert_eq!(t.closed, [25.0]);
        // Long from 100 with a 5% take-profit: out at 105 on the next candle.
        let t = simulate(
            &[0.0],
            &[300.0],
            &p,
            &rules(Side::Long, None, Some(5.0)),
            all,
        );
        assert_eq!(t.closed, [5.0]);
        // Long from 110 with a 2% stop: 99 hits it, out at 107.8.
        let t = simulate(
            &[60.0],
            &[300.0],
            &p,
            &rules(Side::Long, Some(2.0), None),
            all,
        );
        assert!((t.closed[0] + 2.0).abs() < 1e-9, "{:?}", t.closed);
        // With highs and lows, a candle that touches the stop stops it even if it closes
        // above it.
        let low = Series {
            name: "low".into(),
            unit: Unit::default(),
            points: s.points.iter().map(|&(x, v)| (x, v - 15.0)).collect(),
        };
        let p2 = Prices {
            close: &s,
            high: None,
            low: Some(&low),
        };
        let t = simulate(
            &[0.0],
            &[300.0],
            &p2,
            &rules(Side::Long, Some(10.0), None),
            all,
        );
        assert_eq!(t.closed, [-10.0], "110's low is 95: stopped at 90");
        // A window ending early closes the trade at its end.
        let t = simulate(
            &[0.0],
            &[300.0],
            &p,
            &rules(Side::Long, None, None),
            (0.0, 120.0),
        );
        assert_eq!(t.closed, [-1.0]);
        assert!(t.open.is_none());
        // The best way, picked on the first half, then walked forward over the second.
        let b = best(&[0.0, 180.0], &[120.0, 240.0], &p, 0.0).unwrap();
        assert_eq!(b.stretches, 1, "two signals: one stretch");
        assert_eq!(b.same, 1, "picked on the same data: the same way");
        assert_eq!(b.all.len(), 2 * STOPS.len() * TAKES.len());
        assert!(b.picked_on.total() >= b.all.iter().copied().fold(f64::MIN, f64::max) - 1e-9);
        let d = b.rules.describe();
        assert!(
            d.contains(" · ") && (d.contains("SL") && d.contains("TP")),
            "{d}"
        );
        let r = Rules {
            side: Side::Short,
            stop: Some(1.0),
            take: None,
            fee: 0.0,
        };
        assert_eq!(r.describe(), "short · SL 1% · no TP");
    }

    #[test]
    fn close_before_a_raw_c() {
        let mut c = price();
        c.name = "c".into();
        let mut close = price();
        close.name = "close".into();
        let ds = Dataset {
            x_kind: crate::data::XKind::Time,
            series: vec![c, close],
        };
        assert_eq!(series_for(&ds, None, &[]).unwrap().name, "close");
    }

    #[test]
    fn trades_from_entries_to_exits() {
        let s = price();
        // In at 100, out at 99 (−1%); in at 120 (an entry while in is skipped), out at
        // 90 (−25%); in at 100, still open.
        let entries = [mark(0.0), mark(60.0), mark(180.0), mark(300.0)];
        let exits = [mark(120.0), mark(240.0)];
        let t = trades(
            &entries.iter().collect::<Vec<_>>(),
            &exits.iter().collect::<Vec<_>>(),
            &s,
            0.0,
        );
        assert_eq!(t.closed.len(), 2);
        assert!((t.closed[0] + 1.0).abs() < 1e-9 && (t.closed[1] + 25.0).abs() < 1e-9);
        assert_eq!(t.open, Some(0.0));
        assert!((t.drawdown - 25.75).abs() < 1e-6, "{}", t.drawdown);
        let text = t.summary("in", "out", "price");
        assert!(
            text.contains("2 trades") && text.contains("won 0"),
            "{text}"
        );
    }
}
