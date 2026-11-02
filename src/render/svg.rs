//! Standalone SVG export (no font or system dependencies).

use std::fmt::Write;

use ratatui::style::Color;

use super::{ChartOpts, common_unit, hline_near, min_max, nice_ticks, spaced, y_bounds, y_ticks};
use crate::cli::{ChartKind, LegendPos};
use crate::data::{Dataset, Series};

/// Colours that read well on a white background.
fn hex(c: Color) -> String {
    let (r, g, b) = match c {
        Color::Black => (0x22, 0x22, 0x22),
        Color::Red => (0xd6, 0x33, 0x33),
        Color::Green => (0x2f, 0x9e, 0x44),
        Color::Yellow => (0xe0, 0x9f, 0x00),
        Color::Blue => (0x1c, 0x5d, 0xd6),
        Color::Magenta => (0xae, 0x3e, 0xc9),
        Color::Cyan => (0x10, 0x98, 0xad),
        Color::Gray => (0x86, 0x8e, 0x96),
        Color::DarkGray => (0x49, 0x50, 0x57),
        Color::LightRed => (0xff, 0x6b, 0x6b),
        Color::LightGreen => (0x51, 0xcf, 0x66),
        Color::LightYellow => (0xfc, 0xc4, 0x19),
        Color::LightBlue => (0x4d, 0xab, 0xf7),
        Color::LightMagenta => (0xda, 0x77, 0xf2),
        Color::LightCyan => (0x3b, 0xc9, 0xdb),
        Color::White | Color::Reset => (0x99, 0x99, 0x99),
        Color::Rgb(r, g, b) => (r, g, b),
        // The default palette, darkened to read well on white.
        Color::Indexed(81) => (0x1c, 0x7e, 0xd6),
        Color::Indexed(213) => (0xd6, 0x33, 0x6c),
        Color::Indexed(221) => (0xe0, 0x8a, 0x00),
        Color::Indexed(114) => (0x2f, 0x9e, 0x44),
        Color::Indexed(209) => (0xf7, 0x67, 0x07),
        Color::Indexed(141) => (0x70, 0x48, 0xe8),
        Color::Indexed(203) => (0xe0, 0x31, 0x31),
        Color::Indexed(43) => (0x0c, 0x85, 0x99),
        Color::Indexed(33) => (0x19, 0x71, 0xc2),
        Color::Indexed(186) => (0x99, 0x8a, 0x00),
        Color::Indexed(168) => (0xa6, 0x1e, 0x4d),
        Color::Indexed(250) => (0x86, 0x8e, 0x96),
        Color::Indexed(i) => xterm(i),
    };
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn xterm(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => {
            let basic = [
                Color::Black,
                Color::Red,
                Color::Green,
                Color::Yellow,
                Color::Blue,
                Color::Magenta,
                Color::Cyan,
                Color::Gray,
                Color::DarkGray,
                Color::LightRed,
                Color::LightGreen,
                Color::LightYellow,
                Color::LightBlue,
                Color::LightMagenta,
                Color::LightCyan,
                Color::White,
            ];
            let h = hex(basic[i as usize]);
            let p = |a: usize| u8::from_str_radix(&h[a..a + 2], 16).unwrap_or(0);
            (p(1), p(3), p(5))
        }
        16..=231 => {
            let i = i - 16;
            let lvl = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (lvl(i / 36), lvl((i / 6) % 6), lvl(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// Blend a `#rrggbb` colour towards white by `amount` (0..1).
fn tint(hex: &str, amount: f64) -> String {
    let c = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f64;
    let mix = |v: f64| (v + (255.0 - v) * amount).round() as u8;
    format!("#{:02x}{:02x}{:02x}", mix(c(1)), mix(c(3)), mix(c(5)))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

struct Frame {
    left: f64,
    top: f64,
    w: f64,
    h: f64,
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
}

impl Frame {
    fn px(&self, x: f64) -> f64 {
        self.left + (x - self.x0) / (self.x1 - self.x0) * self.w
    }
    fn py(&self, y: f64) -> f64 {
        self.top + self.h - (y - self.y0) / (self.y1 - self.y0) * self.h
    }
}

pub fn render(ds: &Dataset, opts: &ChartOpts, width: u32, height: u32) -> String {
    let (w, h) = (width as f64, height as f64);
    let mut out = String::new();
    let _ = writeln!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="ui-sans-serif, system-ui, sans-serif" font-size="12">"#
    );
    let _ = writeln!(out, r#"<rect width="100%" height="100%" fill="white"/>"#);
    let _ = writeln!(
        out,
        r##"<text x="{}" y="24" text-anchor="middle" font-size="16" font-weight="600" fill="#222">{}</text>"##,
        w / 2.0,
        esc(&opts.title)
    );

    let series: Vec<(usize, &Series)> = ds
        .series
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.points.is_empty())
        .collect();
    if series.is_empty() {
        let _ = writeln!(
            out,
            r##"<text x="{}" y="{}" text-anchor="middle" fill="#888">no data</text></svg>"##,
            w / 2.0,
            h / 2.0
        );
        return out;
    }
    match opts.kind {
        ChartKind::Bar => bars(&mut out, &series, opts, w, h),
        ChartKind::Histogram => histogram(&mut out, series[0], opts, w, h),
        _ if opts.split && series.len() > 1 => {
            let panel = (h - 40.0) / series.len() as f64;
            for (k, s) in series.iter().enumerate() {
                let top = 40.0 + panel * k as f64;
                let _ = writeln!(
                    out,
                    r##"<text x="90" y="{:.1}" font-weight="600" fill="#333">{}</text>"##,
                    top + 14.0,
                    esc(&s.1.name)
                );
                xy(&mut out, ds, &[*s], opts, (top + 22.0, panel - 22.0), w, k);
            }
        }
        _ => xy(&mut out, ds, &series, opts, (40.0, h - 40.0), w, 0),
    }
    out.push_str("</svg>\n");
    out
}

/// Line/scatter plot in the vertical band `(top, height)`; `id` keeps clip paths unique.
fn xy(
    out: &mut String,
    ds: &Dataset,
    series: &[(usize, &Series)],
    opts: &ChartOpts,
    (top, height): (f64, f64),
    w: f64,
    id: usize,
) {
    let log = opts.log;
    let ty = |v: f64| if log { v.log10() } else { v };
    let (x0, x1) = super::plot_x_bounds(ds, opts, None, None);
    // Fitted trends (continued as forecasts) and explicit targets.
    let projections: Vec<(usize, Vec<(f64, f64)>)> = series
        .iter()
        .filter_map(|&(i, s)| {
            let (fit, _) = opts.projection(s)?;
            Some((i, super::projection_points(&fit, fit.x_last, x1)))
        })
        .collect();
    let trends: Vec<(usize, Vec<(f64, f64)>)> = series
        .iter()
        .filter_map(|&(i, s)| Some((i, opts.trend_points(s)?)))
        .collect();
    let raw_shown = opts.show_raw || trends.is_empty();
    let near = super::min_max(
        series
            .iter()
            .flat_map(|(_, s)| s.points.iter().map(|p| p.1))
            .chain(projections.iter().flat_map(|p| p.1.iter().map(|p| p.1))),
    );
    let targets: Vec<(usize, &str, f64)> = series
        .iter()
        .filter(|_| opts.show_targets)
        .filter_map(|&(i, s)| Some((i, s.name.as_str(), opts.analysis.targets.explicit(&s.name)?)))
        .filter(|t| !log || t.2 > 0.0)
        .filter(|t| near.is_none_or(|(lo, hi)| hline_near(t.2, lo, hi)))
        .collect();
    let values: Vec<f64> = series
        .iter()
        .flat_map(|(_, s)| s.points.iter().map(|p| p.1))
        .chain(projections.iter().flat_map(|p| p.1.iter().map(|p| p.1)))
        .chain(targets.iter().map(|t| t.2))
        .filter(|v| !log || *v > 0.0)
        .map(ty)
        .collect();
    let data = min_max(values.iter().copied());
    let panel = opts.split;
    let hl = opts
        .hlines
        .iter()
        .map(|l| l.value)
        .filter(|v| !log || *v > 0.0)
        .map(ty)
        .filter(|&v| !panel || data.is_none_or(|(lo, hi)| hline_near(v, lo, hi)));
    let (y0, y1) = y_bounds(
        values.iter().copied().chain(hl),
        opts.ymin.map(ty),
        opts.ymax.map(ty),
    );
    let f = Frame {
        left: 90.0,
        top,
        w: w - 130.0,
        h: height - 40.0,
        x0,
        x1,
        y0,
        y1,
    };
    let unit = common_unit(series.iter().map(|s| s.1));

    // Grid and axes.
    let _ = writeln!(out, r##"<g stroke="#e9ecef" fill="#555">"##);
    let target = ((f.h / 60.0).round() as usize).clamp(2, 6);
    for (t, label) in y_ticks(y0, y1, target, &unit, log) {
        let y = f.py(t);
        let _ = writeln!(
            out,
            r#"<line x1="{}" x2="{}" y1="{y:.1}" y2="{y:.1}"/>"#,
            f.left,
            f.left + f.w
        );
        let _ = writeln!(
            out,
            r#"<text x="{}" y="{:.1}" text-anchor="end" stroke="none">{}</text>"#,
            f.left - 8.0,
            y + 4.0,
            esc(&label)
        );
    }
    // Calendar-aligned time ticks (or round numbers), like the terminal chart.
    let xticks = super::x_ticks(ds, opts, x0, x1, (f.w / 110.0) as usize);
    let n = xticks.len();
    for (k, (t, label)) in xticks.into_iter().enumerate() {
        let x = f.px(t);
        let anchor = if k == 0 && x - f.left < 30.0 {
            "start"
        } else if k + 1 == n && f.left + f.w - x < 30.0 {
            "end"
        } else {
            "middle"
        };
        let _ = writeln!(
            out,
            r#"<line x1="{x:.1}" x2="{x:.1}" y1="{}" y2="{}"/>"#,
            f.top,
            f.top + f.h
        );
        let _ = writeln!(
            out,
            r#"<text x="{x:.1}" y="{}" text-anchor="{anchor}" stroke="none">{}</text>"#,
            f.top + f.h + 18.0,
            esc(&label)
        );
    }
    let _ = writeln!(out, "</g>");
    let _ = writeln!(
        out,
        r##"<rect x="{}" y="{}" width="{}" height="{}" fill="none" stroke="#adb5bd"/>"##,
        f.left, f.top, f.w, f.h
    );
    let _ = writeln!(
        out,
        r#"<clipPath id="plot{id}"><rect x="{}" y="{}" width="{}" height="{}"/></clipPath>"#,
        f.left, f.top, f.w, f.h
    );
    let _ = writeln!(out, r#"<g clip-path="url(#plot{id})">"#);

    for l in &opts.hlines {
        if log && l.value <= 0.0 {
            continue;
        }
        let y = f.py(ty(l.value));
        let c = hex(l.color.unwrap_or(Color::DarkGray));
        let _ = writeln!(
            out,
            r#"<line x1="{}" x2="{}" y1="{y:.1}" y2="{y:.1}" stroke="{c}" stroke-dasharray="6 4"/>"#,
            f.left,
            f.left + f.w
        );
        if let Some(label) = &l.label {
            let _ = writeln!(
                out,
                r#"<text x="{}" y="{:.1}" fill="{c}">{}</text>"#,
                f.left + 6.0,
                y - 4.0,
                esc(label)
            );
        }
    }
    for &(i, name, t) in &targets {
        let y = f.py(ty(t));
        let c = hex(opts.color(i));
        let unit = ds.get(name).map(|s| s.unit.format(t)).unwrap_or_default();
        let _ = writeln!(
            out,
            r#"<line x1="{}" x2="{}" y1="{y:.1}" y2="{y:.1}" stroke="{c}" stroke-dasharray="2 3"/>"#,
            f.left,
            f.left + f.w
        );
        let _ = writeln!(
            out,
            r#"<text x="{}" y="{:.1}" fill="{c}" text-anchor="end">{} target {}</text>"#,
            f.left + f.w - 6.0,
            y - 4.0,
            esc(name),
            esc(&unit)
        );
    }
    for (i, pts) in &projections {
        let c = hex(opts.color(*i));
        let path: Vec<String> = pts
            .iter()
            .filter(|p| !log || p.1 > 0.0)
            .map(|p| format!("{:.1},{:.1}", f.px(p.0), f.py(ty(p.1))))
            .collect();
        let _ = writeln!(
            out,
            r#"<polyline fill="none" stroke="{c}" stroke-width="1.5" stroke-dasharray="5 4" opacity="0.8" points="{}"/>"#,
            path.join(" ")
        );
    }
    for (i, pts) in &trends {
        let c = hex(opts.color(*i));
        let path: Vec<String> = pts
            .iter()
            .filter(|p| !log || p.1 > 0.0)
            .map(|p| format!("{:.1},{:.1}", f.px(p.0), f.py(ty(p.1))))
            .collect();
        let _ = writeln!(
            out,
            r#"<polyline fill="none" stroke="{c}" stroke-width="2.4" stroke-linejoin="round" points="{}"/>"#,
            path.join(" ")
        );
    }
    for &(i, s) in series.iter().filter(|_| raw_shown) {
        // Under trend lines, a pale tint of the series colour.
        let c = if trends.is_empty() {
            hex(opts.color(i))
        } else {
            tint(&hex(opts.color(i)), 0.6)
        };
        let pts: Vec<(f64, f64)> = s
            .points
            .iter()
            .filter(|p| !log || p.1 > 0.0)
            .map(|p| (f.px(p.0), f.py(ty(p.1))))
            .collect();
        if opts.kind == ChartKind::Scatter {
            for (x, y) in pts {
                let _ = writeln!(
                    out,
                    r#"<circle cx="{x:.1}" cy="{y:.1}" r="2.5" fill="{c}"/>"#
                );
            }
        } else {
            let path: Vec<String> = pts.iter().map(|(x, y)| format!("{x:.1},{y:.1}")).collect();
            let _ = writeln!(
                out,
                r#"<polyline fill="none" stroke="{c}" stroke-width="1.8" stroke-linejoin="round" points="{}"/>"#,
                path.join(" ")
            );
        }
    }
    let _ = writeln!(out, "</g>");
    legend(out, series, opts, &f);
}

fn legend(out: &mut String, series: &[(usize, &Series)], opts: &ChartOpts, f: &Frame) {
    if opts.legend == LegendPos::None {
        return;
    }
    let labels: Vec<(String, String)> = series
        .iter()
        .map(|&(i, s)| {
            (
                hex(opts.color(i)),
                format!(
                    "{} {}",
                    s.name,
                    s.points
                        .last()
                        .map(|p| s.unit.format(p.1))
                        .unwrap_or_default()
                ),
            )
        })
        .collect();
    let lw = labels
        .iter()
        .map(|l| l.1.chars().count())
        .max()
        .unwrap_or(0) as f64
        * 7.0
        + 30.0;
    let lh = labels.len() as f64 * 18.0 + 8.0;
    let left_side = matches!(opts.legend, LegendPos::TopLeft | LegendPos::BottomLeft);
    let bottom = matches!(
        opts.legend,
        LegendPos::BottomLeft | LegendPos::BottomRight | LegendPos::Bottom
    );
    let x = if left_side {
        f.left + 10.0
    } else {
        f.left + f.w - lw - 10.0
    };
    let y = if bottom {
        f.top + f.h - lh - 10.0
    } else {
        f.top + 10.0
    };
    let _ = writeln!(
        out,
        r##"<rect x="{x:.1}" y="{y:.1}" width="{lw:.1}" height="{lh:.1}" fill="white" fill-opacity="0.85" stroke="#dee2e6"/>"##
    );
    for (k, (c, label)) in labels.iter().enumerate() {
        let ly = y + 18.0 * k as f64 + 17.0;
        let _ = writeln!(
            out,
            r#"<rect x="{:.1}" y="{:.1}" width="12" height="4" fill="{c}"/>"#,
            x + 8.0,
            ly - 6.0
        );
        let _ = writeln!(
            out,
            r##"<text x="{:.1}" y="{ly:.1}" fill="#333">{}</text>"##,
            x + 26.0,
            esc(label)
        );
    }
}

fn bars(out: &mut String, series: &[(usize, &Series)], opts: &ChartOpts, w: f64, h: f64) {
    let latest: Vec<(usize, &Series, f64)> = series
        .iter()
        .filter_map(|&(i, s)| s.points.last().map(|p| (i, s, p.1)))
        .collect();
    let max = latest
        .iter()
        .map(|l| l.2.max(0.0))
        .fold(0.0, f64::max)
        .max(f64::MIN_POSITIVE);
    let name_w = latest
        .iter()
        .map(|l| l.1.name.chars().count())
        .max()
        .unwrap_or(0) as f64
        * 7.5
        + 20.0;
    let (left, top, pw) = (name_w, 50.0, w - name_w - 120.0);
    let row = ((h - top - 20.0) / latest.len() as f64).min(40.0);
    for (k, &(i, s, v)) in latest.iter().enumerate() {
        let y = top + row * k as f64;
        let bw = v.max(0.0) / max * pw;
        let _ = writeln!(
            out,
            r##"<text x="{:.1}" y="{:.1}" text-anchor="end" fill="#333">{}</text>"##,
            left - 8.0,
            y + row / 2.0 + 4.0,
            esc(&s.name)
        );
        let _ = writeln!(
            out,
            r#"<rect x="{left:.1}" y="{:.1}" width="{bw:.1}" height="{:.1}" fill="{}"/>"#,
            y + row * 0.15,
            row * 0.7,
            hex(opts.color(i))
        );
        let _ = writeln!(
            out,
            r##"<text x="{:.1}" y="{:.1}" fill="#333">{}</text>"##,
            left + bw + 6.0,
            y + row / 2.0 + 4.0,
            esc(&s.unit.format(v))
        );
    }
}

fn histogram(out: &mut String, (i, s): (usize, &Series), opts: &ChartOpts, w: f64, h: f64) {
    let values: Vec<f64> = s.points.iter().map(|p| p.1).collect();
    let (lo, hi) = values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    let bins = if hi > lo { opts.bins.max(1) } else { 1 };
    let width = (hi - lo) / bins as f64;
    let mut counts = vec![0usize; bins];
    for v in &values {
        let b = if width > 0.0 {
            (((v - lo) / width) as usize).min(bins - 1)
        } else {
            0
        };
        counts[b] += 1;
    }
    let max = *counts.iter().max().unwrap_or(&1) as f64;
    let f = Frame {
        left: 60.0,
        top: 40.0,
        w: w - 80.0,
        h: h - 90.0,
        x0: lo,
        x1: if hi > lo { hi } else { lo + 1.0 },
        y0: 0.0,
        y1: max * 1.05,
    };
    let bw = f.w / bins as f64;
    for (b, &c) in counts.iter().enumerate() {
        let x = f.left + bw * b as f64;
        let y = f.py(c as f64);
        let _ = writeln!(
            out,
            r#"<rect x="{:.1}" y="{y:.1}" width="{:.1}" height="{:.1}" fill="{}"/>"#,
            x + 1.0,
            (bw - 2.0).max(0.5),
            f.top + f.h - y,
            hex(opts.color(i))
        );
    }
    for t in nice_ticks(0.0, f.y1, 5) {
        let _ = writeln!(
            out,
            r##"<text x="{:.1}" y="{:.1}" text-anchor="end" fill="#555">{t}</text>"##,
            f.left - 6.0,
            f.py(t) + 4.0
        );
    }
    let xticks = spaced(f.x0, f.x1, 6);
    for (k, &t) in xticks.iter().enumerate() {
        let anchor = if k == 0 {
            "start"
        } else if k == xticks.len() - 1 {
            "end"
        } else {
            "middle"
        };
        let _ = writeln!(
            out,
            r##"<text x="{:.1}" y="{:.1}" text-anchor="{anchor}" fill="#555">{}</text>"##,
            f.px(t),
            f.top + f.h + 18.0,
            esc(&s.unit.format(t))
        );
    }
    let _ = writeln!(
        out,
        r##"<text x="{:.1}" y="{:.1}" text-anchor="middle" fill="#333">distribution of {} (n={})</text>"##,
        w / 2.0,
        h - 12.0,
        esc(&s.name),
        values.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_are_nice() {
        assert_eq!(
            nice_ticks(0.0, 10.0, 5),
            vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0]
        );
        assert_eq!(nice_ticks(57.3, 60.1, 4), vec![58.0, 59.0, 60.0]);
    }

    #[test]
    fn hex_colours() {
        assert_eq!(hex(Color::Rgb(255, 136, 0)), "#ff8800");
        assert_eq!(hex(Color::Indexed(16)), "#000000");
        assert_eq!(hex(Color::Indexed(1)), "#d63333");
    }
}
