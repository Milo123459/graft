//! The `book` chart: an order book read from its levels' price and size series. The
//! size resting at each price over time is a heatmap (bids green, asks red, brighter
//! for more), with the mid price drawn over it; beside it, the book at one moment (the
//! cursor's, else the newest) as a ladder.

use std::collections::BTreeMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::{ChartWidget, PlotArea, x_bounds};
use crate::data::{Dataset, Series};
use crate::units::fmt_compact;

/// Last parts of a level's price series name, and of its size series.
const PRICE: &[&str] = &["px", "price", "p", "rate"];
const SIZE: &[&str] = &[
    "sz", "size", "qty", "quantity", "q", "amount", "volume", "v",
];

/// Brightness steps for the size resting at a price (256-colour palette).
const BID_RAMP: [u8; 5] = [22, 28, 34, 40, 46];
const ASK_RAMP: [u8; 5] = [52, 88, 124, 160, 196];

/// One price level: its price and the size resting there, over time.
#[derive(Debug, Clone, Copy)]
pub struct Level<'a> {
    pub price: &'a Series,
    pub size: &'a Series,
}

/// Bids (best first, as the source lists them) and asks.
#[derive(Debug, Clone)]
pub struct Book<'a> {
    pub bids: Vec<Level<'a>>,
    pub asks: Vec<Level<'a>>,
}

/// The book at one moment: (price, size) for each side, best first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub at: f64,
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

impl Snapshot {
    pub fn best_bid(&self) -> Option<f64> {
        self.bids.iter().map(|l| l.0).max_by(f64::total_cmp)
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks.iter().map(|l| l.0).min_by(f64::total_cmp)
    }

    pub fn mid(&self) -> Option<f64> {
        Some((self.best_bid()? + self.best_ask()?) / 2.0)
    }

    /// Buyers' size against sellers', -1 to 1.
    pub fn imbalance(&self) -> Option<f64> {
        let b: f64 = self.bids.iter().map(|l| l.1).sum();
        let a: f64 = self.asks.iter().map(|l| l.1).sum();
        (b + a > 0.0).then(|| (b - a) / (b + a))
    }

    /// The smallest gap between two prices: how finely prices are written.
    fn tick(&self) -> f64 {
        let mut p: Vec<f64> = self.bids.iter().chain(&self.asks).map(|l| l.0).collect();
        p.sort_by(f64::total_cmp);
        p.windows(2)
            .map(|w| w[1] - w[0])
            .filter(|d| *d > 1e-12)
            .min_by(f64::total_cmp)
            .unwrap_or(1.0)
    }
}

/// The last value at or before `t`.
fn at(s: &Series, t: f64) -> Option<f64> {
    let i = s.points.partition_point(|p| p.0 <= t);
    s.points[..i]
        .iter()
        .rev()
        .map(|p| p.1)
        .find(|v| v.is_finite())
}

impl Book<'_> {
    /// The book as it was at `t` (each level's latest values by then).
    pub fn at(&self, t: f64) -> Snapshot {
        let side = |levels: &[Level]| {
            levels
                .iter()
                .filter_map(|l| Some((at(l.price, t)?, at(l.size, t)?)))
                .filter(|(p, s)| *p > 0.0 && *s >= 0.0)
                .collect::<Vec<_>>()
        };
        Snapshot {
            at: self.time_at(t).unwrap_or(t),
            bids: side(&self.bids),
            asks: side(&self.asks),
        }
    }

    /// When the snapshot in force at `t` came in.
    fn time_at(&self, t: f64) -> Option<f64> {
        let s = self.bids.first().or(self.asks.first())?.price;
        let i = s.points.partition_point(|p| p.0 <= t);
        i.checked_sub(1).map(|i| s.points[i].0)
    }

    /// When it starts and ends.
    pub fn span(&self) -> Option<(f64, f64)> {
        let s = self.bids.first().or(self.asks.first())?.price;
        Some((s.points.first()?.0, s.points.last()?.0))
    }
}

/// Whether a name says which side it is.
fn side_named(name: &str) -> Option<bool> {
    let n = name.to_ascii_lowercase();
    match (
        n.contains("bid") || n.contains("buy"),
        n.contains("ask") || n.contains("sell"),
    ) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// Find an order book's levels among the series, by their names: a price and a size
/// with the same name up to the last part (`levels.0.3.px` and `levels.0.3.sz`, or
/// `bids.3.0` and `bids.3.1` for `[price, size]` pairs). Levels group by the name
/// before their number (`levels.0`, `bids`); which group is the bids comes from its
/// name, else it's the one with the lower prices.
pub fn find(ds: &Dataset) -> Option<Book<'_>> {
    let by_name: BTreeMap<&str, &Series> = ds
        .series
        .iter()
        .filter(|s| !s.points.is_empty())
        .map(|s| (s.name.as_str(), s))
        .collect();
    // group → (number, level)
    let mut groups: BTreeMap<&str, Vec<(usize, Level)>> = BTreeMap::new();
    for (name, &price) in &by_name {
        let Some((prefix, last)) = name.rsplit_once('.') else {
            continue;
        };
        let pair = side_named(prefix).is_some() && last == "0";
        if !PRICE.contains(&last) && !pair {
            continue;
        }
        let sizes: &[&str] = if pair { &["1"] } else { SIZE };
        let Some(&size) = sizes
            .iter()
            .find_map(|s| by_name.get(format!("{prefix}.{s}").as_str()))
        else {
            continue;
        };
        let (group, n) = match prefix.rsplit_once('.') {
            Some((g, n)) if n.parse::<usize>().is_ok() => (g, n.parse().unwrap_or(0)),
            _ => (prefix, 0),
        };
        groups
            .entry(group)
            .or_default()
            .push((n, Level { price, size }));
    }
    let mut groups: Vec<(&str, Vec<Level>)> = groups
        .into_iter()
        .map(|(g, mut v)| {
            v.sort_by_key(|l| l.0);
            (g, v.into_iter().map(|l| l.1).collect())
        })
        .collect();
    // The two biggest groups are the sides.
    groups.sort_by_key(|g| std::cmp::Reverse(g.1.len()));
    groups.truncate(2);
    let [(a_name, a), (b_name, b)] = <[_; 2]>::try_from(groups).ok()?;
    let latest = |levels: &[Level]| {
        levels
            .first()
            .and_then(|l| l.price.points.last())
            .map(|p| p.1)
    };
    let a_is_bids = match (side_named(a_name), side_named(b_name)) {
        (Some(s), _) => s,
        (None, Some(s)) => !s,
        (None, None) => latest(&a)? < latest(&b)?,
    };
    let (bids, asks) = if a_is_bids { (a, b) } else { (b, a) };
    Some(Book { bids, asks })
}

/// A price as finely as the book writes them.
fn fmt_price(p: f64, tick: f64) -> String {
    let decimals = (-tick.log10().floor()).clamp(0.0, 8.0) as usize;
    format!("{p:.decimals$}")
}

fn ramp(side_bids: bool, k: usize) -> Color {
    Color::Indexed(if side_bids { BID_RAMP[k] } else { ASK_RAMP[k] })
}

impl ChartWidget<'_> {
    pub(super) fn render_book(&self, area: Rect, buf: &mut Buffer) {
        // The main source's raw fields (the levels) are in the full data; a pane with
        // its own source has them in its own.
        let data = self.overlay.full.unwrap_or(self.ds);
        let Some(book) = find(data).or_else(|| find(self.ds)) else {
            return self.render_empty(
                area,
                buf,
                "no order book: needs a price and a size for each level (levels.0.0.px and levels.0.0.sz …)",
            );
        };
        let block = self.block();
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width < 10 || inner.height < 3 {
            return;
        }
        let (x0, x1) = {
            let (a, b) = x_bounds(data, self.overlay.view);
            match (self.overlay.view, book.span()) {
                // Everything: just where there's a book.
                (None, Some((lo, hi))) if hi > lo => (lo, hi),
                _ => (a, b),
            }
        };
        let now = self
            .overlay
            .cursor
            .filter(|c| (x0..=x1).contains(c))
            .unwrap_or(x1);
        let shown = book.at(now);
        let tick = shown.tick();
        // The summary: best bid and ask, spread, imbalance.
        let mut summary = Vec::new();
        if let (Some(b), Some(a)) = (shown.best_bid(), shown.best_ask()) {
            summary.push(Span::styled(
                format!("bid {}", fmt_price(b, tick)),
                Style::default().fg(Color::Green),
            ));
            summary.push(Span::styled(
                format!(" ask {}", fmt_price(a, tick)),
                Style::default().fg(Color::Red),
            ));
            summary.push(Span::styled(
                format!(" spr {}", fmt_price(a - b, tick)),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(i) = shown.imbalance() {
            summary.push(Span::styled(
                format!(" imb {i:+.2}"),
                Style::default().fg(if i >= 0.0 { Color::Green } else { Color::Red }),
            ));
        }
        let when = if self.overlay.cursor.is_some() {
            crate::time::format_time(shown.at, 60.0)
        } else {
            "now".into()
        };
        summary.push(Span::styled(
            format!(" · {when}"),
            Style::default().fg(Color::DarkGray),
        ));
        Line::from(summary).render(Rect { height: 1, ..inner }, buf);
        let body = Rect {
            y: inner.y + 1,
            height: inner.height - 1,
            ..inner
        };
        // Ladder on the right; the heatmap gets the rest (none when it's too narrow).
        let ladder_w = (body.width * 2 / 5).clamp(18, 30).min(body.width);
        let heat_w = body.width.saturating_sub(ladder_w + 1);
        let ladder = Rect {
            x: body.right() - ladder_w,
            width: ladder_w,
            ..body
        };
        self.draw_ladder(&shown, tick, ladder, buf);
        if heat_w >= 8 {
            let heat = Rect {
                width: heat_w,
                ..body
            };
            self.draw_heatmap(&book, (x0, x1), heat, tick, buf);
        }
    }

    /// The book at one moment: asks above (best at the bottom), bids below, a bar for
    /// the size at each price.
    fn draw_ladder(&self, s: &Snapshot, tick: f64, area: Rect, buf: &mut Buffer) {
        let rows = area.height as usize;
        let mut asks: Vec<(f64, f64)> = s.asks.clone();
        asks.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut bids: Vec<(f64, f64)> = s.bids.clone();
        bids.sort_by(|a, b| b.0.total_cmp(&a.0));
        // The levels nearest the price, as many as fit (half each side).
        let n_ask = asks.len().min(rows.div_ceil(2)).min(rows);
        let n_bid = bids.len().min(rows - n_ask);
        let n_ask = asks.len().min(rows - n_bid);
        let shown: Vec<(f64, f64, bool)> = asks[..n_ask]
            .iter()
            .rev()
            .map(|l| (l.0, l.1, false))
            .chain(bids[..n_bid].iter().map(|l| (l.0, l.1, true)))
            .collect();
        let biggest = shown.iter().map(|l| l.1).fold(0.0, f64::max).max(1e-12);
        let width = shown
            .iter()
            .map(|l| fmt_price(l.0, tick).len())
            .max()
            .unwrap_or(0);
        // price · size · bar (the sizes in a column of their own when there's room).
        let sizes: Vec<String> = shown.iter().map(|l| fmt_compact(l.1)).collect();
        let size_w = sizes.iter().map(|t| t.len()).max().unwrap_or(0);
        let room = (area.width as usize).saturating_sub(width + 1);
        let size_w = if room >= size_w + 4 { size_w } else { 0 };
        let bar_room = room.saturating_sub(if size_w > 0 { size_w + 1 } else { 0 });
        let top = area.y + ((rows - shown.len()) / 2) as u16;
        for (k, ((p, size, bid), text)) in shown.iter().zip(&sizes).enumerate() {
            let color = if *bid { Color::Green } else { Color::Red };
            let len = size / biggest * bar_room as f64;
            let mut spans = vec![
                Span::styled(
                    format!("{:>width$}", fmt_price(*p, tick)),
                    Style::default().fg(color),
                ),
                Span::raw(" "),
            ];
            if size_w > 0 {
                spans.push(Span::styled(
                    format!("{text:>size_w$} "),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            spans.push(Span::styled(
                super::eighths_bar(len),
                Style::default().fg(color),
            ));
            Line::from(spans).render(
                Rect {
                    y: top + k as u16,
                    height: 1,
                    ..area
                },
                buf,
            );
        }
    }

    /// Resting size by price over time, two price steps a row (half blocks), with the
    /// mid price over it.
    fn draw_heatmap(
        &self,
        book: &Book,
        (x0, x1): (f64, f64),
        area: Rect,
        tick: f64,
        buf: &mut Buffer,
    ) {
        let plot = PlotArea { rect: area, x0, x1 };
        if let Some(plots) = self.overlay.plots {
            plots.borrow_mut().push(plot);
        }
        let cols: Vec<Snapshot> = (area.x..area.right())
            .map(|c| {
                let t = plot.x_at(c);
                let s = book.at(t);
                // Nothing yet at this time.
                if book.span().is_none_or(|(lo, _)| t < lo) {
                    Snapshot::default()
                } else {
                    s
                }
            })
            .collect();
        let prices = cols
            .iter()
            .flat_map(|s| s.bids.iter().chain(&s.asks).map(|l| l.0));
        let Some((lo, hi)) = super::min_max(prices) else {
            return;
        };
        let (lo, hi) = if hi > lo {
            (lo, hi)
        } else {
            (lo - tick, hi + tick)
        };
        let halves = area.height as usize * 2;
        let row_of =
            |p: f64| -> usize { (((hi - p) / (hi - lo) * halves as f64) as usize).min(halves - 1) };
        // Size in each half-row of each column, per side.
        let mut cells = vec![vec![(0.0f64, 0.0f64); halves]; cols.len()];
        for (c, s) in cols.iter().enumerate() {
            for l in &s.bids {
                cells[c][row_of(l.0)].0 += l.1;
            }
            for l in &s.asks {
                cells[c][row_of(l.0)].1 += l.1;
            }
        }
        let biggest = cells
            .iter()
            .flatten()
            .map(|(b, a)| b.max(*a))
            .fold(0.0, f64::max)
            .max(1e-12);
        let color = |(b, a): (f64, f64)| -> Option<Color> {
            let (v, bid) = if b >= a { (b, true) } else { (a, false) };
            (v > 0.0).then(|| {
                let k = ((v / biggest).sqrt() * 5.0).ceil() as usize;
                ramp(bid, k.clamp(1, 5) - 1)
            })
        };
        for (c, s) in cols.iter().enumerate() {
            let x = area.x + c as u16;
            let mid = s.mid().map(row_of);
            for r in 0..area.height as usize {
                let (up, down) = (2 * r, 2 * r + 1);
                let (cu, cd) = (color(cells[c][up]), color(cells[c][down]));
                let cell = &mut buf[(x, area.y + r as u16)];
                let white = Color::White;
                match (mid == Some(up), mid == Some(down), cu, cd) {
                    (true, _, _, d) => {
                        cell.set_symbol("▀")
                            .set_fg(white)
                            .set_bg(d.unwrap_or(Color::Reset));
                    }
                    (_, true, u, _) => {
                        cell.set_symbol("▄")
                            .set_fg(white)
                            .set_bg(u.unwrap_or(Color::Reset));
                    }
                    (_, _, Some(u), Some(d)) => {
                        cell.set_symbol("▀").set_fg(u).set_bg(d);
                    }
                    (_, _, Some(u), None) => {
                        cell.set_symbol("▀").set_fg(u);
                    }
                    (_, _, None, Some(d)) => {
                        cell.set_symbol("▄").set_fg(d);
                    }
                    _ => {}
                }
            }
        }
        // Where the ladder's moment is.
        if let Some(cur) = self.overlay.cursor.filter(|c| (x0..=x1).contains(c)) {
            let x = plot.col_of(cur);
            for y in area.y..area.bottom() {
                let cell = &mut buf[(x, y)];
                if cell.symbol() == " " {
                    cell.set_symbol("┊").set_fg(Color::Yellow);
                }
            }
        }
        // The price range, in the corners.
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        buf.set_string(area.x, area.y, fmt_price(hi, tick), dim);
        if area.height > 1 {
            buf.set_string(area.x, area.bottom() - 1, fmt_price(lo, tick), dim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::XKind;

    fn series(name: &str, points: Vec<(f64, f64)>) -> Series {
        Series {
            name: name.into(),
            unit: Default::default(),
            points,
        }
    }

    /// A book of `n` levels a side, a second apart for `secs` seconds, with the price
    /// drifting up and a wall of size 3 levels under the best bid.
    pub fn hyperliquid(n: usize, secs: usize) -> Dataset {
        let mut out = Vec::new();
        for side in 0..2 {
            for k in 0..n {
                let (mut px, mut sz) = (Vec::new(), Vec::new());
                for t in 0..secs {
                    let mid = 100.0 + t as f64 * 0.1;
                    let p = if side == 0 {
                        (mid - 0.5 - k as f64).floor()
                    } else {
                        (mid + 0.5 + k as f64).ceil()
                    };
                    let s = if side == 0 && k == 3 {
                        50.0
                    } else {
                        1.0 + k as f64
                    };
                    px.push((t as f64, p));
                    sz.push((t as f64, s));
                }
                out.push(series(&format!("levels.{side}.{k}.px"), px));
                out.push(series(&format!("levels.{side}.{k}.sz"), sz));
                out.push(series(
                    &format!("levels.{side}.{k}.n"),
                    (0..secs).map(|t| (t as f64, 3.0)).collect(),
                ));
            }
        }
        out.push(series(
            "price",
            (0..secs).map(|t| (t as f64, 100.0)).collect(),
        ));
        Dataset {
            x_kind: XKind::Time,
            series: out,
        }
    }

    #[test]
    fn finds_the_levels_by_name() {
        let ds = hyperliquid(5, 10);
        let b = find(&ds).unwrap();
        assert_eq!((b.bids.len(), b.asks.len()), (5, 5));
        // levels.0 has the lower prices: the bids, best (level 0) first.
        assert_eq!(b.bids[0].price.name, "levels.0.0.px");
        assert_eq!(b.asks[0].size.name, "levels.1.0.sz");
        let s = b.at(9.0);
        assert_eq!(s.best_bid(), Some(100.0));
        assert_eq!(s.best_ask(), Some(102.0));
        assert_eq!(s.mid(), Some(101.0));
        assert!(s.imbalance().unwrap() > 0.0, "the wall of bids");
        // Before the first snapshot: nothing.
        assert!(b.at(-1.0).bids.is_empty());

        // Binance's depth: bids.N.0 is the price, bids.N.1 the size — named sides, even
        // when the asks are listed first.
        let mut ds = Dataset::default();
        for (side, base) in [("asks", 101.0), ("bids", 99.0)] {
            for k in 0..3 {
                let p = if side == "bids" {
                    base - k as f64
                } else {
                    base + k as f64
                };
                ds.series
                    .push(series(&format!("{side}.{k}.0"), vec![(0.0, p)]));
                ds.series
                    .push(series(&format!("{side}.{k}.1"), vec![(0.0, 2.0)]));
            }
        }
        ds.series.push(series("lastUpdateId", vec![(0.0, 7.0)]));
        let b = find(&ds).unwrap();
        assert_eq!(b.bids[0].price.name, "bids.0.0");
        assert_eq!(b.at(0.0).best_ask(), Some(101.0));
        // Not a book.
        let ds = Dataset {
            x_kind: XKind::Time,
            series: vec![
                series("price", vec![(0.0, 1.0)]),
                series("vol", vec![(0.0, 2.0)]),
            ],
        };
        assert!(find(&ds).is_none());
    }

    #[test]
    fn draws_the_book_as_a_heatmap_and_a_ladder_at_46_columns() {
        let ds = hyperliquid(10, 120);
        let opts = crate::render::ChartOpts {
            kind: crate::cli::ChartKind::Book,
            title: "book".into(),
            ..Default::default()
        };
        for (w, h) in [(46u16, 16u16), (120, 30), (20, 6)] {
            let buf = crate::render::render_to_buffer(&ds, &opts, w, h);
            let text: String = (0..h)
                .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
                .collect();
            if w < 30 {
                continue;
            }
            // The summary and the ladder: best bid and ask, newest.
            assert!(text.contains("bid 111"), "{text}");
            assert!(text.contains("ask 113"), "{text}");
            assert!(text.contains("· now"), "{text}");
            // The heatmap: green and red cells, and the white mid line.
            let colors: Vec<Color> = (0..w)
                .flat_map(|x| (0..h).map(move |y| (x, y)))
                .flat_map(|(x, y)| [buf[(x, y)].fg, buf[(x, y)].bg])
                .collect();
            assert!(
                colors
                    .iter()
                    .any(|c| BID_RAMP.iter().any(|k| *c == Color::Indexed(*k)))
            );
            assert!(
                colors
                    .iter()
                    .any(|c| ASK_RAMP.iter().any(|k| *c == Color::Indexed(*k)))
            );
            // The wall is the brightest green.
            assert!(colors.contains(&Color::Indexed(46)), "{text}");
        }
    }

    #[test]
    fn the_ladder_shows_the_book_at_the_cursor_and_tapping_finds_the_time() {
        let ds = hyperliquid(10, 120);
        let opts = crate::render::ChartOpts {
            kind: crate::cli::ChartKind::Book,
            ..Default::default()
        };
        let area = Rect::new(0, 0, 60, 16);
        let mut buf = Buffer::empty(area);
        let plots = std::cell::RefCell::new(Vec::new());
        let overlay = crate::render::Overlay {
            cursor: Some(10.0),
            plots: Some(&plots),
            ..Default::default()
        };
        ChartWidget::new(&ds, &opts, overlay).render(area, &mut buf);
        let text: String = (0..16)
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        // At 10 s the mid was 101: best bid 100, best ask 102.
        assert!(
            text.contains("bid 100") && text.contains("ask 102"),
            "{text}"
        );
        // The heatmap is a plot area: a column maps back to a time.
        let p = plots.borrow()[0];
        assert!((p.x_at(p.col_of(60.0)) - 60.0).abs() < 2.0);
        assert_eq!((p.x0, p.x1), (0.0, 119.0));
    }
}
