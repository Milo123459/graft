//! Format suggestions for a sample of input, and live previews of a format.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::cli::Cli;
use crate::data::{LineMeta, SampleInfo, Store, XKind};
use crate::parse;
use crate::script::ScriptHost;
use crate::time::TimeParser;
use crate::units::{Unit, lookup_unit, parse_value};

/// How one sample line parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct LineResult {
    pub raw: String,
    /// `Ok(summary)` like `04:00 node1=59.9 GB`, or `Err(reason)`.
    pub result: Result<String, String>,
}

/// The outcome of parsing a sample with one configuration.
#[derive(Debug, Clone, Default)]
pub struct Preview {
    pub lines: Vec<LineResult>,
    /// Series name, points, unit.
    pub series: Vec<(String, usize, Unit)>,
    pub matched: usize,
    pub total: usize,
    pub x_kind: Option<XKind>,
    /// The configuration itself is invalid (bad regex, ...).
    pub error: Option<String>,
}

/// Parse `sample` with `cli`'s parser settings. `metas` (when known: lines kept by
/// a running viewer) say when each line arrived; others are "now".
pub fn preview(
    cli: &Cli,
    sample: &[String],
    metas: &[LineMeta],
    from_command: bool,
    script: &Rc<ScriptHost>,
    max_lines: usize,
) -> Preview {
    let mut p = Preview {
        total: sample.len(),
        ..Preview::default()
    };
    let mut parser = match parse::build(cli, script.clone()) {
        Ok(parser) => parser,
        Err(e) => {
            p.error = Some(format!("{e:#}"));
            return p;
        }
    };
    let mut store = Store::new(cli.x, None, None);
    let now = crate::time::now_secs();
    let meta = LineMeta {
        arrival: now,
        sample: from_command.then_some(SampleInfo {
            index: 0,
            time: now,
        }),
    };
    for (i, line) in sample.iter().enumerate() {
        let meta = metas.get(i).copied().unwrap_or(meta);
        let before: HashMap<String, usize> = store
            .series
            .iter()
            .map(|(k, s)| (k.clone(), s.points.len()))
            .collect();
        let result = parser
            .parse(line)
            .and_then(|parsed| store.ingest(parsed, &meta));
        let summary = result.map(|x| {
            let kind = store.x_kind.unwrap_or_default();
            let mut text = match kind {
                XKind::Time => crate::time::format_time_full(x),
                XKind::Index => format!("#{}", x as i64),
                XKind::Number => crate::units::fmt_compact(x),
            };
            for (name, s) in &store.series {
                if s.points.len() > before.get(name).copied().unwrap_or(0)
                    && let Some(&(_, y)) = s.points.iter().rev().find(|pt| pt.0 == x)
                {
                    text.push_str(&format!("  {name}={}", s.unit.format(y)));
                }
            }
            text
        });
        if summary.is_ok() {
            p.matched += 1;
        }
        if p.lines.len() < max_lines {
            p.lines.push(LineResult {
                raw: line.clone(),
                result: summary.map_err(|e| e.to_string()),
            });
        }
    }
    p.series = store
        .series
        .values()
        .map(|s| (s.name.clone(), s.points.len(), s.unit.clone()))
        .collect();
    p.x_kind = store.x_kind;
    p
}

/// Which parser a suggestion uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Template,
    Kv,
    Columns,
    Csv,
    Json,
}

#[derive(Debug, Clone)]
pub struct Suggestion {
    pub kind: Kind,
    pub label: String,
    /// The configuration with this parser applied.
    pub cli: Cli,
    pub matched: usize,
    pub total: usize,
    pub series: Vec<String>,
    pub has_time: bool,
}

impl Suggestion {
    pub fn summary(&self) -> String {
        let names: Vec<&str> = self.series.iter().take(4).map(String::as_str).collect();
        let more = if self.series.len() > 4 {
            format!(" +{}", self.series.len() - 4)
        } else {
            String::new()
        };
        format!(
            "{}/{} lines · {}{more}{}",
            self.matched,
            self.total,
            names.join(", "),
            if self.has_time { " · time" } else { "" }
        )
    }
}

/// Ranked format suggestions for a sample.
pub fn suggest(
    sample: &[String],
    base: &Cli,
    from_command: bool,
    script: &Rc<ScriptHost>,
) -> Vec<Suggestion> {
    let lines: Vec<String> = sample
        .iter()
        .filter(|l| !l.trim().is_empty())
        .take(300)
        .cloned()
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let mut candidates: Vec<(Kind, String, Cli)> = Vec::new();
    let with = |f: &dyn Fn(&mut Cli)| {
        let mut c = base.clone();
        c.clear_parser();
        c.group_by = None;
        c.names.clear();
        f(&mut c);
        c
    };
    if let Some(t) = template_from_lines(&lines) {
        candidates.push((
            Kind::Template,
            format!("template  {t}"),
            with(&|c| c.format = Some(t.clone())),
        ));
    }
    candidates.push((Kind::Kv, "key: value pairs".into(), with(&|c| c.kv = true)));
    let first = lines[0].trim_start();
    if first.starts_with('{') || first.starts_with('[') {
        candidates.push((Kind::Json, "JSON".into(), with(&|c| c.json = true)));
    }
    for (delim, name) in [(',', "CSV"), ('\t', "TSV"), (';', "CSV (;)")] {
        let counts: Vec<usize> = lines
            .iter()
            .take(20)
            .map(|l| l.matches(delim).count())
            .collect();
        if counts[0] > 0 && counts.iter().all(|&c| c == counts[0]) {
            candidates.push((
                Kind::Csv,
                name.into(),
                with(&|c| {
                    c.csv = true;
                    c.delimiter = (delim != ',').then_some(delim);
                }),
            ));
        }
    }
    candidates.push((
        Kind::Columns,
        "numeric columns".into(),
        with(&|c| c.columns = Some("all".into())),
    ));

    let mut out: Vec<Suggestion> = Vec::new();
    let mut seen: HashSet<(Kind, Vec<String>)> = HashSet::new();
    for (kind, label, cli) in candidates {
        let p = preview(&cli, &lines, &[], from_command, script, 0);
        if p.error.is_some() || p.series.is_empty() || p.series.len() > 200 {
            continue;
        }
        let series: Vec<String> = p.series.iter().map(|s| s.0.clone()).collect();
        if !seen.insert((kind, series.clone())) {
            continue;
        }
        out.push(Suggestion {
            kind,
            label,
            cli,
            matched: p.matched,
            total: lines.len(),
            series,
            has_time: p.x_kind == Some(XKind::Time),
        });
    }
    // Best coverage first; on ties keep the order above (explicit template first).
    out.sort_by(|a, b| {
        let score =
            |s: &Suggestion| (s.matched * 100 / s.total.max(1)) + if s.has_time { 5 } else { 0 };
        score(b).cmp(&score(a))
    });
    out
}

/// One whitespace-separated token's role in a line.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Number,
    /// A unit following a number (`GB` in `59.9 GB`); absorbed by the placeholder.
    Unit,
    /// `name=12` / `name:12` in one token.
    Pair(String, char),
    Text(String),
}

fn classify(tokens: &[&str], binary: bool) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::with_capacity(tokens.len());
    for t in tokens {
        let prev_number = matches!(out.last(), Some(Tok::Number | Tok::Pair(..)));
        let tok = if parse_value(t, binary)
            .is_some_and(|(_, u)| !matches!(&u.dim, crate::units::Dim::Other(s) if s.len() > 3))
        {
            Tok::Number
        } else if prev_number && lookup_unit(t, binary).is_some() {
            Tok::Unit
        } else if let Some((name, sep, rest)) = t
            .find(['=', ':'])
            .map(|i| (&t[..i], t.as_bytes()[i] as char, &t[i + 1..]))
            && !name.is_empty()
            && !rest.is_empty()
            && parse_value(rest, binary).is_some()
        {
            Tok::Pair(name.to_string(), sep)
        } else {
            Tok::Text(t.to_string())
        };
        out.push(tok);
    }
    out
}

fn clean_name(s: &str) -> String {
    s.trim_end_matches([':', '='])
        .chars()
        .filter(|c| !matches!(c, '{' | '}' | ':'))
        .collect()
}

/// Generalise sample lines into a format template, e.g.
/// `2026-11-01 04:00  node1: 59.9 GB` → `{time} node1: {node1}`.
pub fn template_from_lines(lines: &[String]) -> Option<String> {
    let tp = TimeParser::default();
    let shapes: Vec<Shape> = lines
        .iter()
        .map(|l| {
            let (time, rest) = match tp.detect(l) {
                Some((_, len)) => (true, &l[len..]),
                None => (false, l.as_str()),
            };
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            Shape {
                time,
                toks: classify(&tokens, false),
                tokens: tokens.iter().map(|s| s.to_string()).collect(),
            }
        })
        .collect();
    let data: Vec<&Shape> = shapes
        .iter()
        .filter(|s| {
            s.toks
                .iter()
                .any(|t| matches!(t, Tok::Number | Tok::Pair(..)))
        })
        .collect();
    // The most common token count among data lines.
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for s in &data {
        *counts.entry(s.tokens.len()).or_default() += 1;
    }
    let (&n, _) = counts.iter().max_by_key(|(n, c)| (**c, **n))?;
    let rows: Vec<&Shape> = data.into_iter().filter(|s| s.tokens.len() == n).collect();
    let header: Option<&Shape> = shapes
        .iter()
        .find(|s| s.tokens.len() == n && s.toks.iter().all(|t| matches!(t, Tok::Text(_))) && n > 1);

    let numeric_cols: usize = (0..n)
        .filter(|&i| rows.iter().all(|r| r.toks[i] == Tok::Number))
        .count();
    let mut parts: Vec<String> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut unique = |base: String| {
        let mut name = if base.is_empty() {
            "value".to_string()
        } else {
            base
        };
        let stem = name.clone();
        let mut k = 2;
        while !used.insert(name.clone()) {
            name = format!("{stem}_{k}");
            k += 1;
        }
        name
    };
    let mut has_key = false;
    // Indices of bare `{name}` number placeholders, and the count of `name={name}` pairs.
    let mut numbers: Vec<usize> = Vec::new();
    let mut pairs = 0;
    for i in 0..n {
        let col: Vec<&Tok> = rows.iter().map(|r| &r.toks[i]).collect();
        let first = col[0];
        let all_same = rows.iter().all(|r| r.tokens[i] == rows[0].tokens[i]);
        match first {
            Tok::Unit if col.iter().all(|t| **t == Tok::Unit) => continue,
            Tok::Number if col.iter().all(|t| **t == Tok::Number) => {
                let name = match (&header, i.checked_sub(1).map(|p| &rows[0].toks[p])) {
                    (Some(h), _) => clean_name(&h.tokens[i]),
                    (None, Some(Tok::Text(prev)))
                        if (prev.ends_with(':') || prev.ends_with('='))
                            && all_same_at(&rows, i - 1) =>
                    {
                        clean_name(prev)
                    }
                    _ => format!("v{}", parts.len()),
                };
                numbers.push(parts.len());
                parts.push(format!("{{{}}}", unique(name)));
            }
            Tok::Pair(name, sep) if all_same_pair(&rows, i) => {
                let n = unique(clean_name(name));
                pairs += 1;
                parts.push(format!("{name}{sep}{{{n}}}"));
            }
            _ if all_same && rows.len() > 1 || rows.len() == 1 => {
                parts.push(rows[0].tokens[i].replace('{', "{{").replace('}', "}}"))
            }
            _ if i == 0 && numeric_cols > 0 && !has_key => {
                has_key = true;
                let sep = rows[0].tokens[0].chars().last().filter(|c| {
                    matches!(c, ':' | '=') && rows.iter().all(|r| r.tokens[0].ends_with(*c))
                });
                parts.push(match sep {
                    Some(c) => format!("{{key}}{c}"),
                    None => "{key}".into(),
                });
            }
            _ => parts.push("{_:word}".into()),
        }
    }
    // With a key and a single value, name it `value` so series are named by the key.
    if has_key && numbers.len() == 1 && pairs == 0 {
        parts[numbers[0]] = "{value}".into();
    }
    if numbers.is_empty() && pairs == 0 {
        return None;
    }
    let mut out = String::new();
    if rows.iter().all(|r| r.time) {
        out.push_str("{time} ");
    }
    out.push_str(&parts.join(" "));
    Some(out)
}

fn all_same_at(rows: &[&Shape], i: usize) -> bool {
    rows.iter().all(|r| r.tokens[i] == rows[0].tokens[i])
}

/// Every row has a `name=value` pair at `i` with the same name.
fn all_same_pair(rows: &[&Shape], i: usize) -> bool {
    let name = |r: &Shape| match &r.toks[i] {
        Tok::Pair(n, _) => Some(n.clone()),
        _ => None,
    };
    let first = name(rows[0]);
    first.is_some() && rows.iter().all(|r| name(r) == first)
}

/// A tokenised sample line.
struct Shape {
    time: bool,
    tokens: Vec<String>,
    toks: Vec<Tok>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn templates_from_samples() {
        let log = lines(
            "2026-11-01 04:00  node1: 59.9 GB  node2: 422.6 GB\n2026-11-01 05:00  node1: 59.9 GB  node2: 422 GB",
        );
        assert_eq!(
            template_from_lines(&log).unwrap(),
            "{time} node1: {node1} node2: {node2}"
        );
        let meminfo =
            lines("MemTotal:       16303204 kB\nMemFree:         1234 kB\nCached:   99 kB");
        assert_eq!(template_from_lines(&meminfo).unwrap(), "{key}: {value}");
        let vm = lines("r b free\n1 0 100\n2 0 80");
        assert_eq!(template_from_lines(&vm).unwrap(), "{r} {b} {free}");
        let kv = lines("load=0.5 host=a temp=40C\nload=0.7 host=b temp=41C");
        assert_eq!(
            template_from_lines(&kv).unwrap(),
            "load={load} {_:word} temp={temp}"
        );
        let df = lines("/dev/sda1 100 40 60 40% /\n/dev/sdb1 200 50 150 25% /data");
        assert_eq!(
            template_from_lines(&df).unwrap(),
            "{key} {v1} {v2} {v3} {v4} {_:word}"
        );
        assert!(template_from_lines(&lines("no numbers here")).is_none());
    }

    #[test]
    fn suggestions_are_ranked_and_valid() {
        let script = Rc::new(ScriptHost::new(None, false).unwrap());
        let log: Vec<String> = std::fs::read_to_string("examples/reclaimable.log")
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        let s = suggest(&log, &Cli::empty(), false, &script);
        assert_eq!(s[0].kind, Kind::Template);
        assert_eq!(s[0].matched, 29);
        assert_eq!(s[0].series, ["node1", "node2"]);
        assert!(s[0].has_time);
        assert!(s.iter().any(|x| x.kind == Kind::Kv));
        let csv = lines("time,temp\n2026-11-01 00:00,40\n2026-11-01 01:00,41");
        let s = suggest(&csv, &Cli::empty(), false, &script);
        assert!(
            s.iter()
                .any(|x| x.kind == Kind::Csv && x.series == ["temp"] && x.has_time),
            "{s:#?}"
        );
    }

    #[test]
    fn preview_reports_lines() {
        let script = Rc::new(ScriptHost::new(None, false).unwrap());
        let mut cli = Cli::empty();
        cli.format = Some("{time} node1: {node1}".into());
        let p = preview(
            &cli,
            &lines("2026-11-01 04:00 node1: 5 GB\ngarbage"),
            &[],
            false,
            &script,
            10,
        );
        assert_eq!(p.matched, 1);
        assert_eq!(p.lines[0].result, Ok("2026-11-01 04:00  node1=5 GB".into()));
        assert!(p.lines[1].result.is_err());
        cli.format = Some("{oops".into());
        assert!(preview(&cli, &[], &[], false, &script, 10).error.is_some());
    }
}
