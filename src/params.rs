//! Source parameters: `--param 'interval = 1m | 5m | *1h'` gives the source a setting
//! with those choices (`*` marks the current one, else it's the first), filled in
//! wherever its command says `{interval}`. The viewer offers them as a dropdown, and
//! picking another value runs the source again with it.

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub choices: Vec<String>,
    pub current: usize,
}

impl Param {
    /// `NAME = a | b | *c`.
    pub fn parse(spec: &str) -> Result<Param> {
        let bad = || {
            format!(
                "--param '{spec}': write it as NAME = choice | choice …, e.g. 'interval = 1m | 5m | 1h' (* marks the current one)"
            )
        };
        let Some((name, rest)) = spec.split_once('=') else {
            bail!(bad());
        };
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            bail!(bad());
        }
        let mut current = 0;
        let mut choices = Vec::new();
        for (i, c) in rest.split('|').map(str::trim).enumerate() {
            let c = match c.strip_prefix('*') {
                Some(c) => {
                    current = i;
                    c.trim()
                }
                None => c,
            };
            if c.is_empty() {
                bail!(bad());
            }
            choices.push(c.to_string());
        }
        if choices.is_empty() {
            bail!(bad());
        }
        Ok(Param {
            name: name.to_string(),
            choices,
            current,
        })
    }

    pub fn value(&self) -> &str {
        &self.choices[self.current]
    }

    /// Back to `NAME = a | *b | c` (no * when it's the first).
    pub fn to_spec(&self) -> String {
        let choices: Vec<String> = self
            .choices
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == self.current && i > 0 {
                    format!("*{c}")
                } else {
                    c.clone()
                }
            })
            .collect();
        format!("{} = {}", self.name, choices.join(" | "))
    }
}

/// The parameters of a command line (`--param` values); bad ones are left out.
pub fn parse_all(specs: &[String]) -> Vec<Param> {
    specs.iter().filter_map(|s| Param::parse(s).ok()).collect()
}

/// `text` with each `{name}` replaced by that parameter's current value.
pub fn fill(text: &str, params: &[Param]) -> String {
    let mut out = text.to_string();
    for p in params {
        out = out.replace(&format!("{{{}}}", p.name), p.value());
    }
    out
}

/// `specs` with parameter `name` set to `value` (one of its choices).
pub fn set(specs: &[String], name: &str, value: &str) -> Result<Vec<String>> {
    let mut found = false;
    let out = specs
        .iter()
        .map(|s| match Param::parse(s) {
            Ok(mut p) if p.name == name => {
                found = true;
                match p.choices.iter().position(|c| c == value) {
                    Some(i) => {
                        p.current = i;
                        Ok(p.to_spec())
                    }
                    None => bail!("{name} can be {}, not '{value}'", p.choices.join(", ")),
                }
            }
            _ => Ok(s.clone()),
        })
        .collect::<Result<Vec<_>>>()?;
    if !found {
        bail!("this source has no setting called '{name}'");
    }
    Ok(out)
}

/// Time placeholders, filled for a window that ends at `end` (seconds since the
/// epoch): `{now}` / `{now_ms}` the end, `{from:6h}` / `{from_ms:6h}` that long
/// before it (an hour if no length is given), or `{from_ms:500*1m}` (500 × 1m).
pub fn fill_times(text: &str, end: f64) -> String {
    static TIMES: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"\{(now|now_ms|from|from_ms)(?::(?:([0-9.]+)\s*\*\s*)?([0-9.]+\s*[a-z]+))?\}",
        )
        .expect("valid")
    });
    if !text.contains('{') {
        return text.to_string();
    }
    TIMES
        .replace_all(text, |c: &regex::Captures| {
            // `6h`, or `500*1m` (500 candles of a minute: with {interval} inside, the
            // window follows the candle size).
            let times: f64 = c
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(1.0);
            let span = c
                .get(3)
                .and_then(|m| crate::time::parse_duration_secs(m.as_str()).ok())
                .unwrap_or(3600.0)
                * times;
            let t = match &c[1] {
                "now" | "now_ms" => end,
                _ => end - span,
            };
            if c[1].ends_with("_ms") {
                format!("{}", (t * 1000.0).round() as i64)
            } else {
                format!("{}", t.round() as i64)
            }
        })
        .into_owned()
}

/// Whether a command asks for a time window (so older data can be fetched).
pub fn has_window(text: &str) -> bool {
    text.contains("{from") && text.contains("{now")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_parse_fill_and_change() {
        let p = Param::parse("interval = 1m | 5m | *1h").unwrap();
        assert_eq!((p.name.as_str(), p.value()), ("interval", "1h"));
        assert_eq!(p.to_spec(), "interval = 1m | 5m | *1h");
        let first = Param::parse("coin=ETH|BTC").unwrap();
        assert_eq!(first.value(), "ETH");
        assert_eq!(first.to_spec(), "coin = ETH | BTC");
        assert_eq!(
            fill(
                r#"{"coin":"{coin}","interval":"{interval}"}"#,
                &[p.clone(), first]
            ),
            r#"{"coin":"ETH","interval":"1h"}"#
        );
        let specs = vec!["interval = 1m | 5m | *1h".to_string()];
        assert_eq!(
            set(&specs, "interval", "5m").unwrap(),
            ["interval = 1m | *5m | 1h"]
        );
        assert!(
            set(&specs, "interval", "2d")
                .unwrap_err()
                .to_string()
                .contains("1m, 5m, 1h")
        );
        assert!(set(&specs, "coin", "ETH").is_err());
        assert!(Param::parse("no equals").is_err());
        assert!(Param::parse("x = a || b").is_err());
    }

    #[test]
    fn time_placeholders() {
        let t = fill_times(
            r#"{"startTime":{from_ms:6h},"endTime":{now_ms}} {from:1d} {now}"#,
            100_000.0,
        );
        assert_eq!(
            t,
            r#"{"startTime":78400000,"endTime":100000000} 13600 100000"#
        );
        assert!(has_window("{from_ms:6h} {now_ms}") && !has_window("{now}"));
        assert_eq!(fill_times("no braces", 1.0), "no braces");
        assert_eq!(fill_times("{from:500*1m}", 100_000.0), "70000");
    }
}
