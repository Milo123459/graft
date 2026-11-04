//! Descriptive statistics, trend fits, targets and forecasts.
//!
//! Nothing here is assumed silently: the fit model (`--fit`), the data it's fitted
//! to (`--fit-window`), the unit trends are expressed in (`--per`) and the forecast
//! targets (`--target`) can all be chosen. The only default is that a series which
//! is falling gets a forecast for when it reaches 0 — `--target none` turns that off.

use std::collections::HashMap;
use std::fmt::Write;

use anyhow::{Context, Result, bail};

use crate::cli::{FitModel, StatsFormat, Window};
use crate::data::{Dataset, Series, XKind};
use crate::time::{auto_per, fmt_duration, format_time_full, per_label};
use crate::units::{Unit, fmt_compact, parse_value};

#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub count: usize,
    pub first: f64,
    pub last: f64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub median: f64,
    pub stddev: f64,
    pub p95: f64,
    pub sum: f64,
    /// Least-squares slope, in value units per x unit (per second for time).
    pub slope: f64,
    pub intercept: f64,
    /// Coefficient of determination of the linear fit (0..1).
    pub r2: f64,
    pub x_first: f64,
    pub x_last: f64,
}

/// Linear-interpolated percentile of sorted data (`q` in 0..=1).
pub fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

/// Least squares `y = a + b·x` with x centred for precision. Returns (a, b, r²).
fn least_squares(points: impl Iterator<Item = (f64, f64)> + Clone) -> Option<(f64, f64, f64)> {
    let n = points.clone().count();
    if n == 0 {
        return None;
    }
    let nf = n as f64;
    let (sx, sy) = points
        .clone()
        .fold((0.0, 0.0), |(sx, sy), (x, y)| (sx + x, sy + y));
    let (mx, my) = (sx / nf, sy / nf);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (x, y) in points.clone() {
        sxy += (x - mx) * (y - my);
        sxx += (x - mx).powi(2);
        syy += (y - my).powi(2);
    }
    let b = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    let a = my - b * mx;
    let ss_res: f64 = points.map(|(x, y)| (y - (a + b * x)).powi(2)).sum();
    let r2 = if syy > 0.0 {
        (1.0 - ss_res / syy).clamp(0.0, 1.0)
    } else {
        1.0
    };
    Some((a, b, r2))
}

impl Stats {
    pub fn compute(points: &[(f64, f64)]) -> Option<Self> {
        let n = points.len();
        let (intercept, slope, r2) = least_squares(points.iter().copied())?;
        let mut ys: Vec<f64> = points.iter().map(|p| p.1).collect();
        let sum: f64 = ys.iter().sum();
        let mean = sum / n as f64;
        let var = ys.iter().map(|y| (y - mean).powi(2)).sum::<f64>() / n as f64;
        ys.sort_by(f64::total_cmp);
        Some(Self {
            count: n,
            first: points[0].1,
            last: points[n - 1].1,
            min: ys[0],
            max: ys[n - 1],
            mean,
            median: percentile(&ys, 0.5),
            stddev: var.sqrt(),
            p95: percentile(&ys, 0.95),
            sum,
            slope,
            intercept,
            r2,
            x_first: points[0].0,
            x_last: points[n - 1].0,
        })
    }

    /// Value of the fitted linear trend at `x`.
    pub fn trend_at(&self, x: f64) -> f64 {
        self.slope * x + self.intercept
    }

    /// How far past the last point (in x units) the linear trend reaches `target`.
    pub fn eta(&self, target: f64) -> Option<f64> {
        if self.slope == 0.0 || !self.slope.is_finite() {
            return None;
        }
        let x = (target - self.intercept) / self.slope;
        (x > self.x_last).then_some(x - self.x_last)
    }
}

/// A fitted trend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    pub model: FitModel,
    /// Linear: y = a + b·x. Exponential: ln y = a + b·x.
    pub a: f64,
    pub b: f64,
    pub r2: f64,
    pub x_first: f64,
    pub x_last: f64,
    pub count: usize,
}

impl Fit {
    pub fn compute(points: &[(f64, f64)], model: FitModel) -> Option<Self> {
        let (x_first, x_last) = (points.first()?.0, points.last()?.0);
        let (a, b, r2) = match model {
            FitModel::Linear => least_squares(points.iter().copied())?,
            FitModel::Exp => {
                if points.iter().any(|p| p.1 <= 0.0) {
                    return None;
                }
                least_squares(points.iter().map(|&(x, y)| (x, y.ln())))?
            }
        };
        Some(Self {
            model,
            a,
            b,
            r2,
            x_first,
            x_last,
            count: points.len(),
        })
    }

    pub fn at(&self, x: f64) -> f64 {
        match self.model {
            FitModel::Linear => self.a + self.b * x,
            FitModel::Exp => (self.a + self.b * x).exp(),
        }
    }

    /// x distance past the last point at which the trend reaches `target`, if it does.
    pub fn eta(&self, target: f64) -> Option<f64> {
        if self.b == 0.0 || !self.b.is_finite() {
            return None;
        }
        let x = match self.model {
            FitModel::Linear => (target - self.a) / self.b,
            FitModel::Exp if target > 0.0 => (target.ln() - self.a) / self.b,
            FitModel::Exp => return None,
        };
        (x > self.x_last && x.is_finite()).then_some(x - self.x_last)
    }

    /// Change over `per` x units: value units (linear) or a fraction (exponential).
    pub fn change_per(&self, per: f64) -> f64 {
        match self.model {
            FitModel::Linear => self.b * per,
            FitModel::Exp => (self.b * per).exp_m1(),
        }
    }

    /// Is the change over the fitted span negligible relative to the values?
    pub fn is_flat(&self) -> bool {
        let (y0, y1) = (self.at(self.x_first), self.at(self.x_last));
        (y1 - y0).abs() < y0.abs().max(y1.abs()).max(f64::MIN_POSITIVE) * 0.005
    }

    pub fn falling(&self) -> bool {
        self.b < 0.0
    }

    /// Human rate, e.g. `-1.97 GB/day` or `-0.45%/day`.
    pub fn rate_text(&self, unit: &Unit, per: f64, label: &str) -> String {
        let c = self.change_per(per);
        let sign = if c < 0.0 { "-" } else { "+" };
        match self.model {
            FitModel::Linear => format!("{sign}{}/{label}", unit.format(c.abs())),
            FitModel::Exp => format!("{sign}{}%/{label}", crate::units::fmt_sig(c.abs() * 100.0)),
        }
    }
}

/// What a series' forecast aims at.
#[derive(Debug, Clone, Copy, PartialEq)]
enum TargetRule {
    /// 0 if the series is falling and positive, otherwise none.
    Auto,
    Fixed(f64),
    None,
}

/// Forecast targets from `--target`: `50GB`, `node1=50GB`, `none`, `node2=none`.
#[derive(Debug, Clone, PartialEq)]
pub struct Targets {
    default: TargetRule,
    per_series: HashMap<String, TargetRule>,
}

impl Default for Targets {
    fn default() -> Self {
        Self {
            default: TargetRule::Auto,
            per_series: HashMap::new(),
        }
    }
}

impl Targets {
    pub fn parse(specs: &[String], binary: bool) -> Result<Self> {
        let mut t = Self::default();
        for spec in specs.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let rule = |v: &str| -> Result<TargetRule> {
                Ok(match v.trim() {
                    "none" | "off" => TargetRule::None,
                    "auto" => TargetRule::Auto,
                    v => TargetRule::Fixed(
                        parse_value(v, binary)
                            .map(|p| p.0)
                            .with_context(|| format!("--target: '{v}' is not a number"))?,
                    ),
                })
            };
            // A leading number (possibly negative) is a value, otherwise NAME=VALUE.
            match spec.rsplit_once('=') {
                Some((name, v)) if !name.trim().is_empty() => {
                    t.per_series.insert(name.trim().to_string(), rule(v)?);
                }
                Some(_) => bail!("--target '{spec}': expected NAME=VALUE"),
                None => t.default = rule(spec)?,
            }
        }
        Ok(t)
    }

    /// Set (or clear with `None`) one series' target.
    pub fn set(&mut self, name: &str, value: Option<f64>) {
        self.per_series.insert(
            name.to_string(),
            value.map_or(TargetRule::None, TargetRule::Fixed),
        );
    }

    fn rule(&self, name: &str) -> TargetRule {
        self.per_series.get(name).copied().unwrap_or(self.default)
    }

    /// A target the user asked for explicitly (drawn on charts).
    pub fn explicit(&self, name: &str) -> Option<f64> {
        match self.rule(name) {
            TargetRule::Fixed(v) => Some(v),
            _ => None,
        }
    }

    /// Whether this series' target depends on its trend (`auto`).
    pub fn needs_fit(&self, name: &str) -> bool {
        self.rule(name) == TargetRule::Auto
    }

    /// The target to forecast for a series with this trend.
    pub fn resolve(&self, name: &str, fit: Option<&Fit>, last: f64) -> Option<f64> {
        match self.rule(name) {
            TargetRule::Fixed(v) => Some(v),
            TargetRule::None => None,
            // Only for a clear downward trend: forecasting noise to 0 isn't useful.
            TargetRule::Auto => fit
                .filter(|f| f.falling() && !f.is_flat() && f.r2 >= 0.5 && last > 0.0)
                .map(|_| 0.0),
        }
    }

    /// Back to `--target` syntax (for the builder's command line).
    pub fn to_specs(&self) -> Vec<String> {
        let fmt = |r: TargetRule| match r {
            TargetRule::Auto => "auto".to_string(),
            TargetRule::None => "none".to_string(),
            TargetRule::Fixed(v) => fmt_compact(v),
        };
        let mut out = Vec::new();
        if self.default != TargetRule::Auto {
            out.push(fmt(self.default));
        }
        let mut names: Vec<_> = self.per_series.iter().collect();
        names.sort_by(|a, b| a.0.cmp(b.0));
        out.extend(names.into_iter().map(|(n, r)| format!("{n}={}", fmt(*r))));
        out
    }
}

/// How trends and forecasts are computed.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    pub model: FitModel,
    pub window: Option<Window>,
    pub targets: Targets,
    /// Express trends per this many x units (seconds for time axes).
    pub per: Option<f64>,
}

impl Default for Analysis {
    fn default() -> Self {
        Self {
            model: FitModel::Linear,
            window: None,
            targets: Targets::default(),
            per: None,
        }
    }
}

/// Everything known about one series' trend.
#[derive(Debug, Clone)]
pub struct SeriesReport {
    pub name: String,
    pub unit: Unit,
    pub stats: Stats,
    pub fit: Option<Fit>,
    pub target: Option<f64>,
    /// x distance from the last point to reaching the target.
    pub eta: Option<f64>,
    pub x_kind: XKind,
    pub per: f64,
    pub per_label: String,
}

impl Analysis {
    /// Fit a series using the configured model and window.
    pub fn fit(&self, s: &Series) -> Option<Fit> {
        let pts = match self.window {
            Some(w) => w.apply(&s.points),
            None => &s.points,
        };
        if pts.len() < 2 {
            None
        } else {
            Fit::compute(pts, self.model)
        }
    }

    pub fn report(&self, ds: &Dataset, s: &Series) -> Option<SeriesReport> {
        let stats = Stats::compute(&s.points)?;
        let fit = self.fit(s);
        let target = self.targets.resolve(&s.name, fit.as_ref(), stats.last);
        let eta = target.and_then(|t| fit.and_then(|f| f.eta(t)));
        let (per, per_label) = slope_unit(ds, self.per);
        Some(SeriesReport {
            name: s.name.clone(),
            unit: s.unit.clone(),
            stats,
            fit,
            target,
            eta,
            x_kind: ds.x_kind,
            per,
            per_label,
        })
    }
}

/// The slope unit and label for a dataset, e.g. (86400, "day").
pub fn slope_unit(ds: &Dataset, per: Option<f64>) -> (f64, String) {
    match ds.x_kind {
        XKind::Time => {
            let span = ds.x_range().map_or(0.0, |(a, b)| b - a);
            let per = per.unwrap_or_else(|| auto_per(span));
            (per, per_label(per))
        }
        XKind::Index => (per.unwrap_or(1.0), "sample".into()),
        XKind::Number => (per.unwrap_or(1.0), "x".into()),
    }
}

impl SeriesReport {
    fn f(&self, v: f64) -> String {
        self.unit.format(v)
    }

    pub fn trend_text(&self) -> String {
        self.fit
            .map(|f| f.rate_text(&self.unit, self.per, &self.per_label))
            .unwrap_or_else(|| "—".into())
    }

    pub fn direction(&self) -> &'static str {
        match self.fit {
            None => "single value",
            Some(f) if f.is_flat() => "flat",
            Some(f) if f.falling() => "falling",
            Some(_) => "rising",
        }
    }

    /// When the target is reached, as an absolute x.
    pub fn eta_x(&self) -> Option<f64> {
        Some(self.fit?.x_last + self.eta?)
    }

    pub fn eta_text(&self) -> String {
        let Some(dx) = self.eta else {
            return String::new();
        };
        match self.x_kind {
            XKind::Time => fmt_duration(dx),
            XKind::Index => format!("{} samples", dx.ceil()),
            XKind::Number => format!("Δx {}", fmt_compact(dx)),
        }
    }

    pub fn eta_at_text(&self) -> String {
        match (self.eta_x(), self.x_kind) {
            (Some(x), XKind::Time) => format_time_full((x / 60.0).round() * 60.0),
            (Some(x), _) => fmt_compact(x),
            (None, _) => String::new(),
        }
    }

    /// Short forecast cell for tables: `50 GB in 4.4 days`, flagged when the fit is weak.
    pub fn forecast_cell(&self) -> String {
        match (self.target, self.eta) {
            (Some(t), Some(_)) => {
                let weak = if self.fit.is_some_and(|f| f.r2 < 0.3) {
                    " (weak fit)"
                } else {
                    ""
                };
                format!("{} in {}{weak}", self.f(t), self.eta_text())
            }
            (Some(t), None) if self.fit.is_some() => format!("{}: not on course", self.f(t)),
            _ => String::new(),
        }
    }

    /// One sentence describing the trend and forecast.
    pub fn sentence(&self) -> String {
        let mut out = String::new();
        let Some(fit) = self.fit else {
            let _ = write!(
                out,
                "{}: {} value(s), last {}",
                self.name,
                self.stats.count,
                self.f(self.stats.last)
            );
            return out;
        };
        let model = if fit.model == FitModel::Exp {
            "exponential fit, "
        } else {
            ""
        };
        if fit.is_flat() {
            let _ = write!(
                out,
                "{}: roughly flat around {} (trend {}, {model}",
                self.name,
                self.f(self.stats.mean),
                self.trend_text()
            );
        } else {
            let rate = self.trend_text();
            let _ = write!(
                out,
                "{}: {} by an average of {} ({model}",
                self.name,
                self.direction(),
                rate.trim_start_matches(['-', '+'])
            );
        }
        let _ = write!(out, "R² {:.2}", fit.r2);
        if fit.r2 < 0.3 {
            out.push_str(", noisy — weak trend");
        }
        out.push(')');
        if let Some(t) = self.target {
            let current = fit.at(fit.x_last);
            match self.eta {
                Some(_) => {
                    let _ = write!(
                        out,
                        "; reaches {} in about {} (around {})",
                        self.f(t),
                        self.eta_text(),
                        self.eta_at_text()
                    );
                }
                None if fit.is_flat() => {
                    let _ = write!(out, "; flat, so not heading for {}", self.f(t));
                }
                None => {
                    let side = if current < t { "below" } else { "above" };
                    let _ = write!(out, "; {side} {} and {}", self.f(t), self.direction());
                }
            }
        }
        out
    }

    fn field(&self, name: &str, raw: bool) -> Option<String> {
        let st = &self.stats;
        let num = |v: f64| if raw { v.to_string() } else { self.f(v) };
        let opt = |v: Option<f64>| v.map(num).unwrap_or_default();
        Some(match name {
            "name" => self.name.clone(),
            "n" | "count" => st.count.to_string(),
            "first" => num(st.first),
            "last" => num(st.last),
            "min" => num(st.min),
            "max" => num(st.max),
            "mean" => num(st.mean),
            "median" => num(st.median),
            "stddev" => num(st.stddev),
            "p95" => num(st.p95),
            "sum" | "total" => num(st.sum),
            "change" => num(st.last - st.first),
            "trend" if raw => self
                .fit
                .map(|f| f.change_per(self.per).to_string())
                .unwrap_or_default(),
            "trend" => self.trend_text(),
            "per" => self.per_label.clone(),
            "r2" => self.fit.map(|f| format!("{:.2}", f.r2)).unwrap_or_default(),
            "target" => opt(self.target),
            "eta" if raw => self.eta.map(|e| e.to_string()).unwrap_or_default(),
            "eta" => self.eta_text(),
            "eta_at" => self.eta_at_text(),
            "direction" => self.direction().into(),
            "model" => self
                .fit
                .map(|f| crate::cli::fit_name(f.model))
                .unwrap_or_default(),
            "unit" => self.unit.label(),
            _ => return None,
        })
    }

    /// Fill a `{field}` / `{field:raw}` template.
    pub fn fill(&self, template: &str) -> Result<String> {
        let mut out = String::new();
        let mut rest = template;
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start].replace("}}", "}"));
            let after = &rest[start + 1..];
            if let Some(stripped) = after.strip_prefix('{') {
                out.push('{');
                rest = stripped;
                continue;
            }
            let end = after
                .find('}')
                .context("unclosed '{' in --stats-template")?;
            let (name, raw) = match after[..end].split_once(':') {
                Some((n, "raw")) => (n, true),
                Some((_, m)) => bail!("unknown modifier ':{m}' in --stats-template (only :raw)"),
                None => (&after[..end], false),
            };
            out.push_str(
                &self
                    .field(name.trim(), raw)
                    .with_context(|| format!("unknown field '{{{name}}}' in --stats-template"))?,
            );
            rest = &after[end + 1..];
        }
        out.push_str(&rest.replace("}}", "}"));
        Ok(out)
    }

    pub fn to_json(&self) -> serde_json::Value {
        let st = &self.stats;
        serde_json::json!({
            "name": self.name,
            "unit": self.unit.label(),
            "n": st.count,
            "first": st.first, "last": st.last, "min": st.min, "max": st.max,
            "mean": st.mean, "median": st.median, "stddev": st.stddev, "p95": st.p95,
            "trend": self.fit.map(|f| serde_json::json!({
                "model": crate::cli::fit_name(f.model),
                "per_seconds": self.per,
                "per": self.per_label,
                "change_per": f.change_per(self.per),
                "r2": f.r2,
                "fitted_points": f.count,
            })),
            "direction": self.direction(),
            "target": self.target,
            "eta_seconds": self.eta,
            "eta_at": (!self.eta_at_text().is_empty()).then(|| self.eta_at_text()),
        })
    }
}

#[derive(Clone)]
pub struct ReportOpts {
    pub analysis: Analysis,
    pub color: bool,
    pub format: StatsFormat,
    pub template: Option<String>,
    /// Fit the table to this many columns, dropping the least important ones.
    pub width: Option<usize>,
}

/// Stats for every series, as a table with sentences, JSON, or a custom template.
pub fn report(ds: &Dataset, opts: &ReportOpts) -> Result<String> {
    let reports: Vec<SeriesReport> = ds
        .series
        .iter()
        .filter_map(|s| opts.analysis.report(ds, s))
        .collect();
    if let Some(t) = &opts.template {
        let mut out = String::new();
        for r in &reports {
            out.push_str(&r.fill(t)?);
            out.push('\n');
        }
        return Ok(out);
    }
    if opts.format == StatsFormat::Json {
        let v: Vec<_> = reports.iter().map(SeriesReport::to_json).collect();
        return Ok(serde_json::to_string_pretty(&v)? + "\n");
    }
    if reports.is_empty() {
        return Ok("no data\n".into());
    }
    let label = &reports[0].per_label;
    let headers = [
        "series",
        "n",
        "last",
        "min",
        "mean",
        "max",
        "σ",
        &format!("trend/{label}"),
        "R²",
        "forecast",
    ];
    let rows: Vec<Vec<String>> = reports
        .iter()
        .map(|r| {
            let f = |v: f64| r.unit.format(v);
            let st = &r.stats;
            vec![
                r.name.clone(),
                st.count.to_string(),
                f(st.last),
                f(st.min),
                f(st.mean),
                f(st.max),
                f(st.stddev),
                r.fit
                    .map(|fit| fit.rate_text(&r.unit, r.per, &r.per_label))
                    .map(|t| t.trim_end_matches(&format!("/{label}")).to_string())
                    .unwrap_or("—".into()),
                r.fit.map(|f| format!("{:.2}", f.r2)).unwrap_or("—".into()),
                r.forecast_cell(),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..headers.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain([headers[i].chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    // Which columns fit, most important first: series, trend, forecast, last, R², ...
    const PRIORITY: [usize; 10] = [0, 7, 9, 2, 8, 4, 3, 5, 6, 1];
    let mut keep = [false; 10];
    let mut used = 0;
    for &c in &PRIORITY {
        let w = widths[c] + if used == 0 { 0 } else { 2 };
        if opts.width.is_none_or(|max| used + w <= max) || c == 0 {
            keep[c] = true;
            used += w;
        }
    }
    let pick = |cells: &[String]| -> Vec<String> {
        cells
            .iter()
            .enumerate()
            .filter(|(i, _)| keep[*i])
            .map(|(_, c)| c.clone())
            .collect()
    };
    let widths: Vec<usize> = widths
        .iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, w)| *w)
        .collect();
    let rows: Vec<Vec<String>> = rows.iter().map(|r| pick(r)).collect();
    let headers: Vec<String> = pick(&headers.map(String::from));
    let (bold, dim, reset) = if opts.color {
        ("\x1b[1m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "")
    };
    let mut out = String::new();
    let fmt_row = |cells: &[String], out: &mut String| {
        let mut line = String::new();
        for (i, c) in cells.iter().enumerate() {
            let pad = " ".repeat(widths[i] - c.chars().count());
            match i {
                0 => line.push_str(&format!("{c}{pad}")),
                // Left-align the last (text) column.
                _ if i == cells.len() - 1 => line.push_str(&format!("  {c}{pad}")),
                _ => line.push_str(&format!("  {pad}{c}")),
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    };
    out.push_str(bold);
    fmt_row(&headers, &mut out);
    out.push_str(reset);
    for r in &rows {
        fmt_row(r, &mut out);
    }
    out.push('\n');
    for r in &reports {
        let _ = writeln!(out, "{dim}•{reset} {}", r.sentence());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::Dim;

    const DAY: f64 = 86400.0;

    fn falling() -> Dataset {
        let s = Series {
            name: "node2".into(),
            unit: Unit::new(Dim::Bytes { binary: false }),
            points: (0..5)
                .map(|i| (1e9 + i as f64 * DAY, 400e9 - 6e9 * i as f64))
                .collect(),
        };
        Dataset {
            x_kind: XKind::Time,
            series: vec![s],
        }
    }

    fn analysis(targets: &[&str]) -> Analysis {
        let targets = Targets::parse(
            &targets.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            false,
        )
        .unwrap();
        Analysis {
            targets,
            ..Analysis::default()
        }
    }

    #[test]
    fn regression_and_eta() {
        let pts: Vec<(f64, f64)> = (0..10)
            .map(|i| (i as f64, 100.0 - 2.0 * i as f64))
            .collect();
        let st = Stats::compute(&pts).unwrap();
        assert!((st.slope + 2.0).abs() < 1e-9);
        assert!((st.r2 - 1.0).abs() < 1e-9);
        assert_eq!(st.median, 91.0);
        assert!((st.eta(0.0).unwrap() - 41.0).abs() < 1e-9);
        assert_eq!(st.eta(200.0), None);
    }

    #[test]
    fn percentiles() {
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.5), 2.5);
        assert_eq!(percentile(&[5.0], 0.95), 5.0);
    }

    #[test]
    fn default_target_is_zero_for_falling_series() {
        let ds = falling();
        let r = analysis(&[]).report(&ds, &ds.series[0]).unwrap();
        let text = r.sentence();
        assert!(
            text.starts_with("node2: falling by an average of 6 GB/day"),
            "{text}"
        );
        assert!(text.contains("reaches 0 B in about 62.7 days"), "{text}");
    }

    #[test]
    fn custom_targets() {
        let ds = falling();
        // 376 GB now, 6 GB/day → 200 GB in 29.3 days.
        let r = analysis(&["node2=200GB"])
            .report(&ds, &ds.series[0])
            .unwrap();
        assert!(
            r.sentence().contains("reaches 200 GB in about 29.3 days"),
            "{}",
            r.sentence()
        );
        let r = analysis(&["none"]).report(&ds, &ds.series[0]).unwrap();
        assert!(!r.sentence().contains("reaches"));
        let r = analysis(&["500GB"]).report(&ds, &ds.series[0]).unwrap();
        assert!(
            r.sentence().contains("below 500 GB and falling"),
            "{}",
            r.sentence()
        );
        let r = analysis(&["300GB", "node2=none"])
            .report(&ds, &ds.series[0])
            .unwrap();
        assert_eq!(r.target, None, "per-series overrides the default");
        assert!(Targets::parse(&["=5".into()], false).is_err());
        assert!(Targets::parse(&["node1=lots".into()], false).is_err());
        let t = Targets::parse(&["-5".into(), "a=none".into()], false).unwrap();
        assert_eq!(t.to_specs(), ["-5", "a=none"]);
    }

    #[test]
    fn exponential_fit_and_window() {
        // Doubles every day.
        let pts: Vec<(f64, f64)> = (0..6).map(|i| (i as f64 * DAY, 2f64.powi(i))).collect();
        let f = Fit::compute(&pts, FitModel::Exp).unwrap();
        assert!((f.change_per(DAY) - 1.0).abs() < 1e-9, "100% per day");
        assert!((f.eta(64.0).unwrap() - DAY).abs() < 1e-6);
        assert!(Fit::compute(&[(0.0, 0.0), (1.0, 1.0)], FitModel::Exp).is_none());
        // Windowed fit only sees the recent rise.
        let s = Series {
            name: "s".into(),
            unit: Unit::default(),
            points: vec![(0.0, 10.0), (1.0, 0.0), (2.0, 1.0), (3.0, 2.0)],
        };
        let a = Analysis {
            window: Some(Window::Points(3)),
            ..Analysis::default()
        };
        assert!((a.fit(&s).unwrap().b - 1.0).abs() < 1e-9);
    }

    #[test]
    fn templates_and_json() {
        let ds = falling();
        let r = analysis(&["node2=200GB"])
            .report(&ds, &ds.series[0])
            .unwrap();
        assert_eq!(
            r.fill("{name} {trend} per {per}; {target} at {eta_at} ({last:raw})")
                .unwrap(),
            format!(
                "node2 -6 GB/day per day; 200 GB at {} (376000000000)",
                r.eta_at_text()
            )
        );
        assert!(r.fill("{nope}").is_err());
        assert_eq!(r.fill("{{literal}}").unwrap(), "{literal}");
        let j = r.to_json();
        assert_eq!(j["target"], 200e9);
        assert_eq!(j["trend"]["model"], "linear");
    }
}
