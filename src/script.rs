//! Rhai scripting: console expressions, alerts, derived series, key bindings and
//! custom parsers all share one engine with a small graph-analysis API.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use rhai::{
    AST, Array, CallFnOptions, Dynamic, Engine, EvalAltResult, ImmutableString, Map, Scope,
};

use crate::data::Dataset;
use crate::stats::Stats;
use crate::units::{Unit, lookup_unit};

type FnResult<T> = Result<T, Box<EvalAltResult>>;

/// A value produced by a script `parse(line)` function or a JSON document.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptVal {
    Num(f64),
    Str(String),
}

/// Something a script asked the viewer to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    HLine(f64, Option<String>),
    ClearHLines,
    /// Show the last N seconds (x units).
    Zoom(f64),
    ResetView,
    Show(String),
    Hide(String),
    Chart(String),
    /// Set (`Some`) or remove (`None`) a series' forecast target.
    Target(String, Option<f64>),
    /// Forecast horizon (`"auto"`, `"7d"`), or `None` to turn forecasting off.
    Forecast(Option<String>),
    /// Timeframe (`"7d"`, `"500"`), or `None` for all data.
    Range(Option<String>),
    /// Trend lines: "fit"/"line", "avg"/"average", or `None` for off.
    Trend(Option<String>),
    /// Show the raw data (false = trend only).
    Raw(bool),
    SideBySide(bool),
}

/// State shared between the host and registered functions.
#[derive(Debug, Default)]
pub struct ScriptState {
    pub data: Dataset,
    pub view: Option<(f64, f64)>,
    pub actions: Vec<Action>,
    pub binds: Vec<(String, String)>,
    pub output: Vec<String>,
}

pub struct ScriptHost {
    engine: Engine,
    /// The user's --script (empty if none).
    ast: AST,
    state: Rc<RefCell<ScriptState>>,
    binary: bool,
}

/// Turn a series name into a valid Rhai identifier (`cpu.user` → `cpu_user`).
pub fn ident(name: &str) -> String {
    const RESERVED: &[&str] = &[
        "if", "else", "let", "const", "fn", "in", "for", "while", "loop", "do", "until", "return",
        "true", "false", "this", "switch", "import", "export", "as", "private", "break",
        "continue", "throw", "try", "catch", "global", "Fn", "call", "curry", "type_of", "print",
        "debug", "eval",
    ];
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    if RESERVED.contains(&s.as_str()) {
        s.push('_');
    }
    s
}

static UNIT_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?P<n>\d+(?:\.\d+)?(?:[eE][-+]?\d+)?)",
        r"(?:\s?(?P<long>KiB|MiB|GiB|TiB|PiB|kB|KB|MB|GB|TB|PB|ms|us|ns|min|day|days|week|weeks)",
        r"|(?P<short>[KMGTPBshdw]))\b"
    ))
    .expect("valid unit literal regex")
});

/// Rewrite unit literals (`50GB`, `1.5 h`, `200ms`, `1d`) into plain numbers in base
/// units, leaving string literals untouched.
pub fn preprocess(src: &str, binary: bool) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while !rest.is_empty() {
        // Copy code up to the next string literal, rewriting it; then copy the literal.
        let code_end = rest.find(['"', '`', '\'']).unwrap_or(rest.len());
        out.push_str(&rewrite_units(&rest[..code_end], binary));
        rest = &rest[code_end..];
        if let Some(quote) = rest.chars().next() {
            let mut end = rest.len();
            let mut escaped = false;
            for (i, c) in rest.char_indices().skip(1) {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    c if c == quote => {
                        end = i + 1;
                        break;
                    }
                    _ => {}
                }
            }
            out.push_str(&rest[..end]);
            rest = &rest[end..];
        }
    }
    out
}

fn rewrite_units(code: &str, binary: bool) -> String {
    UNIT_LITERAL
        .replace_all(code, |c: &regex::Captures| {
            let n: f64 = c["n"].parse().unwrap_or(0.0);
            let unit = c
                .name("long")
                .or(c.name("short"))
                .map_or("", |m| m.as_str());
            let mult = match unit {
                "d" | "day" | "days" => 86400.0,
                "w" | "week" | "weeks" => 604_800.0,
                u => lookup_unit(u, binary).map_or(1.0, |(_, m)| m),
            };
            let v = n * mult;
            // Keep it a float literal so arithmetic stays in floating point.
            let s = format!("{v}");
            if s.contains('.') { s } else { format!("{s}.0") }
        })
        .into_owned()
}

fn dyn_to_f64(d: &Dynamic) -> Option<f64> {
    d.as_float()
        .ok()
        .or_else(|| d.as_int().ok().map(|i| i as f64))
}

/// Format a script result for display.
pub fn display(d: &Dynamic) -> String {
    if d.is_unit() {
        "ok".into()
    } else if let Ok(f) = d.as_float() {
        crate::units::fmt_compact(f)
    } else {
        d.to_string()
    }
}

impl ScriptHost {
    pub fn new(script: Option<&Path>, binary: bool) -> Result<Self> {
        let state = Rc::new(RefCell::new(ScriptState::default()));
        let mut engine = Engine::new();
        engine.set_max_operations(50_000_000);
        register_api(&mut engine, &state, binary);
        let ast = match script {
            Some(path) => {
                let src = std::fs::read_to_string(path)
                    .with_context(|| format!("reading script {}", path.display()))?;
                let ast = engine
                    .compile(preprocess(&src, binary))
                    .map_err(|e| anyhow!("{}: {e}", path.display()))?;
                // Run top-level statements once (registers key bindings etc.).
                engine
                    .run_ast(&ast)
                    .map_err(|e| anyhow!("{}: {e}", path.display()))?;
                ast
            }
            None => AST::empty(),
        };
        Ok(Self {
            engine,
            ast,
            state,
            binary,
        })
    }

    pub fn has_fn(&self, name: &str) -> bool {
        self.ast.iter_functions().any(|f| f.name == name)
    }

    /// Replace the data the API functions see.
    pub fn set_data(&self, data: Dataset, view: Option<(f64, f64)>) {
        let mut st = self.state.borrow_mut();
        st.data = data;
        st.view = view;
    }

    pub fn take_actions(&self) -> Vec<Action> {
        std::mem::take(&mut self.state.borrow_mut().actions)
    }

    pub fn take_output(&self) -> Vec<String> {
        std::mem::take(&mut self.state.borrow_mut().output)
    }

    pub fn binds(&self) -> Vec<(String, String)> {
        self.state.borrow().binds.clone()
    }

    /// Compile an expression (unit literals allowed) that may call script functions.
    pub fn compile_expr(&self, src: &str) -> Result<AST> {
        let ast = self
            .engine
            .compile(preprocess(src, self.binary))
            .map_err(|e| anyhow!("{e}"))?;
        Ok(self.ast.clone_functions_only().merge(&ast))
    }

    /// Evaluate a compiled expression with the given variables, expecting a number.
    pub fn eval_number(&self, ast: &AST, vars: &[(String, f64)]) -> Result<f64> {
        let mut scope = Scope::new();
        for (k, v) in vars {
            scope.push(k.as_str(), *v);
        }
        let d: Dynamic = self
            .engine
            .eval_ast_with_scope(&mut scope, ast)
            .map_err(|e| anyhow!("{e}"))?;
        dyn_to_f64(&d)
            .ok_or_else(|| anyhow!("expression returned {} instead of a number", d.type_name()))
    }

    /// Scope with each series' latest (in-view) value bound to its identifier.
    fn series_scope(&self) -> Scope<'static> {
        let st = self.state.borrow();
        let mut scope = Scope::new();
        for s in &st.data.series {
            if let Some(&(_, y)) = s.in_view(st.view).last() {
                scope.push(ident(&s.name), y);
            }
        }
        scope
    }

    /// Evaluate console input. Series names are variables holding their latest value.
    pub fn eval(&self, src: &str) -> Result<Dynamic> {
        let ast = self.compile_expr(src)?;
        let mut scope = self.series_scope();
        self.engine
            .eval_ast_with_scope(&mut scope, &ast)
            .map_err(|e| anyhow!("{e}"))
    }

    /// Evaluate an alert condition.
    pub fn eval_bool(&self, src: &str) -> Result<bool> {
        let d = self.eval(src)?;
        d.as_bool()
            .map_err(|t| anyhow!("alert '{src}' returned {t}, expected true/false"))
    }

    /// Call the script's `parse(line)` function.
    pub fn parse_line(&self, line: &str) -> Result<Option<Vec<(String, ScriptVal)>>> {
        let opts = CallFnOptions::new().eval_ast(false);
        let d: Dynamic = self
            .engine
            .call_fn_with_options(
                opts,
                &mut Scope::new(),
                &self.ast,
                "parse",
                (line.to_string(),),
            )
            .map_err(|e| anyhow!("parse(): {e}"))?;
        if d.is_unit() {
            return Ok(None);
        }
        let map = d
            .try_cast::<Map>()
            .ok_or_else(|| anyhow!("parse() must return a map or ()"))?;
        let mut out = Vec::new();
        flatten_map(&map, "", &mut out);
        Ok(Some(out))
    }
}

fn flatten_map(map: &Map, prefix: &str, out: &mut Vec<(String, ScriptVal)>) {
    for (k, v) in map {
        let key = if prefix.is_empty() {
            k.to_string()
        } else {
            format!("{prefix}.{k}")
        };
        if let Some(n) = dyn_to_f64(v) {
            out.push((key, ScriptVal::Num(n)));
        } else if let Ok(b) = v.as_bool() {
            out.push((key, ScriptVal::Num(f64::from(u8::from(b)))));
        } else if let Some(inner) = v.read_lock::<Map>() {
            flatten_map(&inner, &key, out);
        } else if !v.is_unit() {
            out.push((key, ScriptVal::Str(v.to_string())));
        }
    }
}

type State = Rc<RefCell<ScriptState>>;
type StatFn = fn(&Stats) -> f64;

/// A series' in-view points with its unit.
struct ViewPoints {
    points: Vec<(f64, f64)>,
    unit: Unit,
}

/// Points of a series within the current view.
fn points(state: &State, name: &str) -> FnResult<ViewPoints> {
    let st = state.borrow();
    let s = st.data.get(name).ok_or_else(|| {
        let names: Vec<&str> = st.data.series.iter().map(|s| s.name.as_str()).collect();
        format!("unknown series '{name}' (have: {})", names.join(", "))
    })?;
    Ok(ViewPoints {
        points: s.in_view(st.view).to_vec(),
        unit: s.unit.clone(),
    })
}

fn stats(state: &State, name: &str) -> FnResult<Stats> {
    let pts = points(state, name)?.points;
    Ok(Stats::compute(&pts).ok_or_else(|| format!("series '{name}' has no points in view"))?)
}

fn register_api(engine: &mut Engine, state: &State, binary: bool) {
    {
        let st = state.clone();
        engine.on_print(move |s| st.borrow_mut().output.push(s.to_string()));
        let st = state.clone();
        engine.on_debug(move |s, _, _| st.borrow_mut().output.push(s.to_string()));
    }

    let st = state.clone();
    engine.register_fn("series", move || -> Array {
        st.borrow()
            .data
            .series
            .iter()
            .map(|s| Dynamic::from(s.name.clone()))
            .collect()
    });

    let stat_fns: [(&str, StatFn); 15] = [
        ("first", |s| s.first),
        ("last", |s| s.last),
        ("min", |s| s.min),
        ("max", |s| s.max),
        ("mean", |s| s.mean),
        ("avg", |s| s.mean),
        ("median", |s| s.median),
        ("stddev", |s| s.stddev),
        ("p95", |s| s.p95),
        ("sum", |s| s.sum),
        ("r2", |s| s.r2),
        ("slope", |s| s.slope),
        ("change", |s| s.last - s.first),
        ("pct_change", |s| {
            if s.first == 0.0 {
                f64::NAN
            } else {
                (s.last - s.first) / s.first.abs() * 100.0
            }
        }),
        ("span", |s| s.x_last - s.x_first),
    ];
    for (fname, f) in stat_fns {
        let st = state.clone();
        engine.register_fn(fname, move |name: &str| -> FnResult<f64> {
            stats(&st, name).map(|s| f(&s))
        });
    }

    let st = state.clone();
    engine.register_fn("count", move |name: &str| -> FnResult<i64> {
        Ok(points(&st, name)?.points.len() as i64)
    });

    let st = state.clone();
    engine.register_fn("percentile", move |name: &str, q: f64| -> FnResult<f64> {
        let mut ys: Vec<f64> = points(&st, name)?.points.iter().map(|p| p.1).collect();
        ys.sort_by(f64::total_cmp);
        Ok(crate::stats::percentile(&ys, q / 100.0))
    });

    // slope per duration: slope("node1", "1d")
    let st = state.clone();
    engine.register_fn("slope", move |name: &str, per: &str| -> FnResult<f64> {
        let per = crate::time::parse_duration_secs(per).map_err(|e| e.to_string())?;
        Ok(stats(&st, name)?.slope * per)
    });

    // eta(name, target) -> seconds until the trend reaches target, or ()
    let st = state.clone();
    engine.register_fn(
        "eta",
        move |name: &str, target: Dynamic| -> FnResult<Dynamic> {
            let target = dyn_to_f64(&target).ok_or("eta target must be a number")?;
            Ok(stats(&st, name)?
                .eta(target)
                .map_or(Dynamic::UNIT, Dynamic::from_float))
        },
    );

    // forecast(name, ahead) -> trend value `ahead` seconds after the last point
    let st = state.clone();
    engine.register_fn(
        "forecast",
        move |name: &str, ahead: Dynamic| -> FnResult<f64> {
            let ahead = dyn_to_f64(&ahead).ok_or("forecast offset must be a number")?;
            let s = stats(&st, name)?;
            Ok(s.trend_at(s.x_last + ahead))
        },
    );

    for (fname, take_x) in [("values", false), ("xs", true)] {
        let st = state.clone();
        engine.register_fn(fname, move |name: &str| -> FnResult<Array> {
            Ok(points(&st, name)?
                .points
                .iter()
                .map(|p| Dynamic::from_float(if take_x { p.0 } else { p.1 }))
                .collect())
        });
    }

    // fmt(name, value) -> value formatted in the series' unit
    let st = state.clone();
    engine.register_fn("fmt", move |name: &str, v: Dynamic| -> FnResult<String> {
        let v = dyn_to_f64(&v).ok_or("fmt value must be a number")?;
        Ok(points(&st, name)?.unit.format(v))
    });
    engine.register_fn("fmt_dur", |secs: Dynamic| -> FnResult<String> {
        Ok(crate::time::fmt_duration(
            dyn_to_f64(&secs).ok_or("fmt_dur needs a number")?,
        ))
    });
    engine.register_fn("fmt_time", |x: Dynamic| -> FnResult<String> {
        Ok(crate::time::format_time_full(
            dyn_to_f64(&x).ok_or("fmt_time needs a number")?,
        ))
    });
    engine.register_fn("bytes", move |v: Dynamic| -> FnResult<String> {
        Ok(Unit::new(crate::units::Dim::Bytes { binary })
            .format(dyn_to_f64(&v).ok_or("bytes needs a number")?))
    });

    // Viewer actions.
    let push = |state: &State, a: Action| state.borrow_mut().actions.push(a);
    let st = state.clone();
    engine.register_fn("hline", move |v: Dynamic| -> FnResult<()> {
        push(
            &st,
            Action::HLine(dyn_to_f64(&v).ok_or("hline needs a number")?, None),
        );
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("hline", move |v: Dynamic, label: &str| -> FnResult<()> {
        push(
            &st,
            Action::HLine(
                dyn_to_f64(&v).ok_or("hline needs a number")?,
                Some(label.to_string()),
            ),
        );
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("clear_hlines", move || push(&st, Action::ClearHLines));
    let st = state.clone();
    engine.register_fn("zoom", move |d: Dynamic| -> FnResult<()> {
        let secs = match d.clone().into_immutable_string() {
            Ok(s) => crate::time::parse_duration_secs(&s).map_err(|e| e.to_string())?,
            Err(_) => dyn_to_f64(&d).ok_or("zoom needs a duration like \"6h\" or a number")?,
        };
        push(&st, Action::Zoom(secs));
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("reset", move || push(&st, Action::ResetView));
    let st = state.clone();
    engine.register_fn("show", move |n: &str| push(&st, Action::Show(n.into())));
    let st = state.clone();
    engine.register_fn("hide", move |n: &str| push(&st, Action::Hide(n.into())));
    let st = state.clone();
    engine.register_fn("chart", move |k: &str| push(&st, Action::Chart(k.into())));
    let st = state.clone();
    engine.register_fn("target", move |n: &str, v: Dynamic| -> FnResult<()> {
        push(
            &st,
            Action::Target(
                n.into(),
                Some(dyn_to_f64(&v).ok_or("target needs a number, e.g. target(\"node1\", 50GB)")?),
            ),
        );
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("trend", move |t: &str| {
        push(
            &st,
            Action::Trend((!matches!(t, "off" | "none" | "false")).then(|| t.to_string())),
        );
    });
    let st = state.clone();
    engine.register_fn("raw", move |on: bool| push(&st, Action::Raw(on)));
    let st = state.clone();
    engine.register_fn("side_by_side", move |on: bool| {
        push(&st, Action::SideBySide(on))
    });
    let st = state.clone();
    engine.register_fn("range", move |r: &str| {
        push(
            &st,
            Action::Range((!matches!(r, "all" | "off" | "none")).then(|| r.to_string())),
        );
    });
    let st = state.clone();
    engine.register_fn("clear_target", move |n: &str| {
        push(&st, Action::Target(n.into(), None))
    });
    let st = state.clone();
    engine.register_fn("forecast", move |h: &str| {
        push(
            &st,
            Action::Forecast((!matches!(h, "off" | "none" | "false")).then(|| h.to_string())),
        );
    });
    let st = state.clone();
    engine.register_fn("forecast", move |on: bool| {
        push(&st, Action::Forecast(on.then(|| "auto".to_string())))
    });
    let st = state.clone();
    engine.register_fn(
        "bind",
        move |key: ImmutableString, code: ImmutableString| {
            st.borrow_mut()
                .binds
                .push((key.to_string(), code.to_string()));
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Series, XKind};
    use crate::units::Dim;

    fn host_with_data() -> ScriptHost {
        let host = ScriptHost::new(None, false).unwrap();
        let unit = Unit::new(Dim::Bytes { binary: false });
        let s = Series {
            name: "node1".into(),
            unit,
            points: vec![(0.0, 60e9), (86400.0, 58e9), (172_800.0, 56e9)],
        };
        host.set_data(
            Dataset {
                x_kind: XKind::Time,
                series: vec![s],
            },
            None,
        );
        host
    }

    #[test]
    fn preprocess_unit_literals() {
        assert_eq!(preprocess("node1 < 50GB", false), "node1 < 50000000000.0");
        assert_eq!(preprocess("x * 1d + 2 h", false), "x * 86400.0 + 2 h");
        assert_eq!(preprocess("t > 1.5s", false), "t > 1.5");
        assert_eq!(
            preprocess(r#"zoom("6h") + node1"#, false),
            r#"zoom("6h") + node1"#
        );
        assert_eq!(preprocess("1G", true), "1073741824.0");
    }

    #[test]
    fn idents() {
        assert_eq!(ident("cpu.user"), "cpu_user");
        assert_eq!(ident("Active(anon)"), "Active_anon_");
        assert_eq!(ident("1min"), "_1min");
        assert_eq!(ident("in"), "in_");
    }

    #[test]
    fn api_functions() {
        let h = host_with_data();
        assert_eq!(h.eval("last(\"node1\")").unwrap().as_float().unwrap(), 56e9);
        assert!(h.eval_bool("node1 < 57GB").unwrap());
        assert!(h.eval_bool("slope(\"node1\") * 1d < -1.9GB").unwrap());
        assert_eq!(
            h.eval("fmt(\"node1\", slope(\"node1\", \"1d\"))")
                .unwrap()
                .to_string(),
            "-2 GB"
        );
        // 56 GB at 2 GB/day → 28 days.
        assert_eq!(
            h.eval("fmt_dur(eta(\"node1\", 0))").unwrap().to_string(),
            "28 days"
        );
        assert!(h.eval("last(\"nope\")").is_err());
        assert_eq!(h.eval("count(\"node1\") + 1").unwrap().as_int().unwrap(), 4);
    }

    #[test]
    fn actions_and_print() {
        let h = host_with_data();
        assert!(
            h.eval("hline(50GB, \"limit\"); zoom(\"6h\"); print(\"hi\")")
                .unwrap()
                .is_unit()
        );
        assert_eq!(
            h.take_actions(),
            vec![
                Action::HLine(50e9, Some("limit".into())),
                Action::Zoom(21600.0)
            ]
        );
        assert_eq!(h.take_output(), vec!["hi"]);
    }

    #[test]
    fn derived_expression() {
        let h = ScriptHost::new(None, false).unwrap();
        let ast = h.compile_expr("a - b / 2").unwrap();
        assert_eq!(
            h.eval_number(&ast, &[("a".into(), 10.0), ("b".into(), 4.0)])
                .unwrap(),
            8.0
        );
    }
}
