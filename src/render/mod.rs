//! Chart rendering. One ratatui widget renders every chart type; it's used by the
//! interactive viewer and, via an off-screen buffer, by `--once`.

pub mod ansi;
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

use crate::cli::{ChartKind, FitModel, LegendPos, MarkerKind};
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
    /// Draw the fitted trend over the data.
    pub show_trend: bool,
    /// Draw explicit target lines.
    pub show_targets: bool,
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
            show_trend: false,
            show_targets: true,
        }
    }
}

impl ChartOpts {
    pub fn color(&self, idx: usize) -> Color {
        self.palette[idx % self.palette.len()]
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
    let mut t = (lo / step).ceil() * step;
    let mut out = Vec::new();
    while t <= hi + step * 1e-9 {
        out.push(if t.abs() < step * 1e-9 { 0.0 } else { t });
        t += step;
    }
    out
}

/// Round y ticks in the unit they're displayed in (`100 GB`, `250 ms`, ...).
pub fn y_ticks(y0: f64, y1: f64, target: usize, unit: &Unit, log: bool) -> Vec<(f64, String)> {
    if log {
        let decades: Vec<f64> = (y0.ceil() as i64..=y1.floor() as i64)
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
        if self.opts.log {
            title.push(Span::styled("[log] ", Style::default().fg(Color::DarkGray)));
        }
        if self.opts.forecast.is_some() {
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
        let block = self.block();
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
        let buckets = area.width.saturating_sub(14).max(10) as usize * 2;
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
                let mut pts = visible_points(s, x0, x1, buckets);
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
            // The trend over the data (optional), then the forecast to the right edge.
            if self.opts.show_trend
                && let Some(fit) = self.opts.analysis.fit(s)
            {
                let (from, to) = (fit.x_first.max(x0), fit.x_last.min(x1));
                if to > from {
                    let pieces = (cols * (to - from) / (x1 - x0) / 3.0) as usize;
                    let mut dashes = dashed(|x| fit.at(x), from, to, pieces);
                    dashes.iter_mut().for_each(&to_log);
                    projections.push((i, dashes));
                }
            }
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

        let in_range: Vec<f64> = lines
            .iter()
            .flat_map(|l| l.2.iter().filter(|p| p.0 >= x0 && p.0 <= x1).map(|p| p.1))
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
            self.opts.ymin.map(ty),
            self.opts.ymax.map(ty),
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
            }
        }
        if let Some(c) = self.overlay.cursor {
            overlays.push((
                vec![(c, y0), (c, y1)],
                Style::default().fg(Color::White),
                Marker::Braille,
                None,
            ));
        }

        let graph = if self.opts.kind == ChartKind::Scatter {
            GraphType::Scatter
        } else {
            GraphType::Line
        };
        let mk = marker(self.opts.marker);
        let mut datasets: Vec<RDataset> = overlays
            .iter()
            .map(|(pts, style, m, label)| {
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
        for (i, s, pts) in &lines {
            let last = s
                .in_view(self.overlay.view)
                .last()
                .or(s.points.last())
                .map(|p| s.unit.format(p.1))
                .unwrap_or_default();
            datasets.push(
                RDataset::default()
                    .name(format!("{} {last}", s.name))
                    .data(pts)
                    .graph_type(graph)
                    .marker(mk)
                    .style(Style::default().fg(self.opts.color(*i))),
            );
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
        if let Some(c) = self.overlay.cursor
            && (x0..=x1).contains(&c)
        {
            self.render_readout(
                buf,
                &plot,
                c,
                &lines.iter().map(|l| (l.0, l.1)).collect::<Vec<_>>(),
            );
        }
    }

    /// A small box beside the cursor listing each series' value there.
    fn render_readout(
        &self,
        buf: &mut Buffer,
        plot: &PlotArea,
        x: f64,
        series: &[(usize, &Series)],
    ) {
        let mut lines = vec![Line::styled(
            self.ds.fmt_x_full(round_x(self.ds, x)),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        for &(i, s) in series {
            if let Some((v, projected)) = value_at(s, self.opts, x) {
                let text = if projected {
                    format!("≈{} (forecast)", s.unit.format(v))
                } else {
                    s.unit.format(v)
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ", s.name),
                        Style::default().fg(self.opts.color(i)),
                    ),
                    Span::raw(text),
                ]));
            }
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
        let latest: Vec<(usize, &Series, f64)> = self
            .visible()
            .into_iter()
            .filter_map(|(i, s)| s.in_view(self.overlay.view).last().map(|p| (i, s, p.1)))
            .collect();
        if latest.is_empty() {
            return self.render_empty(area, buf, "waiting for data…");
        }
        let block = self.block();
        let inner = block.inner(area);
        block.render(area, buf);
        let max = latest
            .iter()
            .map(|l| l.2.abs())
            .fold(0.0, f64::max)
            .max(f64::MIN_POSITIVE);
        let name_w = (latest
            .iter()
            .map(|l| l.1.name.chars().count())
            .max()
            .unwrap_or(0) as u16)
            .min(inner.width / 3);
        let value_w = latest
            .iter()
            .map(|l| l.1.unit.format(l.2).chars().count())
            .max()
            .unwrap_or(0) as u16;
        let bar_w = inner.width.saturating_sub(name_w + value_w + 3);
        let gap = u16::from(latest.len() as u16 * 2 <= inner.height);
        for (k, &(i, s, v)) in latest.iter().enumerate() {
            let row = inner.y + k as u16 * (1 + gap);
            if row >= inner.bottom() {
                break;
            }
            let color = self.opts.color(i);
            let name: String = s.name.chars().take(name_w as usize).collect();
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
            " {} · distribution of {} · n={} ",
            self.opts.title,
            s.name,
            values.len()
        );
        let block = Block::bordered()
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            ));
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

impl Widget for ChartWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 4 || area.height < 3 {
            return;
        }
        match self.opts.kind {
            ChartKind::Line | ChartKind::Scatter if self.opts.split => self.render_split(area, buf),
            ChartKind::Line | ChartKind::Scatter => {
                self.render_xy(area, buf, self.visible(), &self.opts.title, false)
            }
            ChartKind::Bar => self.render_bar(area, buf),
            ChartKind::Histogram => self.render_histogram(area, buf),
            ChartKind::Sparkline => self.render_sparklines(area, buf),
        }
    }
}

/// Render a chart into an off-screen buffer.
pub fn render_to_buffer(ds: &Dataset, opts: &ChartOpts, width: u16, height: u16) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    ChartWidget::new(ds, opts, Overlay::default()).render(area, &mut buf);
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
