//! Format templates: a readable alternative to regexes.
//!
//! `{time} node1: {node1} node2: {node2}` compiles to a regex with named groups:
//!
//! | placeholder        | matches                                               |
//! |--------------------|-------------------------------------------------------|
//! | `{name}`           | a number with an optional unit → series `name`        |
//! | `{name:int}`       | an integer                                            |
//! | `{name:word}`      | any non-space text (useful with `{key:word}`)         |
//! | `{name:/re/}`      | a custom regex                                        |
//! | `{time}`           | a timestamp (auto-detected shapes)                    |
//! | `{time:%d/%m %H:%M}` | a timestamp in this strftime format                 |
//! | `{x}`              | a numeric x value                                     |
//! | `{key}`            | the series name (one series per distinct value)       |
//! | `{_}` / `{_:word}` | skip any text / one word                              |
//! | `{{`, `}}`         | literal braces                                        |
//!
//! Whitespace in the template matches any run of whitespace.

use std::collections::HashMap;

use anyhow::{Result, bail};

/// A number, optionally followed by a unit: any attached suffix (`12ms`, `40%`, `5x`),
/// or a known unit after one space (`59.9 GB`) — so `a: 1 b: 2` doesn't read `b` as a unit.
pub const NUMBER: &str = concat!(
    r"[-+]?(?:[0-9][0-9,]*(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][-+]?[0-9]+)?",
    r"(?:[A-Za-zµ°%][A-Za-zµ°%/]*|\s(?:[KMGTP]iB|[kKMGTP]B|[KMGTP]|B|bytes|[nuµm]?s|sec|min|h|°[CF])(?:/s)?\b)?"
);

/// Timestamp shapes recognised by auto-detection.
const AUTO_TIME: &str = concat!(
    r"(?:\d{4}[-/]\d{2}[-/]\d{2}[T ]\d{2}:\d{2}(?::\d{2}(?:[.,]\d+)?)?(?:\s?Z|[+-]\d{2}:?\d{2}|\s[+-]\d{2}:\d{2})?",
    r"|\d{2}/[A-Z][a-z]{2}/\d{4}:\d{2}:\d{2}:\d{2}(?:\s[+-]\d{4})?",
    r"|[A-Z][a-z]{2}\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}",
    r"|\d{2}:\d{2}:\d{2}(?:\.\d+)?",
    r"|\d{4}-\d{2}-\d{2}",
    r"|\d{13}|\d{10}(?:\.\d+)?)"
);

#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub regex: String,
    /// Set when `{time:FORMAT}` was used.
    pub time_format: Option<String>,
    /// Regex group name → series name, for names that aren't valid group names.
    pub rename: HashMap<String, String>,
}

/// Translate a strftime format into a regex matching it.
pub fn strftime_regex(fmt: &str) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            if c.is_whitespace() {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                out.push_str(r"\s+");
            } else {
                out.push_str(&regex::escape(&c.to_string()));
            }
            continue;
        }
        // Skip flags/widths like %-d, %3f, %.f
        let mut spec = String::new();
        while let Some(&n) = chars.peek() {
            spec.push(n);
            chars.next();
            if n.is_ascii_alphabetic() || n == '%' {
                break;
            }
        }
        let re = match spec.chars().last().unwrap_or('%') {
            'Y' => r"[+-]?\d{4}",
            'C' | 'y' => r"\d{2}",
            'm' | 'd' | 'H' | 'M' | 'S' | 'I' | 'l' | 'k' => r"\s?\d{1,2}",
            'e' => r"\s?\d{1,2}",
            'j' => r"\d{1,3}",
            'f' if spec.starts_with('.') => r"(?:\.\d+)?",
            'f' => r"\d+",
            'b' | 'h' | 'a' => r"[A-Za-z]{3}",
            'B' | 'A' => r"[A-Za-z]+",
            'p' | 'P' => r"[AaPp][Mm]",
            'z' => r"(?:Z|[+-]\d{2}:?\d{2})",
            'Z' => r"[A-Za-z]+",
            's' => r"\d+",
            'T' => r"\d{2}:\d{2}:\d{2}",
            'R' => r"\d{2}:\d{2}",
            'D' => r"\d{2}/\d{2}/\d{2}",
            'F' => r"\d{4}-\d{2}-\d{2}",
            'n' | 't' => r"\s+",
            '%' => "%",
            _ => r".+?",
        };
        out.push_str(re);
    }
    out
}

fn is_group_name(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Compile a template into a regex.
pub fn compile(src: &str) -> Result<Template> {
    let mut regex = String::new();
    let mut time_format = None;
    let mut rename = HashMap::new();
    let mut used: Vec<String> = Vec::new();
    let mut chars = src.trim().chars().peekable();
    let mut has_capture = false;
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                regex.push_str(r"\{");
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                regex.push_str(r"\}");
            }
            '}' => bail!("unmatched '}}' in template (use '}}}}' for a literal brace)"),
            '{' => {
                // Read the placeholder, allowing `}` inside a `/regex/` spec.
                let mut body = String::new();
                let mut in_re = false;
                loop {
                    let Some(n) = chars.next() else {
                        bail!("unclosed '{{' in template")
                    };
                    if n == '}' && !in_re {
                        break;
                    }
                    if n == '/' && (body.ends_with(":") || in_re) && !body.ends_with('\\') {
                        in_re = !in_re;
                    }
                    body.push(n);
                }
                let (name, spec) = match body.split_once(':') {
                    Some((n, s)) => (n.trim(), Some(s)),
                    None => (body.trim(), None),
                };
                let custom = spec
                    .and_then(|s| s.strip_prefix('/'))
                    .and_then(|s| s.strip_suffix('/'));
                let skip = name.is_empty() || name == "_";
                let pattern = match (name, spec) {
                    (_, _) if custom.is_some() => custom.unwrap_or_default().to_string(),
                    ("time", Some(fmt)) if !skip => {
                        time_format = Some(fmt.to_string());
                        strftime_regex(fmt)
                    }
                    ("time", None) => AUTO_TIME.to_string(),
                    ("key" | "series", None) => r"[^\s:=,;]+".to_string(),
                    (_, Some("word")) => r"\S+".to_string(),
                    (_, Some("int")) => r"[-+]?\d+".to_string(),
                    (_, Some("num" | "number") | None) if !skip => NUMBER.to_string(),
                    (_, None) => r".*?".to_string(),
                    (_, Some(other)) => bail!(
                        "unknown placeholder type '{other}' in {{{body}}} (use int, num, word or /regex/)"
                    ),
                };
                if skip {
                    regex.push_str(&format!("(?:{pattern})"));
                    continue;
                }
                let group = if is_group_name(name) {
                    name.to_string()
                } else {
                    let g = format!(
                        "v{}_{}",
                        used.len(),
                        name.chars()
                            .filter(char::is_ascii_alphanumeric)
                            .collect::<String>()
                    );
                    rename.insert(g.clone(), name.to_string());
                    g
                };
                if used.contains(&group) {
                    bail!("'{{{name}}}' appears twice in the template");
                }
                used.push(group.clone());
                has_capture = true;
                regex.push_str(&format!("(?P<{group}>{pattern})"));
            }
            c if c.is_whitespace() => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                regex.push_str(r"\s+");
            }
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
    }
    if !has_capture {
        bail!("the template has no placeholders; add one like {{value}}");
    }
    // Validate early for a clear error.
    regex::Regex::new(&regex)
        .map_err(|e| anyhow::anyhow!("template compiles to an invalid regex: {e}"))?;
    Ok(Template {
        regex,
        time_format,
        rename,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{LineParser, RegexParser};
    use crate::time::{TimeParser, format_time_full};

    fn parser(t: &str) -> RegexParser {
        let t = compile(t).unwrap();
        RegexParser::new(
            &t.regex,
            None,
            TimeParser::new(t.time_format),
            false,
            t.rename,
        )
        .unwrap()
    }

    fn names(p: &mut RegexParser, line: &str) -> Vec<(String, f64)> {
        p.parse(line)
            .unwrap()
            .points
            .into_iter()
            .map(|p| (p.series, p.value))
            .collect()
    }

    #[test]
    fn reclaimable_template() {
        let mut p = parser("{time} node1: {node1} node2: {node2}");
        let r = p
            .parse("2026-11-01 04:00  node1: 59.9 GB  node2: 422 GB")
            .unwrap();
        assert!(r.x.is_some());
        assert_eq!(
            names(&mut p, "2026-11-01 04:00  node1: 59.9 GB  node2: 422 GB"),
            [("node1".into(), 59.9e9), ("node2".into(), 422e9)]
        );
    }

    #[test]
    fn strftime_and_skip() {
        let mut p = parser("[{time:%d/%m/%Y %H:%M}] {_} took {latency}");
        let r = p.parse("[01/11/2026 04:00] GET /api took 12ms").unwrap();
        assert_eq!(r.points[0].series, "latency");
        assert!((r.points[0].value - 0.012).abs() < 1e-12);
        let Some(crate::parse::XVal::Time(t)) = r.x else {
            panic!()
        };
        assert_eq!(format_time_full(t), "2026-11-01 04:00");
    }

    #[test]
    fn key_groups_rows() {
        let mut p = parser("{key} {_:word} {used} {avail} {pct}");
        assert_eq!(
            names(&mut p, "/dev/sda1 30G 12G 17G 42%")[0].0,
            "/dev/sda1.used"
        );
        let mut p = parser("{key}: {value}");
        assert_eq!(
            names(&mut p, "a: 1 b: 2"),
            [("a".into(), 1.0), ("b".into(), 2.0)]
        );
        assert_eq!(
            names(&mut p, "x: 5 GB y: 2 s"),
            [("x".into(), 5e9), ("y".into(), 2.0)]
        );
    }

    #[test]
    fn odd_names_custom_regex_and_braces() {
        let mut p = parser("{{{Active(anon)}}} {n:/\\d{2}/}");
        assert_eq!(
            names(&mut p, "{5} 42"),
            [("Active(anon)".into(), 5.0), ("n".into(), 42.0)]
        );
    }

    #[test]
    fn errors() {
        assert!(compile("{a").is_err());
        assert!(compile("a}").is_err());
        assert!(compile("{a} {a}").is_err());
        assert!(compile("plain text").is_err());
        assert!(compile("{a:wat}").is_err());
    }

    #[test]
    fn strftime_translation() {
        assert_eq!(strftime_regex("%Y-%m"), r"[+-]?\d{4}\-\s?\d{1,2}");
        assert_eq!(
            strftime_regex("%H:%M:%S%.f"),
            r"\s?\d{1,2}:\s?\d{1,2}:\s?\d{1,2}(?:\.\d+)?"
        );
    }
}
