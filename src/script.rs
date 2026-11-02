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
    /// Trend lines: a comma list ("avg,median,p95", "line"), or `None` for off.
    Trend(Option<String>),
    /// Every line to show, including "live": "live,avg"; with a second list, the
    /// right-hand chart of side by side (which turns it on).
    Lines(String, Option<String>),
    /// Points in the rolling window (`None` = automatic).
    TrendWindow(Option<usize>),
    /// Timeframe as a number: seconds on a time axis, else a point count.
    RangeNumber(f64),
    /// Timeframe between two times (`until` open-ended when `None`).
    Between(String, Option<String>),
    /// An on/off toolbar button: `Some` sets it, `None` toggles.
    Flag(Flag, Option<bool>),
    /// Move the view by a fraction of its width (negative = left).
    Pan(f64),
    /// Zoom in (`true`) or out one step.
    ZoomStep(bool),
    /// Write the view to an SVG (default name when `None`).
    Svg(Option<String>),
    Help,
    Quit,
    Pane(PaneAction),
    /// Save the current view (source, format, panes, lines…) as a named profile.
    SaveProfile(String),
    /// Put text (default: the view's command line) on the clipboard.
    Copy(Option<String>),
    /// A marker here (at the cursor, or the newest data) with this label.
    MarkHere(String),
    /// Remove every marker and marker rule.
    ClearMarks,
    Undo,
    Redo,
    /// Add (or replace) a merged source: `name = args…` or a profile name.
    Merge(String),
    Unmerge(String),
    /// Load a Rhai script file while running.
    Load(String),
    /// Add (or, when editing, replace) an alert condition.
    Alert(String),
    Unalert(String),
    /// Add / remove a notification target (`bell`, `ntfy:topic`…).
    NotifyTo(String),
    NotifyOff(String),
    /// Show only these series (hide the rest).
    Only(Vec<String>),
    Toggle(String),
    ShowAll,
    /// The chart title.
    Title(String),
    /// The unit for values that don't carry one (`USD`, `ms`, `%`…); "" for none.
    Unit(String),
    /// A custom line: name (optionally `name:unit`) and Rhai expression.
    AddLine(String, String),
    RemoveLine(String),
    /// Open the builder to change the format or source.
    Format,
}

/// Pane commands from the console / scripts.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneAction {
    /// Split the focused pane: `true` = below, `false` = to the right.
    Split(bool),
    Close,
    /// "left"/"right"/"up"/"down", or a pane number.
    Focus(String),
    Move(String),
    /// Direction and step (a share of the split, e.g. 0.1).
    Resize(String, f64),
    Even,
    /// Back to one chart.
    Single,
    /// Rearrange: `1 | 2 / 3`.
    Layout(String),
    /// Set the focused pane: `node1,node2 kind=bar lines=live,avg`.
    Set(String),
    /// Where the focused pane's data comes from: a preset/profile/recent name, or
    /// "" / "main" for the main chart.
    Source(String),
}

/// Toolbar buttons that are on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Live,
    SideBySide,
    Stats,
    Split,
    Log,
    Targets,
    Pause,
    Follow,
    Cursor,
    Mouse,
    Measure,
}

/// Script names of the on/off buttons. Each takes an optional bool (none = toggle).
pub const FLAGS: &[(&str, Flag)] = &[
    ("live", Flag::Live),
    ("raw", Flag::Live),
    ("side_by_side", Flag::SideBySide),
    ("stats", Flag::Stats),
    ("split", Flag::Split),
    ("log", Flag::Log),
    ("targets", Flag::Targets),
    ("pause", Flag::Pause),
    ("follow", Flag::Follow),
    ("cursor", Flag::Cursor),
    ("mouse", Flag::Mouse),
    ("measure", Flag::Measure),
];

/// State shared between the host and registered functions.
#[derive(Debug, Default)]
pub struct ScriptState {
    pub data: Dataset,
    pub view: Option<(f64, f64)>,
    pub actions: Vec<Action>,
    pub binds: Vec<(String, String)>,
    pub output: Vec<String>,
    /// Marker rules added with `mark_when` (picked up by the viewer).
    pub mark_rules: Vec<crate::marks::MarkRule>,
}

pub struct ScriptHost {
    engine: Engine,
    /// The user's --script (empty if none).
    ast: RefCell<AST>,
    state: Rc<RefCell<ScriptState>>,
    binary: bool,
}

/// Turn a series name into a valid Rhai identifier (`cpu.user` → `cpu_user`).
pub fn ident(name: &str) -> String {
    const RESERVED: &[&str] = &[
        "if",
        "else",
        "let",
        "const",
        "fn",
        "in",
        "for",
        "while",
        "loop",
        "do",
        "until",
        "return",
        "true",
        "false",
        "this",
        "switch",
        "import",
        "export",
        "as",
        "private",
        "break",
        "continue",
        "throw",
        "try",
        "catch",
        "global",
        "Fn",
        "call",
        "curry",
        "type_of",
        "print",
        "debug",
        "eval",
        // Reserved by Rhai for the future: also not usable as names.
        "var",
        "static",
        "shared",
        "goto",
        "exit",
        "match",
        "case",
        "public",
        "protected",
        "new",
        "use",
        "with",
        "module",
        "package",
        "super",
        "thread",
        "spawn",
        "go",
        "await",
        "async",
        "sync",
        "yield",
        "default",
        "void",
        "null",
        "nil",
        "is",
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
        r"|(?P<short>[KMGTPBshdw]))\b",
        r"|\b(?P<pn>\d+(?:\.\d+)?)%(?P<pct>)"
    ))
    .expect("valid unit literal regex")
});

/// Rewrite unit literals (`50GB`, `1.5 h`, `200ms`, `1d`) into plain numbers in base
/// units, leaving string literals untouched.
pub fn preprocess(src: &str, binary: bool) -> String {
    let src = &quote_bind_code(src);
    let mut out = String::with_capacity(src.len());
    let mut rest = src.as_str();
    while !rest.is_empty() {
        // Copy code up to the next string literal or comment, rewriting it; then copy
        // the literal or comment as it is (an apostrophe in `// we're` isn't a quote).
        let code_end = rest
            .char_indices()
            .find(|&(i, c)| {
                matches!(c, '"' | '`' | '\'')
                    || rest[i..].starts_with("//")
                    || rest[i..].starts_with("/*")
            })
            .map_or(rest.len(), |(i, _)| i);
        out.push_str(&rewrite_units(&rest[..code_end], binary));
        rest = &rest[code_end..];
        if rest.starts_with("//") {
            let end = rest.find('\n').unwrap_or(rest.len());
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        if rest.starts_with("/*") {
            let end = rest.find("*/").map_or(rest.len(), |i| i + 2);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
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

/// `bind("r", only("eth"))` → `bind("r", "only(\"eth\")")`: the code a key runs is
/// written as code, and kept as text to run when the key is pressed. (A string, a
/// `|| …` closure or a `{ … }` block work too.)
fn quote_bind_code(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        match b[i] {
            // Skip strings and comments: `bind(` inside them isn't a call.
            q @ (b'"' | b'`' | b'\'') => i = skip_string(b, i, q),
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = src[i..].find("*/").map_or(b.len(), |e| i + e + 2);
            }
            b'b' if src[i..].starts_with("bind")
                && (i == 0 || !is_ident(b[i - 1]))
                && src[i + 4..].trim_start().starts_with('(') =>
            {
                let open = i + 4 + (src[i + 4..].len() - src[i + 4..].trim_start().len());
                // The first argument ends at a top-level comma; the second at the
                // matching close paren.
                let Some(comma) = scan_to(b, open + 1, b',') else {
                    i += 4;
                    continue;
                };
                let Some(close) = scan_to(b, comma + 1, b')') else {
                    i += 4;
                    continue;
                };
                let arg = src[comma + 1..close].trim();
                let quoted = arg.starts_with('"') || arg.starts_with('`');
                if !quoted && !arg.is_empty() {
                    let code = arg.strip_prefix("||").unwrap_or(arg).trim();
                    out.push_str(&src[copied..=comma]);
                    out.push_str(" \"");
                    out.push_str(
                        &code
                            .replace('\\', "\\\\")
                            .replace('"', "\\\"")
                            .replace('\n', "\\n")
                            .replace('\r', "\\r")
                            .replace('\t', "\\t"),
                    );
                    out.push('"');
                    copied = close;
                }
                i = close;
            }
            _ => i += 1,
        }
    }
    out.push_str(&src[copied..]);
    out
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Index just past the string starting at `i` (quote `q`), honouring escapes.
fn skip_string(b: &[u8], mut i: usize, q: u8) -> usize {
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == q => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// The next `target` at nesting depth 0 from `i` (skipping strings and brackets).
fn scan_to(b: &[u8], mut i: usize, target: u8) -> Option<usize> {
    let mut depth = 0i32;
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'`' | b'\'') => {
                i = skip_string(b, i, q);
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            c @ (b')' | b']' | b'}') => {
                if depth == 0 {
                    return (c == target).then_some(i);
                }
                depth -= 1;
            }
            c if c == target && depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn rewrite_units(code: &str, binary: bool) -> String {
    UNIT_LITERAL
        .replace_all(code, |c: &regex::Captures| {
            // `90%` is a percentage — unless an operand follows (`10 % 3`, remainder).
            if let Some(pn) = c.name("pn") {
                let whole = c.get(0).expect("match");
                let next = code[whole.end()..].trim_start().chars().next();
                if next.is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '(' | '_' | '"')) {
                    return whole.as_str().to_string();
                }
                let s = pn.as_str().to_string();
                return if s.contains('.') { s } else { format!("{s}.0") };
            }
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
        // Room for real indicator code (the defaults are tuned for one-liners).
        engine.set_max_expr_depths(256, 256);
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
            ast: RefCell::new(ast),
            state,
            binary,
        })
    }

    /// Load another script while running: its functions join the ones already
    /// loaded, and its top-level code runs (key bindings, `mark_when` rules…).
    pub fn load(&self, path: &Path) -> Result<usize> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("reading script {}", path.display()))?;
        let ast = self
            .engine
            .compile(preprocess(&src, self.binary))
            .map_err(|e| anyhow!("{}: {e}", path.display()))?;
        // Named functions (closures are compiled to anonymous ones).
        let functions = ast
            .iter_functions()
            .filter(|f| !f.name.starts_with("anon$"))
            .count();
        let merged = self.ast.borrow().merge(&ast);
        self.engine
            .run_ast(&merged.clone_functions_only().merge(&ast))
            .map_err(|e| anyhow!("{}: {e}", path.display()))?;
        *self.ast.borrow_mut() = merged.clone_functions_only();
        Ok(functions)
    }

    pub fn has_fn(&self, name: &str) -> bool {
        self.ast.borrow().iter_functions().any(|f| f.name == name)
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

    /// Marker rules added since last asked (`mark_when` in the script or console).
    pub fn take_mark_rules(&self) -> Vec<crate::marks::MarkRule> {
        std::mem::take(&mut self.state.borrow_mut().mark_rules)
    }

    /// Whether `mark_when` added rules that haven't been picked up yet.
    pub fn has_new_mark_rules(&self) -> bool {
        !self.state.borrow().mark_rules.is_empty()
    }

    /// Evaluate a compiled expression with variables already in `scope`.
    pub fn eval_ast_with_scope(&self, scope: &mut Scope, ast: &AST) -> Result<Dynamic> {
        self.engine
            .eval_ast_with_scope(scope, ast)
            .map_err(|e| anyhow!("{e}"))
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
        Ok(self.ast.borrow().clone_functions_only().merge(&ast))
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
                &self.ast.borrow(),
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

    // Array helpers for custom lines.
    let floats = |a: &Array| -> Vec<f64> { a.iter().filter_map(dyn_to_f64).collect() };
    let to_array = |v: Vec<f64>| -> Array { v.into_iter().map(Dynamic::from_float).collect() };
    engine.register_fn("ema", move |a: Array, n: i64| -> FnResult<Array> {
        if n < 1 {
            return Err("ema(values, n): n must be at least 1".into());
        }
        Ok(to_array(crate::lines::ema(&floats(&a), n as usize)))
    });
    engine.register_fn("sma", move |a: Array, n: i64| -> FnResult<Array> {
        if n < 1 {
            return Err("sma(values, n): n must be at least 1".into());
        }
        Ok(to_array(crate::lines::sma(&floats(&a), n as usize)))
    });
    let st = state.clone();
    engine.register_fn("mean_of", move |names: Array| -> FnResult<Array> {
        // The average of several series, on the first one's times (each where it has data).
        let names: Vec<String> = names.iter().map(|d| d.to_string()).collect();
        if names.is_empty() {
            return Err("mean_of([\"a\", \"b\"]) needs at least one series".into());
        }
        // Sources that haven't sent anything yet are left out (until they do).
        let others: Vec<Vec<(f64, f64)>> = names
            .iter()
            .filter_map(|n| points(&st, n).ok().map(|p| p.points))
            .filter(|p| !p.is_empty())
            .collect();
        let Some(base) = others.first().cloned() else {
            // None at all: say which names are unknown.
            points(&st, &names[0])?;
            return Ok(Array::new());
        };
        Ok(base
            .iter()
            .map(|&(x, _)| {
                let vals: Vec<f64> = others
                    .iter()
                    .filter_map(|p| crate::render::value_near(p, x))
                    .collect();
                if vals.is_empty() {
                    Dynamic::UNIT
                } else {
                    Dynamic::from_float(vals.iter().sum::<f64>() / vals.len() as f64)
                }
            })
            .collect())
    });
    let st = state.clone();
    engine.register_fn(
        "values_at",
        move |name: &str, xs: Array| -> FnResult<Array> {
            // Another series' values at these times (interpolated; () outside its range).
            let pts = points(&st, name)?.points;
            Ok(xs
                .iter()
                .map(|x| {
                    dyn_to_f64(x)
                        .and_then(|x| crate::render::value_near(&pts, x))
                        .map_or(Dynamic::UNIT, Dynamic::from_float)
                })
                .collect())
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
    engine.register_fn("toggle", move |n: &str| push(&st, Action::Toggle(n.into())));
    let st = state.clone();
    engine.register_fn("show_all", move || push(&st, Action::ShowAll));
    // only("eth"), only("eth", "ema30"), only(["eth", "ema30"])…
    let st = state.clone();
    engine.register_fn("only", move |names: Array| {
        push(
            &st,
            Action::Only(names.iter().map(|d| d.to_string()).collect()),
        )
    });
    let st = state.clone();
    engine.register_fn("only", move |a: &str| {
        push(&st, Action::Only(vec![a.into()]))
    });
    let st = state.clone();
    engine.register_fn("only", move |a: &str, b: &str| {
        push(&st, Action::Only(vec![a.into(), b.into()]))
    });
    let st = state.clone();
    engine.register_fn("only", move |a: &str, b: &str, c: &str| {
        push(&st, Action::Only(vec![a.into(), b.into(), c.into()]))
    });
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
    for &(name, flag) in FLAGS {
        let st = state.clone();
        engine.register_fn(name, move |on: bool| {
            push(&st, Action::Flag(flag, Some(on)))
        });
        let st = state.clone();
        engine.register_fn(name, move || push(&st, Action::Flag(flag, None)));
    }
    let st = state.clone();
    engine.register_fn("lines", move |l: &str| {
        push(&st, Action::Lines(l.to_string(), None))
    });
    let st = state.clone();
    engine.register_fn("lines", move |l: &str, r: &str| {
        push(&st, Action::Lines(l.to_string(), Some(r.to_string())))
    });
    let st = state.clone();
    engine.register_fn("window", move |w: Dynamic| -> FnResult<()> {
        let n = match w.clone().into_immutable_string() {
            Ok(s) if s == "auto" => None,
            Ok(s) => Some(
                s.trim()
                    .parse::<usize>()
                    .map_err(|_| "window needs a number of points, or \"auto\"")?,
            ),
            Err(_) => Some(
                dyn_to_f64(&w)
                    .ok_or("window needs a number of points, or \"auto\"")?
                    .round()
                    .max(1.0) as usize,
            ),
        };
        push(&st, Action::TrendWindow(n));
        Ok(())
    });
    // `timeframe` is the toolbar button's name; `range` the original one.
    for name in ["range", "timeframe"] {
        let st = state.clone();
        engine.register_fn(name, move |r: Dynamic| -> FnResult<()> {
            match r.clone().into_immutable_string() {
                Ok(s) => push(
                    &st,
                    Action::Range(
                        (!matches!(s.as_str(), "all" | "off" | "none")).then(|| s.to_string()),
                    ),
                ),
                Err(_) => push(
                    &st,
                    Action::RangeNumber(
                        dyn_to_f64(&r).ok_or("timeframe needs e.g. 1h, \"7d\", 500 or \"all\"")?,
                    ),
                ),
            }
            Ok(())
        });
        let st = state.clone();
        engine.register_fn(name, move |a: &str, b: &str| {
            push(
                &st,
                Action::Between(a.to_string(), (!b.is_empty()).then(|| b.to_string())),
            )
        });
    }
    let st = state.clone();
    engine.register_fn("pan", move |by: Dynamic| -> FnResult<()> {
        let f = match by.clone().into_immutable_string() {
            Ok(s) if s == "left" => -0.2,
            Ok(s) if s == "right" => 0.2,
            Ok(_) => return Err("pan needs \"left\", \"right\" or a fraction of the view (-0.5 = half a screen left)".into()),
            Err(_) => dyn_to_f64(&by).ok_or("pan needs \"left\" or \"right\"")?,
        };
        push(&st, Action::Pan(f));
        Ok(())
    });
    let pane = |st: &State, a: PaneAction| st.borrow_mut().actions.push(Action::Pane(a));
    let st = state.clone();
    engine.register_fn("split_right", move || pane(&st, PaneAction::Split(false)));
    let st = state.clone();
    engine.register_fn("split_down", move || pane(&st, PaneAction::Split(true)));
    let st = state.clone();
    engine.register_fn("close_pane", move || pane(&st, PaneAction::Close));
    let st = state.clone();
    engine.register_fn("focus_pane", move |to: Dynamic| -> FnResult<()> {
        let to = match to.clone().into_immutable_string() {
            Ok(s) => s.to_string(),
            Err(_) => to
                .as_int()
                .map_err(
                    |_| "focus_pane needs \"left\", \"right\", \"up\", \"down\" or a pane number",
                )?
                .to_string(),
        };
        pane(&st, PaneAction::Focus(to));
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("move_pane", move |d: &str| {
        pane(&st, PaneAction::Move(d.into()))
    });
    let st = state.clone();
    engine.register_fn("resize_pane", move |d: &str| {
        pane(&st, PaneAction::Resize(d.into(), 0.05))
    });
    let st = state.clone();
    engine.register_fn("resize_pane", move |d: &str, by: Dynamic| -> FnResult<()> {
        let v = dyn_to_f64(&by).ok_or("resize_pane(\"right\", 10) moves the edge 10%")?;
        pane(&st, PaneAction::Resize(d.into(), v / 100.0));
        Ok(())
    });
    let st = state.clone();
    engine.register_fn("even_panes", move || pane(&st, PaneAction::Even));
    let st = state.clone();
    engine.register_fn("single", move || pane(&st, PaneAction::Single));
    let st = state.clone();
    engine.register_fn("layout", move |l: &str| {
        pane(&st, PaneAction::Layout(l.into()))
    });
    let st = state.clone();
    engine.register_fn("pane", move |spec: &str| {
        pane(&st, PaneAction::Set(spec.into()))
    });
    let st = state.clone();
    engine.register_fn("pane_source", move |src: &str| {
        pane(&st, PaneAction::Source(src.into()))
    });
    let st = state.clone();
    engine.register_fn("zoom_in", move || push(&st, Action::ZoomStep(true)));
    let st = state.clone();
    engine.register_fn("zoom_out", move || push(&st, Action::ZoomStep(false)));
    let st = state.clone();
    engine.register_fn("svg", move || push(&st, Action::Svg(None)));
    let st = state.clone();
    engine.register_fn("svg", move |path: &str| {
        push(&st, Action::Svg(Some(path.to_string())))
    });
    let st = state.clone();
    engine.register_fn("save", move |name: &str| {
        push(&st, Action::SaveProfile(name.into()))
    });
    // mark_when(expr, label, true): also notify for each new marker.
    let st = state.clone();
    engine.register_fn(
        "mark_when",
        move |expr: &str, label: &str, notify: bool| -> FnResult<()> {
            let mut rule = crate::marks::MarkRule::parse(&format!("{expr} => {label}"))?;
            rule.notify = notify;
            st.borrow_mut().mark_rules.push(rule);
            Ok(())
        },
    );
    for with_label in [false, true] {
        let st = state.clone();
        let f = move |expr: &str, label: &str| -> FnResult<()> {
            let spec = if with_label {
                format!("{expr} => {label}")
            } else {
                expr.to_string()
            };
            let rule = crate::marks::MarkRule::parse(&spec)?;
            st.borrow_mut().mark_rules.push(rule);
            Ok(())
        };
        if with_label {
            engine.register_fn("mark_when", f);
        } else {
            engine.register_fn("mark_when", move |expr: &str| f(expr, ""));
        }
    }
    let st = state.clone();
    engine.register_fn("mark", move |label: &str| {
        push(&st, Action::MarkHere(label.into()))
    });
    let st = state.clone();
    engine.register_fn("clear_marks", move || push(&st, Action::ClearMarks));
    let st = state.clone();
    engine.register_fn("merge", move |spec: &str| {
        push(&st, Action::Merge(spec.into()))
    });
    let st = state.clone();
    engine.register_fn("unmerge", move |name: &str| {
        push(&st, Action::Unmerge(name.into()))
    });
    let st = state.clone();
    engine.register_fn("alert", move |e: &str| push(&st, Action::Alert(e.into())));
    let st = state.clone();
    engine.register_fn("unalert", move |e: &str| {
        push(&st, Action::Unalert(e.into()))
    });
    let st = state.clone();
    engine.register_fn("notify_to", move |t: &str| {
        push(&st, Action::NotifyTo(t.into()))
    });
    let st = state.clone();
    engine.register_fn("notify_off", move |t: &str| {
        push(&st, Action::NotifyOff(t.into()))
    });
    let st = state.clone();
    engine.register_fn("load", move |path: &str| {
        push(&st, Action::Load(path.into()))
    });
    let st = state.clone();
    engine.register_fn("title", move |t: &str| push(&st, Action::Title(t.into())));
    let st = state.clone();
    engine.register_fn("unit", move |u: &str| push(&st, Action::Unit(u.into())));
    let st = state.clone();
    engine.register_fn("add_line", move |name: &str, expr: &str| {
        push(&st, Action::AddLine(name.into(), expr.into()))
    });
    let st = state.clone();
    engine.register_fn("remove_line", move |name: &str| {
        push(&st, Action::RemoveLine(name.into()))
    });
    let st = state.clone();
    engine.register_fn("undo", move || push(&st, Action::Undo));
    let st = state.clone();
    engine.register_fn("redo", move || push(&st, Action::Redo));
    let st = state.clone();
    engine.register_fn("copy", move || push(&st, Action::Copy(None)));
    let st = state.clone();
    engine.register_fn("copy", move |t: Dynamic| {
        push(&st, Action::Copy(Some(t.to_string())))
    });
    let st = state.clone();
    engine.register_fn("help", move || push(&st, Action::Help));
    let st = state.clone();
    engine.register_fn("quit", move || push(&st, Action::Quit));
    let st = state.clone();
    engine.register_fn("format", move || push(&st, Action::Format));
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
    fn bind_takes_code() {
        assert_eq!(
            quote_bind_code(r#"bind("r", only("eth", "ema30"))"#),
            r#"bind("r", "only(\"eth\", \"ema30\")")"#
        );
        assert_eq!(
            quote_bind_code(r#"bind("r", "hide(\"x\")")"#),
            r#"bind("r", "hide(\"x\")")"#
        );
        assert_eq!(
            quote_bind_code(r#"bind("x", { hide("a"); zoom(6h) }); let y = 1;"#),
            r#"bind("x", "{ hide(\"a\"); zoom(6h) }"); let y = 1;"#
        );
        assert_eq!(
            quote_bind_code(r#"bind("c", || toggle("cb"))"#),
            r#"bind("c", "toggle(\"cb\")")"#
        );
        // Not calls: inside strings and comments, or other names.
        let s = r#"print("bind(\"r\", x)"); // bind("r", y)
rebind("a", b)"#;
        assert_eq!(quote_bind_code(s), s);
        // A bound key really runs it.
        let host = ScriptHost::new(None, false).unwrap();
        let _ = host.eval(r#"bind("r", hide("node1"))"#).unwrap();
        assert_eq!(
            host.binds(),
            [("r".to_string(), r#"hide("node1")"#.to_string())]
        );
        assert!(
            host.take_actions().is_empty(),
            "binding doesn't run the code"
        );
    }

    #[test]
    fn comments_percent_and_multiline_binds() {
        // An apostrophe in a comment doesn't hide the code after it from unit rewriting.
        assert_eq!(
            preprocess("// we're over\nx > 5GB", false),
            "// we're over\nx > 5000000000.0"
        );
        assert_eq!(
            preprocess("/* it's */ x > 1KB", false),
            "/* it's */ x > 1000.0"
        );
        // Percentages, but not the remainder operator.
        assert_eq!(preprocess("value > 90%", false), "value > 90.0");
        assert_eq!(preprocess("10 % 3 == 1", false), "10 % 3 == 1");
        assert_eq!(preprocess("(90%)", false), "(90.0)");
        // A bind() block over several lines.
        let host = ScriptHost::new(None, false).unwrap();
        let _ = host
            .eval("bind(\"x\", {\n  hide(\"a\");\n  zoom(6h)\n})")
            .unwrap();
        assert_eq!(host.binds()[0].1, "{\n  hide(\"a\");\n  zoom(6h)\n}");
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
