//! Descriptive statistics, linear trends and forecasts.

use std::fmt::Write;

use crate::data::{Dataset, Series, XKind};
use crate::time::{auto_per, fmt_duration, format_time_full, per_label};

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

impl Stats {
    pub fn compute(points: &[(f64, f64)]) -> Option<Self> {
        let n = points.len();
        if n == 0 {
            return None;
        }
        let nf = n as f64;
        let mut ys: Vec<f64> = points.iter().map(|p| p.1).collect();
        let sum: f64 = ys.iter().sum();
        let mean = sum / nf;
        let var = ys.iter().map(|y| (y - mean).powi(2)).sum::<f64>() / nf;
        // Regression with x centred to keep precision with epoch timestamps.
        let mx = points.iter().map(|p| p.0).sum::<f64>() / nf;
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for &(x, y) in points {
            sxy += (x - mx) * (y - mean);
            sxx += (x - mx).powi(2);
        }
        let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
        let intercept = mean - slope * mx;
        let ss_tot = var * nf;
        let ss_res: f64 = points
            .iter()
            .map(|&(x, y)| (y - (slope * x + intercept)).powi(2))
            .sum();
        let r2 = if ss_tot > 0.0 {
            (1.0 - ss_res / ss_tot).clamp(0.0, 1.0)
        } else {
            1.0
        };
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

    /// Value of the fitted trend line at `x`.
    pub fn trend_at(&self, x: f64) -> f64 {
        self.slope * x + self.intercept
    }

    /// How far past the last point (in x units) the trend reaches `target`, if ever.
    pub fn eta(&self, target: f64) -> Option<f64> {
        if self.slope == 0.0 || !self.slope.is_finite() {
            return None;
        }
        let x = (target - self.intercept) / self.slope;
        (x > self.x_last).then_some(x - self.x_last)
    }

    /// Is the change over the whole span negligible relative to the values?
    pub fn is_flat(&self) -> bool {
        let change = (self.slope * (self.x_last - self.x_first)).abs();
        let scale = self
            .mean
            .abs()
            .max(self.max - self.min)
            .max(f64::MIN_POSITIVE);
        change < scale * 0.005
    }
}

pub struct ReportOpts {
    /// Express slopes per this many x units (seconds for time axes).
    pub per: Option<f64>,
    /// Forecast target; defaults to 0 for falling, positive series.
    pub target: Option<f64>,
    pub color: bool,
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

/// Describe the trend of one series in a sentence.
pub fn describe(ds: &Dataset, s: &Series, st: &Stats, opts: &ReportOpts) -> String {
    let (per, label) = slope_unit(ds, opts.per);
    let mut out = String::new();
    let rate = st.slope * per;
    if st.count < 2 {
        let _ = write!(
            out,
            "{}: a single value ({})",
            s.name,
            s.unit.format(st.last)
        );
        return out;
    }
    if st.is_flat() {
        let _ = write!(
            out,
            "{}: roughly flat around {} (trend {} {}/{label})",
            s.name,
            s.unit.format(st.mean),
            sign(rate),
            s.unit.format(rate.abs())
        );
    } else {
        let dir = if rate < 0.0 { "falling" } else { "rising" };
        let _ = write!(
            out,
            "{}: {dir} by an average of {}/{label}",
            s.name,
            s.unit.format(rate.abs())
        );
    }
    let _ = write!(out, " (R² {:.2}", st.r2);
    if st.r2 < 0.3 {
        out.push_str(", noisy — weak trend");
    }
    out.push(')');

    let target = opts
        .target
        .or_else(|| (st.slope < 0.0 && st.last > 0.0).then_some(0.0));
    if let Some(target) = target
        && !st.is_flat()
    {
        match st.eta(target) {
            Some(dx) => {
                let when = match ds.x_kind {
                    XKind::Time => format!(
                        "~{} (around {})",
                        fmt_duration(dx),
                        format_time_full(((st.x_last + dx) / 60.0).round() * 60.0)
                    ),
                    XKind::Index => format!("~{} more samples", dx.ceil()),
                    XKind::Number => {
                        format!("at x ≈ {}", crate::units::fmt_compact(st.x_last + dx))
                    }
                };
                let _ = write!(out, "; reaches {} in {when}", s.unit.format(target));
            }
            None if opts.target.is_some() => {
                let _ = write!(out, "; moving away from {}", s.unit.format(target));
            }
            None => {}
        }
    }
    out
}

fn sign(v: f64) -> &'static str {
    if v < 0.0 { "-" } else { "+" }
}

/// A table of stats plus one trend sentence per series.
pub fn report(ds: &Dataset, opts: &ReportOpts) -> String {
    let (per, label) = slope_unit(ds, opts.per);
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
    ];
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut sentences = Vec::new();
    for s in &ds.series {
        let Some(st) = Stats::compute(&s.points) else {
            continue;
        };
        let f = |v: f64| s.unit.format(v);
        let trend = st.slope * per;
        let (trend, r2) = if st.count < 2 {
            ("—".to_string(), "—".to_string())
        } else {
            (
                format!("{}{}", sign(trend), f(trend.abs())),
                format!("{:.2}", st.r2),
            )
        };
        rows.push(vec![
            s.name.clone(),
            st.count.to_string(),
            f(st.last),
            f(st.min),
            f(st.mean),
            f(st.max),
            f(st.stddev),
            trend,
            r2,
        ]);
        sentences.push(describe(ds, s, &st, opts));
    }
    if rows.is_empty() {
        return "no data\n".into();
    }
    let widths: Vec<usize> = (0..headers.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain([headers[i].chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let (bold, dim, reset) = if opts.color {
        ("\x1b[1m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "")
    };
    let mut out = String::new();
    let fmt_row = |cells: &[String], out: &mut String| {
        for (i, c) in cells.iter().enumerate() {
            let pad = widths[i] - c.chars().count();
            if i == 0 {
                let _ = write!(out, "{c}{}", " ".repeat(pad));
            } else {
                let _ = write!(out, "  {}{c}", " ".repeat(pad));
            }
        }
        out.push('\n');
    };
    out.push_str(bold);
    fmt_row(&headers.map(String::from), &mut out);
    out.push_str(reset);
    for r in &rows {
        fmt_row(r, &mut out);
    }
    out.push('\n');
    for s in sentences {
        let _ = writeln!(out, "{dim}•{reset} {s}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::{Dim, Unit};

    #[test]
    fn regression_and_eta() {
        let pts: Vec<(f64, f64)> = (0..10)
            .map(|i| (i as f64, 100.0 - 2.0 * i as f64))
            .collect();
        let st = Stats::compute(&pts).unwrap();
        assert!((st.slope + 2.0).abs() < 1e-9);
        assert!((st.r2 - 1.0).abs() < 1e-9);
        assert_eq!(st.median, 91.0);
        // 82 at x=9, hits 0 at x=50.
        assert!((st.eta(0.0).unwrap() - 41.0).abs() < 1e-9);
        assert_eq!(st.eta(200.0), None);
    }

    #[test]
    fn percentiles() {
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.5), 2.5);
        assert_eq!(percentile(&[5.0], 0.95), 5.0);
    }

    #[test]
    fn describes_falling_series_per_day() {
        let day = 86400.0;
        let s = Series {
            name: "node2".into(),
            unit: Unit::new(Dim::Bytes { binary: false }),
            points: (0..5)
                .map(|i| (1e9 + i as f64 * day, 400e9 - 6e9 * i as f64))
                .collect(),
        };
        let ds = Dataset {
            x_kind: XKind::Time,
            series: vec![s],
        };
        let st = Stats::compute(&ds.series[0].points).unwrap();
        let text = describe(
            &ds,
            &ds.series[0],
            &st,
            &ReportOpts {
                per: None,
                target: None,
                color: false,
            },
        );
        assert!(
            text.starts_with("node2: falling by an average of 6 GB/day"),
            "{text}"
        );
        assert!(text.contains("reaches 0 B in ~62.7 days"), "{text}");
    }
}
