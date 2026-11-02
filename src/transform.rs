//! The series pipeline: derived series → select/exclude → rate/delta → resample → smooth.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use regex::Regex;
use rhai::AST;

use crate::cli::{Agg, Cli, Resample, Window};
use crate::data::{Dataset, Series, XKind};
use crate::script::{ScriptHost, ident};
use crate::units::Unit;

/// A `*`/`?` glob compiled to a regex.
#[derive(Debug, Clone)]
pub struct Glob(Regex);

impl Glob {
    pub fn new(pattern: &str) -> Result<Self> {
        let re = regex::escape(pattern)
            .replace(r"\*", ".*")
            .replace(r"\?", ".");
        Ok(Self(Regex::new(&format!("^{re}$"))?))
    }

    pub fn matches(&self, s: &str) -> bool {
        self.0.is_match(s)
    }
}

pub struct Derived {
    pub name: String,
    pub source: String,
    pub ast: AST,
    /// From `NAME:UNIT = EXPR`; otherwise guessed from the first series used.
    pub unit: Option<Unit>,
}

pub struct Pipeline {
    select: Vec<Glob>,
    exclude: Vec<Glob>,
    derived: Vec<Derived>,
    rate: bool,
    delta: bool,
    resample: Option<Resample>,
    smooth: Option<usize>,
}

impl Pipeline {
    pub fn from_cli(cli: &Cli, script: &ScriptHost) -> Result<Self> {
        let globs = |v: &[String]| {
            v.iter()
                .filter(|s| !s.is_empty())
                .map(|s| Glob::new(s))
                .collect::<Result<Vec<_>>>()
        };
        let derived = cli
            .derived
            .iter()
            .map(|spec| {
                let (name, expr) = spec
                    .split_once('=')
                    .with_context(|| format!("--series '{spec}' should look like NAME=EXPR"))?;
                let (name, expr) = (name.trim(), expr.trim());
                let (name, unit) = match name.split_once(':') {
                    Some((n, u)) => {
                        let u = u.trim();
                        let unit = crate::units::lookup_unit(u, cli.binary_units).map_or_else(
                            || Unit::new(crate::units::Dim::Other(u.to_string())),
                            |(dim, _)| Unit::new(dim),
                        );
                        (n.trim(), Some(unit))
                    }
                    None => (name, None),
                };
                if name.is_empty() || expr.is_empty() {
                    bail!("--series '{spec}' should look like NAME=EXPR (or NAME:UNIT=EXPR)");
                }
                let ast = script
                    .compile_expr(expr)
                    .with_context(|| format!("in --series '{spec}'"))?;
                Ok(Derived {
                    name: name.to_string(),
                    source: expr.to_string(),
                    ast,
                    unit,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if cli.smooth == Some(0) {
            bail!("--smooth must be at least 1");
        }
        Ok(Self {
            select: globs(&cli.select)?,
            exclude: globs(&cli.exclude)?,
            derived,
            rate: cli.rate,
            delta: cli.delta,
            resample: cli.resample,
            smooth: cli.smooth,
        })
    }

    /// Add derived series and apply transforms to every series.
    /// Rate/delta, then derived series (so `busy = user / total` works on rates),
    /// then resampling and smoothing.
    pub fn apply(&self, mut ds: Dataset, script: &ScriptHost) -> Dataset {
        let time = ds.x_kind == XKind::Time;
        for s in &mut ds.series {
            if self.rate {
                rate(s, time);
            }
            if self.delta {
                delta(s);
            }
        }
        let derived: Vec<Series> = self
            .derived
            .iter()
            .map(|d| derive(&ds, d, script))
            .collect();
        ds.series
            .retain(|s| !self.derived.iter().any(|d| d.name == s.name));
        ds.series.extend(derived);
        for s in &mut ds.series {
            if let Some(r) = self.resample {
                resample(s, r);
            }
            if let Some(n) = self.smooth {
                smooth(s, n);
            }
        }
        ds
    }

    /// The series to display: `--select` matches (in that order) plus derived series,
    /// minus `--exclude` matches.
    pub fn select(&self, ds: &Dataset) -> Dataset {
        let is_derived = |name: &str| self.derived.iter().any(|d| d.name == name);
        let mut series: Vec<Series> = ds
            .series
            .iter()
            .filter(|s| {
                self.select.is_empty()
                    || is_derived(&s.name)
                    || self.select.iter().any(|g| g.matches(&s.name))
            })
            .filter(|s| !self.exclude.iter().any(|g| g.matches(&s.name)))
            .cloned()
            .collect();
        if !self.select.is_empty() {
            let rank = |s: &Series| {
                self.select
                    .iter()
                    .position(|g| g.matches(&s.name))
                    .unwrap_or(usize::MAX)
            };
            series.sort_by_key(rank);
        }
        Dataset {
            x_kind: ds.x_kind,
            series,
        }
    }
}

/// A timeframe: the part of the data that's shown and analysed.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Timeframe {
    /// The most recent duration (relative to the newest point) or number of points.
    pub last: Option<Window>,
    pub since: Option<f64>,
    pub until: Option<f64>,
}

impl Timeframe {
    pub fn is_all(&self) -> bool {
        *self == Self::default()
    }

    pub fn last(w: Window) -> Self {
        Self {
            last: Some(w),
            ..Self::default()
        }
    }

    /// Restrict every series to the timeframe.
    pub fn apply(&self, ds: &mut Dataset) {
        if self.is_all() {
            return;
        }
        let newest = ds
            .series
            .iter()
            .filter_map(|s| {
                s.points
                    .iter()
                    .rev()
                    .find(|p| self.until.is_none_or(|u| p.0 <= u))
                    .map(|p| p.0)
            })
            .reduce(f64::max);
        for s in &mut ds.series {
            let a = self
                .since
                .map_or(0, |t| s.points.partition_point(|p| p.0 < t));
            let b = self
                .until
                .map_or(s.points.len(), |t| s.points.partition_point(|p| p.0 <= t));
            let mut pts = &s.points[a..b.max(a)];
            match self.last {
                Some(Window::Points(n)) => pts = &pts[pts.len().saturating_sub(n)..],
                Some(Window::Span(d)) => {
                    if let Some(newest) = newest {
                        pts = &pts[pts.partition_point(|p| p.0 < newest - d)..];
                    }
                }
                None => {}
            }
            s.points = pts.to_vec();
        }
    }

    /// Short label, e.g. `7d`, `500 pts`, `since 10-15`, or `all`.
    pub fn label(&self, ds: &Dataset) -> String {
        let t = |x: f64| ds.fmt_x(x, 86400.0 * 10.0);
        match (self.last, self.since, self.until) {
            (Some(Window::Span(d)), ..) => crate::time::fmt_duration_compact(d),
            (Some(Window::Points(n)), ..) => format!("{n} pts"),
            (None, Some(a), Some(b)) => format!("{} → {}", t(a), t(b)),
            (None, Some(a), None) => format!("since {}", t(a)),
            (None, None, Some(b)) => format!("until {}", t(b)),
            (None, None, None) => "all".into(),
        }
    }
}

/// Timeframe presets that make sense for a dataset (shorter than its span), smallest first.
pub fn timeframe_presets(ds: &Dataset) -> Vec<Window> {
    let Some((a, b)) = ds.x_range() else {
        return Vec::new();
    };
    match ds.x_kind {
        XKind::Time => [
            3600.0,
            6.0 * 3600.0,
            86400.0,
            7.0 * 86400.0,
            30.0 * 86400.0,
            90.0 * 86400.0,
            365.0 * 86400.0,
        ]
        .into_iter()
        .filter(|d| *d < b - a)
        .map(Window::Span)
        .collect(),
        _ => {
            let n = ds.series.iter().map(|s| s.points.len()).max().unwrap_or(0);
            [50, 100, 500, 1000, 5000, 10_000]
                .into_iter()
                .filter(|p| *p < n)
                .map(Window::Points)
                .collect()
        }
    }
}

/// Evaluate a derived series at every x where its inputs line up.
fn derive(ds: &Dataset, d: &Derived, script: &ScriptHost) -> Series {
    let mut rows: BTreeMap<u64, (f64, Vec<(String, f64)>)> = BTreeMap::new();
    for s in &ds.series {
        let var = ident(&s.name);
        for &(x, y) in &s.points {
            // Order-preserving key for non-negative and negative floats alike.
            let bits = x.to_bits();
            let key = if x.is_sign_negative() {
                !bits
            } else {
                bits | (1 << 63)
            };
            rows.entry(key)
                .or_insert_with(|| (x, Vec::new()))
                .1
                .push((var.clone(), y));
        }
    }
    // The given unit, or a guess from the first series mentioned in the expression.
    let unit = d.unit.clone().unwrap_or_else(|| {
        ds.series
            .iter()
            .filter_map(|s| d.source.find(&ident(&s.name)).map(|pos| (pos, &s.unit)))
            .min_by_key(|(pos, _)| *pos)
            .map(|(_, u)| u.clone())
            .unwrap_or_default()
    });
    let points = rows
        .into_values()
        .filter_map(|(x, vars)| {
            script
                .eval_number(&d.ast, &vars)
                .ok()
                .filter(|v| v.is_finite())
                .map(|v| (x, v))
        })
        .collect();
    Series {
        name: d.name.clone(),
        unit,
        points,
    }
}

/// Per-second (or per-x-unit) rate of change.
pub fn rate(s: &mut Series, time: bool) {
    s.points = s
        .points
        .windows(2)
        .filter(|w| w[1].0 > w[0].0)
        .map(|w| (w[1].0, (w[1].1 - w[0].1) / (w[1].0 - w[0].0)))
        .collect();
    if time {
        s.unit = Unit {
            rate: true,
            ..s.unit.clone()
        };
    }
}

pub fn delta(s: &mut Series) {
    s.points = s
        .points
        .windows(2)
        .map(|w| (w[1].0, w[1].1 - w[0].1))
        .collect();
}

pub fn resample(s: &mut Series, r: Resample) {
    let mut out: Vec<(f64, f64)> = Vec::new();
    let mut bucket: Vec<f64> = Vec::new();
    let mut current: Option<f64> = None;
    let flush = |b: f64, vals: &mut Vec<f64>, out: &mut Vec<(f64, f64)>| {
        if vals.is_empty() {
            return;
        }
        let v = match r.agg {
            Agg::Mean => vals.iter().sum::<f64>() / vals.len() as f64,
            Agg::Min => vals.iter().copied().fold(f64::INFINITY, f64::min),
            Agg::Max => vals.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            Agg::Sum => vals.iter().sum(),
            Agg::Last => *vals.last().expect("non-empty"),
            Agg::Count => vals.len() as f64,
        };
        out.push((b, v));
        vals.clear();
    };
    for &(x, y) in &s.points {
        let b = (x / r.width).floor() * r.width;
        if current != Some(b) {
            if let Some(c) = current {
                flush(c, &mut bucket, &mut out);
            }
            current = Some(b);
        }
        bucket.push(y);
    }
    if let Some(c) = current {
        flush(c, &mut bucket, &mut out);
    }
    if r.agg == Agg::Count {
        s.unit = Unit::default();
    }
    s.points = out;
}

/// Trailing moving average.
pub fn smooth(s: &mut Series, n: usize) {
    let mut sum = 0.0;
    let ys: Vec<f64> = s.points.iter().map(|p| p.1).collect();
    for (i, p) in s.points.iter_mut().enumerate() {
        sum += ys[i];
        if i >= n {
            sum -= ys[i - n];
        }
        p.1 = sum / (i + 1).min(n) as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(pts: &[(f64, f64)]) -> Series {
        Series {
            name: "s".into(),
            unit: Unit::default(),
            points: pts.to_vec(),
        }
    }

    #[test]
    fn timeframes() {
        let s = |name: &str, xs: &[f64]| Series {
            name: name.into(),
            unit: Unit::default(),
            points: xs.iter().map(|&x| (x, x)).collect(),
        };
        let ds = Dataset {
            x_kind: XKind::Time,
            series: vec![s("a", &[0.0, 10.0, 20.0, 30.0]), s("b", &[5.0, 25.0])],
        };
        let xs = |d: &Dataset| {
            d.series
                .iter()
                .map(|s| s.points.iter().map(|p| p.0).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        };
        let mut d = ds.clone();
        Timeframe::last(Window::Span(10.0)).apply(&mut d);
        assert_eq!(
            xs(&d),
            [vec![20.0, 30.0], vec![25.0]],
            "relative to the newest point overall"
        );
        let mut d = ds.clone();
        Timeframe {
            since: Some(5.0),
            until: Some(25.0),
            last: None,
        }
        .apply(&mut d);
        assert_eq!(xs(&d), [vec![10.0, 20.0], vec![5.0, 25.0]]);
        let mut d = ds.clone();
        Timeframe {
            last: Some(Window::Points(1)),
            until: Some(20.0),
            since: None,
        }
        .apply(&mut d);
        assert_eq!(xs(&d), [vec![20.0], vec![5.0]]);
        assert_eq!(
            timeframe_presets(&ds),
            Vec::<Window>::new(),
            "30 s of data: no presets"
        );
        let long = Dataset {
            x_kind: XKind::Time,
            series: vec![s("a", &[0.0, 2.0 * 86400.0])],
        };
        assert_eq!(
            timeframe_presets(&long),
            [
                Window::Span(3600.0),
                Window::Span(21600.0),
                Window::Span(86400.0)
            ]
        );
    }

    #[test]
    fn glob_matching() {
        let g = Glob::new("Mem*").unwrap();
        assert!(g.matches("MemFree") && !g.matches("SwapFree"));
        assert!(Glob::new("node?").unwrap().matches("node1"));
        assert!(
            Glob::new("a.b").unwrap().matches("a.b") && !Glob::new("a.b").unwrap().matches("axb")
        );
    }

    #[test]
    fn rate_delta_smooth_resample() {
        let mut s = series(&[(0.0, 0.0), (2.0, 10.0), (4.0, 30.0)]);
        rate(&mut s, true);
        assert_eq!(s.points, vec![(2.0, 5.0), (4.0, 10.0)]);
        assert!(s.unit.rate);

        let mut s = series(&[(0.0, 1.0), (1.0, 4.0), (2.0, 9.0)]);
        delta(&mut s);
        assert_eq!(s.points, vec![(1.0, 3.0), (2.0, 5.0)]);

        let mut s = series(&[(0.0, 2.0), (1.0, 4.0), (2.0, 6.0)]);
        smooth(&mut s, 2);
        assert_eq!(s.points, vec![(0.0, 2.0), (1.0, 3.0), (2.0, 5.0)]);

        let mut s = series(&[(0.0, 1.0), (30.0, 3.0), (60.0, 10.0), (61.0, 2.0)]);
        resample(
            &mut s,
            Resample {
                width: 60.0,
                agg: Agg::Max,
            },
        );
        assert_eq!(s.points, vec![(0.0, 3.0), (60.0, 10.0)]);
    }
}
