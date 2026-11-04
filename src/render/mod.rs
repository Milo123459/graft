//! Chart rendering. One ratatui widget renders every chart type; it's used by the
//! interactive viewer and, via an off-screen buffer, by `--once`.

pub mod ansi;
pub mod book;
pub mod panes;
pub mod pixels;
pub mod svg;

use std::cell::RefCell;
use std::collections::HashSet;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Chart, Clear, Dataset as RDataset, GraphType, LegendPosition, Paragraph,
    Sparkline, Widget,
};

use crate::cli::{ChartKind, FitModel, LegendPos, MarkerKind, TrendStyle};
use crate::data::{Dataset, Series, XKind};
use crate::stats::{Analysis, Fit};
use crate::units::Unit;

/// Series colours from the 256-colour palette: vivid on dark backgrounds and not
/// remapped by terminal themes (the basic 16 colours often are, sometimes to dim tones).
pub const DEFAULT_PALETTE: [Color; 12] = [
    Color::Indexed(81),  // sky blue
    Color::Indexed(213), // pink
    Color::Indexed(221), // gold
    Color::Indexed(114), // green
    Color::Indexed(209), // orange
    Color::Indexed(141), // lavender
    Color::Indexed(203), // red
    Color::Indexed(43),  // teal
    Color::Indexed(33),  // blue
    Color::Indexed(186), // khaki
    Color::Indexed(168), // rose
    Color::Indexed(250), // grey
];

#[derive(Debug, Clone, PartialEq)]
pub struct HLine {
    pub value: f64,
    pub color: Option<Color>,
    pub label: Option<String>,
}

/// How far ahead to project trends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Horizon {
    /// Far enough to reach the target if that's within 4× the data span, else half the span.
    Auto,
    Span(f64),
}

impl Horizon {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(if s == "auto" {
            Self::Auto
        } else {
            Self::Span(crate::time::parse_duration_secs(s)?)
        })
    }
}

#[derive(Debug, Clone)]
pub struct ChartOpts {
    pub kind: ChartKind,
    pub marker: MarkerKind,
    pub title: String,
    pub palette: Vec<Color>,
    pub legend: LegendPos,
    pub log: bool,
    pub ymin: Option<f64>,
    pub ymax: Option<f64>,
    pub hlines: Vec<HLine>,
    pub bins: usize,
    /// Draw each series in its own panel.
    pub split: bool,
    /// strftime format for time-axis labels.
    pub x_format: Option<String>,
    /// Trend fitting and targets (targets are drawn; forecasts use the fit).
    pub analysis: Analysis,
    /// Project trends this far into the future.
    pub forecast: Option<Horizon>,
    /// Trend lines drawn over the data (any combination).
    pub trends: Vec<TrendStyle>,
    /// Points in the rolling windows (default: 10% of the data, at least 10).
    pub trend_window: Option<usize>,
    /// Draw the raw data (off for "trend only").
    pub show_raw: bool,
    /// Live data and trends in two charts side by side.
    pub side_by_side: bool,
    /// The right-hand chart's lines in side by side (the left uses `show_raw`/`trends`).
    pub right: crate::cli::Lines,
    /// Several charts in one view (`None`: just this one).
    pub panes: Option<panes::Panes>,
    /// Markers (from `--mark` rules or `mark()`).
    pub marks: Vec<Mark>,
    /// Markers in a strip of their own below the chart (a row per rule), not on the
    /// lines.
    pub marks_below: bool,

    /// Draw explicit target lines.
    pub show_targets: bool,

    /// Short titles: no [log]/[forecast] tags, panes don't repeat the chart's title.
    pub tidy: bool,
    /// Series drawn on top of the others (the first listed topmost).
    pub front: Vec<String>,
}

impl Default for ChartOpts {
    fn default() -> Self {
        Self {
            kind: ChartKind::Line,
            marker: MarkerKind::Braille,
            title: String::new(),
            palette: DEFAULT_PALETTE.to_vec(),
            legend: LegendPos::TopRight,
            log: false,
            ymin: None,
            ymax: None,
            hlines: Vec::new(),
            bins: 20,
            split: false,
            x_format: None,
            analysis: Analysis::default(),
            forecast: None,
            trends: Vec::new(),
            trend_window: None,
            show_raw: true,
            side_by_side: false,
            right: crate::cli::Lines::default_right(),
            panes: None,
            marks: Vec::new(),
            marks_below: false,
            show_targets: true,
            tidy: false,
            front: Vec::new(),
        }
    }
}

impl ChartOpts {
    pub fn color(&self, idx: usize) -> Color {
        self.palette[idx % self.palette.len()]
    }

    /// Points in the rolling windows for a series of `n` points.
    pub fn window_for(&self, n: usize) -> usize {
        self.trend_window
            .unwrap_or((n / 10).clamp(10, 500))
            .clamp(1, n.max(1))
    }

    /// A trend layer for a series, evaluated at up to `max_out` points.
    pub fn trend_points(
        &self,
        s: &Series,
        style: TrendStyle,
        max_out: usize,
    ) -> Option<Vec<(f64, f64)>> {
        match style {
            TrendStyle::Fit => {
                let fit = self.analysis.fit(s)?;
                Some(projection_points(&fit, fit.x_first, fit.x_last))
            }
            rolling => {
                let n = s.points.len();
                (n >= 2).then(|| rolling_stat(&s.points, self.window_for(n), rolling, max_out))
            }
        }
    }

    /// A trend layer's value at x (for the cursor readout).
    pub fn trend_at(&self, s: &Series, style: TrendStyle, x: f64) -> Option<f64> {
        match style {
            TrendStyle::Fit => {
                let fit = self.analysis.fit(s)?;
                (fit.x_first..=fit.x_last).contains(&x).then(|| fit.at(x))
            }
            rolling => {
                let n = s.points.len();
                if n < 2 || x < s.points[0].0 || x > s.points[n - 1].0 {
                    return None;
                }
                let i = s.points.partition_point(|p| p.0 < x).min(n - 1);
                Some(window_stat(&s.points, i, self.window_for(n), rolling))
            }
        }
    }

    /// The latest value of a trend layer.
    pub fn trend_last(&self, s: &Series, style: TrendStyle) -> Option<f64> {
        let last = s.points.last()?.0;
        self.trend_at(s, style, last)
    }

    /// The projection for one series: its fit and how far ahead to draw it.
    pub fn projection(&self, s: &Series) -> Option<(Fit, f64)> {
        let horizon = self.forecast?;
        let fit = self.analysis.fit(s)?;
        let span = (fit.x_last - s.points.first()?.0).max(f64::MIN_POSITIVE);
        let ahead = match horizon {
            Horizon::Span(h) => h,
            Horizon::Auto => {
                let target = self
                    .analysis
                    .targets
                    .resolve(&s.name, Some(&fit), s.points.last()?.1);
                match target.and_then(|t| fit.eta(t)) {
                    Some(eta) if eta <= span * 4.0 => eta * 1.1,
                    _ => span * 0.5,
                }
            }
        };
        (ahead > 0.0).then_some((fit, ahead))
    }
}

/// A plotted area on screen and the x range it shows (for mapping taps to x).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlotArea {
    pub rect: Rect,
    pub x0: f64,
    pub x1: f64,
}

impl PlotArea {
    /// The x value under a terminal column.
    pub fn x_at(&self, col: u16) -> f64 {
        let frac =
            (f64::from(col.saturating_sub(self.rect.x)) + 0.5) / f64::from(self.rect.width.max(1));
        self.x0 + frac.clamp(0.0, 1.0) * (self.x1 - self.x0)
    }

    /// The terminal column showing x.
    pub fn col_of(&self, x: f64) -> u16 {
        let frac = ((x - self.x0) / (self.x1 - self.x0)).clamp(0.0, 1.0);
        self.rect.x
            + ((frac * f64::from(self.rect.width)) as u16).min(self.rect.width.saturating_sub(1))
    }

    pub fn contains(&self, col: u16, row: u16) -> bool {
        self.rect
            .contains(ratatui::layout::Position { x: col, y: row })
    }
}

/// Interactive state drawn on top of the chart.
#[derive(Debug, Clone, Copy, Default)]
pub struct Overlay<'a> {
    /// Visible x range; `None` fits all data (plus any forecast).
    pub view: Option<(f64, f64)>,
    pub cursor: Option<f64>,
    /// A range being selected (start fixed, end at cursor) or selected.
    pub selection: Option<(f64, f64)>,
    pub hidden: Option<&'a HashSet<String>>,
    /// Receives the plot areas drawn (one per panel).
    pub plots: Option<&'a RefCell<Vec<PlotArea>>>,
    /// The focused pane, outlined when there are several.
    pub pane_focus: Option<usize>,
    /// Data for panes with a `source=` of their own.
    pub sources: Option<&'a [SourceView]>,
    /// Every series read, before `--select` (the `book` chart finds its levels here).
    pub full: Option<&'a Dataset>,
    /// Draw lines as pixels (kitty graphics): the pictures go here instead of text.
    pub pixels: Option<&'a pixels::Pixels>,
}

/// Most rows the markers strip takes (the rest share the last one).
const MAX_LANES: usize = 6;

/// The markers strip row a marker goes in.
fn lane_of(m: &Mark) -> &str {
    m.lane.as_deref().unwrap_or("marks")
}

/// A marker on the chart: at a point of a series, or (no series) a whole moment.
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub series: Option<String>,
    pub x: f64,
    pub y: Option<f64>,
    pub label: String,
    /// The rule that placed it (index), or `None` when placed by hand.
    pub rule: Option<usize>,
    pub symbol: String,
    /// `None`: the series' colour (or yellow for a marker placed by hand).
    pub color: Option<Color>,
    /// Text effects for its symbol and label.
    pub effects: Modifier,
    /// The label on the marker's colour, like a tag.
    pub badge: bool,
    pub bg: Option<Color>,
    /// Its row in the markers strip below the chart (`--marks-below`): the rule's
    /// name, the strategy… (`None`: "marks").
    pub lane: Option<String>,
}

/// What a pane's own source has produced so far.
#[derive(Debug, Clone)]
pub struct SourceView {
    pub name: String,
    pub ds: Dataset,
    /// The source's own chart settings (title, units, y range…).
    pub opts: ChartOpts,
    pub error: Option<String>,
}

impl Overlay<'_> {
    fn is_hidden(&self, name: &str) -> bool {
        self.hidden.is_some_and(|h| h.contains(name))
    }
}

fn marker(m: MarkerKind) -> Marker {
    match m {
        MarkerKind::Braille => Marker::Braille,
        MarkerKind::Dot => Marker::Dot,
        MarkerKind::Block => Marker::Block,
        MarkerKind::HalfBlock => Marker::HalfBlock,
        MarkerKind::Bar => Marker::Bar,
    }
}

fn legend(l: LegendPos) -> Option<LegendPosition> {
    Some(match l {
        LegendPos::TopRight => LegendPosition::TopRight,
        LegendPos::TopLeft => LegendPosition::TopLeft,
        LegendPos::BottomRight => LegendPosition::BottomRight,
        LegendPos::BottomLeft => LegendPosition::BottomLeft,
        LegendPos::Top => LegendPosition::Top,
        LegendPos::Bottom => LegendPosition::Bottom,
        LegendPos::None => return None,
    })
}

/// The x range of the data alone.
pub fn x_bounds(ds: &Dataset, view: Option<(f64, f64)>) -> (f64, f64) {
    let (a, b) = view.or_else(|| ds.x_range()).unwrap_or((0.0, 1.0));
    if b > a { (a, b) } else { (a - 0.5, b + 0.5) }
}

/// The x range to plot: the view, or all data extended by any forecast.
pub fn plot_x_bounds(
    ds: &Dataset,
    opts: &ChartOpts,
    view: Option<(f64, f64)>,
    hidden: Option<&HashSet<String>>,
) -> (f64, f64) {
    let (a, mut b) = x_bounds(ds, view);
    if view.is_none()
        && opts.forecast.is_some()
        && matches!(opts.kind, ChartKind::Line | ChartKind::Scatter)
    {
        for s in ds
            .series
            .iter()
            .filter(|s| !hidden.is_some_and(|h| h.contains(&s.name)))
        {
            if let Some((fit, ahead)) = opts.projection(s) {
                b = b.max(fit.x_last + ahead);
            }
        }
    }
    (a, b)
}

/// The unit shared by all given series, or none if they differ.
pub fn common_unit<'a>(mut series: impl Iterator<Item = &'a Series>) -> Unit {
    let Some(first) = series.next() else {
        return Unit::default();
    };
    if series.all(|s| s.unit == first.unit) {
        first.unit.clone()
    } else {
        Unit::default()
    }
}

/// Points in `[x0, x1]` plus one neighbour on each side (so lines reach the edges),
/// reduced to roughly `buckets` min/max pairs when there are too many to draw.
pub fn visible_points(s: &Series, x0: f64, x1: f64, buckets: usize) -> Vec<(f64, f64)> {
    let a = s.points.partition_point(|p| p.0 < x0).saturating_sub(1);
    let b = (s.points.partition_point(|p| p.0 <= x1) + 1).min(s.points.len());
    let pts = &s.points[a..b.max(a)];
    if pts.len() <= buckets * 2 || buckets == 0 {
        return pts.to_vec();
    }
    let width = (x1 - x0) / buckets as f64;
    let mut out = Vec::with_capacity(buckets * 2 + 2);
    let mut i = 0;
    while i < pts.len() {
        let bucket = ((pts[i].0 - x0) / width).floor();
        let start = i;
        while i < pts.len() && ((pts[i].0 - x0) / width).floor() == bucket {
            i += 1;
        }
        let chunk = &pts[start..i];
        let (lo, hi) = chunk.iter().fold((chunk[0], chunk[0]), |(lo, hi), &p| {
            (
                if p.1 < lo.1 { p } else { lo },
                if p.1 > hi.1 { p } else { hi },
            )
        });
        if lo.0 <= hi.0 {
            out.extend([lo, hi]);
        } else {
            out.extend([hi, lo]);
        }
    }
    out.dedup();
    out
}

/// One trend line of a series: (series index, series, style, points).
type TrendLayer<'a> = (usize, &'a Series, TrendStyle, Vec<(f64, f64)>);
/// Points to draw, their style, and an optional legend label.
type StyledLine = (Vec<(f64, f64)>, Style, Option<String>);

/// A rolling statistic around index `i`.
///
/// Median and p95 look at the `window` points centred on `i`. The average is weighted
/// towards the centre (a Gaussian with σ = window/3, reaching out one window each side):
/// a plain boxcar mean jumps by spike/window every time a spike enters or leaves the
/// window, which draws a staircase; weighting lets spikes fade in and out instead.
fn window_stat(points: &[(f64, f64)], i: usize, window: usize, style: TrendStyle) -> f64 {
    let n = points.len();
    let window = window.max(1);
    match style {
        TrendStyle::Median | TrendStyle::P95 => {
            let half = window / 2;
            let (a, b) = (i.saturating_sub(half), (i + half + 1).min(n));
            let mut v: Vec<f64> = points[a..b].iter().map(|p| p.1).collect();
            v.sort_by(f64::total_cmp);
            crate::stats::percentile(&v, if style == TrendStyle::P95 { 0.95 } else { 0.5 })
        }
        _ => {
            let sigma = (window as f64 / 3.0).max(0.5);
            let (a, b) = (i.saturating_sub(window), (i + window + 1).min(n));
            let (mut sum, mut weights) = (0.0, 0.0);
            for (j, p) in points.iter().enumerate().take(b).skip(a) {
                let d = (j as f64 - i as f64) / sigma;
                let w = (-0.5 * d * d).exp();
                sum += w * p.1;
                weights += w;
            }
            sum / weights
        }
    }
}

/// A centred rolling statistic, evaluated at up to `max_out` evenly spaced points.
pub fn rolling_stat(
    points: &[(f64, f64)],
    window: usize,
    style: TrendStyle,
    max_out: usize,
) -> Vec<(f64, f64)> {
    let n = points.len();
    let step = (n / max_out.max(1)).max(1);
    let mut out: Vec<(f64, f64)> = (0..n)
        .step_by(step)
        .map(|i| (points[i].0, window_stat(points, i, window, style)))
        .collect();
    if !(n - 1).is_multiple_of(step) {
        out.push((points[n - 1].0, window_stat(points, n - 1, window, style)));
    }
    out
}

/// A lighter tint of a series colour (median / p95 lines).
pub fn light_color(c: Color) -> Color {
    match c {
        Color::Indexed(81) => Color::Indexed(153),
        Color::Indexed(213) => Color::Indexed(219),
        Color::Indexed(221) => Color::Indexed(229),
        Color::Indexed(114) => Color::Indexed(157),
        Color::Indexed(209) => Color::Indexed(216),
        Color::Indexed(141) => Color::Indexed(183),
        Color::Indexed(203) => Color::Indexed(210),
        Color::Indexed(43) => Color::Indexed(122),
        Color::Indexed(33) => Color::Indexed(75),
        Color::Indexed(186) => Color::Indexed(230),
        Color::Indexed(168) => Color::Indexed(218),
        Color::Rgb(r, g, b) => Color::Rgb(r / 2 + 128, g / 2 + 128, b / 2 + 128),
        _ => Color::White,
    }
}

/// Split a polyline into dashes: every other run of `run` points.
fn dash_polyline(pts: &[(f64, f64)], run: usize) -> Vec<Vec<(f64, f64)>> {
    let run = run.max(2);
    pts.chunks(run)
        .step_by(2)
        .map(|c| c.to_vec())
        .filter(|c| c.len() >= 2)
        .collect()
}

/// Points along a fitted trend from `from` to `to`.
pub fn projection_points(fit: &Fit, from: f64, to: f64) -> Vec<(f64, f64)> {
    let steps = if fit.model == FitModel::Linear { 1 } else { 48 };
    (0..=steps)
        .map(|i| from + (to - from) * i as f64 / steps as f64)
        .map(|x| (x, fit.at(x)))
        .collect()
}

/// A horizontal bar of `len` cells, with eighth-block precision at the end.
fn eighths_bar(len: f64) -> String {
    let full = len.floor() as usize;
    let rest = ((len - full as f64) * 8.0).round() as usize;
    let mut s = "█".repeat(full);
    if rest > 0 {
        s.push(['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'][rest - 1]);
    }
    if s.is_empty() {
        s.push('▏');
    }
    s
}

/// Split a curve over `[from, to]` into dashes (every other piece of `pieces`).
pub fn dashed(f: impl Fn(f64) -> f64, from: f64, to: f64, pieces: usize) -> Vec<Vec<(f64, f64)>> {
    let pieces = pieces.max(2);
    let step = (to - from) / pieces as f64;
    (0..pieces)
        .step_by(2)
        .map(|k| {
            let a = from + step * k as f64;
            (0..=3)
                .map(|j| a + step * j as f64 / 3.0)
                .map(|x| (x, f(x)))
                .collect()
        })
        .collect()
}

/// A cursor position rounded to what's meaningful for the data's span.
pub fn round_x(ds: &Dataset, x: f64) -> f64 {
    match (ds.x_kind, ds.x_range()) {
        (XKind::Time, Some((a, b))) if b - a > 7200.0 => (x / 60.0).round() * 60.0,
        (XKind::Time, _) => x.round(),
        _ => x,
    }
}

/// Padded y bounds for a set of values, honouring explicit overrides.
pub fn y_bounds(
    values: impl Iterator<Item = f64>,
    ymin: Option<f64>,
    ymax: Option<f64>,
) -> (f64, f64) {
    let (data_lo, data_hi) = min_max(values).unwrap_or((0.0, 1.0));
    let pad = if data_hi - data_lo < f64::EPSILON * data_hi.abs().max(1.0) {
        if data_lo == 0.0 {
            1.0
        } else {
            data_lo.abs() * 0.05
        }
    } else {
        (data_hi - data_lo) * 0.05
    };
    // Don't pad non-negative data (sizes, counts, …) into negative territory.
    let lo = if data_lo >= 0.0 {
        (data_lo - pad).max(0.0)
    } else {
        data_lo - pad
    };
    (ymin.unwrap_or(lo), ymax.unwrap_or(data_hi + pad))
}

/// In split panels, only let a reference line stretch the y axis when it's near the
/// panel's own data, so a threshold meant for one series doesn't flatten another.
pub fn hline_near(v: f64, lo: f64, hi: f64) -> bool {
    let reach = (hi - lo).max(hi.abs().max(lo.abs()) * 0.25);
    v >= lo - reach && v <= hi + reach
}

/// A polyline's value at `x` (linear between its points), if `x` is within it.
pub fn interpolate(pts: &[(f64, f64)], x: f64) -> Option<f64> {
    let i = pts.partition_point(|p| p.0 < x);
    match (i.checked_sub(1).and_then(|j| pts.get(j)), pts.get(i)) {
        (_, Some(b)) if b.0 == x => Some(b.1),
        (Some(a), Some(b)) if b.0 > a.0 => Some(a.1 + (b.1 - a.1) * (x - a.0) / (b.0 - a.0)),
        _ => None,
    }
}

/// A series' value at `x` for lining sources up: interpolated between its points, and
/// after its newest point the last value holds (until it's older than a few of its
/// usual gaps — a source that stopped isn't carried on forever). `None` before it began.
pub fn value_near(pts: &[(f64, f64)], x: f64) -> Option<f64> {
    if let Some(v) = interpolate(pts, x) {
        return Some(v);
    }
    let &(last_x, last_y) = pts.last()?;
    if x < last_x {
        return None;
    }
    let n = pts.len();
    let gap = if n >= 2 {
        let mut gaps: Vec<f64> = pts[n.saturating_sub(20)..]
            .windows(2)
            .map(|w| w[1].0 - w[0].0)
            .filter(|g| *g > 0.0)
            .collect();
        gaps.sort_by(f64::total_cmp);
        gaps.get(gaps.len() / 2).copied().unwrap_or(1.0)
    } else {
        1.0
    };
    (x - last_x <= gap * 4.0).then_some(last_y)
}

/// Min/max of finite values.
pub fn min_max(values: impl Iterator<Item = f64>) -> Option<(f64, f64)> {
    let (lo, hi) = values
        .filter(|v| v.is_finite())
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
    lo.is_finite().then_some((lo, hi))
}

/// `n` evenly spaced values from `lo` to `hi` inclusive.
pub fn spaced(lo: f64, hi: f64, n: usize) -> Vec<f64> {
    let n = n.max(2);
    (0..n)
        .map(|i| lo + (hi - lo) * i as f64 / (n - 1) as f64)
        .collect()
}

/// The value of a series at x: the nearest point, or the projected trend past the end.
pub fn value_at(s: &Series, opts: &ChartOpts, x: f64) -> Option<(f64, bool)> {
    let last = s.points.last()?;
    if x > last.0
        && let Some((fit, _)) = opts.projection(s)
    {
        return Some((fit.at(x), true));
    }
    let i = s.points.partition_point(|p| p.0 < x);
    [i.checked_sub(1), Some(i)]
        .into_iter()
        .flatten()
        .filter_map(|i| s.points.get(i))
        .min_by(|a, b| (a.0 - x).abs().total_cmp(&(b.0 - x).abs()))
        .map(|p| (p.1, false))
}

/// A darker shade of a series colour, for the raw data under its (bright) trend line.
pub fn dim_color(c: Color) -> Color {
    match c {
        Color::Indexed(81) => Color::Indexed(31),
        Color::Indexed(213) => Color::Indexed(133),
        Color::Indexed(221) => Color::Indexed(136),
        Color::Indexed(114) => Color::Indexed(65),
        Color::Indexed(209) => Color::Indexed(130),
        Color::Indexed(141) => Color::Indexed(97),
        Color::Indexed(203) => Color::Indexed(124),
        Color::Indexed(43) => Color::Indexed(30),
        Color::Indexed(33) => Color::Indexed(25),
        Color::Indexed(186) => Color::Indexed(101),
        Color::Indexed(168) => Color::Indexed(125),
        Color::Rgb(r, g, b) => Color::Rgb(r / 2, g / 2, b / 2),
        Color::Cyan | Color::LightCyan => Color::Indexed(30),
        Color::Magenta | Color::LightMagenta => Color::Indexed(90),
        Color::Yellow | Color::LightYellow => Color::Indexed(136),
        Color::Green | Color::LightGreen => Color::Indexed(28),
        Color::Red | Color::LightRed => Color::Indexed(88),
        Color::Blue | Color::LightBlue => Color::Indexed(25),
        _ => Color::Indexed(242),
    }
}

/// "Nice" tick values covering `[lo, hi]` with roughly `target` steps.
pub fn nice_ticks(lo: f64, hi: f64, target: usize) -> Vec<f64> {
    let span = hi - lo;
    if span <= 0.0 || !span.is_finite() {
        return vec![lo];
    }
    let raw = span / target.max(1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .iter()
        .map(|m| m * mag)
        .find(|s| span / s <= target as f64)
        .unwrap_or(10.0 * mag);
    let start = (lo / step).ceil() * step;
    // Past float precision (huge values, tiny span) the step can't move `start`.
    if !step.is_finite() || step <= 0.0 || start + step == start {
        return vec![lo, hi];
    }
    let mut out = Vec::new();
    for k in 0..=target.max(1) * 4 + 2 {
        let t = start + k as f64 * step;
        if t > hi + step * 1e-9 {
            break;
        }
        out.push(if t.abs() < step * 1e-9 { 0.0 } else { t });
    }
    out
}

/// Round y ticks in the unit they're displayed in (`100 GB`, `250 ms`, ...).
pub fn y_ticks(y0: f64, y1: f64, target: usize, unit: &Unit, log: bool) -> Vec<(f64, String)> {
    if !y0.is_finite() || !y1.is_finite() {
        return Vec::new();
    }
    if log {
        let (lo, hi) = (y0.ceil() as i64, y1.floor() as i64);
        // Many decades: label every few so the list stays short.
        let every = ((hi - lo) / target.max(1) as i64).max(1);
        let decades: Vec<f64> = (lo..=hi)
            .filter(|e| (e - lo) % every == 0)
            .map(|e| e as f64)
            .collect();
        let ticks = if decades.len() >= 2 {
            decades
        } else {
            spaced(y0, y1, target)
        };
        return ticks
            .into_iter()
            .map(|t| (t, unit.format(10f64.powf(t))))
            .collect();
    }
    let scale = unit.tick_scale(y0.abs().max(y1.abs()));
    // At least two labels: use a finer step when the range only holds one round value.
    let mut ticks = nice_ticks(y0 / scale, y1 / scale, target);
    let mut t = target;
    while ticks.len() < 2 && t < target * 8 {
        t += 1;
        ticks = nice_ticks(y0 / scale, y1 / scale, t);
    }
    ticks
        .into_iter()
        .map(|t| (t * scale, unit.format(t * scale)))
        .collect()
}

/// Round x ticks: calendar-aligned for time (midnights, hours, months), nice numbers otherwise.
pub fn x_ticks(
    ds: &Dataset,
    opts: &ChartOpts,
    x0: f64,
    x1: f64,
    max_labels: usize,
) -> Vec<(f64, String)> {
    let span = x1 - x0;
    let max_labels = max_labels.max(2);
    if ds.x_kind != XKind::Time {
        return nice_ticks(x0, x1, max_labels)
            .into_iter()
            .map(|t| (t, crate::units::fmt_compact(t)))
            .collect();
    }
    const STEPS: [f64; 22] = [
        1.0,
        2.0,
        5.0,
        10.0,
        15.0,
        30.0,
        60.0,
        120.0,
        300.0,
        600.0,
        900.0,
        1800.0,
        3600.0,
        7200.0,
        10800.0,
        21600.0,
        43200.0,
        86400.0,
        172_800.0,
        604_800.0,
        1_209_600.0,
        2_592_000.0,
    ];
    let step = STEPS
        .iter()
        .copied()
        .find(|s| span / s <= max_labels as f64)
        .unwrap_or(f64::INFINITY);
    let to_dt = |x: f64| chrono::DateTime::from_timestamp(x as i64, 0).map(|d| d.naive_utc());
    // (format_time_with tolerates invalid user formats instead of panicking)
    let fmt = |x: f64, f: &str| crate::time::format_time_with(x, f);
    let label = |x: f64| -> String {
        if let Some(f) = &opts.x_format {
            return fmt(x, f);
        }
        let midnight = x.rem_euclid(86400.0) == 0.0;
        match step {
            s if s >= 2_592_000.0 => fmt(
                x,
                if span > 86400.0 * 400.0 {
                    "%Y-%m"
                } else {
                    "%b"
                },
            ),
            s if s >= 86400.0 => fmt(
                x,
                if span > 86400.0 * 300.0 {
                    "%Y-%m-%d"
                } else {
                    "%m-%d"
                },
            ),
            _ if midnight => fmt(x, "%m-%d"),
            s if s >= 60.0 => fmt(x, "%H:%M"),
            _ => fmt(x, "%H:%M:%S"),
        }
    };
    let mut ticks = Vec::new();
    if step.is_infinite() || step >= 2_592_000.0 {
        // Month starts (every 1, 2, 3, 6 or 12 months).
        use chrono::Datelike;
        let months = [1, 2, 3, 6, 12]
            .into_iter()
            .find(|m| span / (2_592_000.0 * *m as f64) <= max_labels as f64)
            .unwrap_or(12);
        let Some(start) = to_dt(x0) else { return ticks };
        let (mut y, mut m) = (start.year(), start.month0() as i32);
        m = (m / months + 1) * months;
        loop {
            let (yy, mm) = (y + m / 12, (m % 12) as u32 + 1);
            let Some(t) =
                chrono::NaiveDate::from_ymd_opt(yy, mm, 1).and_then(|d| d.and_hms_opt(0, 0, 0))
            else {
                break;
            };
            let x = t.and_utc().timestamp() as f64;
            if x > x1 {
                break;
            }
            ticks.push((x, label(x)));
            m += months;
            if m > 12 * 400 {
                break;
            }
        }
        let _ = &mut y;
        return ticks;
    }
    // Weeks start on Monday (the epoch was a Thursday).
    let offset = if step >= 604_800.0 {
        4.0 * 86400.0
    } else {
        0.0
    };
    let mut t = ((x0 - offset) / step).ceil() * step + offset;
    while t <= x1 {
        ticks.push((t, label(t)));
        t += step;
    }
    ticks
}

/// Write axis labels at their true positions (ratatui spaces its own evenly).
/// Ticks for both axes: (value, label).
struct Ticks<'a> {
    x: &'a [(f64, String)],
    y: &'a [(f64, String)],
}

fn draw_axis_labels(
    buf: &mut Buffer,
    inner: Rect,
    area: PlotArea,
    (y0, y1): (f64, f64),
    ticks: Ticks,
) {
    let (plot, x0, x1, yt, xt) = (area.rect, area.x0, area.x1, ticks.y, ticks.x);
    let style = Style::default().fg(Color::Gray);
    // y: right-aligned in the gutter left of the axis.
    let gutter = plot.x.saturating_sub(inner.x + 1);
    let mut last_row: Option<u16> = None;
    for (v, text) in yt {
        let frac = (v - y0) / (y1 - y0);
        if !(-1e-9..=1.0 + 1e-9).contains(&frac) || plot.height == 0 {
            continue;
        }
        let row = plot.y + ((1.0 - frac) * f64::from(plot.height - 1)).round() as u16;
        if last_row.is_some_and(|r| r.abs_diff(row) < 2) {
            continue;
        }
        let w = text.chars().count() as u16;
        if w > gutter {
            continue;
        }
        buf.set_string(inner.x + gutter - w, row, text, style);
        last_row = Some(row);
    }
    // x: centred under their position on the label row, without overlaps.
    let row = inner.bottom().saturating_sub(1);
    let mut next_free = inner.x;
    for (v, text) in xt {
        let frac = (v - x0) / (x1 - x0);
        if !(0.0..=1.0).contains(&frac) {
            continue;
        }
        let col = plot.x + (frac * f64::from(plot.width.saturating_sub(1))).round() as u16;
        let w = text.chars().count() as u16;
        let start = col
            .saturating_sub(w / 2)
            .max(inner.x)
            .min(inner.right().saturating_sub(w));
        if start < next_free {
            continue;
        }
        buf.set_string(start, row, text, style);
        next_free = start + w + 2;
    }
}

/// Which of `names` are a candlestick's open, high, low and close: by name (`open`
/// or `o`, also as the end of a name like `eth.open`), else the first four.
pub fn candle_roles_of(names: &[&str]) -> Option<[usize; 4]> {
    let found = candle_roles_named(names);
    if let [Some(o), Some(h), Some(l), Some(c)] = found {
        return Some([o, h, l, c]);
    }
    (names.len() >= 4).then_some([0, 1, 2, 3])
}

/// Only those found by name (open/o, high/h, low/l, close/c).
pub fn candle_roles_named(names: &[&str]) -> [Option<usize>; 4] {
    const NAMES: [[&str; 2]; 4] = [["open", "o"], ["high", "h"], ["low", "l"], ["close", "c"]];
    let role = |name: &str| {
        let name = name.to_lowercase();
        let tail = name
            .rsplit(['.', '_', ' ', '-'])
            .next()
            .unwrap_or(&name)
            .to_string();
        NAMES
            .iter()
            .position(|n| n.contains(&name.as_str()) || n.contains(&tail.as_str()))
    };
    let mut found: [Option<usize>; 4] = [None; 4];
    for (k, name) in names.iter().enumerate() {
        if let Some(r) = role(name)
            && found[r].is_none()
        {
            found[r] = Some(k);
        }
    }
    found
}

/// The open, high, low and close series of a candlestick chart (see `candle_roles_of`).
fn candle_roles<'a>(visible: &[(usize, &'a Series)]) -> Option<[(usize, &'a Series); 4]> {
    let names: Vec<&str> = visible.iter().map(|v| v.1.name.as_str()).collect();
    candle_roles_of(&names).map(|r| r.map(|k| visible[k]))
}

/// `O $1 H $2 L $0.5 C $1.5` for the candle at `x` (or the one before it), green
/// when it closed up, red when down.
pub fn candle_text([open, high, low, close]: [&Series; 4], x: f64) -> Option<Line<'static>> {
    let at = |s: &Series| {
        let i = s.points.partition_point(|p| p.0 <= x);
        s.points.get(i.checked_sub(1)?).map(|p| p.1)
    };
    let (o, h, l, c) = (at(open)?, at(high)?, at(low)?, at(close)?);
    let color = if c >= o { Color::Green } else { Color::Red };
    let u = &close.unit;
    let mut spans = Vec::new();
    for (k, v) in [("O", o), ("H", h), ("L", l), ("C", c)] {
        spans.push(Span::styled(
            format!("{k} "),
            Style::default().fg(Color::Gray),
        ));
        spans.push(Span::styled(
            format!("{} ", u.format(v)),
            Style::default().fg(color),
        ));
    }
    Some(Line::from(spans))
}

/// Paint candlesticks into a plot: one per column (several candles sharing a
/// column are merged into one), half-block bodies, │ wicks, green up, red down.
fn draw_candles(
    buf: &mut Buffer,
    plot: &PlotArea,
    (y0, y1): (f64, f64),
    [open, high, low, close]: [&Series; 4],
    ty: &dyn Fn(f64) -> f64,
) {
    let r = plot.rect;
    if r.width == 0 || r.height == 0 || y1 <= y0 {
        return;
    }
    let at = |s: &Series, x: f64| {
        let i = s.points.partition_point(|p| p.0 < x);
        s.points.get(i).filter(|p| p.0 == x).map(|p| p.1)
    };
    // (open, high, low, close) per column.
    let mut cols: Vec<Option<(f64, f64, f64, f64)>> = vec![None; r.width as usize];
    for &(x, o) in &open.points {
        if x < plot.x0 || x > plot.x1 {
            continue;
        }
        let (Some(h), Some(l), Some(c)) = (at(high, x), at(low, x), at(close, x)) else {
            continue;
        };
        let k = (plot.col_of(x) - r.x) as usize;
        cols[k] = Some(match cols[k] {
            None => (o, h.max(o).max(c), l.min(o).min(c), c),
            Some((o0, h0, l0, _)) => (o0, h0.max(h), l0.min(l), c),
        });
    }
    let halves = f64::from(r.height) * 2.0;
    // Half-rows from the top (0 = top edge of the plot).
    let half = |v: f64| ((1.0 - (ty(v) - y0) / (y1 - y0)) * halves).clamp(0.0, halves);
    for (k, c) in cols.iter().enumerate() {
        let Some((o, h, l, c)) = *c else {
            continue;
        };
        let color = if c >= o { Color::Green } else { Color::Red };
        let top = half(o.max(c)).floor() as i64;
        let bottom = (half(o.min(c)).ceil() as i64).max(top + 1);
        let (wick_top, wick_bottom) = (half(h).floor() as i64, half(l).ceil() as i64);
        let col = r.x + k as u16;
        for row in 0..r.height {
            let (a, b) = (i64::from(row) * 2, i64::from(row) * 2 + 1);
            let upper = (top..bottom).contains(&a);
            let lower = (top..bottom).contains(&b);
            let symbol = match (upper, lower) {
                (true, true) => "█",
                (true, false) => "▀",
                (false, true) => "▄",
                _ if wick_top <= b && a < wick_bottom => "│",
                _ => continue,
            };
            // A line through here (an EMA, the cursor) stays visible: over a full
            // body its dots go on the candle's colour, elsewhere the line wins.
            let cell = &mut buf[(col, r.y + row)];
            let line_here = cell
                .symbol()
                .chars()
                .next()
                .is_some_and(|ch| ('\u{2801}'..='\u{28FF}').contains(&ch));
            if line_here {
                if symbol == "█" {
                    cell.set_bg(color);
                }
                continue;
            }
            cell.set_symbol(symbol)
                .set_style(Style::default().fg(color));
        }
    }
}

/// Where ratatui's Chart puts the plotting area (mirrors its layout rules).
fn graph_area(inner: Rect, y_labels: &[Line], first_x_label: Option<&Line>) -> Rect {
    let y_w = y_labels.iter().map(Line::width).max().unwrap_or(0) as u16;
    let x_w = first_x_label.map_or(0, |l| (l.width() as u16).saturating_sub(1));
    let left = y_w.max(x_w).min(inner.width / 3) + 1;
    Rect {
        x: inner.x + left,
        y: inner.y,
        width: inner.width.saturating_sub(left),
        height: inner.height.saturating_sub(2),
    }
}

/// A polyline in chart coordinates.
type Line2 = Vec<(f64, f64)>;
/// A series' prepared line: palette index, series, points (y possibly log-scaled).
type Prepared<'a> = (usize, &'a Series, Vec<(f64, f64)>);
/// Reference / cursor / selection / forecast lines: points, style, marker, legend label.
type OverlayLine = (Vec<(f64, f64)>, Style, Marker, Option<String>);

pub struct ChartWidget<'a> {
    pub ds: &'a Dataset,
    pub opts: &'a ChartOpts,
    pub overlay: Overlay<'a>,
}

impl<'a> ChartWidget<'a> {
    pub fn new(ds: &'a Dataset, opts: &'a ChartOpts, overlay: Overlay<'a>) -> Self {
        Self { ds, opts, overlay }
    }

    /// Visible series with their stable palette index.
    fn visible(&self) -> Vec<(usize, &'a Series)> {
        self.ds
            .series
            .iter()
            .enumerate()
            .filter(|(_, s)| !self.overlay.is_hidden(&s.name) && !s.points.is_empty())
            .collect()
    }

    fn block(&self) -> Block<'a> {
        self.block_titled(&self.opts.title)
    }

    fn block_titled(&self, text: &str) -> Block<'a> {
        let mut title = vec![Span::styled(
            format!(" {text} "),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if self.opts.log && !self.opts.tidy {
            title.push(Span::styled("[log] ", Style::default().fg(Color::DarkGray)));
        }
        if self.opts.forecast.is_some() && !self.opts.tidy {
            title.push(Span::styled(
                "[forecast] ",
                Style::default().fg(Color::DarkGray),
            ));
        }
        Block::bordered()
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Line::from(title))
    }

    fn render_empty(&self, area: Rect, buf: &mut Buffer, msg: &str) {
        self.render_empty_titled(area, buf, &self.opts.title, msg);
    }

    fn render_empty_titled(&self, area: Rect, buf: &mut Buffer, title: &str, msg: &str) {
        let block = self.block_titled(title);
        let inner = block.inner(area);
        block.render(area, buf);
        let y = inner.y + inner.height / 2;
        Paragraph::new(Line::from(msg).centered())
            .style(Style::default().fg(Color::DarkGray))
            .render(
                Rect {
                    y,
                    height: 1,
                    ..inner
                },
                buf,
            );
    }

    /// One panel per series, each with its own y axis.
    fn render_split(&self, area: Rect, buf: &mut Buffer) {
        let visible = self.visible();
        if visible.len() < 2 {
            return self.render_xy(area, buf, visible, &self.opts.title, false);
        }
        let rows = Layout::vertical(vec![
            Constraint::Ratio(1, visible.len() as u32);
            visible.len()
        ])
        .split(area);
        for (&(i, s), row) in visible.iter().zip(rows.iter()) {
            self.render_xy(
                *row,
                buf,
                vec![(i, s)],
                &format!("{} · {}", self.opts.title, s.name),
                true,
            );
        }
    }

    fn render_xy(
        &self,
        area: Rect,
        buf: &mut Buffer,
        visible: Vec<(usize, &'a Series)>,
        title: &str,
        panel: bool,
    ) {
        let (x0, x1) = plot_x_bounds(self.ds, self.opts, self.overlay.view, self.overlay.hidden);
        // Markers below: a strip under the chart, a row per lane (when there's room).
        let lanes = if self.opts.marks_below && !panel {
            self.lanes()
        } else {
            Vec::new()
        };
        let rows = lanes.len().min(MAX_LANES) as u16;
        let (area, strip) = if rows > 0 && area.height >= rows + 2 + 8 {
            let h = rows + 2;
            (
                Rect {
                    height: area.height - h,
                    ..area
                },
                Some(Rect {
                    y: area.y + area.height - h,
                    height: h,
                    ..area
                }),
            )
        } else {
            (area, None)
        };
        let buckets = area.width.saturating_sub(14).max(10) as usize * 2;
        // Lines as pixels: a point pair per pixel column instead of per half cell.
        let px = self.overlay.pixels.filter(|_| {
            matches!(
                self.opts.kind,
                ChartKind::Line | ChartKind::Scatter | ChartKind::Candles
            )
        });
        let line_buckets = px.map_or(buckets, |p| {
            ((f64::from(area.width) * p.cell.0) as usize).clamp(buckets, 4000)
        });
        let log = self.opts.log;
        let ty = |v: f64| if log { v.log10() } else { v };
        let to_log = |pts: &mut Vec<(f64, f64)>| {
            if log {
                pts.retain(|p| p.1 > 0.0);
                pts.iter_mut().for_each(|p| p.1 = p.1.log10());
            }
        };

        let mut lines: Vec<Prepared> = visible
            .iter()
            .map(|&(i, s)| {
                let mut pts = visible_points(s, x0, x1, line_buckets);
                to_log(&mut pts);
                (i, s, pts)
            })
            .collect();
        lines.retain(|l| !l.2.is_empty());
        if lines.is_empty() {
            return self.render_empty(
                area,
                buf,
                if self.ds.is_empty() {
                    "waiting for data…"
                } else {
                    "no data in view"
                },
            );
        }
        if area.height < 5 {
            return;
        }

        // Forecast projections and explicit targets, per series.
        // Dashes ≈ every 3 columns of the projected stretch.
        let cols = f64::from(area.width.saturating_sub(12).max(10));
        let mut projections: Vec<(usize, Vec<Line2>)> = Vec::new();
        let mut targets: Vec<(usize, &str, f64)> = Vec::new();
        for &(i, s, _) in &lines {
            // The forecast, from the last point to the right edge.
            if let Some((fit, _)) = self.opts.projection(s) {
                let (from, to) = (fit.x_last.max(x0), x1);
                if to > from {
                    let pieces = (cols * (to - from) / (x1 - x0) / 3.0) as usize;
                    let mut dashes = dashed(|x| fit.at(x), from, to, pieces);
                    dashes.iter_mut().for_each(&to_log);
                    projections.push((i, dashes));
                }
            }
            if let Some(t) = self.opts.analysis.targets.explicit(&s.name)
                && self.opts.show_targets
                && (!log || t > 0.0)
            {
                targets.push((i, &s.name, ty(t)));
            }
        }

        // Trend layers, limited to the view (each evaluated at ~2 points per column).
        let mut trends: Vec<TrendLayer> = Vec::new();
        for &(i, s, _) in &lines {
            for &style in &self.opts.trends {
                let Some(pts) = self.opts.trend_points(s, style, buckets * 4) else {
                    continue;
                };
                let tmp = Series {
                    name: String::new(),
                    unit: s.unit.clone(),
                    points: pts,
                };
                let mut pts = visible_points(&tmp, x0, x1, buckets);
                to_log(&mut pts);
                trends.push((i, s, style, pts));
            }
        }
        let raw_shown = self.opts.show_raw || trends.is_empty();
        // Points in view, plus where each line crosses the view's edges — zoomed in
        // between two samples there may be no point inside at all.
        let in_view = |pts: &[(f64, f64)]| {
            let inside = pts.iter().filter(|p| p.0 >= x0 && p.0 <= x1).map(|p| p.1);
            let edges = [x0, x1].into_iter().filter_map(|x| interpolate(pts, x));
            inside.chain(edges).collect::<Vec<_>>()
        };
        let in_range: Vec<f64> = lines
            .iter()
            .filter(|_| raw_shown)
            .flat_map(|l| in_view(&l.2))
            .chain(trends.iter().flat_map(|t| in_view(&t.3)))
            .chain(
                projections
                    .iter()
                    .flat_map(|p| p.1.iter().flatten().map(|p| p.1)),
            )
            .collect();
        let data = min_max(in_range.iter().copied());
        // A distant target (not reached within the view) would only squash the data;
        // it's still reported in the stats.
        targets.retain(|t| data.is_none_or(|(lo, hi)| hline_near(t.2, lo, hi)));
        let hl = self
            .opts
            .hlines
            .iter()
            .map(|h| h.value)
            .filter(|v| !log || *v > 0.0)
            .map(ty)
            .filter(|&v| !panel || data.is_none_or(|(lo, hi)| hline_near(v, lo, hi)));
        let (y0, y1) = y_bounds(
            in_range
                .iter()
                .copied()
                .chain(hl)
                .chain(targets.iter().map(|t| t.2)),
            self.opts.ymin.filter(|v| !log || *v > 0.0).map(ty),
            self.opts.ymax.filter(|v| !log || *v > 0.0).map(ty),
        );

        let unit = common_unit(lines.iter().map(|l| l.1));
        let ny = (area.height as usize / 4).clamp(2, 6);
        let y_tick_list = y_ticks(y0, y1, ny, &unit, log);
        // ratatui reserves the label gutter and row; the labels themselves are drawn
        // afterwards at their exact positions.
        let gutter = y_tick_list
            .iter()
            .map(|t| t.1.chars().count())
            .max()
            .unwrap_or(1);
        let y_labels: Vec<Line> = vec![Line::from(" ".repeat(gutter)); 2];
        let x_labels: Vec<Line> = vec![Line::from(""); 2];

        // Overlay lines first so data is drawn on top.
        let mut overlays: Vec<OverlayLine> = Vec::new();
        // Overlays that stay text with pixels (the cursor and a selection's edges move
        // with the mouse; as text they don't make the picture be sent again).
        let mut as_text: Vec<bool> = Vec::new();
        for h in &self.opts.hlines {
            if log && h.value <= 0.0 {
                continue;
            }
            let style = Style::default().fg(h.color.unwrap_or(Color::DarkGray));
            overlays.push((
                vec![(x0, ty(h.value)), (x1, ty(h.value))],
                style,
                Marker::Braille,
                h.label.clone(),
            ));
            as_text.push(false);
        }
        for &(i, name, t) in &targets {
            let label = format!(
                "{name} target {}",
                self.ds
                    .get(name)
                    .map_or(String::new(), |s| s.unit.format(if log {
                        10f64.powf(t)
                    } else {
                        t
                    }))
            );
            let style = Style::default().fg(self.opts.color(i));
            // Short dashes; the first carries the legend label.
            for (k, dash) in dashed(|_| t, x0, x1, (cols / 2.0) as usize)
                .into_iter()
                .enumerate()
            {
                overlays.push((
                    dash,
                    style,
                    Marker::Braille,
                    (k == 0).then(|| label.clone()),
                ));
                as_text.push(false);
            }
        }
        if let Some((a, b)) = self.overlay.selection {
            for x in [a, b] {
                overlays.push((
                    vec![(x, y0), (x, y1)],
                    Style::default().fg(Color::Yellow),
                    Marker::Braille,
                    None,
                ));
                as_text.push(true);
            }
        }
        for (i, dashes) in &projections {
            for dash in dashes {
                overlays.push((
                    dash.clone(),
                    Style::default().fg(self.opts.color(*i)),
                    Marker::Braille,
                    None,
                ));
                as_text.push(false);
            }
        }
        if let Some(c) = self.overlay.cursor {
            overlays.push((
                vec![(c, y0), (c, y1)],
                Style::default().fg(Color::White),
                Marker::Braille,
                None,
            ));
            as_text.push(true);
        }

        let graph = if self.opts.kind == ChartKind::Scatter {
            GraphType::Scatter
        } else {
            GraphType::Line
        };
        let mk = marker(self.opts.marker);
        // With pixels, lines become strokes of the picture and their datasets stay
        // empty (still named, for the legend).
        let mut strokes: Vec<pixels::Stroke> = Vec::new();
        // `false`: it's in the picture instead.
        let mut as_chars = |pts: &[(f64, f64)], style: &Style, dots: bool| {
            if px.is_some() {
                strokes.push(pixels::Stroke {
                    points: pts.to_vec(),
                    color: style.fg.unwrap_or(Color::White),
                    dots,
                });
            }
            px.is_none()
        };
        let none: &[(f64, f64)] = &[];
        let mut datasets: Vec<RDataset> = overlays
            .iter()
            .zip(&as_text)
            .map(|((pts, style, m, label), &text)| {
                let pts = if text || as_chars(pts, style, false) {
                    pts
                } else {
                    none
                };
                let d = RDataset::default()
                    .data(pts)
                    .graph_type(GraphType::Line)
                    .marker(*m)
                    .style(*style);
                match label {
                    Some(l) => d.name(l.clone()),
                    None => d,
                }
            })
            .collect();
        let last_text = |s: &Series| {
            s.in_view(self.overlay.view)
                .last()
                .or(s.points.last())
                .map(|p| s.unit.format(p.1))
                .unwrap_or_default()
        };
        // Candlesticks: their four series set the y range and the readout but are
        // painted as candles below, not as lines.
        let ohlc = (self.opts.kind == ChartKind::Candles)
            .then(|| candle_roles(&visible))
            .flatten();
        let is_ohlc = |s: &Series| ohlc.is_some_and(|r| r.iter().any(|&(_, o)| std::ptr::eq(o, s)));
        if raw_shown {
            // Under trend lines the raw data takes a darker shade of the series colour,
            // so each trend is clearly distinct from (but matches) its data.
            // Series in front are drawn last (the first listed last of all).
            let rank = |s: &Series| {
                self.opts
                    .front
                    .iter()
                    .position(|f| *f == s.name)
                    .map_or(0, |p| self.opts.front.len() - p)
            };
            let mut order: Vec<&Prepared> = lines.iter().filter(|l| !is_ohlc(l.1)).collect();
            order.sort_by_key(|l| rank(l.1));
            for (i, s, pts) in order {
                let color = self.opts.color(*i);
                let style = Style::default().fg(if trends.is_empty() {
                    color
                } else {
                    dim_color(color)
                });
                let d = RDataset::default()
                    .data(if as_chars(pts, &style, graph == GraphType::Scatter) {
                        pts
                    } else {
                        none
                    })
                    .graph_type(graph)
                    .marker(mk)
                    .style(style);
                let label = if trends.is_empty() {
                    format!("{} {}", s.name, last_text(s))
                } else {
                    format!("{} live {}", s.name, last_text(s))
                };
                datasets.push(d.name(label));
            }
        }
        // avg: bright, solid · median: light, solid · p95: light, dashed · trend: bright, dashed.
        let mut trend_lines: Vec<StyledLine> = Vec::new();
        for (i, s, style, pts) in &trends {
            // e.g. a fit on a log scale over data that's all ≤ 0.
            if pts.is_empty() {
                continue;
            }
            let color = self.opts.color(*i);
            let color = match style {
                TrendStyle::Median | TrendStyle::P95 => light_color(color),
                _ => color,
            };
            let value = self
                .opts
                .trend_last(s, *style)
                .map(|v| s.unit.format(v))
                .unwrap_or_default();
            let label = Some(format!("{} {} {value}", s.name, style.label()));
            match style {
                TrendStyle::Avg | TrendStyle::Median => {
                    trend_lines.push((pts.clone(), Style::default().fg(color), label))
                }
                TrendStyle::P95 | TrendStyle::Fit => {
                    let run = (pts.len() / ((cols as usize / 3).max(2))).max(2);
                    let dashes = if *style == TrendStyle::Fit && pts.len() <= 49 {
                        let (a, b) = (pts[0], pts[pts.len() - 1]);
                        let pieces = (cols * (b.0 - a.0) / (x1 - x0) / 3.0) as usize;
                        dashed(
                            |x| a.1 + (b.1 - a.1) * (x - a.0) / (b.0 - a.0).max(f64::MIN_POSITIVE),
                            a.0,
                            b.0,
                            pieces,
                        )
                    } else {
                        dash_polyline(pts, run)
                    };
                    for (k, dash) in dashes.into_iter().enumerate() {
                        trend_lines.push((
                            dash,
                            Style::default().fg(color),
                            if k == 0 { label.clone() } else { None },
                        ));
                    }
                }
            }
        }
        for (pts, style, label) in &trend_lines {
            let d = RDataset::default()
                .data(if as_chars(pts, style, false) {
                    pts
                } else {
                    none
                })
                .graph_type(GraphType::Line)
                .marker(Marker::Braille)
                .style(*style);
            datasets.push(match label {
                Some(l) => d.name(l.clone()),
                None => d,
            });
        }

        let block = self.block_titled(title);
        let inner = block.inner(area);
        let plot = graph_area(inner, &y_labels, x_labels.first());
        let x_tick_list = x_ticks(self.ds, self.opts, x0, x1, (plot.width as usize / 9).max(2));
        let axis_style = Style::default().fg(Color::Gray);
        Chart::new(datasets)
            .block(block)
            .x_axis(
                Axis::default()
                    .bounds([x0, x1])
                    .labels(x_labels)
                    .style(axis_style),
            )
            .y_axis(
                Axis::default()
                    .bounds([y0, y1])
                    .labels(y_labels)
                    .style(axis_style),
            )
            // With a cursor, its readout box takes the legend's place; narrow charts
            // skip the legend so it doesn't cover the data.
            .legend_position(if self.overlay.cursor.is_some() || plot.width < 56 {
                None
            } else {
                legend(self.opts.legend)
            })
            .hidden_legend_constraints((Constraint::Ratio(2, 3), Constraint::Ratio(2, 3)))
            .render(area, buf);
        let plot = PlotArea { rect: plot, x0, x1 };
        if let Some(px) = px {
            px.pictures.borrow_mut().push(pixels::Picture {
                rect: plot.rect,
                x: (x0, x1),
                y: (y0, y1),
                strokes,
                shade: self.overlay.selection.into_iter().collect(),
                covered: Vec::new(),
            });
        }
        // Shade a measured stretch (in the picture, with pixels).
        if let Some((a, b)) = self.overlay.selection
            && b > x0
            && a < x1
            && px.is_none()
        {
            for col in plot.col_of(a.max(x0))..=plot.col_of(b.min(x1)) {
                for row in plot.rect.y..plot.rect.bottom() {
                    buf[(col, row)].set_bg(Color::Indexed(236));
                }
            }
        }
        draw_axis_labels(
            buf,
            inner,
            plot,
            (y0, y1),
            Ticks {
                x: &x_tick_list,
                y: &y_tick_list,
            },
        );
        if let Some(plots) = self.overlay.plots {
            plots.borrow_mut().push(plot);
        }
        if let Some(roles) = ohlc {
            draw_candles(buf, &plot, (y0, y1), roles.map(|r| r.1), &ty);
        }
        match strip {
            Some(strip) => self.draw_lanes(buf, strip, &plot, &lanes),
            None => self.draw_marks(buf, &plot, (y0, y1), &visible),
        }
        if let Some(c) = self.overlay.cursor
            && (x0..=x1).contains(&c)
        {
            self.render_readout(
                buf,
                &plot,
                c,
                &lines.iter().map(|l| (l.0, l.1)).collect::<Vec<_>>(),
                ohlc.map(|r| r.map(|v| v.1)),
            );
        }
    }

    /// The markers strip's rows, in the order their markers first come.
    fn lanes(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for m in &self.opts.marks {
            let lane = lane_of(m);
            if !out.iter().any(|l| *l == lane) {
                out.push(lane.to_string());
            }
        }
        out
    }

    /// The markers strip: a row per lane (the rule, the strategy…), named on the left,
    /// each marker at its moment — lined up with the chart above, so the lines stay
    /// clear and every marker can be seen. Past `MAX_LANES` the rest share the last
    /// row.
    fn draw_lanes(&self, buf: &mut Buffer, strip: Rect, plot: &PlotArea, lanes: &[String]) {
        let block = self.block_titled("markers");
        let inner = block.inner(strip);
        block.render(strip, buf);
        let rows = inner.height as usize;
        if rows == 0 || plot.rect.width == 0 {
            return;
        }
        let row_of = |lane: &str| {
            let k = lanes.iter().position(|l| l == lane).unwrap_or(0);
            k.min(rows - 1)
        };
        let gutter = plot.rect.x.saturating_sub(inner.x);
        let dim = Style::default().fg(Color::DarkGray);
        for (k, name) in lanes.iter().enumerate().take(rows) {
            let name = if k + 1 == rows && lanes.len() > rows {
                format!("+{} more", lanes.len() - k)
            } else {
                name.clone()
            };
            if gutter >= 3 {
                let w = gutter as usize - 1;
                let text: String = name.chars().take(w).collect();
                buf.set_stringn(inner.x, inner.y + k as u16, &text, w, dim);
            }
        }
        // Where the cursor is, down the strip too.
        if let Some(c) = self.overlay.cursor
            && (plot.x0..=plot.x1).contains(&c)
        {
            let col = plot.col_of(c);
            for k in 0..rows {
                buf[(col, inner.y + k as u16)]
                    .set_symbol("┊")
                    .set_fg(Color::DarkGray);
            }
        }
        let color_of = |m: &Mark| {
            m.color.unwrap_or_else(|| {
                m.series
                    .as_ref()
                    .and_then(|n| self.ds.series.iter().position(|s| &s.name == n))
                    .map_or(Color::Yellow, |i| self.opts.color(i))
            })
        };
        let mut placed: Vec<(u16, u16, &Mark)> = Vec::new();
        for m in self
            .opts
            .marks
            .iter()
            .filter(|m| m.x >= plot.x0 && m.x <= plot.x1)
        {
            let row = inner.y + row_of(lane_of(m)) as u16;
            let col = plot.col_of(m.x);
            let mut style = Style::default()
                .fg(color_of(m))
                .add_modifier(Modifier::BOLD | m.effects);
            if let Some(bg) = m.bg {
                style = style.bg(bg);
            }
            buf[(col, row)].set_symbol(&m.symbol).set_style(style);
            placed.push((col, row, m));
        }
        // Labels where there's room: the newest first, a few per row.
        placed.sort_by(|a, b| b.2.x.total_cmp(&a.2.x));
        let mut per_row: std::collections::HashMap<u16, usize> = Default::default();
        // A set: thousands of markers (a day of trades) each checked against the rest
        // would take seconds a frame.
        let marked: std::collections::HashSet<(u16, u16)> =
            placed.iter().map(|p| (p.0, p.1)).collect();
        for &(col, row, m) in &placed {
            let n = per_row.entry(row).or_default();
            if *n >= 6 || m.label.is_empty() {
                continue;
            }
            let text = if m.badge {
                format!(" {} ", m.label)
            } else {
                m.label.clone()
            };
            let w = unicode_width::UnicodeWidthStr::width(text.as_str()) as u16;
            let fits = |x: u16| {
                x >= plot.rect.x
                    && x + w <= plot.rect.right()
                    && (x.saturating_sub(1)..x + w + 1)
                        .all(|c| !marked.contains(&(c, row)) && free_cell(&buf[(c, row)]))
            };
            let right = Some(col + 2).filter(|&x| fits(x));
            let left = col.checked_sub(w + 1).filter(|&x| fits(x));
            let Some(x) = right.or(left) else {
                continue;
            };
            let style = if m.badge {
                Style::default()
                    .fg(Color::Black)
                    .bg(m.bg.unwrap_or(color_of(m)))
            } else {
                Style::default().fg(color_of(m))
            };
            buf.set_stringn(x, row, &text, w as usize, style);
            *n += 1;
        }
    }

    /// Markers: ▼ just above the point (in its series' colour) with its label beside it,
    /// or for a moment marked by hand, a dotted line down the plot. Labels are left out
    /// when there are too many to read; the cursor readout still shows them.
    fn draw_marks(
        &self,
        buf: &mut Buffer,
        plot: &PlotArea,
        (y0, y1): (f64, f64),
        visible: &[(usize, &Series)],
    ) {
        let in_view: Vec<&Mark> = self
            .opts
            .marks
            .iter()
            .filter(|m| m.x >= plot.x0 && m.x <= plot.x1)
            .filter(|m| {
                m.series
                    .as_ref()
                    .is_none_or(|n| visible.iter().any(|(_, s)| &s.name == n))
            })
            .collect();
        let r = plot.rect;
        if r.height == 0 || r.width == 0 || y1 <= y0 {
            return;
        }
        // First every ▼ (and where it is), then labels in the space left.
        let mut placed: Vec<(u16, u16, Color, &Mark)> = Vec::new();
        // Where they are, to look up quickly (there can be thousands).
        let mut at: std::collections::HashSet<(u16, u16)> = Default::default();
        for m in in_view {
            let col = plot.col_of(m.x);
            let (color, row) = match (&m.series, m.y) {
                (Some(name), Some(y)) => {
                    let Some(&(i, _)) = visible.iter().find(|(_, s)| &s.name == name) else {
                        continue;
                    };
                    let v = if self.opts.log {
                        if y <= 0.0 {
                            continue;
                        }
                        y.log10()
                    } else {
                        y
                    };
                    let frac = ((y1 - v) / (y1 - y0)).clamp(0.0, 1.0);
                    let point = r.y + (frac * f64::from(r.height - 1)).round() as u16;
                    // Upward symbols sit below the point, the rest above it.
                    let row = if crate::marks::MarkRule::points_up(&m.symbol) {
                        (point + 1).min(r.bottom() - 1)
                    } else {
                        point.saturating_sub(1).max(r.y)
                    };
                    (m.color.unwrap_or(self.opts.color(i)), row)
                }
                _ => {
                    for row in r.y..r.bottom() {
                        if buf[(col, row)].symbol() == " " {
                            buf[(col, row)].set_symbol("┊").set_fg(Color::Yellow);
                        }
                    }
                    (m.color.unwrap_or(Color::Yellow), r.y)
                }
            };
            // Never over the legend or other text (not even a gap between words).
            let beside_text = [col.checked_sub(1), Some(col + 1)]
                .into_iter()
                .flatten()
                .filter(|&c| c >= r.x && c < r.right())
                .any(|c| !free_cell(&buf[(c, row)]) && !at.contains(&(c, row)));
            if !free_cell(&buf[(col, row)]) || beside_text {
                continue;
            }
            let mut style = Style::default()
                .fg(color)
                .add_modifier(Modifier::BOLD | m.effects);
            if let Some(bg) = m.bg {
                style = style.bg(bg);
            }
            buf[(col, row)].set_symbol(&m.symbol).set_style(style);
            placed.push((col, row, color, m));
            at.insert((col, row));
        }
        let mut taken: Vec<(u16, u16, u16)> = placed.iter().map(|p| (p.1, p.0, p.0)).collect();
        // Labels where there's room — the newest first, and only a few when it's busy
        // (the cursor readout still names every marker).
        let budget = if placed.len() > 12 { 6 } else { placed.len() };
        let mut newest: Vec<&(u16, u16, Color, &Mark)> = placed.iter().collect();
        newest.sort_by(|a, b| b.3.x.total_cmp(&a.3.x));
        for &&(col, row, color, m) in newest.iter().take(budget) {
            // A badge: the label on the marker's colour, with a space either side.
            let text = if m.badge {
                format!(" {} ", m.label)
            } else {
                m.label.clone()
            };
            let w = unicode_width::UnicodeWidthStr::width(text.as_str()) as u16;
            // Labels only on empty space: never over the plotted lines.
            let fits = |x: u16| {
                !taken
                    .iter()
                    .any(|&(tr, a, b)| tr == row && x <= b && a <= x + w)
                    && (x..x + w).all(|c| matches!(buf[(c, row)].symbol(), " " | "┊"))
            };
            let right = (col + 2 + w <= r.right())
                .then_some(col + 2)
                .filter(|&x| fits(x));
            let left = (col > r.x + w).then(|| col - w - 1).filter(|&x| fits(x));
            let Some(x) = right.or(left) else {
                continue;
            };
            let mut style = Style::default().add_modifier(m.effects);
            style = if m.badge {
                style
                    .fg(Color::Black)
                    .bg(m.bg.unwrap_or(color))
                    .add_modifier(Modifier::BOLD)
            } else {
                match m.bg {
                    Some(bg) => style.fg(color).bg(bg),
                    None => style.fg(color),
                }
            };
            buf.set_stringn(x, row, &text, w as usize, style);
            taken.push((row, x, x + w));
        }
    }

    /// A small box beside the cursor listing each series' value there.
    fn render_readout(
        &self,
        buf: &mut Buffer,
        plot: &PlotArea,
        x: f64,
        series: &[(usize, &Series)],
        ohlc: Option<[&Series; 4]>,
    ) {
        let mut lines = vec![Line::styled(
            self.ds.fmt_x_full(round_x(self.ds, x)),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        // A candle: one line, O H L C, green if it closed up.
        if let Some(c) = ohlc
            && let Some(text) = candle_text(c, x)
        {
            lines.push(text);
        }
        let in_candle = |s: &Series| ohlc.is_some_and(|c| c.iter().any(|o| std::ptr::eq(*o, s)));
        for &(i, s) in series.iter().filter(|(_, s)| !in_candle(s)) {
            if let Some((v, projected)) = value_at(s, self.opts, x) {
                // Live value (unless hidden), then each trend layer's value here.
                let mut parts: Vec<String> = Vec::new();
                if projected {
                    parts.push(format!("≈{} (forecast)", s.unit.format(v)));
                } else {
                    if self.opts.show_raw || self.opts.trends.is_empty() {
                        parts.push(if self.opts.trends.is_empty() {
                            s.unit.format(v)
                        } else {
                            format!("live {}", s.unit.format(v))
                        });
                    }
                    for &style in &self.opts.trends {
                        if let Some(t) = self.opts.trend_at(s, style, x) {
                            parts.push(format!("{} {}", style.label(), s.unit.format(t)));
                        }
                    }
                }
                let text = parts.join(" · ");
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ", s.name),
                        Style::default().fg(self.opts.color(i)),
                    ),
                    Span::raw(text),
                ]));
            }
        }
        // Markers under the cursor.
        let col = plot.col_of(x);
        for m in self
            .opts
            .marks
            .iter()
            .filter(|m| m.x >= plot.x0 && m.x <= plot.x1 && plot.col_of(m.x).abs_diff(col) <= 1)
            .filter(|m| {
                m.series
                    .as_ref()
                    .is_none_or(|n| series.iter().any(|(_, s)| &s.name == n))
            })
            .take(4)
        {
            lines.push(Line::styled(
                format!("{} {}", m.symbol, m.label),
                Style::default().fg(m.color.unwrap_or(Color::Yellow)),
            ));
        }
        let w = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16 + 2;
        let h = lines.len() as u16;
        // In small panels the box would hide the data; the status line has the values.
        if w * 2 > plot.rect.width || h + 3 > plot.rect.height {
            return;
        }
        let col = plot.col_of(x);
        let x_pos = if col + 2 + w <= plot.rect.right() {
            col + 2
        } else {
            col.saturating_sub(w + 1).max(plot.rect.x)
        };
        let rect = Rect {
            x: x_pos,
            y: plot.rect.y,
            width: w,
            height: h,
        };
        Clear.render(rect, buf);
        Paragraph::new(lines)
            .style(Style::default().bg(Color::Black).fg(Color::White))
            .block(Block::default().padding(ratatui::widgets::Padding::horizontal(1)))
            .render(rect, buf);
    }

    /// Latest value of each series as a horizontal bar, with the value after it.
    fn render_bar(self, area: Rect, buf: &mut Buffer) {
        let tiles = self.tiles();
        let latest: Vec<(Color, String, &Series, f64)> = tiles
            .iter()
            .filter_map(|t| Some((t.color, t.name(), t.series, self.summarise(t)?.value)))
            .collect();
        if latest.is_empty() {
            let why = if self.overlay.selection.is_some_and(|(a, b)| b > a) && !tiles.is_empty() {
                "no data in selection"
            } else {
                "waiting for data…"
            };
            return self.render_empty(area, buf, why);
        }
        let block = self.block();
        let inner = block.inner(area);
        block.render(area, buf);
        let max = latest
            .iter()
            .map(|l| l.3.abs())
            .fold(0.0, f64::max)
            .max(f64::MIN_POSITIVE);
        let name_w = (latest
            .iter()
            .map(|l| l.1.chars().count())
            .max()
            .unwrap_or(0) as u16)
            .min(inner.width / 3);
        let value_w = latest
            .iter()
            .map(|l| l.2.unit.format(l.3).chars().count())
            .max()
            .unwrap_or(0) as u16;
        let bar_w = inner.width.saturating_sub(name_w + value_w + 3);
        let gap = u16::from(latest.len() as u16 * 2 <= inner.height);
        for (k, (color, label, s, v)) in latest.iter().enumerate() {
            let (color, v) = (*color, *v);
            let row = inner.y + k as u16 * (1 + gap);
            if row >= inner.bottom() {
                break;
            }
            let name: String = label.chars().take(name_w as usize).collect();
            buf.set_string(inner.x, row, &name, Style::default().fg(color));
            let len = v.max(0.0) / max * f64::from(bar_w);
            let bar = eighths_bar(len);
            let x = inner.x + name_w + 1;
            buf.set_string(x, row, &bar, Style::default().fg(color));
            let text = s.unit.format(v);
            buf.set_string(
                x + bar.chars().count() as u16 + 1,
                row,
                &text,
                Style::default().fg(Color::White),
            );
        }
    }

    /// Distribution of the first visible series: bins fill the width, with count and
    /// value axes.
    fn render_histogram(self, area: Rect, buf: &mut Buffer) {
        let Some((i, s)) = self.visible().into_iter().next() else {
            return self.render_empty(area, buf, "waiting for data…");
        };
        let values: Vec<f64> = s.in_view(self.overlay.view).iter().map(|p| p.1).collect();
        if values.is_empty() {
            return self.render_empty(area, buf, "no data in view");
        }
        let (lo, hi) = min_max(values.iter().copied()).unwrap_or((0.0, 1.0));
        let title = format!(
            "{} · distribution of {} · n={}",
            self.opts.title,
            s.name,
            values.len()
        );
        let block = self.block_titled(&title);
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.height < 3 || inner.width < 10 {
            return;
        }
        let mut counts = vec![0usize; 1];
        let gutter = 6u16;
        let plot = Rect {
            x: inner.x + gutter,
            y: inner.y,
            width: inner.width - gutter,
            height: inner.height - 2,
        };
        let bins = if hi > lo {
            self.opts.bins.clamp(1, plot.width as usize)
        } else {
            1
        };
        counts.resize(bins, 0);
        let width = (hi - lo) / bins as f64;
        for v in &values {
            let b = if width > 0.0 {
                (((v - lo) / width) as usize).min(bins - 1)
            } else {
                0
            };
            counts[b] += 1;
        }
        let max = *counts.iter().max().unwrap_or(&1) as f64;
        let color = self.opts.color(i);
        let per_bin = f64::from(plot.width) / bins as f64;
        for col in 0..plot.width {
            let b = ((f64::from(col) / per_bin) as usize).min(bins - 1);
            // Leave a gap column at the end of each wide bin.
            if per_bin >= 3.0 && b + 1 < bins && ((f64::from(col + 1) / per_bin) as usize) != b {
                continue;
            }
            let height = counts[b] as f64 / max * f64::from(plot.height);
            for r in 0..plot.height {
                let fill = (height - f64::from(r)).clamp(0.0, 1.0);
                if fill <= 0.0 {
                    break;
                }
                let ch = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█']
                    [((fill * 8.0).ceil() as usize).clamp(1, 8) - 1];
                buf[(plot.x + col, plot.bottom() - 1 - r)]
                    .set_char(ch)
                    .set_style(Style::default().fg(color));
            }
        }
        let axis = Style::default().fg(Color::Gray);
        // Count axis.
        for (frac, label) in [
            (1.0, format!("{max}")),
            (0.5, format!("{}", (max / 2.0).round())),
            (0.0, "0".into()),
        ] {
            let row = plot.y + ((1.0 - frac) * f64::from(plot.height - 1)).round() as u16;
            let w = label.chars().count() as u16;
            if w < gutter {
                buf.set_string(inner.x + gutter - 1 - w, row, &label, axis);
            }
        }
        // Value axis with round ticks.
        let base = inner.bottom() - 2;
        buf.set_string(
            plot.x,
            base,
            "─".repeat(plot.width as usize),
            Style::default().fg(Color::DarkGray),
        );
        let ticks = y_ticks(
            lo,
            if hi > lo { hi } else { lo + 1.0 },
            (plot.width as usize / 14).max(2),
            &s.unit,
            false,
        );
        let mut next_free = inner.x;
        for (v, text) in ticks {
            let frac = (v - lo) / (hi - lo).max(f64::MIN_POSITIVE);
            let col = plot.x + (frac.clamp(0.0, 1.0) * f64::from(plot.width - 1)).round() as u16;
            let w = text.chars().count() as u16;
            let start = col
                .saturating_sub(w / 2)
                .max(inner.x)
                .min(inner.right().saturating_sub(w));
            if start < next_free {
                continue;
            }
            buf.set_string(start, base + 1, &text, axis);
            buf[(col, base)].set_char('┴');
            next_free = start + w + 2;
        }
    }

    fn render_sparklines(self, area: Rect, buf: &mut Buffer) {
        let visible = self.visible();
        if visible.is_empty() {
            return self.render_empty(area, buf, "waiting for data…");
        }
        let block = self.block();
        let inner = block.inner(area);
        block.render(area, buf);
        let n = visible.len().min((inner.height as usize).max(1));
        let rows = Layout::vertical(vec![Constraint::Ratio(1, n as u32); n]).split(inner);
        for (&(i, s), row) in visible.iter().zip(rows.iter()) {
            let pts = s.in_view(self.overlay.view);
            let color = self.opts.color(i);
            let (lo, hi) = pts
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
                    (lo.min(p.1), hi.max(p.1))
                });
            let last = pts.last().map(|p| s.unit.format(p.1)).unwrap_or_default();
            let header = Line::from(vec![
                Span::styled(
                    format!("● {} ", s.name),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{last} ")),
                Span::styled(
                    if lo.is_finite() {
                        format!("({} … {})", s.unit.format(lo), s.unit.format(hi))
                    } else {
                        String::new()
                    },
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            header.render(Rect { height: 1, ..*row }, buf);
            if row.height < 2 || pts.is_empty() {
                continue;
            }
            // One column per cell across the full width: averages when there are more
            // points than columns, the nearest point when there are fewer.
            let w = row.width as usize;
            let n = pts.len();
            let range = (hi - lo).max(f64::MIN_POSITIVE);
            let data: Vec<u64> = (0..w)
                .map(|c| {
                    let (a, b) = (c * n / w, ((c + 1) * n / w).max(c * n / w + 1).min(n));
                    let chunk = &pts[a.min(n - 1)..b.max(a.min(n - 1) + 1)];
                    chunk.iter().map(|p| p.1).sum::<f64>() / chunk.len() as f64
                })
                .map(|v| ((v - lo) / range * 99.0) as u64 + 1)
                .collect();
            Sparkline::default()
                .data(data)
                .max(100)
                .style(Style::default().fg(color))
                .render(
                    Rect {
                        y: row.y + 1,
                        height: row.height - 1,
                        ..*row
                    },
                    buf,
                );
        }
    }
}

/// One value a bar or stat tile shows: a series' live data or one of its trend lines.
struct Tile<'s> {
    series: &'s Series,
    style: Option<TrendStyle>,
    color: Color,
}

impl Tile<'_> {
    fn name(&self) -> String {
        match self.style {
            None => self.series.name.clone(),
            Some(t) => format!("{} {}", self.series.name, t.label()),
        }
    }
}

/// What a tile summarises: its value, the points behind it (for sparklines and the
/// change), and the selected range when there is one.
struct Summary {
    value: f64,
    points: Vec<(f64, f64)>,
    /// Data points in the stretch (`points` may be a decimated trend line).
    count: usize,
    selected: Option<(f64, f64)>,
}

impl ChartWidget<'_> {
    /// The live data, plus a tile per trend line (lighter when shown next to live).
    fn tiles(&self) -> Vec<Tile<'_>> {
        let live = self.opts.show_raw || self.opts.trends.is_empty();
        let mut out = Vec::new();
        for (i, s) in self.visible() {
            let c = self.opts.color(i);
            if live {
                out.push(Tile {
                    series: s,
                    style: None,
                    color: c,
                });
            }
            for &t in &self.opts.trends {
                out.push(Tile {
                    series: s,
                    style: Some(t),
                    color: if live { light_color(c) } else { c },
                });
            }
        }
        out
    }

    /// The latest value (or trend value) in view — or, with a range selected, that
    /// stretch: live shows the value at its end, a trend its statistic over the range.
    fn summarise(&self, t: &Tile) -> Option<Summary> {
        let s = t.series;
        let selected = self.overlay.selection.filter(|(a, b)| b > a);
        let raw = match selected {
            Some((a, b)) => s.range(a, b),
            None => s.in_view(self.overlay.view),
        };
        let &(x_last, last) = raw.last()?;
        let Some(style) = t.style else {
            return Some(Summary {
                value: last,
                points: raw.to_vec(),
                count: raw.len(),
                selected,
            });
        };
        let value = match (selected, style) {
            (Some(_), TrendStyle::Fit) => self.opts.analysis.fit(s)?.at(x_last),
            (Some(_), TrendStyle::Avg) => raw.iter().map(|p| p.1).sum::<f64>() / raw.len() as f64,
            (Some(_), _) => {
                let mut v: Vec<f64> = raw.iter().map(|p| p.1).collect();
                v.sort_by(f64::total_cmp);
                crate::stats::percentile(&v, if style == TrendStyle::P95 { 0.95 } else { 0.5 })
            }
            (None, _) => self.opts.trend_at(s, style, x_last)?,
        };
        // The trend line over the same stretch, for the sparkline and change.
        let (x0, x1) = (raw[0].0, x_last);
        let points = self
            .opts
            .trend_points(s, style, 400)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.0 >= x0 && p.0 <= x1)
            .collect();
        Some(Summary {
            value,
            points,
            count: raw.len(),
            selected,
        })
    }
}

/// 3×5 pixel digits, drawn 3 rows tall with half blocks.
fn glyph(c: char) -> Option<&'static [&'static str; 5]> {
    Some(match c {
        '0' => &["###", "#.#", "#.#", "#.#", "###"],
        '1' => &[".#.", "##.", ".#.", ".#.", "###"],
        '2' => &["###", "..#", "###", "#..", "###"],
        '3' => &["###", "..#", "###", "..#", "###"],
        '4' => &["#.#", "#.#", "###", "..#", "..#"],
        '5' => &["###", "#..", "###", "..#", "###"],
        '6' => &["###", "#..", "###", "#.#", "###"],
        '7' => &["###", "..#", "..#", "..#", "..#"],
        '8' => &["###", "#.#", "###", "#.#", "###"],
        '9' => &["###", "#.#", "###", "..#", "###"],
        '.' => &[".", ".", ".", ".", "#"],
        ',' => &[".", ".", ".", "#", "#"],
        '-' | '−' => &["...", "...", "###", "...", "..."],
        ':' => &[".", "#", ".", "#", "."],
        _ => return None,
    })
}

/// A number in big digits: three rows of text, or `None` if it has other characters.
pub fn big_text(s: &str) -> Option<[String; 3]> {
    let glyphs: Vec<_> = s.chars().map(glyph).collect::<Option<_>>()?;
    let mut rows: [String; 3] = Default::default();
    for (i, g) in glyphs.iter().enumerate() {
        for (r, row) in rows.iter_mut().enumerate() {
            if i > 0 {
                row.push(' ');
            }
            let top = g[r * 2].as_bytes();
            let bottom = g.get(r * 2 + 1).map(|b| b.as_bytes());
            for c in 0..top.len() {
                let t = top[c] == b'#';
                let b = bottom.is_some_and(|b| b[c] == b'#');
                row.push(match (t, b) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
        }
    }
    Some(rows)
}

/// "58.8 GB" → ("58.8", " GB"): the number part and the rest.
fn split_number(s: &str) -> (&str, &str) {
    let end = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || matches!(c, '.' | ',' | '-' | '−')))
        .map_or(s.len(), |(i, _)| i);
    s.split_at(end)
}

impl ChartWidget<'_> {
    /// A tile per series: name, the latest value in big digits, its change over the
    /// view, a sparkline and (with a target) when it gets there.
    fn render_stat_tiles(self, area: Rect, buf: &mut Buffer) {
        let tiles = self.tiles();
        if tiles.is_empty() {
            return self.render_empty(area, buf, "waiting for data…");
        }
        let n = tiles.len();
        let cols = ((area.width / 26).max(1) as usize).min(n);
        let rows = n.div_ceil(cols);
        let row_areas = Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(area);
        for (k, t) in tiles.iter().enumerate() {
            let (r, c) = (k / cols, k % cols);
            let in_row = if r + 1 == rows { n - r * cols } else { cols };
            let cells = Layout::horizontal(vec![Constraint::Ratio(1, in_row as u32); in_row])
                .split(row_areas[r]);
            self.render_stat_tile(cells[c], buf, t);
        }
    }

    fn render_stat_tile(&self, area: Rect, buf: &mut Buffer, t: &Tile) {
        let (s, color) = (t.series, t.color);
        let Some(sum) = self.summarise(t) else {
            // Still show which tile this is, and why it's blank.
            let block = Block::bordered()
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    format!(" {} ", t.name()),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
            let inner = block.inner(area);
            block.render(area, buf);
            let why = if self.overlay.selection.is_some_and(|(a, b)| b > a) {
                "no data in selection"
            } else {
                "waiting for data…"
            };
            Line::styled(format!(" {why}"), Style::default().fg(Color::DarkGray)).render(
                Rect {
                    height: inner.height.min(1),
                    ..inner
                },
                buf,
            );
            return;
        };
        let mut title = vec![Span::styled(
            format!(" {} ", t.name()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        if let Some((a, b)) = sum.selected {
            let span = match self.ds.x_kind {
                XKind::Time => crate::time::fmt_duration(b - a),
                _ => format!("{} points", sum.count),
            };
            title.push(Span::styled(
                format!("selected {span} "),
                Style::default().fg(Color::Yellow),
            ));
        } else if let Some(&(x, _)) = sum.points.last()
            && self.ds.x_kind == XKind::Time
        {
            // When the value is from (a tile has no time axis to read it off).
            title.push(Span::styled(
                format!("at {} ", self.ds.fmt_x(round_x(self.ds, x), 60.0)),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let block = Block::bordered()
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Line::from(title));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width < 4 || inner.height < 1 {
            return;
        }
        let pts = &sum.points;
        let last = sum.value;
        let x_last = pts.last().map_or(0.0, |p| p.0);
        let mut y = inner.y;
        let put = |buf: &mut Buffer, y: u16, line: Line| {
            if y < inner.bottom() {
                line.render(
                    Rect {
                        x: inner.x + 1,
                        y,
                        width: inner.width.saturating_sub(2),
                        height: 1,
                    },
                    buf,
                );
            }
        };
        // The value: big when there's room.
        let text = s.unit.format(last);
        let (num, rest) = split_number(&text);
        let big = big_text(num).filter(|b| {
            inner.height >= 5
                && b[0].chars().count() + rest.chars().count() + 2 <= inner.width as usize
        });
        let value_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        match big {
            Some(rows) => {
                for (r, row) in rows.iter().enumerate() {
                    let mut spans = vec![Span::styled(row.clone(), Style::default().fg(color))];
                    if r == 2 {
                        spans.push(Span::styled(rest.to_string(), value_style));
                    }
                    put(buf, y, Line::from(spans));
                    y += 1;
                }
            }
            None => {
                put(buf, y, Line::styled(text.clone(), value_style));
                y += 1;
            }
        }
        // Change over what's shown.
        let dim = Style::default().fg(Color::Gray);
        if let Some(&(x0, first)) = pts.first()
            && pts.len() > 1
        {
            // Along the line drawn (live data or the trend), first to last.
            let d = pts.last().map_or(last, |p| p.1) - first;
            let arrow = if d > 0.0 {
                "▲"
            } else if d < 0.0 {
                "▼"
            } else {
                "■"
            };
            let pct = if first != 0.0 {
                format!(" ({:+.1}%)", d / first.abs() * 100.0)
            } else {
                String::new()
            };
            let over = match self.ds.x_kind {
                XKind::Time => format!(" in {}", crate::time::fmt_duration(x_last - x0)),
                _ => format!(" over {} points", sum.count),
            };
            put(
                buf,
                y,
                Line::from(vec![
                    Span::styled(
                        format!("{arrow} {}", s.unit.format(d.abs())),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("{pct}{over}"), dim),
                ]),
            );
            y += 1;
        }
        // Trend and target, when known.
        let report = self.opts.analysis.report(self.ds, s);
        let mut facts = Vec::new();
        if let Some(r) = &report {
            if r.fit.is_some() {
                facts.push(r.trend_text());
            }
            let f = r.forecast_cell();
            if r.target.is_some() && !f.is_empty() {
                facts.push(f);
            }
        }
        let spark_rows = inner.bottom().saturating_sub(y);
        if !facts.is_empty() && spark_rows >= 2 {
            put(buf, y, Line::styled(facts.join(" · "), dim));
            y += 1;
        }
        // Sparkline in what's left.
        let h = inner.bottom().saturating_sub(y);
        if h >= 1 && pts.len() > 1 {
            let (lo, hi) = pts
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
                    (lo.min(p.1), hi.max(p.1))
                });
            let range = (hi - lo).max(f64::MIN_POSITIVE);
            let w = inner.width.saturating_sub(2) as usize;
            let n = pts.len();
            let data: Vec<u64> = (0..w)
                .map(|c| {
                    let (a, b) = (c * n / w, ((c + 1) * n / w).max(c * n / w + 1).min(n));
                    let chunk = &pts[a.min(n - 1)..b.max(a.min(n - 1) + 1)];
                    chunk.iter().map(|p| p.1).sum::<f64>() / chunk.len() as f64
                })
                .map(|v| ((v - lo) / range * 99.0) as u64 + 1)
                .collect();
            Sparkline::default()
                .data(data)
                .max(100)
                .style(Style::default().fg(dim_color(color)))
                .render(
                    Rect {
                        x: inner.x + 1,
                        y,
                        width: w as u16,
                        height: h,
                    },
                    buf,
                );
        }
    }
}

impl ChartWidget<'_> {
    /// Live data on the left (or top, when narrow), its trends on the right.
    fn render_side_by_side(self, area: Rect, buf: &mut Buffer) {
        let base = ChartOpts {
            side_by_side: false,
            ..self.opts.clone()
        };
        let left_lines = crate::cli::Lines {
            live: base.show_raw || base.trends.is_empty(),
            trends: base.trends.clone(),
        };
        let right_lines = if base.right.is_empty() {
            crate::cli::Lines::default_right()
        } else {
            base.right.clone()
        };
        let live = ChartOpts {
            forecast: None,
            title: format!("{} · {}", base.title, left_lines.label()),
            ..base.clone()
        };
        let trend = ChartOpts {
            show_raw: right_lines.live,
            trends: right_lines.trends.clone(),
            title: format!("{} · {}", base.title, right_lines.label()),
            ..base
        };
        let [a, b] = if area.width >= 100 {
            Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(area)
        } else {
            Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(area)
        };
        ChartWidget::new(self.ds, &live, self.overlay).render(a, buf);
        ChartWidget::new(self.ds, &trend, self.overlay).render(b, buf);
    }
}

/// The data and options one pane draws: its series (keeping their colours), chart type
/// and lines.
pub fn pane_view(ds: &Dataset, opts: &ChartOpts, pane: &panes::Pane) -> (Dataset, ChartOpts) {
    let mut sub = ChartOpts {
        panes: None,
        side_by_side: false,
        ..opts.clone()
    };
    if let Some(k) = pane.kind {
        sub.kind = k;
    }
    if let Some(l) = &pane.lines {
        sub.show_raw = l.live;
        sub.trends = l.trends.clone();
    }
    if !pane.front.is_empty() {
        sub.front = pane.front.clone();
    }
    let mut picked: Vec<usize> = if pane.series.is_empty() {
        (0..ds.series.len()).collect()
    } else {
        pane.series
            .iter()
            .filter_map(|n| ds.series.iter().position(|s| &s.name == n))
            .collect()
    };
    // Candles bring their open, high, low and close along (found by name).
    if sub.kind == ChartKind::Candles && !pane.series.is_empty() {
        let names: Vec<&str> = ds.series.iter().map(|s| s.name.as_str()).collect();
        if let [Some(o), Some(h), Some(l), Some(c)] = candle_roles_named(&names) {
            for k in [o, h, l, c] {
                if !picked.contains(&k) {
                    picked.push(k);
                }
            }
            picked.sort_unstable();
        }
    }
    sub.palette = picked.iter().map(|&i| opts.color(i)).collect();
    if !pane.series.is_empty() {
        sub.marks
            .retain(|m| m.series.as_ref().is_none_or(|n| pane.series.contains(n)));
    }
    if sub.palette.is_empty() {
        sub.palette = opts.palette.clone();
    }
    let data = Dataset {
        x_kind: ds.x_kind,
        series: picked.iter().map(|&i| ds.series[i].clone()).collect(),
    };
    let mut parts = Vec::new();
    if !pane.series.is_empty() && pane.series.len() <= 3 {
        parts.push(pane.series.join(", "));
    }
    let lines = crate::cli::Lines {
        live: sub.show_raw || sub.trends.is_empty(),
        trends: sub.trends.clone(),
    };
    if pane.lines.is_some() || !sub.trends.is_empty() {
        parts.push(lines.label());
    }
    if let Some(t) = &pane.title {
        sub.title = t.clone();
    } else if !parts.is_empty() {
        // What this pane shows comes first: narrow panes cut the title short.
        sub.title = if opts.tidy {
            parts.join(" · ")
        } else {
            format!("{} · {}", parts.join(" · "), opts.title)
        };
    }
    (data, sub)
}

/// Bar, histogram and sparkline charts have no lines to draw over the data: each trend
/// becomes a series of its own ("node1 avg"), in a lighter shade of the series' colour.
pub fn with_trend_series(
    ds: &Dataset,
    opts: &ChartOpts,
    hidden: Option<&HashSet<String>>,
) -> (Dataset, ChartOpts) {
    let mut out = Dataset {
        x_kind: ds.x_kind,
        series: Vec::new(),
    };
    let mut palette = Vec::new();
    let live = opts.show_raw || opts.trends.is_empty();
    for (i, s) in ds.series.iter().enumerate() {
        let c = opts.color(i);
        if live {
            out.series.push(s.clone());
            palette.push(c);
        }
        // A hidden series' trends are hidden with it.
        if hidden.is_some_and(|h| h.contains(&s.name)) {
            continue;
        }
        for &t in &opts.trends {
            if let Some(points) = opts.trend_points(s, t, s.points.len().max(2)) {
                out.series.push(Series {
                    name: format!("{} {}", s.name, t.label()),
                    unit: s.unit.clone(),
                    points,
                });
                palette.push(if live { light_color(c) } else { c });
            }
        }
    }
    let sub = ChartOpts {
        trends: Vec::new(),
        show_raw: true,
        palette: if palette.is_empty() {
            opts.palette.clone()
        } else {
            palette
        },
        ..opts.clone()
    };
    (out, sub)
}

impl ChartWidget<'_> {
    /// Several panes, each a chart of its own.
    fn render_panes(&self, p: &panes::Panes, area: Rect, buf: &mut Buffer) {
        for (id, rect) in p.rects(area) {
            let pane = &p.panes[id];
            match &pane.source {
                None => {
                    let (ds, opts) = pane_view(self.ds, self.opts, pane);
                    ChartWidget::new(&ds, &opts, self.overlay).render(rect, buf);
                }
                Some(name) => self.render_source_pane(name, pane, rect, buf),
            }
            if p.count() > 1 && self.overlay.pane_focus == Some(id) {
                outline(rect, buf, Color::Yellow);
            }
        }
    }
}

impl ChartWidget<'_> {
    /// A pane showing another source's data. Zoom and cursor are shared when the time
    /// ranges overlap; otherwise the pane shows all of its own data.
    fn render_source_pane(&self, name: &str, pane: &panes::Pane, rect: Rect, buf: &mut Buffer) {
        let Some(src) = self
            .overlay
            .sources
            .and_then(|v| v.iter().find(|s| s.name == name))
        else {
            return ChartWidget::new(self.ds, self.opts, self.overlay).render_empty_titled(
                rect,
                buf,
                name,
                &format!("starting {name}…"),
            );
        };
        if src.ds.is_empty() {
            let msg = src
                .error
                .clone()
                .unwrap_or_else(|| "waiting for data…".into());
            return ChartWidget::new(self.ds, self.opts, self.overlay).render_empty_titled(
                rect,
                buf,
                &src.opts.title,
                &msg,
            );
        }
        // The source's own settings, with what's shared across panes.
        let base = ChartOpts {
            trend_window: self.opts.trend_window,
            show_targets: self.opts.show_targets,
            marks_below: self.opts.marks_below,
            forecast: self.opts.forecast,
            split: self.opts.split,
            log: self.opts.log,
            tidy: self.opts.tidy,
            legend: if self.opts.legend == LegendPos::None {
                LegendPos::None
            } else {
                src.opts.legend
            },
            ..src.opts.clone()
        };
        let (ds, opts) = pane_view(&src.ds, &base, pane);
        let mut overlay = self.overlay;
        // Its own source's fields, not the main one's.
        overlay.full = None;
        let overlaps = match (overlay.view, ds.x_range()) {
            (Some((a, b)), Some((lo, hi))) => lo <= b && hi >= a,
            (Some(_), None) => false,
            _ => true,
        };
        if !overlaps || ds.x_kind != self.ds.x_kind {
            overlay.view = None;
            overlay.cursor = None;
            overlay.selection = None;
        }
        ChartWidget::new(&ds, &opts, overlay).render(rect, buf);
    }
}

/// Whether a marker may be drawn over this cell: blank, a plotted line (braille) or
/// a marker guide — not text, the legend or the readout box.
fn free_cell(cell: &ratatui::buffer::Cell) -> bool {
    cell.symbol()
        .chars()
        .all(|c| c == ' ' || c == '┊' || ('\u{2800}'..='\u{28FF}').contains(&c))
}

/// Recolour a rectangle's border.
fn outline(r: Rect, buf: &mut Buffer, color: Color) {
    if r.width < 2 || r.height < 2 {
        return;
    }
    let mut paint = |x: u16, y: u16| {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_fg(color);
        }
    };
    for x in r.x..r.right() {
        paint(x, r.y);
        paint(x, r.bottom() - 1);
    }
    for y in r.y..r.bottom() {
        paint(r.x, y);
        paint(r.right() - 1, y);
    }
}

impl Widget for ChartWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 4 || area.height < 3 {
            return;
        }
        if let Some(p) = &self.opts.panes {
            return self.render_panes(p, area, buf);
        }
        if self.opts.side_by_side {
            return self.render_side_by_side(area, buf);
        }
        if !matches!(
            self.opts.kind,
            ChartKind::Line
                | ChartKind::Scatter
                | ChartKind::Bar
                | ChartKind::Stat
                | ChartKind::Candles
                | ChartKind::Book
        ) && !self.opts.trends.is_empty()
        {
            let (ds, opts) = with_trend_series(self.ds, self.opts, self.overlay.hidden);
            return ChartWidget::new(&ds, &opts, self.overlay).render(area, buf);
        }
        match self.opts.kind {
            ChartKind::Line | ChartKind::Scatter if self.opts.split => self.render_split(area, buf),
            ChartKind::Line | ChartKind::Scatter | ChartKind::Candles => {
                self.render_xy(area, buf, self.visible(), &self.opts.title, false)
            }
            ChartKind::Bar => self.render_bar(area, buf),
            ChartKind::Histogram => self.render_histogram(area, buf),
            ChartKind::Sparkline => self.render_sparklines(area, buf),
            ChartKind::Stat => self.render_stat_tiles(area, buf),
            ChartKind::Book => self.render_book(area, buf),
        }
    }
}

/// Render a chart into an off-screen buffer.
#[cfg(test)]
pub fn render_to_buffer(ds: &Dataset, opts: &ChartOpts, width: u16, height: u16) -> Buffer {
    render_with_sources(ds, None, opts, width, height, &[])
}

/// Render a chart whose panes may show other sources (`full`: every series read,
/// before `--select`, for a book's levels).
pub fn render_with_sources(
    ds: &Dataset,
    full: Option<&Dataset>,
    opts: &ChartOpts,
    width: u16,
    height: u16,
    sources: &[SourceView],
) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    let overlay = Overlay {
        sources: Some(sources),
        full,
        ..Overlay::default()
    };
    ChartWidget::new(ds, opts, overlay).render(area, &mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::XKind;

    #[test]
    fn decimation_keeps_extremes() {
        let s = Series {
            name: "s".into(),
            unit: Unit::default(),
            points: (0..10_000).map(|i| (i as f64, (i % 100) as f64)).collect(),
        };
        let pts = visible_points(&s, 0.0, 9999.0, 50);
        assert!(pts.len() <= 102 + 2);
        assert!(pts.iter().any(|p| p.1 == 99.0) && pts.iter().any(|p| p.1 == 0.0));
    }

    #[test]
    fn plot_area_maps_columns() {
        let p = PlotArea {
            rect: Rect::new(10, 0, 100, 10),
            x0: 0.0,
            x1: 100.0,
        };
        assert_eq!(p.x_at(10), 0.5);
        assert_eq!(p.col_of(50.0), 60);
        assert_eq!(p.x_at(500), 100.0);
    }

    #[test]
    fn forecast_extends_bounds_and_values() {
        let s = Series {
            name: "a".into(),
            unit: Unit::default(),
            points: (0..11).map(|i| (i as f64, 100.0 - i as f64)).collect(),
        };
        let ds = Dataset {
            x_kind: XKind::Index,
            series: vec![s],
        };
        let mut opts = ChartOpts {
            forecast: Some(Horizon::Span(10.0)),
            ..ChartOpts::default()
        };
        assert_eq!(plot_x_bounds(&ds, &opts, None, None), (0.0, 20.0));
        assert_eq!(value_at(&ds.series[0], &opts, 15.0), Some((85.0, true)));
        assert_eq!(value_at(&ds.series[0], &opts, 4.4), Some((96.0, false)));
        // Auto horizon reaches a near target: 80 is reached 10 x-units after the end.
        opts.forecast = Some(Horizon::Auto);
        opts.analysis.targets = crate::stats::Targets::parse(&["a=80".into()], false).unwrap();
        let (_, ahead) = opts.projection(&ds.series[0]).unwrap();
        assert!((ahead - 10.0 * 1.1).abs() < 1e-9);
        // A far target falls back to half the span.
        opts.analysis.targets = crate::stats::Targets::parse(&["a=-1000".into()], false).unwrap();
        assert_eq!(opts.projection(&ds.series[0]).unwrap().1, 5.0);
    }

    #[test]
    fn big_digits() {
        assert_eq!(
            big_text("-1.5").unwrap(),
            [
                "    ▄█    █▀▀".to_string(),
                "▀▀▀  █    ▀▀█".to_string(),
                "    ▀▀▀ ▀ ▀▀▀".to_string(),
            ]
        );
        assert!(big_text("12a").is_none());
        assert_eq!(split_number("58.8 GB"), ("58.8", " GB"));
        assert_eq!(split_number("97%"), ("97", "%"));
    }

    #[test]
    fn stat_tiles_show_value_change_and_target() {
        let s = Series {
            name: "node1".into(),
            unit: Unit::new(crate::units::Dim::Bytes { binary: false }),
            points: vec![(0.0, 60e9), (86400.0, 58e9), (172_800.0, 56e9)],
        };
        let ds = Dataset {
            x_kind: XKind::Time,
            series: vec![s],
        };
        let opts = ChartOpts {
            kind: ChartKind::Stat,
            analysis: Analysis {
                targets: crate::stats::Targets::parse(&["node1=50GB".into()], false).unwrap(),
                ..Analysis::default()
            },
            ..ChartOpts::default()
        };
        let text = crate::render::ansi::to_ansi(&render_to_buffer(&ds, &opts, 40, 10), false);
        assert!(text.contains("node1") && text.contains("GB"), "{text}");
        assert!(text.contains("▼ 4 GB (-6.7%) in 2 days"), "{text}");
        assert!(text.contains("50 GB in 3 days"), "{text}");
        // Too small for big digits: plain text.
        let text = crate::render::ansi::to_ansi(&render_to_buffer(&ds, &opts, 40, 4), false);
        assert!(text.contains("56 GB"), "{text}");
    }

    #[test]
    fn candles_from_open_high_low_close() {
        let mk = |name: &str, v: [f64; 3]| Series {
            name: name.into(),
            unit: Unit::default(),
            points: v
                .iter()
                .enumerate()
                .map(|(i, &y)| (i as f64 * 60.0, y))
                .collect(),
        };
        // Up, down, up; with an EMA drawn over them.
        let ds = Dataset {
            x_kind: XKind::Time,
            series: vec![
                mk("ema", [12.0, 13.0, 13.0]),
                mk("eth.o", [10.0, 16.0, 11.0]),
                mk("eth.h", [17.0, 18.0, 19.0]),
                mk("eth.l", [9.0, 10.0, 10.0]),
                mk("eth.c", [15.0, 12.0, 18.0]),
            ],
        };
        let roles = candle_roles(&ds.series.iter().enumerate().collect::<Vec<_>>()).unwrap();
        assert_eq!(
            roles.map(|r| r.1.name.as_str()),
            ["eth.o", "eth.h", "eth.l", "eth.c"]
        );
        let opts = ChartOpts {
            kind: ChartKind::Candles,
            ..ChartOpts::default()
        };
        let buf = render_to_buffer(&ds, &opts, 40, 16);
        let cells = |sym: &str, color: Color| {
            buf.content
                .iter()
                .filter(|c| c.symbol() == sym && c.fg == color)
                .count()
        };
        assert!(cells("█", Color::Green) > 0 && cells("█", Color::Red) > 0);
        assert!(cells("│", Color::Green) > 0 && cells("│", Color::Red) > 0);
        // The EMA crosses the bodies: its dots stay, on the candle's colour.
        let braille = |c: &ratatui::buffer::Cell| {
            c.symbol()
                .chars()
                .next()
                .is_some_and(|ch| ('\u{2801}'..='\u{28FF}').contains(&ch))
        };
        assert!(
            buf.content
                .iter()
                .any(|c| braille(c) && matches!(c.bg, Color::Green | Color::Red))
        );
        // Only the EMA is a line: no braille in the candles' colours.
        let text = crate::render::ansi::to_ansi(&buf, false);
        assert!(
            text.contains(['⠁', '⠂', '⠄', '⡀', '⠈', '⠐', '⠠', '⢀', '⠉', '⠒', '⠤', '⣀']),
            "{text}"
        );
        // Fewer than four series: a plain line chart.
        let two = Dataset {
            x_kind: XKind::Time,
            series: ds.series[..2].to_vec(),
        };
        let buf = render_to_buffer(&two, &opts, 40, 16);
        assert!(!buf.content.iter().any(|c| c.symbol() == "█"));
    }

    #[test]
    fn series_in_front_are_drawn_on_top() {
        let line = |name: &str| Series {
            name: name.into(),
            unit: Unit::default(),
            points: (0..20).map(|i| (f64::from(i), 5.0)).collect(),
        };
        let ds = Dataset {
            x_kind: XKind::Index,
            series: vec![line("rsi"), line("level")],
        };
        let colors = |opts: &ChartOpts| -> Vec<Color> {
            let buf = render_to_buffer(&ds, opts, 40, 12);
            let mut seen: Vec<Color> = buf
                .content
                .iter()
                .filter(|c| {
                    c.symbol()
                        .chars()
                        .all(|ch| ('\u{2801}'..='\u{28FF}').contains(&ch))
                })
                .map(|c| c.fg)
                .collect();
            seen.dedup();
            seen
        };
        let opts = ChartOpts::default();
        // Normally the later series is on top; front= puts rsi there.
        assert_eq!(colors(&opts), [opts.color(1)]);
        let front = ChartOpts {
            front: vec!["rsi".into()],
            ..ChartOpts::default()
        };
        assert_eq!(colors(&front), [opts.color(0)]);
    }

    #[test]
    fn marker_badges_sit_on_their_colour() {
        let ds = Dataset {
            x_kind: XKind::Time,
            series: vec![Series {
                name: "eth".into(),
                unit: Unit::default(),
                points: (0..20)
                    .map(|i| (f64::from(i) * 60.0, f64::from(i % 5)))
                    .collect(),
            }],
        };
        let mark = Mark {
            series: Some("eth".into()),
            x: 300.0,
            y: Some(0.0),
            label: "cross".into(),
            rule: Some(0),
            symbol: "▲".into(),
            color: Some(Color::Indexed(208)),
            effects: Modifier::ITALIC,
            badge: true,
            bg: None,
            lane: None,
        };
        let opts = ChartOpts {
            marks: vec![mark],
            ..ChartOpts::default()
        };
        let buf = render_to_buffer(&ds, &opts, 60, 16);
        let label: Vec<&ratatui::buffer::Cell> = buf
            .content
            .iter()
            .filter(|c| c.bg == Color::Indexed(208))
            .collect();
        let text: String = label.iter().map(|c| c.symbol()).collect();
        assert_eq!(text, " cross ", "the label on the marker's colour");
        assert!(
            label
                .iter()
                .all(|c| c.modifier.contains(Modifier::ITALIC | Modifier::BOLD))
        );
    }

    #[test]
    fn values_hold_briefly_after_the_newest_point() {
        let pts = [(0.0, 10.0), (1.0, 20.0), (2.0, 30.0)];
        assert_eq!(value_near(&pts, 1.5), Some(25.0));
        assert_eq!(
            value_near(&pts, 2.5),
            Some(30.0),
            "a moment later: still the last price"
        );
        assert_eq!(value_near(&pts, 9.0), None, "long gone: not carried on");
        assert_eq!(value_near(&pts, -1.0), None);
    }

    #[test]
    fn interpolates_between_points() {
        let pts = [(0.0, 10.0), (10.0, 20.0)];
        assert_eq!(interpolate(&pts, 5.0), Some(15.0));
        assert_eq!(interpolate(&pts, 0.0), Some(10.0));
        assert_eq!(interpolate(&pts, 10.0), Some(20.0));
        assert_eq!(interpolate(&pts, 11.0), None);
    }

    #[test]
    fn y_bounds_pad_and_override() {
        assert_eq!(y_bounds([5.0, 5.0].into_iter(), None, None), (4.75, 5.25));
        let (lo, hi) = y_bounds([0.0, 10.0].into_iter(), Some(0.0), None);
        assert_eq!((lo, hi), (0.0, 10.5));
    }

    #[test]
    fn renders_every_kind_without_panicking() {
        let s = Series {
            name: "a".into(),
            unit: Unit::default(),
            points: (0..50).map(|i| (i as f64, (i as f64).sin())).collect(),
        };
        let ds = Dataset {
            x_kind: XKind::Index,
            series: vec![s],
        };
        for kind in [
            ChartKind::Line,
            ChartKind::Scatter,
            ChartKind::Bar,
            ChartKind::Histogram,
            ChartKind::Sparkline,
            ChartKind::Stat,
        ] {
            let opts = ChartOpts {
                kind,
                title: "t".into(),
                hlines: vec![HLine {
                    value: 0.5,
                    color: None,
                    label: Some("half".into()),
                }],
                bins: 10,
                split: kind == ChartKind::Line,
                forecast: Some(Horizon::Auto),
                analysis: Analysis {
                    targets: crate::stats::Targets::parse(&["a=0.5".into()], false).unwrap(),
                    ..Analysis::default()
                },
                ..ChartOpts::default()
            };
            for (w, h) in [(80, 20), (10, 4), (200, 60)] {
                let buf = render_to_buffer(&ds, &opts, w, h);
                assert_eq!(buf.area.width, w);
                // With a cursor (and readout) too.
                let area = Rect::new(0, 0, w, h);
                let mut buf = Buffer::empty(area);
                let plots = RefCell::new(Vec::new());
                let overlay = Overlay {
                    cursor: Some(60.0),
                    plots: Some(&plots),
                    ..Overlay::default()
                };
                ChartWidget::new(&ds, &opts, overlay).render(area, &mut buf);
            }
        }
    }
}
