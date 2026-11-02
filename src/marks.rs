//! Markers on the chart where a condition holds: `--mark 'delta < -1GB => dropped {delta}'`.
//!
//! A rule is a Rhai condition evaluated at every point of every series, with:
//! `value`, `prev` (the point before), `delta` (value − prev), `pct` (delta as % of
//! prev), `rate` (delta per second), `time`/`x`, `i` (point index) and `name` (the
//! series). Unit literals work (`1GB`, `5%`). Restrict a rule to one series with
//! `name == "node2" && …`. The label after `=>` may use `{name} {value} {prev}
//! {delta} {pct} {time}`.

use anyhow::{Context, Result};
use rhai::{Dynamic, Scope};

use crate::data::Dataset;
use crate::render::Mark;
use crate::script::ScriptHost;

/// Most markers one rule may place (a rule that matches everything isn't useful).
const MAX_PER_RULE: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct MarkRule {
    pub expr: String,
    pub label: Option<String>,
    /// Send a notification (`--notify` targets) for each new marker it places.
    pub notify: bool,
    /// The marker's symbol (default ▼) and colour (default: the series' colour).
    pub symbol: Option<String>,
    pub color: Option<String>,
}

/// Symbols the markers menu cycles through.
pub const SYMBOLS: [&str; 8] = ["▼", "▲", "●", "◆", "★", "✕", "↑", "↓"];
/// Colours it cycles through (`None` = the series' own).
pub const COLORS: [Option<&str>; 7] = [
    None,
    Some("green"),
    Some("red"),
    Some("yellow"),
    Some("cyan"),
    Some("magenta"),
    Some("white"),
];

impl MarkRule {
    pub fn symbol(&self) -> &str {
        self.symbol.as_deref().unwrap_or("▼")
    }

    /// Upward symbols sit below the point, the rest above it.
    pub fn points_up(symbol: &str) -> bool {
        matches!(symbol, "▲" | "↑" | "△" | "⬆")
    }
}

impl MarkRule {
    /// `EXPR` or `EXPR => LABEL`, optionally starting with `notify `.
    pub fn parse(s: &str) -> Result<MarkRule, String> {
        let trimmed = s.trim_start();
        let (notify, s) = match trimmed.strip_prefix("notify ") {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };
        // A style after the label: `… => label | ▲ green`.
        // (Only after `=>`: `a | b` in the condition is Rhai's "or".)
        let (s, style) = match s.split_once("=>") {
            Some((cond, label)) => match label.rsplit_once(" | ") {
                Some((l, style)) => (&s[..cond.len() + 2 + l.len()], Some(style)),
                None => (s, None),
            },
            None => (s, None),
        };
        let (mut symbol, mut color) = (None, None);
        for tok in style.unwrap_or("").split_whitespace() {
            if tok.parse::<ratatui::style::Color>().is_ok() {
                color = Some(tok.to_string());
            } else if unicode_width::UnicodeWidthStr::width(tok) <= 2 {
                symbol = Some(tok.to_string());
            } else {
                return Err(format!(
                    "'{tok}': a marker style is a symbol (▲ ● ★…) and/or a colour (green, #ff8800, 208)"
                ));
            }
        }
        let (expr, label) = match s.split_once("=>") {
            Some((e, l)) => (
                e.trim(),
                Some(l.trim().to_string()).filter(|l| !l.is_empty()),
            ),
            None => (s.trim(), None),
        };
        if expr.is_empty() {
            return Err("give a condition, e.g. 'delta < -1GB => dropped {delta}'".into());
        }
        Ok(MarkRule {
            expr: expr.to_string(),
            label,
            notify,
            symbol,
            color,
        })
    }

    pub fn to_spec(&self) -> String {
        let mut rule = match &self.label {
            Some(l) => format!("{} => {l}", self.expr),
            None => self.expr.clone(),
        };
        let style: Vec<&str> = [self.symbol.as_deref(), self.color.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        if !style.is_empty() {
            if self.label.is_none() {
                rule.push_str(" => {name} {delta}");
            }
            rule.push_str(" | ");
            rule.push_str(&style.join(" "));
        }
        if self.notify {
            format!("notify {rule}")
        } else {
            rule
        }
    }
}

/// Place markers for every rule. An error stops that rule (reported, the rest go on).
pub fn evaluate(host: &ScriptHost, rules: &[MarkRule], ds: &Dataset) -> (Vec<Mark>, Vec<String>) {
    let mut marks = Vec::new();
    let mut errors = Vec::new();
    for (n, rule) in rules.iter().enumerate() {
        if let Err(e) = evaluate_rule(host, n, rule, ds, &mut marks) {
            errors.push(format!("mark '{}': {e:#}", rule.expr));
        }
    }
    marks.sort_by(|a, b| a.x.total_cmp(&b.x));
    (marks, errors)
}

fn evaluate_rule(
    host: &ScriptHost,
    n: usize,
    rule: &MarkRule,
    ds: &Dataset,
    out: &mut Vec<Mark>,
) -> Result<()> {
    let ast = host.compile_expr(&rule.expr)?;
    // Every series is also a variable: its value at this point (`ema30`) and at the
    // previous one (`ema30_prev`), so rules can compare series — e.g. crossings.
    let named: Vec<(String, &[(f64, f64)])> = ds
        .series
        .iter()
        .map(|s| (crate::script::ident(&s.name), s.points.as_slice()))
        .filter(|(id, _)| {
            !matches!(
                id.as_str(),
                "value"
                    | "prev"
                    | "delta"
                    | "pct"
                    | "rate"
                    | "x"
                    | "time"
                    | "prev_time"
                    | "i"
                    | "name"
            )
        })
        .collect();
    // `x_prev` is x at the previous point — unless a series is itself called `x_prev`.
    let idents: Vec<String> = named.iter().map(|(id, _)| id.clone()).collect();
    let mut placed = 0;
    for s in &ds.series {
        for (i, &(x, value)) in s.points.iter().enumerate() {
            let (px, prev) = if i > 0 { s.points[i - 1] } else { (x, value) };
            let delta = value - prev;
            let pct = if prev != 0.0 {
                delta / prev.abs() * 100.0
            } else {
                0.0
            };
            let rate = if x > px { delta / (x - px) } else { 0.0 };
            let mut scope = Scope::new();
            scope.push("value", value);
            scope.push("prev", prev);
            scope.push("delta", delta);
            scope.push("pct", pct);
            scope.push("rate", rate);
            scope.push("x", x);
            scope.push("time", x);
            scope.push("i", i as i64);
            scope.push("name", s.name.clone());
            scope.push("prev_time", px);
            for (id, pts) in &named {
                let at = |x: f64| {
                    crate::render::interpolate(pts, x).map_or(Dynamic::UNIT, Dynamic::from_float)
                };
                scope.push_dynamic(id.as_str(), at(x));
                let prev_name = format!("{id}_prev");
                if !idents.contains(&prev_name) {
                    scope.push_dynamic(prev_name, at(px));
                }
            }
            let hit: Dynamic = host
                .eval_ast_with_scope(&mut scope, &ast)
                .with_context(|| format!("at {} point {i}", s.name))?;
            let hit = hit
                .as_bool()
                .map_err(|t| anyhow::anyhow!("returned {t}, expected true/false"))?;
            if !hit {
                continue;
            }
            placed += 1;
            if placed > MAX_PER_RULE {
                return Err(anyhow::anyhow!(
                    "matched more than {MAX_PER_RULE} points; make the condition stricter"
                ));
            }
            let fill = |t: &str| {
                t.replace("{name}", &s.name)
                    .replace("{value}", &s.unit.format(value))
                    .replace("{prev}", &s.unit.format(prev))
                    .replace("{delta}", &signed(&s.unit, delta))
                    .replace("{pct}", &format!("{pct:+.1}%"))
                    .replace("{time}", &ds.fmt_x_full(x))
            };
            out.push(Mark {
                series: Some(s.name.clone()),
                x,
                y: Some(value),
                label: fill(rule.label.as_deref().unwrap_or("{name} {delta}")),
                rule: Some(n),
                symbol: rule.symbol().to_string(),
                color: rule.color.as_deref().and_then(|c| c.parse().ok()),
            });
        }
    }
    Ok(())
}

fn signed(unit: &crate::units::Unit, v: f64) -> String {
    if v < 0.0 {
        format!("−{}", unit.format(-v))
    } else {
        format!("+{}", unit.format(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Series, XKind};
    use crate::units::{Dim, Unit};

    #[test]
    fn rules_place_labelled_marks() {
        let host = ScriptHost::new(None, false).unwrap();
        let ds = Dataset {
            x_kind: XKind::Time,
            series: vec![
                Series {
                    name: "node1".into(),
                    unit: Unit::new(Dim::Bytes { binary: false }),
                    points: vec![
                        (0.0, 60e9),
                        (3600.0, 58.5e9),
                        (7200.0, 58.4e9),
                        (10800.0, 56e9),
                    ],
                },
                Series {
                    name: "node2".into(),
                    unit: Unit::new(Dim::Bytes { binary: false }),
                    points: vec![(0.0, 400e9), (3600.0, 390e9)],
                },
            ],
        };
        let rule = MarkRule::parse("name == \"node1\" && delta < -1GB => dropped {delta} ({pct})")
            .unwrap();
        let (marks, errors) = evaluate(&host, std::slice::from_ref(&rule), &ds);
        assert!(errors.is_empty(), "{errors:?}");
        let labels: Vec<_> = marks.iter().map(|m| (m.x, m.label.as_str())).collect();
        assert_eq!(
            labels,
            [
                (3600.0, "dropped −1.5 GB (-2.5%)"),
                (10800.0, "dropped −2.4 GB (-4.1%)")
            ]
        );
        assert_eq!(MarkRule::parse(&rule.to_spec()).unwrap(), rule);
        // Default label, and every series.
        let (marks, _) = evaluate(&host, &[MarkRule::parse("delta < -5GB").unwrap()], &ds);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].label, "node2 −10 GB");
        // Errors are reported per rule.
        let (_, errors) = evaluate(&host, &[MarkRule::parse("value").unwrap()], &ds);
        assert!(errors[0].contains("expected true/false"), "{errors:?}");
        assert!(MarkRule::parse(" => x").is_err());
        let styled =
            MarkRule::parse("prev < ema30_prev && value >= ema30 => up | ▲ green").unwrap();
        assert_eq!(
            (
                styled.symbol(),
                styled.color.as_deref(),
                styled.label.as_deref()
            ),
            ("▲", Some("green"), Some("up"))
        );
        assert_eq!(MarkRule::parse(&styled.to_spec()).unwrap(), styled);
        assert!(MarkRule::parse("value > 1 => x | notacolour").is_err());
        let n = MarkRule::parse("notify value > 5 => hot").unwrap();
        assert!(n.notify && n.expr == "value > 5");
        assert_eq!(MarkRule::parse(&n.to_spec()).unwrap(), n);
        // Other series by name, now and at the previous point: crossings.
        let crossing = Dataset {
            x_kind: XKind::Time,
            series: vec![
                Series {
                    name: "price".into(),
                    unit: Unit::default(),
                    points: vec![(0.0, 9.0), (1.0, 11.0), (2.0, 12.0), (3.0, 8.0)],
                },
                Series {
                    name: "ema30".into(),
                    unit: Unit::default(),
                    points: vec![(0.0, 10.0), (1.0, 10.0), (2.0, 10.0), (3.0, 10.0)],
                },
            ],
        };
        let up = MarkRule::parse("name == \"price\" && prev < ema30_prev && value >= ema30 => up")
            .unwrap();
        let down =
            MarkRule::parse("name == \"price\" && prev > ema30_prev && value <= ema30 => down")
                .unwrap();
        let (marks, errors) = evaluate(&host, &[up, down], &crossing);
        assert!(errors.is_empty(), "{errors:?}");
        let got: Vec<_> = marks.iter().map(|m| (m.x, m.label.as_str())).collect();
        assert_eq!(got, [(1.0, "up"), (3.0, "down")]);
    }
}
