//! The series pipeline: derived series → select/exclude → rate/delta → resample → smooth.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use regex::Regex;
use rhai::AST;

use crate::cli::{Agg, Cli, Resample};
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
                if name.is_empty() || expr.is_empty() {
                    bail!("--series '{spec}' should look like NAME=EXPR");
                }
                let ast = script
                    .compile_expr(expr)
                    .with_context(|| format!("in --series '{spec}'"))?;
                Ok(Derived {
                    name: name.to_string(),
                    source: expr.to_string(),
                    ast,
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
    pub fn apply(&self, mut ds: Dataset, script: &ScriptHost) -> Dataset {
        let derived: Vec<Series> = self
            .derived
            .iter()
            .map(|d| derive(&ds, d, script))
            .collect();
        ds.series
            .retain(|s| !self.derived.iter().any(|d| d.name == s.name));
        ds.series.extend(derived);
        let time = ds.x_kind == XKind::Time;
        for s in &mut ds.series {
            if self.rate {
                rate(s, time);
            }
            if self.delta {
                delta(s);
            }
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
    // Guess the unit from the first series mentioned in the expression.
    let unit = ds
        .series
        .iter()
        .filter_map(|s| d.source.find(&ident(&s.name)).map(|pos| (pos, &s.unit)))
        .min_by_key(|(pos, _)| *pos)
        .map(|(_, u)| u.clone())
        .unwrap_or_default();
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
