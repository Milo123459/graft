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
    for line in lines {
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
    let pick = match len {
        Some(n) => order
            .iter()
            .find(|o| o.2.points.len() == n)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::XKind;

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
}
