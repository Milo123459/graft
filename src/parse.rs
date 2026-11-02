//! Line parsers: turn raw text into named numeric values plus an optional x value.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::LazyLock;

use anyhow::{Result, bail};
use regex::Regex;
use serde_json::Value;

use crate::cli::Cli;
use crate::script::{ScriptHost, ScriptVal};
use crate::time::TimeParser;
use crate::units::{Unit, lookup_unit, parse_value};

/// The x coordinate a parser found on a line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum XVal {
    Time(f64),
    Num(f64),
}

/// One value for one series.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub series: String,
    pub value: f64,
    pub unit: Unit,
}

/// Everything extracted from a single line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub x: Option<XVal>,
    pub points: Vec<Point>,
}

/// Why a line produced no data. Shown by `--explain`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Skip {
    #[error("pattern did not match")]
    NoMatch,
    #[error("no numeric values found")]
    NoValues,
    #[error("header row")]
    Header,
    #[error("incomplete, waiting for more input")]
    Incomplete,
    #[error("{0}")]
    Invalid(String),
}

pub trait LineParser {
    fn parse(&mut self, line: &str) -> Result<Parsed, Skip>;
}

/// Build the parser selected on the command line.
pub fn build(cli: &Cli, script: Rc<ScriptHost>) -> Result<Box<dyn LineParser>> {
    let time = TimeParser::new(cli.time_format.clone());
    let binary = cli.binary_units;
    Ok(if let Some(src) = &cli.format {
        let t = crate::template::compile(src)?;
        let time = if cli.time_format.is_some() {
            time
        } else {
            TimeParser::new(t.time_format)
        };
        Box::new(RegexParser::new(
            &t.regex,
            cli.group_by.as_deref(),
            time,
            binary,
            t.rename,
        )?)
    } else if let Some(re) = &cli.regex {
        Box::new(RegexParser::new(
            re,
            cli.group_by.as_deref(),
            time,
            binary,
            HashMap::new(),
        )?)
    } else if cli.csv {
        Box::new(CsvParser::new(cli, time))
    } else if cli.json {
        Box::new(JsonParser {
            buf: String::new(),
            time_field: cli.time_field.clone(),
            time,
            binary,
        })
    } else if let Some(cols) = &cli.columns {
        let select = parse_columns(cols)?;
        Box::new(ColumnsParser::new(
            select,
            cli.names.clone(),
            cli.delimiter,
            time,
            binary,
        ))
    } else if cli.parse_fn {
        Box::new(ScriptParser {
            host: script,
            time,
            binary,
        })
    } else {
        if cli.group_by.is_some() {
            bail!("--group-by needs --regex or --format");
        }
        let kv = KvParser {
            time: time.clone(),
            binary,
        };
        let cols = ColumnsParser::new(None, cli.names.clone(), cli.delimiter, time, binary);
        if cli.kv {
            // (--auto and "nothing chosen" both use the auto parser below.)
            Box::new(kv)
        } else {
            Box::new(AutoParser {
                kv,
                cols,
                mode: AutoMode::Undecided,
            })
        }
    })
}

/// `all` → every numeric column; `1,3` → those columns (1-based).
pub fn parse_columns(spec: &str) -> Result<Option<Vec<usize>>> {
    if spec.trim().eq_ignore_ascii_case("all") || spec.trim().is_empty() {
        return Ok(None);
    }
    let cols = spec
        .split(',')
        .map(|c| {
            c.trim().parse::<usize>().map_err(|_| {
                anyhow::anyhow!("--columns: '{c}' is not a column number (use e.g. 1,3 or all)")
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if cols.contains(&0) {
        bail!("--columns are 1-based");
    }
    Ok(Some(cols))
}

fn time_x(time: &TimeParser, line: &str) -> (Option<XVal>, usize) {
    match time.detect(line) {
        Some((ts, len)) => (Some(XVal::Time(ts)), len),
        None => (None, 0),
    }
}

fn finish(x: Option<XVal>, points: Vec<Point>) -> Result<Parsed, Skip> {
    if points.is_empty() {
        Err(Skip::NoValues)
    } else {
        Ok(Parsed { x, points })
    }
}

/// Parse a value, tolerating thousands separators (`1,234`).
fn value(s: &str, binary: bool) -> Option<(f64, Unit)> {
    parse_value(s, binary).or_else(|| {
        s.contains(',')
            .then(|| parse_value(&s.replace(',', ""), binary))
            .flatten()
    })
}

// ───────────────────────────── key/value ─────────────────────────────

/// `key: number` / `key=number`. Deliberately capture-free and ASCII-only so the
/// regex crate can use its fast DFA; the match is split by hand.
static KV_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[A-Za-z_][A-Za-z0-9_.\-/()\[\]]*"?[ \t]*[:=][ \t]*[-+]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][-+]?[0-9]+)?"#)
        .expect("valid kv regex")
});

/// `key: value [unit]` and `key=value[unit]` pairs, anywhere in the line.
pub struct KvParser {
    time: TimeParser,
    binary: bool,
}

impl KvParser {
    /// After a number, decide whether the following token is its unit.
    fn unit_after(rest: &str, binary: bool) -> &str {
        let trimmed = rest.trim_start();
        let attached = trimmed.len() == rest.len();
        let end = trimmed
            .find(|c: char| !(c.is_alphabetic() || matches!(c, '%' | '/' | '°' | 'µ')))
            .unwrap_or(trimmed.len());
        let tok = &trimmed[..end];
        if tok.is_empty() || trimmed[end..].trim_start().starts_with([':', '=']) {
            return "";
        }
        let known = lookup_unit(tok.strip_suffix("/s").unwrap_or(tok), binary).is_some();
        if known || attached { tok } else { "" }
    }
}

impl LineParser for KvParser {
    fn parse(&mut self, line: &str) -> Result<Parsed, Skip> {
        let (x, skip) = time_x(&self.time, line);
        let body = &line[skip..];
        let points = KV_RE
            .find_iter(body)
            .filter_map(|m| {
                // The key can't contain ':' or '=', so the first one is the separator.
                let text = m.as_str();
                let sep = text.find([':', '='])?;
                let key = text[..sep].trim_end().trim_end_matches('"');
                let n: f64 = text[sep + 1..].trim_start().parse().ok()?;
                let unit = Self::unit_after(&body[m.end()..], self.binary);
                let (value, unit) = crate::units::apply_unit(n, unit, self.binary)?;
                Some(Point {
                    series: key.to_string(),
                    value,
                    unit,
                })
            })
            .collect();
        finish(x, points)
    }
}

// ───────────────────────────── columns ─────────────────────────────

/// Whitespace (or delimiter) separated columns, with header detection.
pub struct ColumnsParser {
    select: Option<Vec<usize>>,
    names: Vec<String>,
    header: Option<Vec<String>>,
    delimiter: Option<char>,
    time: TimeParser,
    binary: bool,
}

impl ColumnsParser {
    fn new(
        select: Option<Vec<usize>>,
        names: Vec<String>,
        delimiter: Option<char>,
        time: TimeParser,
        binary: bool,
    ) -> Self {
        Self {
            select,
            names,
            header: None,
            delimiter,
            time,
            binary,
        }
    }

    fn split<'a>(&self, s: &'a str) -> Vec<&'a str> {
        match self.delimiter {
            Some(d) => s.split(d).map(str::trim).collect(),
            None => s.split_whitespace().collect(),
        }
    }

    /// Name for the column at token position `pos` (0-based); `nth` is its index among
    /// the selected/numeric columns.
    fn name(&self, pos: usize, nth: usize, ntokens: usize) -> String {
        if let Some(n) = self.names.get(nth) {
            return n.clone();
        }
        match &self.header {
            Some(h) if h.len() == ntokens => h[pos].clone(),
            _ => format!("col{}", pos + 1),
        }
    }
}

impl LineParser for ColumnsParser {
    fn parse(&mut self, line: &str) -> Result<Parsed, Skip> {
        let (x, skip) = time_x(&self.time, line);
        let tokens = self.split(&line[skip..]);
        if tokens.is_empty() {
            return Err(Skip::NoValues);
        }
        let numeric = |t: &str| value(t, self.binary).is_some();
        if tokens.len() > 1 && !tokens.iter().any(|t| numeric(t)) {
            self.header = Some(tokens.iter().map(|t| t.to_string()).collect());
            return Err(Skip::Header);
        }
        let mut points = Vec::new();
        match &self.select {
            Some(cols) => {
                for (nth, &col) in cols.iter().enumerate() {
                    let Some(tok) = tokens.get(col - 1) else {
                        continue;
                    };
                    if let Some((v, unit)) = value(tok, self.binary) {
                        points.push(Point {
                            series: self.name(col - 1, nth, tokens.len()),
                            value: v,
                            unit,
                        });
                    }
                }
            }
            None => {
                let mut i = 0;
                while i < tokens.len() {
                    let pos = i;
                    if let Some((mut v, mut unit)) = value(tokens[i], self.binary) {
                        // "59.9 GB" split across two tokens.
                        if unit.is_none()
                            && let Some(next) = tokens.get(i + 1)
                            && let Some((dim, mult)) = lookup_unit(next, self.binary)
                        {
                            v *= mult;
                            unit = Unit::new(dim);
                            i += 1;
                        }
                        let nth = points.len();
                        points.push(Point {
                            series: self.name(pos, nth, tokens.len()),
                            value: v,
                            unit,
                        });
                    }
                    i += 1;
                }
            }
        }
        finish(x, points)
    }
}

/// Default parser: key/value pairs if the input has them, else numeric columns.
pub struct AutoParser {
    kv: KvParser,
    cols: ColumnsParser,
    mode: AutoMode,
}

#[derive(PartialEq)]
enum AutoMode {
    Undecided,
    Kv,
    Columns,
}

impl LineParser for AutoParser {
    fn parse(&mut self, line: &str) -> Result<Parsed, Skip> {
        match self.mode {
            AutoMode::Kv => self.kv.parse(line),
            AutoMode::Columns => self.cols.parse(line),
            AutoMode::Undecided => {
                if let Ok(p) = self.kv.parse(line) {
                    self.mode = AutoMode::Kv;
                    return Ok(p);
                }
                let r = self.cols.parse(line);
                if r.is_ok() {
                    self.mode = AutoMode::Columns;
                }
                r
            }
        }
    }
}

// ───────────────────────────── regex ─────────────────────────────

const SPECIAL: [&str; 6] = ["time", "x", "key", "series", "unit", "value"];

pub struct RegexParser {
    re: Regex,
    time: TimeParser,
    binary: bool,
    group_by: Option<String>,
    /// Named captures that hold values.
    values: Vec<String>,
    /// Group name → series name (for template names that aren't valid group names).
    rename: HashMap<String, String>,
}

impl RegexParser {
    pub fn new(
        pattern: &str,
        group_by: Option<&str>,
        time: TimeParser,
        binary: bool,
        rename: HashMap<String, String>,
    ) -> Result<Self> {
        let re = Regex::new(pattern)?;
        let names: Vec<&str> = re.capture_names().flatten().collect();
        let group_by = match group_by {
            Some(g) if !names.contains(&g) => {
                bail!("--group-by '{g}' is not a named capture in the regex")
            }
            Some(g) => Some(g.to_string()),
            None => ["key", "series"]
                .into_iter()
                .find(|n| names.contains(n))
                .map(String::from),
        };
        let mut values: Vec<String> = names
            .iter()
            .filter(|n| !SPECIAL.contains(n) && Some(**n) != group_by.as_deref())
            .map(|n| n.to_string())
            .collect();
        if names.contains(&"value") {
            values.insert(0, "value".into());
        }
        Ok(Self {
            re,
            time,
            binary,
            group_by,
            values,
            rename,
        })
    }
}

impl LineParser for RegexParser {
    fn parse(&mut self, line: &str) -> Result<Parsed, Skip> {
        let mut x = None;
        let mut points = Vec::new();
        let mut matched = false;
        for caps in self.re.captures_iter(line) {
            matched = true;
            if let Some(t) = caps.name("time") {
                let ts = self
                    .time
                    .parse(t.as_str())
                    .ok_or_else(|| Skip::Invalid(format!("can't parse time '{}'", t.as_str())))?;
                x.get_or_insert(XVal::Time(ts));
            } else if let Some(t) = caps.name("x") {
                let n =
                    t.as_str().trim().parse().map_err(|_| {
                        Skip::Invalid(format!("x '{}' is not a number", t.as_str()))
                    })?;
                x.get_or_insert(XVal::Num(n));
            }
            let key = self
                .group_by
                .as_ref()
                .and_then(|g| caps.name(g))
                .map(|m| m.as_str());
            let unit = caps.name("unit").map_or("", |m| m.as_str());
            let rename = &self.rename;
            let mut push = |name: &str, text: &str| {
                let name = rename.get(name).map_or(name, String::as_str);
                if let Some((value, u)) = value(&format!("{text}{unit}"), self.binary) {
                    let series = match (key, name) {
                        (Some(k), "value") => k.to_string(),
                        (Some(k), n) => format!("{k}.{n}"),
                        (None, n) => n.to_string(),
                    };
                    points.push(Point {
                        series,
                        value,
                        unit: u,
                    });
                }
            };
            if !self.values.is_empty() {
                for name in &self.values {
                    if let Some(m) = caps.name(name) {
                        push(name, m.as_str());
                    }
                }
            } else if caps.len() > 1 {
                // Unnamed groups: g1, g2, ...
                for (i, m) in caps.iter().enumerate().skip(1) {
                    if let Some(m) = m
                        && self.re.capture_names().nth(i).flatten().is_none()
                    {
                        push(&format!("g{i}"), m.as_str());
                    }
                }
            } else {
                push("value", &caps[0]);
            }
        }
        if !matched {
            return Err(Skip::NoMatch);
        }
        if x.is_none() {
            x = time_x(&self.time, line).0;
        }
        finish(x, points)
    }
}

// ───────────────────────────── CSV ─────────────────────────────

const TIME_NAMES: [&str; 7] = [
    "time",
    "timestamp",
    "ts",
    "date",
    "datetime",
    "@timestamp",
    "t",
];

pub struct CsvParser {
    delimiter: u8,
    names: Vec<String>,
    header: Option<Vec<String>>,
    time_field: Option<String>,
    time_idx: Option<Option<usize>>,
    first: bool,
    time: TimeParser,
    binary: bool,
}

impl CsvParser {
    fn new(cli: &Cli, time: TimeParser) -> Self {
        Self {
            delimiter: cli.delimiter.map_or(b',', |c| c as u8),
            names: cli.names.clone(),
            header: None,
            time_field: cli.time_field.clone(),
            time_idx: None,
            first: true,
            time,
            binary: cli.binary_units,
        }
    }

    fn column_name(&self, i: usize) -> String {
        self.names
            .get(i)
            .or_else(|| self.header.as_ref().and_then(|h| h.get(i)))
            .cloned()
            .unwrap_or_else(|| format!("col{}", i + 1))
    }

    fn find_time_column(&self, fields: &[String]) -> Option<usize> {
        if let Some(tf) = &self.time_field {
            return (0..fields.len())
                .find(|&i| self.column_name(i) == *tf)
                .or_else(|| tf.parse::<usize>().ok().map(|n| n - 1));
        }
        (0..fields.len())
            .find(|&i| TIME_NAMES.contains(&self.column_name(i).to_lowercase().as_str()))
            .or_else(|| {
                (0..fields.len()).find(|&i| {
                    self.time.parse(&fields[i]).is_some()
                        && parse_value(&fields[i], false).is_none()
                })
            })
    }
}

impl LineParser for CsvParser {
    fn parse(&mut self, line: &str) -> Result<Parsed, Skip> {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false)
            .delimiter(self.delimiter)
            .flexible(true)
            .from_reader(line.as_bytes());
        let record = match rdr.records().next() {
            Some(Ok(r)) => r,
            Some(Err(e)) => return Err(Skip::Invalid(e.to_string())),
            None => return Err(Skip::NoValues),
        };
        let fields: Vec<String> = record.iter().map(|f| f.trim().to_string()).collect();
        if std::mem::take(&mut self.first)
            && fields
                .iter()
                .all(|f| value(f, self.binary).is_none() && self.time.parse(f).is_none())
        {
            self.header = Some(fields);
            return Err(Skip::Header);
        }
        let time_idx = match self.time_idx {
            Some(t) => t,
            None => *self.time_idx.insert(self.find_time_column(&fields)),
        };
        let mut x = None;
        let mut points = Vec::new();
        for (i, f) in fields.iter().enumerate() {
            if Some(i) == time_idx {
                let ts = self
                    .time
                    .parse(f)
                    .ok_or_else(|| Skip::Invalid(format!("can't parse time '{f}'")))?;
                x = Some(XVal::Time(ts));
            } else if let Some((v, unit)) = value(f, self.binary) {
                points.push(Point {
                    series: self.column_name(i),
                    value: v,
                    unit,
                });
            }
        }
        finish(x, points)
    }
}

// ───────────────────────────── JSON ─────────────────────────────

pub struct JsonParser {
    buf: String,
    time_field: Option<String>,
    time: TimeParser,
    binary: bool,
}

fn flatten(v: &Value, prefix: &str, out: &mut Vec<(String, ScriptVal)>) {
    let key = |k: &str| {
        if prefix.is_empty() {
            k.to_string()
        } else {
            format!("{prefix}.{k}")
        }
    };
    match v {
        Value::Object(map) => map.iter().for_each(|(k, v)| flatten(v, &key(k), out)),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .for_each(|(i, v)| flatten(v, &key(&i.to_string()), out)),
        Value::Number(n) => out.extend(n.as_f64().map(|f| (prefix.to_string(), ScriptVal::Num(f)))),
        Value::Bool(b) => out.push((prefix.to_string(), ScriptVal::Num(f64::from(u8::from(*b))))),
        Value::String(s) => out.push((prefix.to_string(), ScriptVal::Str(s.clone()))),
        Value::Null => {}
    }
}

/// Convert flattened fields into points, pulling out a timestamp/x field.
fn fields_to_parsed(
    fields: Vec<(String, ScriptVal)>,
    time_field: Option<&str>,
    time: &TimeParser,
    binary: bool,
) -> Result<Parsed, Skip> {
    let is_time = |k: &str| match time_field {
        Some(tf) => k == tf,
        None => TIME_NAMES.contains(&k.to_lowercase().as_str()),
    };
    let mut x = None;
    let mut points = Vec::new();
    for (k, v) in fields {
        if k == "x" {
            if let ScriptVal::Num(n) = v {
                x = Some(XVal::Num(n));
            }
            continue;
        }
        if x.is_none() && is_time(&k) {
            let ts = match &v {
                ScriptVal::Num(n) => Some(if *n > 1e11 { n / 1000.0 } else { *n }),
                ScriptVal::Str(s) => time.parse(s),
            };
            if let Some(ts) = ts {
                x = Some(XVal::Time(ts));
                continue;
            }
        }
        let parsed = match v {
            ScriptVal::Num(n) => Some((n, Unit::default())),
            ScriptVal::Str(s) => value(&s, binary),
        };
        if let Some((value, unit)) = parsed {
            points.push(Point {
                series: k,
                value,
                unit,
            });
        }
    }
    finish(x, points)
}

impl LineParser for JsonParser {
    fn parse(&mut self, line: &str) -> Result<Parsed, Skip> {
        if self.buf.is_empty() && line.trim().is_empty() {
            return Err(Skip::NoValues);
        }
        self.buf.push_str(line);
        self.buf.push('\n');
        let v: Value = match serde_json::from_str(&self.buf) {
            Ok(v) => v,
            Err(e) if e.is_eof() => return Err(Skip::Incomplete),
            Err(e) => {
                self.buf.clear();
                return Err(Skip::Invalid(format!("invalid JSON: {e}")));
            }
        };
        self.buf.clear();
        let mut fields = Vec::new();
        flatten(&v, "", &mut fields);
        fields_to_parsed(fields, self.time_field.as_deref(), &self.time, self.binary)
    }
}

// ───────────────────────────── script ─────────────────────────────

/// Calls the user's Rhai `parse(line)` function.
pub struct ScriptParser {
    host: Rc<ScriptHost>,
    time: TimeParser,
    binary: bool,
}

impl LineParser for ScriptParser {
    fn parse(&mut self, line: &str) -> Result<Parsed, Skip> {
        match self.host.parse_line(line) {
            Ok(Some(fields)) => {
                let mut parsed = fields_to_parsed(fields, Some("time"), &self.time, self.binary)?;
                if parsed.x.is_none() {
                    parsed.x = time_x(&self.time, line).0;
                }
                Ok(parsed)
            }
            Ok(None) => Err(Skip::NoMatch),
            Err(e) => Err(Skip::Invalid(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::Dim;

    fn kv() -> KvParser {
        KvParser {
            time: TimeParser::default(),
            binary: false,
        }
    }

    #[test]
    fn kv_parses_reclaimable_log() {
        let p = kv()
            .parse("2026-11-01 04:00  node1: 59.9 GB  node2: 422 GB")
            .unwrap();
        assert!(matches!(p.x, Some(XVal::Time(_))));
        assert_eq!(p.points.len(), 2);
        assert_eq!(p.points[0].series, "node1");
        assert_eq!(p.points[0].value, 59.9e9);
        assert_eq!(p.points[1].value, 422e9);
        assert_eq!(p.points[1].unit.dim, Dim::Bytes { binary: false });
    }

    #[test]
    fn kv_meminfo_and_equals() {
        let mut p = KvParser {
            time: TimeParser::default(),
            binary: true,
        };
        let r = p.parse("Active(anon):     123 kB").unwrap();
        assert_eq!(r.points[0].series, "Active(anon)");
        assert_eq!(r.points[0].value, 123.0 * 1024.0);
        let r = kv().parse("a=1 b=2.5ms c=3").unwrap();
        let names: Vec<_> = r.points.iter().map(|p| p.series.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(
            r.points[0].unit,
            Unit::default(),
            "`b` must not be taken as a unit of a"
        );
        assert!((r.points[1].value - 0.0025).abs() < 1e-12);
    }

    #[test]
    fn columns_with_header() {
        let mut p = ColumnsParser::new(None, vec![], None, TimeParser::default(), false);
        assert_eq!(p.parse("r b free"), Err(Skip::Header));
        let r = p.parse("1 0 5000").unwrap();
        assert_eq!(r.points[2].series, "free");
        let mut p = ColumnsParser::new(
            Some(vec![2]),
            vec!["load5".into()],
            None,
            TimeParser::default(),
            false,
        );
        assert_eq!(
            p.parse("0.5 0.7 0.9 1/200 42").unwrap().points[0],
            Point {
                series: "load5".into(),
                value: 0.7,
                unit: Unit::default()
            }
        );
    }

    #[test]
    fn regex_group_by() {
        let mut p = RegexParser::new(
            r"(?P<node>node\d): (?P<value>\S+ GB)",
            Some("node"),
            TimeParser::default(),
            false,
            HashMap::new(),
        )
        .unwrap();
        let r = p
            .parse("2026-11-01 04:00  node1: 59.9 GB  node2: 422.6 GB")
            .unwrap();
        assert!(r.x.is_some(), "falls back to line timestamp");
        assert_eq!(
            r.points
                .iter()
                .map(|p| p.series.as_str())
                .collect::<Vec<_>>(),
            ["node1", "node2"]
        );
        assert_eq!(p.parse("nothing here"), Err(Skip::NoMatch));
    }

    #[test]
    fn regex_named_values_and_time() {
        let mut p = RegexParser::new(
            r"^(?P<time>\S+ \S+) .*took (?P<latency>\d+ms)",
            None,
            TimeParser::default(),
            false,
            HashMap::new(),
        )
        .unwrap();
        let r = p.parse("2026-11-01 04:00:01 GET / took 12ms").unwrap();
        assert_eq!(r.points[0].series, "latency");
        assert!((r.points[0].value - 0.012).abs() < 1e-12);
    }

    #[test]
    fn json_lines_and_multiline() {
        let mut p = JsonParser {
            buf: String::new(),
            time_field: None,
            time: TimeParser::default(),
            binary: false,
        };
        let r = p
            .parse(
                r#"{"ts": "2026-11-01T04:00:00", "mem": {"free": 10, "used": "2 GB"}, "ok": true}"#,
            )
            .unwrap();
        assert!(r.x.is_some());
        let names: Vec<_> = r.points.iter().map(|p| p.series.as_str()).collect();
        assert_eq!(names, ["mem.free", "mem.used", "ok"]);
        assert_eq!(p.parse("{"), Err(Skip::Incomplete));
        assert_eq!(p.parse(r#""a": 1}"#).unwrap().points[0].series, "a");
    }

    #[test]
    fn auto_falls_back_to_columns() {
        let time = TimeParser::default();
        let mut p = AutoParser {
            kv: KvParser {
                time: time.clone(),
                binary: false,
            },
            cols: ColumnsParser::new(None, vec![], None, time, false),
            mode: AutoMode::Undecided,
        };
        assert_eq!(p.parse("1 2 3").unwrap().points.len(), 3);
    }
}
