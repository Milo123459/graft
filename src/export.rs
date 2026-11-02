//! Export the processed data or chart to files.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::data::{Dataset, XKind};
use crate::render::{ChartOpts, svg};

pub const SVG_SIZE: (u32, u32) = (1000, 500);

pub fn export(path: &Path, ds: &Dataset, opts: &ChartOpts) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let write = |data: &[u8]| {
        std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))
    };
    match ext.as_str() {
        "svg" => write(svg::render(ds, opts, SVG_SIZE.0, SVG_SIZE.1).as_bytes()),
        "png" => write(&png(ds, opts)?),
        "csv" => write(to_csv(ds).as_bytes()),
        "json" => write(serde_json::to_string_pretty(&to_json(ds))?.as_bytes()),
        other => bail!("don't know how to export '.{other}' (use .svg, .png, .csv or .json)"),
    }
}

#[cfg(feature = "png")]
fn png(ds: &Dataset, opts: &ChartOpts) -> Result<Vec<u8>> {
    let svg = svg::render(ds, opts, SVG_SIZE.0, SVG_SIZE.1);
    let mut options = resvg::usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    let tree = resvg::usvg::Tree::from_str(&svg, &options)?;
    let size = tree.size().to_int_size();
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(size.width(), size.height()).context("allocating image")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap.encode_png()?)
}

#[cfg(not(feature = "png"))]
fn png(_: &Dataset, _: &ChartOpts) -> Result<Vec<u8>> {
    bail!("PNG export needs graft built with `--features png` (SVG export is always available)")
}

fn fmt_x(ds: &Dataset, x: f64) -> String {
    match ds.x_kind {
        XKind::Time => crate::time::format_time_full(x),
        _ => x.to_string(),
    }
}

/// Wide CSV: one row per x, one column per series (values in base units).
pub fn to_csv(ds: &Dataset) -> String {
    let mut rows: BTreeMap<i64, (f64, Vec<Option<f64>>)> = BTreeMap::new();
    let n = ds.series.len();
    for (i, s) in ds.series.iter().enumerate() {
        for &(x, y) in &s.points {
            // Microsecond resolution is plenty to align rows.
            let key = (x * 1e6).round() as i64;
            rows.entry(key).or_insert_with(|| (x, vec![None; n])).1[i] = Some(y);
        }
    }
    let quote = |s: &str| {
        if s.contains([',', '"', '\n']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };
    let mut out = String::new();
    let xname = if ds.x_kind == XKind::Time {
        "time"
    } else {
        "x"
    };
    let _ = writeln!(
        out,
        "{xname},{}",
        ds.series
            .iter()
            .map(|s| quote(&s.name))
            .collect::<Vec<_>>()
            .join(",")
    );
    for (x, vals) in rows.into_values() {
        let cells: Vec<String> = vals
            .iter()
            .map(|v| v.map(|v| v.to_string()).unwrap_or_default())
            .collect();
        let _ = writeln!(out, "{},{}", quote(&fmt_x(ds, x)), cells.join(","));
    }
    out
}

pub fn to_json(ds: &Dataset) -> serde_json::Value {
    serde_json::json!({
        "x": match ds.x_kind { XKind::Time => "time", XKind::Index => "index", XKind::Number => "number" },
        "series": ds.series.iter().map(|s| serde_json::json!({
            "name": s.name,
            "unit": s.unit.label(),
            "points": s.points.iter().map(|&(x, y)| serde_json::json!([fmt_x(ds, x), y])).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Series;
    use crate::units::Unit;

    #[test]
    fn csv_aligns_rows() {
        let ds = Dataset {
            x_kind: XKind::Index,
            series: vec![
                Series {
                    name: "a".into(),
                    unit: Unit::default(),
                    points: vec![(0.0, 1.0), (1.0, 2.0)],
                },
                Series {
                    name: "b,c".into(),
                    unit: Unit::default(),
                    points: vec![(1.0, 5.0)],
                },
            ],
        };
        assert_eq!(to_csv(&ds), "x,a,\"b,c\"\n0,1,\n1,2,5\n");
    }
}
