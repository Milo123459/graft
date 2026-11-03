//! In-memory storage of parsed series.

use indexmap::IndexMap;

use crate::cli::{Window, XMode};
use crate::parse::{Parsed, Skip, XVal};
use crate::units::{Dim, Unit};

/// What the x axis represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum XKind {
    /// Line / sample number.
    #[default]
    Index,
    /// Seconds since the epoch (wall-clock).
    Time,
    /// Some other number captured from the input.
    Number,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    pub unit: Unit,
    /// Points sorted by x.
    pub points: Vec<(f64, f64)>,
}

impl Series {
    pub fn new(name: impl Into<String>, unit: Unit) -> Self {
        Self {
            name: name.into(),
            unit,
            points: Vec::new(),
        }
    }

    /// Points with `x0 <= x <= x1`.
    pub fn range(&self, x0: f64, x1: f64) -> &[(f64, f64)] {
        let a = self.points.partition_point(|p| p.0 < x0);
        let b = self.points.partition_point(|p| p.0 <= x1);
        &self.points[a..b.max(a)]
    }

    pub fn in_view(&self, view: Option<(f64, f64)>) -> &[(f64, f64)] {
        match view {
            Some((a, b)) => self.range(a, b),
            None => &self.points,
        }
    }

    /// Add a point; with `replace`, one at an x already there replaces it (an API that
    /// returns the same candles each poll, the newest still changing).
    fn push_or_replace(&mut self, x: f64, y: f64, replace: bool) {
        if replace {
            let i = self.points.partition_point(|p| p.0 < x);
            if let Some(p) = self.points.get_mut(i)
                && p.0 == x
            {
                p.1 = y;
                return;
            }
        }
        self.push(x, y);
    }

    fn push(&mut self, x: f64, y: f64) {
        match self.points.last() {
            Some(&(lx, _)) if x < lx => {
                let i = self.points.partition_point(|p| p.0 <= x);
                self.points.insert(i, (x, y));
            }
            _ => self.points.push((x, y)),
        }
    }
}

/// A set of series ready for display/analysis.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dataset {
    pub x_kind: XKind,
    pub series: Vec<Series>,
}

impl Dataset {
    pub fn get(&self, name: &str) -> Option<&Series> {
        self.series.iter().find(|s| s.name == name)
    }

    /// Overall x range across all series.
    pub fn x_range(&self) -> Option<(f64, f64)> {
        self.series
            .iter()
            .filter_map(|s| Some((s.points.first()?.0, s.points.last()?.0)))
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
    }

    pub fn is_empty(&self) -> bool {
        self.series.iter().all(|s| s.points.is_empty())
    }

    /// Format an x value for display.
    pub fn fmt_x(&self, x: f64, span: f64) -> String {
        match self.x_kind {
            XKind::Time => crate::time::format_time(x, span),
            _ => crate::units::fmt_compact(x),
        }
    }

    pub fn fmt_x_full(&self, x: f64) -> String {
        match self.x_kind {
            XKind::Time => crate::time::format_time_full(x),
            XKind::Index => format!("#{}", x as i64),
            XKind::Number => crate::units::fmt_compact(x),
        }
    }
}

/// Context the input source attaches to every line.
#[derive(Debug, Clone, Copy)]
pub struct LineMeta {
    /// When the line was read.
    pub arrival: f64,
    /// For `--cmd`: which run produced the line.
    pub sample: Option<SampleInfo>,
}

#[derive(Debug, Clone, Copy)]
pub struct SampleInfo {
    pub index: u64,
    pub time: f64,
}

/// Accumulates parsed lines into series.
#[derive(Debug)]
pub struct Store {
    pub x_kind: Option<XKind>,
    pub series: IndexMap<String, Series>,
    mode: XMode,
    default_unit: Option<(Dim, f64)>,
    window: Option<Window>,
    next_index: u64,
    /// Bumped whenever data changes, so views know when to rebuild.
    pub revision: u64,
    /// `--replace`: a point at an x already seen replaces it.
    pub replace: bool,
}

impl Store {
    pub fn new(mode: XMode, default_unit: Option<(Dim, f64)>, window: Option<Window>) -> Self {
        Self {
            x_kind: None,
            series: IndexMap::new(),
            mode,
            default_unit,
            window,
            next_index: 0,
            revision: 0,
            replace: false,
        }
    }

    /// Add a parsed line, returning the x value it was placed at.
    pub fn ingest(&mut self, mut parsed: Parsed, meta: &LineMeta) -> Result<f64, Skip> {
        // Several records on one line: each in turn.
        let more = std::mem::take(&mut parsed.more);
        let mut x = self.ingest_one(parsed, meta)?;
        for m in more {
            x = self.ingest_one(m, meta)?;
        }
        Ok(x)
    }

    fn ingest_one(&mut self, parsed: Parsed, meta: &LineMeta) -> Result<f64, Skip> {
        let sample_time = meta.sample.map(|s| (XKind::Time, s.time));
        let (kind, x) = match self.mode {
            XMode::Auto => match parsed.x {
                Some(XVal::Time(t)) => (XKind::Time, t),
                Some(XVal::Num(n)) => (XKind::Number, n),
                None => sample_time.unwrap_or((XKind::Index, self.next_index as f64)),
            },
            XMode::Time => match parsed.x {
                Some(XVal::Time(t)) => (XKind::Time, t),
                _ => sample_time
                    .ok_or_else(|| Skip::Invalid("no timestamp on line (--x time)".into()))?,
            },
            XMode::Index => (
                XKind::Index,
                meta.sample.map_or(self.next_index, |s| s.index) as f64,
            ),
            XMode::Arrival => (XKind::Time, meta.arrival),
        };
        if meta.sample.is_none() {
            self.next_index += 1;
        }
        // One x axis per chart. Timestamps turning up after untimed lines (a header, a
        // "boot" line) take over: those points were only line numbers. Once on a time
        // axis, lines without one are skipped rather than plotted at "line 3".
        match self.x_kind {
            None => self.x_kind = Some(kind),
            Some(k) if k == kind => {}
            Some(XKind::Index) if kind == XKind::Time => {
                self.series.clear();
                self.x_kind = Some(kind);
            }
            Some(k) => {
                return Err(Skip::Invalid(format!(
                    "x is {} here, but the chart's x axis is {}",
                    kind_name(kind),
                    kind_name(k)
                )));
            }
        }
        for p in parsed.points {
            let (value, unit) = match &self.default_unit {
                Some((dim, mult)) if p.unit.is_none() => (p.value * mult, Unit::new(dim.clone())),
                _ => (p.value, p.unit),
            };
            match self.series.get_mut(&p.series) {
                Some(series) => series.push_or_replace(x, value, self.replace),
                None => {
                    let mut series = Series::new(p.series.clone(), unit);
                    series.push(x, value);
                    self.series.insert(p.series, series);
                }
            }
        }
        self.prune();
        self.revision += 1;
        Ok(x)
    }

    /// Drop data outside the window (amortised: only once enough has accumulated).
    fn prune(&mut self) {
        let Some(window) = self.window else { return };
        for s in self.series.values_mut() {
            let excess = match window {
                Window::Points(n) => s.points.len().saturating_sub(n),
                Window::Span(span) => {
                    let Some(&(last, _)) = s.points.last() else {
                        continue;
                    };
                    s.points.partition_point(|p| p.0 < last - span)
                }
            };
            if excess > 0 && (excess >= 64 || excess * 8 >= s.points.len()) {
                s.points.drain(..excess);
            }
        }
    }

    /// Snapshot of the raw data, with the window applied exactly.
    pub fn dataset(&self) -> Dataset {
        let mut series: Vec<Series> = self.series.values().cloned().collect();
        if let Some(window) = self.window {
            for s in &mut series {
                let cut = match window {
                    Window::Points(n) => s.points.len().saturating_sub(n),
                    Window::Span(span) => match s.points.last() {
                        Some(&(last, _)) => s.points.partition_point(|p| p.0 < last - span),
                        None => 0,
                    },
                };
                s.points.drain(..cut);
            }
        }
        Dataset {
            x_kind: self.x_kind.unwrap_or_default(),
            series,
        }
    }
}

fn kind_name(k: XKind) -> &'static str {
    match k {
        XKind::Time => "a time",
        XKind::Index => "a line number",
        XKind::Number => "a number",
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn replace_updates_a_point_at_the_same_time() {
        let meta = LineMeta {
            arrival: 0.0,
            sample: None,
        };
        let mut s = Store::new(XMode::Auto, None, None);
        s.replace = true;
        s.ingest(line(Some(XVal::Num(1.0)), &[("c", 10.0)]), &meta)
            .unwrap();
        s.ingest(line(Some(XVal::Num(2.0)), &[("c", 11.0)]), &meta)
            .unwrap();
        // The same candles again, the newest one changed.
        s.ingest(line(Some(XVal::Num(1.0)), &[("c", 10.0)]), &meta)
            .unwrap();
        s.ingest(line(Some(XVal::Num(2.0)), &[("c", 12.0)]), &meta)
            .unwrap();
        assert_eq!(s.series["c"].points, [(1.0, 10.0), (2.0, 12.0)]);
    }

    use super::*;
    use crate::parse::Point;

    fn line(x: Option<XVal>, vals: &[(&str, f64)]) -> Parsed {
        Parsed {
            more: Vec::new(),
            x,
            points: vals
                .iter()
                .map(|(n, v)| Point {
                    series: n.to_string(),
                    value: *v,
                    unit: Unit::default(),
                })
                .collect(),
        }
    }

    const META: LineMeta = LineMeta {
        arrival: 0.0,
        sample: None,
    };

    #[test]
    fn index_and_time_assignment() {
        let mut s = Store::new(XMode::Auto, None, None);
        s.ingest(line(None, &[("a", 1.0)]), &META).unwrap();
        s.ingest(line(None, &[("a", 2.0)]), &META).unwrap();
        assert_eq!(s.dataset().series[0].points, vec![(0.0, 1.0), (1.0, 2.0)]);

        let mut s = Store::new(XMode::Time, None, None);
        assert!(s.ingest(line(None, &[("a", 1.0)]), &META).is_err());
        s.ingest(line(Some(XVal::Time(10.0)), &[("a", 1.0)]), &META)
            .unwrap();
        s.ingest(line(Some(XVal::Time(5.0)), &[("a", 0.0)]), &META)
            .unwrap();
        assert_eq!(
            s.dataset().series[0].points,
            vec![(5.0, 0.0), (10.0, 1.0)],
            "out-of-order points are sorted"
        );
    }

    #[test]
    fn window_and_default_unit() {
        let mut s = Store::new(
            XMode::Auto,
            Some((Dim::Seconds, 0.5)),
            Some(Window::Points(3)),
        );
        for i in 0..10 {
            s.ingest(line(None, &[("a", i as f64)]), &META).unwrap();
        }
        let ds = s.dataset();
        assert_eq!(
            ds.series[0].points.iter().map(|p| p.1).collect::<Vec<_>>(),
            vec![3.5, 4.0, 4.5]
        );
        assert_eq!(ds.series[0].unit.dim, Dim::Seconds);
    }
}
