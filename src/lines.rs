//! Custom lines: `--line 'ema30 = ema(values("ETH"), 30)'`.
//!
//! The expression is Rhai, evaluated over the whole data, and returns:
//! - an array of numbers: one per point of the series it's based on — the first series
//!   named in the expression (`values("ETH")`), else the first series;
//! - an array of `[x, y]` pairs, for full control; or
//! - a single number: a flat line (e.g. `mean("ETH")`).
//!
//! Built-ins for arrays: `ema(a, n)`, `sma(a, n)`, `values_at(name, xs)`, plus anything
//! defined in `--script`. The line becomes a series like any other.

use anyhow::{Result, bail};
use rhai::Dynamic;

use crate::data::{Dataset, Series};
use crate::script::ScriptHost;
use crate::units::Unit;

#[derive(Debug, Clone, PartialEq)]
pub struct CustomLine {
    pub name: String,
    /// From `NAME:UNIT = …`; otherwise the unit of the series it's based on.
    pub unit: Option<String>,
    pub expr: String,
}

impl CustomLine {
    /// `NAME = EXPR` or `NAME:UNIT = EXPR`.
    pub fn parse(spec: &str) -> Result<CustomLine, String> {
        let bad = || {
            format!("'{spec}' should look like NAME = EXPR, e.g. ema30 = ema(values(\"ETH\"), 30)")
        };
        let (lhs, expr) = spec.split_once('=').ok_or_else(bad)?;
        let (name, unit) = match lhs.split_once(':') {
            Some((n, u)) => (
                n.trim(),
                Some(u.trim().to_string()).filter(|u| !u.is_empty()),
            ),
            None => (lhs.trim(), None),
        };
        let expr = expr.trim();
        if name.is_empty() || expr.is_empty() || name.contains(char::is_whitespace) {
            return Err(bad());
        }
        Ok(CustomLine {
            name: name.to_string(),
            unit,
            expr: expr.to_string(),
        })
    }

    pub fn to_spec(&self) -> String {
        match &self.unit {
            Some(u) => format!("{}:{u} = {}", self.name, self.expr),
            None => format!("{} = {}", self.name, self.expr),
        }
    }
}

/// Add each line to `ds` (in order, so later lines can use earlier ones). Lines that
/// fail are left out; their errors are returned.
pub fn apply(
    host: &ScriptHost,
    lines: &[CustomLine],
    ds: &mut Dataset,
    binary: bool,
) -> Vec<String> {
    let mut errors = Vec::new();
    let sources: Vec<String> = ds.series.iter().map(|s| s.name.clone()).collect();
    for line in lines {
        if sources.contains(&line.name) {
            errors.push(format!(
                "line {}: there's already a series called '{}' — give the line another name",
                line.name, line.name
            ));
            continue;
        }
        ds.series.retain(|s| s.name != line.name);
        match evaluate(host, line, ds, binary) {
            Ok(Some(s)) => ds.series.push(s),
            Ok(None) => {}
            Err(e) => errors.push(format!("line {}: {e:#}", line.name)),
        }
    }
    errors
}

fn evaluate(
    host: &ScriptHost,
    line: &CustomLine,
    ds: &Dataset,
    binary: bool,
) -> Result<Option<Series>> {
    if ds.series.iter().all(|s| s.points.is_empty()) {
        return Ok(None);
    }
    host.set_data(ds.clone(), None);
    // `user + sys`: series used by name are worked out point by point (like --series).
    if uses_series_by_name(&line.expr, ds) {
        let unit = line.unit.as_ref().map(|u| {
            crate::units::lookup_unit(u, binary).map_or_else(
                || Unit::new(crate::units::Dim::Other(u.clone())),
                |(dim, _)| Unit::new(dim),
            )
        });
        let d = crate::transform::Derived {
            name: line.name.clone(),
            source: line.expr.clone(),
            ast: host.compile_expr(&line.expr)?,
            unit,
        };
        let s = crate::transform::derive(ds, &d, host);
        if s.points.is_empty() {
            bail!("it gave no numbers — e.g. user + sys, or api - mean(\"api\")");
        }
        return Ok(Some(s));
    }
    let result = host.eval(&line.expr)?;
    let base = base_series(line, ds, &result);
    let points = to_points(&result, base)?;
    let unit = match &line.unit {
        Some(u) => crate::units::lookup_unit(u, binary).map_or_else(
            || Unit::new(crate::units::Dim::Other(u.clone())),
            |(dim, _)| Unit::new(dim),
        ),
        None => base.unit.clone(),
    };
    Ok(Some(Series {
        name: line.name.clone(),
        unit,
        points,
    }))
}

/// Whether `expr` uses a series as a variable (`user + sys`), outside of text.
fn uses_series_by_name(expr: &str, ds: &Dataset) -> bool {
    static TEXT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#""(?:[^"\\]|\\.)*"|`[^`]*`"#).expect("valid")
    });
    let code = TEXT.replace_all(expr, "\"\"");
    // The words used as names: not followed by `(` (a function call).
    let mut words = std::collections::HashSet::new();
    let mut rest = code.as_ref();
    while let Some(start) = rest.find(|c: char| c.is_alphanumeric() || c == '_') {
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(tail.len());
        if !tail[end..].starts_with('(') {
            words.insert(&tail[..end]);
        }
        rest = &tail[end..];
    }
    ds.series
        .iter()
        .any(|s| words.contains(crate::script::ident(&s.name).as_str()))
}

/// The series a line's values belong to: of the series the expression names (those in
/// `xs("…")` first, then in order of mention, then the rest), the first one the result
/// fits exactly, else the first it fits at all.
fn base_series<'a>(line: &CustomLine, ds: &'a Dataset, result: &Dynamic) -> &'a Series {
    let pos = |pat: String| line.expr.find(&pat);
    let mut order: Vec<(usize, usize, &Series)> = ds
        .series
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let rank = if pos(format!("xs(\"{}\")", s.name)).is_some() {
                0
            } else if let Some(p) = pos(format!("\"{}\"", s.name)) {
                1 + p
            } else {
                usize::MAX - ds.series.len() + i
            };
            (rank, i, s)
        })
        .collect();
    order.sort_by_key(|o| (o.0, o.1));
    let len = result
        .clone()
        .try_cast::<rhai::Array>()
        .filter(|a| !a.first().is_some_and(|d| d.is_array()))
        .map(|a| a.len());
    // Series the expression names first; others only if none of those fit.
    let named = order.iter().filter(|o| o.0 < usize::MAX - ds.series.len());
    let pick = match len {
        Some(n) => named
            .clone()
            .find(|o| o.2.points.len() == n)
            .or_else(|| named.clone().find(|o| o.2.points.len() >= n))
            .or_else(|| order.iter().find(|o| o.2.points.len() == n))
            .or_else(|| order.iter().find(|o| o.2.points.len() >= n)),
        None => None,
    };
    pick.or(order.first()).expect("non-empty").2
}

fn num(d: &Dynamic) -> Option<f64> {
    d.as_float()
        .ok()
        .or_else(|| d.as_int().ok().map(|i| i as f64))
}

fn to_points(result: &Dynamic, base: &Series) -> Result<Vec<(f64, f64)>> {
    if let Some(v) = num(result) {
        return Ok(base.points.iter().map(|p| (p.0, v)).collect());
    }
    let Some(arr) = result.clone().try_cast::<rhai::Array>() else {
        bail!(
            "returned {} — return an array (one value per point of \"{}\"), [x, y] pairs or a number",
            result.type_name(),
            base.name
        );
    };
    // [[x, y], …]
    if arr.first().is_some_and(|d| d.is_array()) {
        return arr
            .iter()
            .map(|d| {
                let pair = d.clone().cast::<rhai::Array>();
                match (pair.first().and_then(num), pair.get(1).and_then(num)) {
                    (Some(x), Some(y)) => Ok((x, y)),
                    _ => bail!("[x, y] pairs must hold two numbers"),
                }
            })
            .collect();
    }
    // One value per point, lined up with the end of the base series (so a shorter
    // result, e.g. one that skips a warm-up, still lines up with the newest data).
    let n = base.points.len();
    if arr.len() > n {
        bail!(
            "returned {} values but \"{}\" has {n} points",
            arr.len(),
            base.name
        );
    }
    let start = n - arr.len();
    Ok(arr
        .iter()
        .zip(&base.points[start..])
        .filter_map(|(d, p)| num(d).filter(|v| v.is_finite()).map(|v| (p.0, v)))
        .collect())
}

/// Exponential moving average, seeded with the first value.
pub fn ema(values: &[f64], n: usize) -> Vec<f64> {
    let k = 2.0 / (n.max(1) as f64 + 1.0);
    let mut out = Vec::with_capacity(values.len());
    let mut prev: Option<f64> = None;
    for &v in values {
        let e = prev.map_or(v, |p| v * k + p * (1.0 - k));
        out.push(e);
        prev = Some(e);
    }
    out
}

/// Simple moving average over the last `n` values (fewer at the start).
pub fn sma(values: &[f64], n: usize) -> Vec<f64> {
    let n = n.max(1);
    let mut out = Vec::with_capacity(values.len());
    let mut sum = 0.0;
    for (i, &v) in values.iter().enumerate() {
        sum += v;
        if i >= n {
            sum -= values[i - n];
        }
        out.push(sum / (i + 1).min(n) as f64);
    }
    out
}

/// Relative strength index with Wilder's smoothing: 0–100 (above 70 is often read
/// as overbought, below 30 as oversold). The first n changes warm up as a plain average.
pub fn rsi(values: &[f64], n: usize) -> Vec<f64> {
    let n = n.max(1) as f64;
    let (mut gain, mut loss) = (0.0, 0.0);
    let mut out = Vec::with_capacity(values.len());
    for (i, &v) in values.iter().enumerate() {
        if i == 0 {
            out.push(50.0);
            continue;
        }
        let d = v - values[i - 1];
        let (g, l) = (d.max(0.0), (-d).max(0.0));
        if i as f64 <= n {
            gain += g / n;
            loss += l / n;
        } else {
            gain = (gain * (n - 1.0) + g) / n;
            loss = (loss * (n - 1.0) + l) / n;
        }
        out.push(if loss == 0.0 {
            100.0
        } else {
            100.0 - 100.0 / (1.0 + gain / loss)
        });
    }
    out
}

/// MACD line: the 12-point EMA minus the 26-point EMA.
pub fn macd(values: &[f64]) -> Vec<f64> {
    let (fast, slow) = (ema(values, 12), ema(values, 26));
    fast.iter().zip(&slow).map(|(a, b)| a - b).collect()
}

/// Bollinger band: the n-point moving average plus k standard deviations (k < 0: the
/// lower band, 0: the middle).
pub fn bollinger(values: &[f64], n: usize, k: f64) -> Vec<f64> {
    let n = n.max(1);
    (0..values.len())
        .map(|i| {
            let w = &values[(i + 1).saturating_sub(n)..=i];
            let mean = w.iter().sum::<f64>() / w.len() as f64;
            let var = w.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / w.len() as f64;
            mean + k * var.sqrt()
        })
        .collect()
}

/// Volume-weighted average price, running from the first point.
pub fn vwap(price: &[f64], volume: &[f64]) -> Vec<f64> {
    let (mut pv, mut v) = (0.0, 0.0);
    price
        .iter()
        .zip(volume)
        .map(|(p, q)| {
            pv += p * q;
            v += q;
            if v > 0.0 { pv / v } else { *p }
        })
        .collect()
}

/// Average true range over n points (Wilder's smoothing): how far price moves.
pub fn atr(high: &[f64], low: &[f64], close: &[f64], n: usize) -> Vec<f64> {
    let n = n.max(1) as f64;
    let len = high.len().min(low.len()).min(close.len());
    let mut out = Vec::with_capacity(len);
    let mut avg = 0.0;
    for i in 0..len {
        let range = high[i] - low[i];
        let tr = if i == 0 {
            range
        } else {
            range
                .max((high[i] - close[i - 1]).abs())
                .max((low[i] - close[i - 1]).abs())
        };
        avg = if (i as f64) < n {
            (avg * i as f64 + tr) / (i as f64 + 1.0)
        } else {
            (avg * (n - 1.0) + tr) / n
        };
        out.push(avg);
    }
    out
}

/// Highest value over the last n points (including this one).
pub fn highest(values: &[f64], n: usize) -> Vec<f64> {
    let n = n.max(1);
    (0..values.len())
        .map(|i| {
            values[(i + 1).saturating_sub(n)..=i]
                .iter()
                .copied()
                .fold(f64::MIN, f64::max)
        })
        .collect()
}

/// Lowest value over the last n points (including this one).
pub fn lowest(values: &[f64], n: usize) -> Vec<f64> {
    let n = n.max(1);
    (0..values.len())
        .map(|i| {
            values[(i + 1).saturating_sub(n)..=i]
                .iter()
                .copied()
                .fold(f64::MAX, f64::min)
        })
        .collect()
}

/// Stochastic %K: where the close is within the high–low range of the last n points,
/// 0–100 (above 80 is often read as overbought, below 20 as oversold).
pub fn stoch(high: &[f64], low: &[f64], close: &[f64], n: usize) -> Vec<f64> {
    let (hh, ll) = (highest(high, n), lowest(low, n));
    let len = hh.len().min(ll.len()).min(close.len());
    (0..len)
        .map(|i| {
            let range = hh[i] - ll[i];
            if range > 0.0 {
                (close[i] - ll[i]) / range * 100.0
            } else {
                50.0
            }
        })
        .collect()
}

// ───────────── indicators for short-term trading (bp = 1/100 of a percent) ─────────────
// Each is NaN until it has n points behind it.

/// Change over the last n points, in bp: `(v / v[n back] − 1) × 10000`.
pub fn change(values: &[f64], n: usize) -> Vec<f64> {
    let n = n.max(1);
    (0..values.len())
        .map(|i| {
            if i >= n && values[i - n] != 0.0 {
                (values[i] / values[i - n] - 1.0) * 1e4
            } else {
                f64::NAN
            }
        })
        .collect()
}

/// How far each value is from its average over the last n points, in standard
/// deviations of those points (0 when they're all the same).
pub fn zscore(values: &[f64], n: usize) -> Vec<f64> {
    let n = n.max(2);
    // Measured from the first value, so big numbers (prices) keep their precision.
    let base = values.first().copied().unwrap_or(0.0);
    let (mut sum, mut sq) = (0.0, 0.0);
    let mut out = Vec::with_capacity(values.len());
    for (i, &v) in values.iter().enumerate() {
        let d = v - base;
        sum += d;
        sq += d * d;
        if i >= n {
            let old = values[i - n] - base;
            sum -= old;
            sq -= old * old;
        }
        if i + 1 < n {
            out.push(f64::NAN);
            continue;
        }
        let mean = sum / n as f64;
        let sd = (sq / n as f64 - mean * mean).max(0.0).sqrt();
        out.push(if sd > 0.0 { (d - mean) / sd } else { 0.0 });
    }
    out
}

/// Where each value sits in the range of the last n points: 0 at the lowest, 100 at
/// the highest (50 when flat).
pub fn range_pos(values: &[f64], n: usize) -> Vec<f64> {
    use std::collections::VecDeque;
    let n = n.max(2);
    // Indices of candidate highs (falling values) and lows (rising values).
    let (mut hi, mut lo): (VecDeque<usize>, VecDeque<usize>) = (VecDeque::new(), VecDeque::new());
    let mut out = Vec::with_capacity(values.len());
    for (i, &v) in values.iter().enumerate() {
        while hi.back().is_some_and(|&j| values[j] <= v) {
            hi.pop_back();
        }
        hi.push_back(i);
        while lo.back().is_some_and(|&j| values[j] >= v) {
            lo.pop_back();
        }
        lo.push_back(i);
        while hi.front().is_some_and(|&j| j + n <= i) {
            hi.pop_front();
        }
        while lo.front().is_some_and(|&j| j + n <= i) {
            lo.pop_front();
        }
        if i + 1 < n {
            out.push(f64::NAN);
            continue;
        }
        let (h, l) = (values[hi[0]], values[lo[0]]);
        out.push(if h > l {
            (v - l) / (h - l) * 100.0
        } else {
            50.0
        });
    }
    out
}

/// The share of the last n moves that were up, in % (one that didn't move counts half).
pub fn updown(values: &[f64], n: usize) -> Vec<f64> {
    let n = n.max(1);
    let score = |i: usize| -> f64 {
        match values[i].partial_cmp(&values[i - 1]) {
            Some(std::cmp::Ordering::Greater) => 1.0,
            Some(std::cmp::Ordering::Less) | None => 0.0,
            Some(std::cmp::Ordering::Equal) => 0.5,
        }
    };
    let mut sum = 0.0;
    let mut out = Vec::with_capacity(values.len());
    for i in 0..values.len() {
        if i >= 1 {
            sum += score(i);
        }
        if i > n {
            sum -= score(i - n);
        }
        out.push(if i >= n {
            sum / n as f64 * 100.0
        } else {
            f64::NAN
        });
    }
    out
}

/// Volatility: the standard deviation of the point-to-point changes over the last n
/// points, in bp.
pub fn volatility(values: &[f64], n: usize) -> Vec<f64> {
    let n = n.max(2);
    let step = |i: usize| -> f64 {
        if values[i - 1] != 0.0 {
            (values[i] / values[i - 1] - 1.0) * 1e4
        } else {
            0.0
        }
    };
    let (mut sum, mut sq) = (0.0, 0.0);
    let mut out = Vec::with_capacity(values.len());
    for i in 0..values.len() {
        if i >= 1 {
            let r = step(i);
            sum += r;
            sq += r * r;
        }
        if i > n {
            let r = step(i - n);
            sum -= r;
            sq -= r * r;
        }
        if i < n {
            out.push(f64::NAN);
            continue;
        }
        let mean = sum / n as f64;
        out.push((sq / n as f64 - mean * mean).max(0.0).sqrt());
    }
    out
}

/// `a` relative to `b`, in bp: `(a / b − 1) × 10000` (lined up at their ends when one
/// is shorter).
pub fn gap(a: &[f64], b: &[f64]) -> Vec<f64> {
    let len = a.len().min(b.len());
    let (a, b) = (&a[a.len() - len..], &b[b.len() - len..]);
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            if *y != 0.0 {
                (x / y - 1.0) * 1e4
            } else {
                f64::NAN
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::XKind;

    #[test]
    fn more_indicators() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(bollinger(&v, 3, 0.0)[4], 4.0, "k = 0: the moving average");
        let up = bollinger(&v, 3, 2.0)[4];
        assert!((up - (4.0 + 2.0 * (2.0f64 / 3.0).sqrt())).abs() < 1e-9);
        assert_eq!(vwap(&[10.0, 20.0], &[1.0, 3.0]), [10.0, 17.5]);
        assert_eq!(highest(&v, 2), [1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(lowest(&v, 2), [1.0, 1.0, 2.0, 3.0, 4.0]);
        let (h, l, c) = ([2.0, 3.0, 4.0], [1.0, 1.0, 2.0], [1.5, 3.0, 2.0]);
        assert_eq!(stoch(&h, &l, &c, 3), [50.0, 100.0, 1.0 / 3.0 * 100.0]);
        let a = atr(&h, &l, &c, 2);
        assert_eq!(a[0], 1.0);
        assert_eq!(a[1], 1.5, "true range 2: high 3 - low 1");
    }

    #[test]
    fn rsi_and_macd() {
        let up: Vec<f64> = (0..40).map(f64::from).collect();
        assert_eq!(rsi(&up, 14)[39], 100.0, "only gains");
        let zigzag: Vec<f64> = (0..40)
            .map(|i| if i % 2 == 0 { 1.0 } else { 2.0 })
            .collect();
        assert!(
            (rsi(&zigzag, 14)[39] - 50.0).abs() < 5.0,
            "even gains and losses"
        );
        let m = macd(&up);
        assert_eq!(m.len(), 40);
        assert!(
            m[39] > 0.0,
            "rising: the fast average is above the slow one"
        );
    }

    fn ds() -> Dataset {
        Dataset {
            x_kind: XKind::Time,
            series: vec![
                Series {
                    name: "ETH".into(),
                    unit: Unit::default(),
                    points: (0..10).map(|i| (i as f64, 100.0 + i as f64)).collect(),
                },
                Series {
                    name: "other".into(),
                    unit: Unit::default(),
                    points: vec![(0.5, 1.0), (8.5, 9.0)],
                },
            ],
        }
    }

    #[test]
    fn moving_averages() {
        assert_eq!(sma(&[1.0, 2.0, 3.0, 4.0], 2), [1.0, 1.5, 2.5, 3.5]);
        let e = ema(&[10.0, 20.0], 3);
        assert_eq!(e, [10.0, 15.0]);
    }

    #[test]
    fn lines_from_rhai() {
        let host = ScriptHost::new(None, false).unwrap();
        let mut d = ds();
        let lines: Vec<CustomLine> = [
            "ema3 = ema(values(\"ETH\"), 3)",
            "avg:USD = mean(\"ETH\")",
            "pairs = [[1.0, 5.0], [2.0, 6.0]]",
            "lagged = { let v = values(\"ETH\"); v.extract(1) }",
            "other_at = values_at(\"other\", xs(\"ETH\"))",
            "bad = \"text\"",
        ]
        .iter()
        .map(|s| CustomLine::parse(s).unwrap())
        .collect();
        let errors = apply(&host, &lines, &mut d, false);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("line bad") && errors[0].contains("return an array"));
        let get = |n: &str| d.series.iter().find(|s| s.name == n).unwrap().clone();
        assert_eq!(get("ema3").points.len(), 10);
        assert!((get("ema3").points[1].1 - 100.5).abs() < 1e-9);
        assert!(get("avg").points.iter().all(|p| p.1 == 104.5));
        assert_eq!(get("avg").unit.label(), "$");
        assert_eq!(get("pairs").points, [(1.0, 5.0), (2.0, 6.0)]);
        // A shorter result lines up with the newest points.
        assert_eq!(get("lagged").points.first(), Some(&(1.0, 101.0)));
        // Interpolated onto ETH's times (inside "other"'s range only).
        let o = get("other_at");
        assert_eq!(o.points.first(), Some(&(1.0, 1.5)));
        // …and just past its newest point, its last value holds.
        assert_eq!(o.points.len(), 9);
        assert_eq!(o.points.last(), Some(&(9.0, 9.0)));
        assert_eq!(
            CustomLine::parse("ema30:USD = ema(values(\"ETH\"), 30)")
                .unwrap()
                .to_spec(),
            "ema30:USD = ema(values(\"ETH\"), 30)"
        );
        assert!(CustomLine::parse("no equals").is_err());
    }

    #[test]
    fn series_by_name_work_point_by_point() {
        let host = ScriptHost::new(None, false).unwrap();
        let mut d = ds();
        let lines: Vec<CustomLine> = [
            "double = ETH * 2",
            "above = ETH - mean(\"ETH\")",
            "flat = mean(\"ETH\")",
        ]
        .iter()
        .map(|s| CustomLine::parse(s).unwrap())
        .collect();
        assert!(apply(&host, &lines, &mut d, false).is_empty());
        let get = |n: &str| d.series.iter().find(|s| s.name == n).unwrap().clone();
        assert_eq!(get("double").points.len(), 10);
        assert_eq!(get("double").points[3], (3.0, 206.0));
        assert_eq!(get("above").points[0], (0.0, -4.5));
        assert!(get("flat").points.iter().all(|p| p.1 == 104.5));
    }
}
