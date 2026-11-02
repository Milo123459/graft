//! `--explain`: show how each input line is parsed, to help build a format.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::sync::mpsc::Receiver;

use anyhow::Result;

use crate::session::Session;
use crate::source::Event;

pub fn run(session: &mut Session, rx: Receiver<Event>, limit: usize, color: bool) -> Result<()> {
    let paint = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let mut out = std::io::stdout().lock();
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    let mut shown = 0;
    for ev in rx {
        let (line, meta) = match ev {
            Event::Line(l, m) => (l, m),
            Event::Error(e) => {
                writeln!(out, "{}", paint("31", &format!("error: {e}")))?;
                continue;
            }
            Event::SampleDone => continue,
            Event::Eof => break,
        };
        let before: BTreeMap<String, usize> = session
            .store
            .series
            .iter()
            .map(|(k, s)| (k.clone(), s.points.len()))
            .collect();
        let result = session.feed(&line, &meta);
        if let Err(skip) = &result {
            *reasons.entry(skip.to_string()).or_default() += 1;
        }
        if shown >= limit {
            continue;
        }
        shown += 1;
        let n = session.lines;
        let text: String = line.chars().take(100).collect();
        match result {
            Ok(x) => {
                let ds = session.store.dataset();
                let mut vals = String::new();
                for s in &ds.series {
                    let grew = s.points.len() > before.get(&s.name).copied().unwrap_or(0);
                    if let Some(&(_, y)) = s.points.iter().rev().find(|p| p.0 == x).filter(|_| grew)
                    {
                        let _ = write!(vals, "{}={}  ", paint("1", &s.name), s.unit.format(y));
                    }
                }
                writeln!(
                    out,
                    "{n:>5} {} {} {} {vals}",
                    paint("32", "✓"),
                    paint("36", &ds.fmt_x_full(x)),
                    paint("2", "│")
                )?;
            }
            Err(skip) => {
                writeln!(
                    out,
                    "{n:>5} {} {} {} {}",
                    paint("31", "✗"),
                    paint("33", &skip.to_string()),
                    paint("2", "│"),
                    paint("2", &text)
                )?;
            }
        }
    }

    let ds = session.store.dataset();
    writeln!(out)?;
    writeln!(
        out,
        "{} {} of {} lines produced data",
        paint("1", "summary:"),
        session.matched,
        session.lines
    )?;
    for (reason, count) in &reasons {
        writeln!(out, "  {count} skipped: {reason}")?;
    }
    if let Some((a, b)) = ds.x_range() {
        let kind = match ds.x_kind {
            crate::data::XKind::Time => "time",
            crate::data::XKind::Index => "line number",
            crate::data::XKind::Number => "number",
        };
        writeln!(
            out,
            "  x axis: {kind}, {} → {}",
            ds.fmt_x_full(a),
            ds.fmt_x_full(b)
        )?;
    }
    for s in &ds.series {
        let (lo, hi) = s
            .points
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
                (lo.min(p.1), hi.max(p.1))
            });
        let unit = if s.unit.is_none() {
            String::new()
        } else {
            format!(", {}", s.unit)
        };
        writeln!(
            out,
            "  series {}: {} points{unit}, {} … {}",
            paint("1", &s.name),
            s.points.len(),
            s.unit.format(lo),
            s.unit.format(hi)
        )?;
    }
    if session.matched == 0 {
        writeln!(
            out,
            "\nhint: nothing matched. Try --regex with named groups, e.g. -r '(?P<time>\\S+ \\S+) .*?(?P<value>[\\d.]+)'"
        )?;
    }
    Ok(())
}
