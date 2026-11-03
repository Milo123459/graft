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
    /// The closed trades' compounded return, in %.
    pub fn total(&self) -> f64 {
        (self
            .closed
            .iter()
            .fold(1.0, |acc, r| acc * (1.0 + r / 100.0))
            - 1.0)
            * 100.0
    }

    /// One line for a table of strategies: `name +3.1% · 6 trades, 67% won · dd 1.2%`.
    pub fn line(&self, name: &str) -> String {
        let n = self.closed.len();
        if n == 0 {
            return format!("{name}: no finished trades");
        }
        let wins = self.closed.iter().filter(|r| **r > 0.0).count();
        format!(
            "{name} {:+.2}% · {n} {}, {:.0}% won · drawdown {:.1}%",
            self.total(),
            if n == 1 { "trade" } else { "trades" },
            wins as f64 / n as f64 * 100.0,
            self.drawdown
        )
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

/// Buy at each entry (when not already in), sell at the next exit after it.
/// `fee`: % taken on each buy and each sell (0.05 = 0.05%).
pub fn trades(entries: &[&Mark], exits: &[&Mark], s: &Series, fee: f64) -> Trades {
    let mut entries: Vec<f64> = entries.iter().map(|m| m.x).collect();
    let mut exits: Vec<f64> = exits.iter().map(|m| m.x).collect();
    entries.sort_by(f64::total_cmp);
    exits.sort_by(f64::total_cmp);
    let mut closed = Vec::new();
    let mut open = None;
    let (mut equity, mut peak, mut drawdown) = (1.0f64, 1.0f64, 0.0f64);
    let mut time_in = 0.0;
    let mut from = f64::NEG_INFINITY;
    for &x in &entries {
        if x <= from {
            continue; // already in a trade
        }
        let Some((_, buy)) = value_at(s, x) else {
            continue;
        };
        match exits.iter().find(|e| **e > x) {
            Some(&out) => {
                let Some((_, sell)) = value_at(s, out) else {
                    continue;
                };
                if let Some(r) = pct(buy, sell) {
                    let r = r - 2.0 * fee;
                    closed.push(r);
                    equity *= 1.0 + r / 100.0;
                    peak = peak.max(equity);
                    drawdown = drawdown.max((peak - equity) / peak * 100.0);
                }
                time_in += out - x;
                from = out;
            }
            None => {
                let last = s.points.last().map(|p| (p.0, p.1));
                if let Some((end, now)) = last {
                    open = pct(buy, now).map(|r| r - 2.0 * fee);
                    time_in += end - x;
                }
                break;
            }
        }
    }
    let span = match (entries.first(), s.points.last()) {
        (Some(a), Some(b)) if b.0 > *a => b.0 - a,
        _ => 0.0,
    };
    Trades {
        closed,
        open,
        drawdown,
        in_market: if span > 0.0 { time_in / span } else { 0.0 },
    }
}

/// What simply holding `s` from its first point to its last returned, in %.
pub fn hold(s: &Series) -> Option<f64> {
    pct(s.points.first()?.1, s.points.last()?.1)
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
