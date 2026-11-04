//! Formulas over series: a small language that discovery writes (and evolves), and
//! that charts can use (`formula("z(pct(price, 5) - pct(volume, 20), 50)")`).
//!
//! - a series by name (`price`, `"feed.0.bid"` quoted when it isn't a plain word), or
//!   a number;
//! - over the last n points: `lag diff pct mean ema std max min z pos rsi slope`
//!   (`pct` is the change in bp, `z` the distance from the mean in standard
//!   deviations, `pos` the place in the range 0–100);
//! - `abs neg sign log`, `a + b`, `a - b`, `a * b`, `a / b`, `gap(a, b)` (a relative
//!   to b, in bp);
//! - patterns: `motif(x, "++-")` (1 when the last moves went up, up, down; `=` is
//!   unchanged), `run(x)` (the streak: +3 after three rises, -2 after two falls),
//!   `since_high(x, n)` / `since_low(x, n)` (points since the n-point high / low),
//!   `corr(a, b, n)` (how the two moved together over n points, -1 to 1).
//!
//! Every value uses only points up to its own: no peeking ahead.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op1 {
    Abs,
    Neg,
    Sign,
    Log,
    /// The streak of moves the same way: +n after n rises, -n after n falls.
    Run,
}

/// An operation over the last n points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpW {
    Lag,
    Diff,
    Pct,
    Mean,
    Ema,
    Std,
    Max,
    Min,
    Z,
    Pos,
    Rsi,
    Slope,
    SinceHigh,
    SinceLow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op2 {
    Add,
    Sub,
    Mul,
    Div,
    Gap,
}

const OPS1: [(Op1, &str); 5] = [
    (Op1::Abs, "abs"),
    (Op1::Neg, "neg"),
    (Op1::Sign, "sign"),
    (Op1::Log, "log"),
    (Op1::Run, "run"),
];
const OPSW: [(OpW, &str); 14] = [
    (OpW::Lag, "lag"),
    (OpW::Diff, "diff"),
    (OpW::Pct, "pct"),
    (OpW::Mean, "mean"),
    (OpW::Ema, "ema"),
    (OpW::Std, "std"),
    (OpW::Max, "max"),
    (OpW::Min, "min"),
    (OpW::Z, "z"),
    (OpW::Pos, "pos"),
    (OpW::Rsi, "rsi"),
    (OpW::Slope, "slope"),
    (OpW::SinceHigh, "since_high"),
    (OpW::SinceLow, "since_low"),
];

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Col(String),
    Num(f64),
    F1(Op1, Box<Expr>),
    W(OpW, Box<Expr>, usize),
    F2(Op2, Box<Expr>, Box<Expr>),
    /// `corr(a, b, n)`.
    Corr(Box<Expr>, Box<Expr>, usize),
    /// `motif(x, "++-")`: the shape of the last moves.
    Motif(Box<Expr>, String),
}

/// What scale a formula's values are on: what can sensibly be added, compared or
/// divided. New formulas that mix scales (a price plus an RSI), divide by something
/// that crosses zero, or take a % change of something that isn't a level are never
/// tried: they can't mean anything, however well they happen to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    /// A level of a series (the price, its average): its own units.
    Level,
    /// A change or spread in a series' own units (diff, slope, std).
    Own,
    /// A relative change (bp: pct, gap).
    Bp,
    /// Unitless, around zero (z, corr, sign, a ratio of like things).
    Unitless,
    /// 0 to 100 (rsi, pos).
    Hundred,
    /// 0 to 1 (a motif, how often it came up).
    Fraction,
    /// A number of points (since_high, run).
    Points,
    Number,
}

/// A formula's scale, and whether it's always above zero (so it can be divided by,
/// or taken a % change or log of).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kind {
    pub scale: Scale,
    pub positive: bool,
}

impl Kind {
    fn of(scale: Scale, positive: bool) -> Option<Kind> {
        Some(Kind { scale, positive })
    }

    /// Unitless or bounded: it can scale anything without changing what it measures.
    fn pure(&self) -> bool {
        !matches!(self.scale, Scale::Level | Scale::Own)
    }
}

/// Where a formula gets its series: each lined up with the others, the same length.
pub trait Columns {
    fn column(&self, name: &str) -> Option<&[f64]>;
    fn rows(&self) -> usize;
}

impl Expr {
    pub fn w(op: OpW, e: Expr, n: usize) -> Expr {
        Expr::W(op, Box::new(e), n)
    }

    pub fn f2(op: Op2, a: Expr, b: Expr) -> Expr {
        Expr::F2(op, Box::new(a), Box::new(b))
    }

    pub fn col(name: &str) -> Expr {
        Expr::Col(name.to_string())
    }

    /// Its outermost operation (`z`, `+`, `motif`…).
    pub fn head(&self) -> String {
        match self {
            Expr::Col(_) => "series".into(),
            Expr::Num(_) => "number".into(),
            Expr::F1(op, _) => OPS1.iter().find(|o| o.0 == *op).expect("named").1.into(),
            Expr::W(op, _, _) => OPSW.iter().find(|o| o.0 == *op).expect("named").1.into(),
            Expr::F2(op, _, _) => format!("{op:?}"),
            Expr::Corr(..) => "corr".into(),
            Expr::Motif(..) => "motif".into(),
        }
    }

    /// Its operations, without windows or series: `slope(rsi(_, _), _)`.
    pub fn shape(&self) -> String {
        let text = self.to_string();
        let mut out = String::new();
        let mut word = String::new();
        let flush = |w: &mut String, out: &mut String, next: char| {
            if !w.is_empty() {
                out.push_str(if next == '(' { w.as_str() } else { "_" });
                w.clear();
            }
        };
        for c in text.chars() {
            if c.is_alphanumeric() || c == '_' || c == '.' || c == '"' {
                word.push(c);
            } else {
                flush(&mut word, &mut out, c);
                out.push(c);
            }
        }
        flush(&mut word, &mut out, ' ');
        out
    }

    /// What its values are (`None`: it mixes things that don't go together).
    pub fn kind(&self) -> Option<Kind> {
        use Scale::*;
        match self {
            Expr::Col(_) => Kind::of(Level, true),
            Expr::Num(v) => Kind::of(Number, *v > 0.0),
            Expr::F1(op, e) => {
                let k = e.kind()?;
                match op {
                    Op1::Abs => Kind::of(k.scale, k.positive),
                    // The tails of both sides are tried anyway: only useful inside.
                    Op1::Neg if k.scale == Level => None,
                    Op1::Neg => Kind::of(k.scale, false),
                    // The sign of something always positive is always 1.
                    Op1::Sign if k.positive || matches!(k.scale, Number | Hundred | Fraction) => {
                        None
                    }
                    Op1::Sign => Kind::of(Unitless, false),
                    Op1::Log if !k.positive || k.scale == Number => None,
                    Op1::Log => Kind::of(if k.scale == Level { Level } else { Unitless }, false),
                    Op1::Run if k.scale == Number => None,
                    Op1::Run => Kind::of(Points, false),
                }
            }
            Expr::W(op, e, _) => {
                let k = e.kind()?;
                if k.scale == Number {
                    return None;
                }
                let own = |s: Scale| if s == Level { Own } else { s };
                match op {
                    OpW::Lag | OpW::Mean | OpW::Ema | OpW::Max | OpW::Min => Some(k),
                    OpW::Diff | OpW::Slope => Kind::of(
                        match k.scale {
                            Hundred | Fraction | Points => Unitless,
                            s => own(s),
                        },
                        false,
                    ),
                    OpW::Pct if !k.positive => None,
                    OpW::Pct => Kind::of(Bp, false),
                    OpW::Std => Kind::of(own(k.scale), true),
                    OpW::Z => Kind::of(Unitless, false),
                    OpW::Pos | OpW::Rsi => Kind::of(Hundred, false),
                    OpW::SinceHigh | OpW::SinceLow => Kind::of(Points, false),
                }
            }
            Expr::F2(op, a, b) => {
                let (a, b) = (a.kind()?, b.kind()?);
                if a.scale == Number && b.scale == Number {
                    return None;
                }
                match op {
                    Op2::Add | Op2::Sub => {
                        let s = if a.scale == Number {
                            b.scale
                        } else if b.scale == Number || a.scale == b.scale {
                            a.scale
                        } else {
                            return None;
                        };
                        // Two levels apart: a distance in their units.
                        let s = if *op == Op2::Sub && s == Level && a.scale == b.scale {
                            Own
                        } else {
                            s
                        };
                        Kind::of(s, *op == Op2::Add && a.positive && b.positive)
                    }
                    Op2::Mul => {
                        let s = match (a.pure(), b.pure()) {
                            (false, false) => return None,
                            (true, true) if a.scale == Number => b.scale,
                            (true, true) if b.scale == Number => a.scale,
                            (true, true) => Unitless,
                            // A level times anything is still mostly the level.
                            _ if a.scale == Level || b.scale == Level => return None,
                            // A change or spread, scaled or gated.
                            _ => Own,
                        };
                        Kind::of(s, a.positive && b.positive)
                    }
                    Op2::Div => {
                        // Dividing by something that can be zero explodes.
                        if !b.positive {
                            return None;
                        }
                        let alike =
                            a.scale == b.scale || (!a.pure() && !b.pure()) || b.scale == Number;
                        if b.scale == Number {
                            Kind::of(a.scale, a.positive)
                        } else if alike || (a.pure() && b.pure()) {
                            Kind::of(Unitless, a.positive)
                        } else {
                            None
                        }
                    }
                    Op2::Gap => {
                        let level = |k: Kind| k.positive && matches!(k.scale, Level | Own);
                        (level(a) && level(b)).then_some(Kind {
                            scale: Bp,
                            positive: false,
                        })
                    }
                }
            }
            Expr::Corr(a, b, _) => {
                let (a, b) = (a.kind()?, b.kind()?);
                (a.scale != Number && b.scale != Number).then_some(Kind {
                    scale: Unitless,
                    positive: false,
                })
            }
            Expr::Motif(e, _) => {
                let k = e.kind()?;
                (k.scale != Number).then_some(Kind {
                    scale: Fraction,
                    positive: false,
                })
            }
        }
    }

    /// Whether it's a formula worth trying: its parts go together, and it isn't a
    /// constant.
    pub fn sensible(&self) -> bool {
        self.kind().is_some_and(|k| k.scale != Scale::Number)
    }

    /// Centred on zero (so its sign says something).
    fn centred(&self) -> bool {
        self.kind().is_some_and(|k| {
            !k.positive && matches!(k.scale, Scale::Unitless | Scale::Bp | Scale::Own)
        })
    }

    /// How many parts it has (simpler formulas are preferred).
    pub fn size(&self) -> usize {
        match self {
            Expr::Col(_) | Expr::Num(_) => 1,
            Expr::F1(_, e) | Expr::W(_, e, _) => 1 + e.size(),
            // A longer shape is a bigger idea.
            Expr::Motif(e, m) => m.len().div_ceil(2) + e.size(),
            Expr::F2(_, a, b) | Expr::Corr(a, b, _) => 1 + a.size() + b.size(),
        }
    }

    pub fn depth(&self) -> usize {
        match self {
            Expr::Col(_) | Expr::Num(_) => 1,
            Expr::F1(_, e) | Expr::W(_, e, _) | Expr::Motif(e, _) => 1 + e.depth(),
            Expr::F2(_, a, b) | Expr::Corr(a, b, _) => 1 + a.depth().max(b.depth()),
        }
    }

    /// The series it reads, in the order they first appear.
    pub fn columns(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let Expr::Col(c) = e
                && !out.contains(c)
            {
                out.push(c.clone());
            }
        });
        out
    }

    fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Col(_) | Expr::Num(_) => {}
            Expr::F1(_, e) | Expr::W(_, e, _) | Expr::Motif(e, _) => e.walk(f),
            Expr::F2(_, a, b) | Expr::Corr(a, b, _) => {
                a.walk(f);
                b.walk(f);
            }
        }
    }

    /// Whether it adds, subtracts or compares things of wildly different sizes (one
    /// swamps the other: it's just a rescaled copy of the bigger one).
    pub fn lopsided(&self, data: &dyn Columns) -> bool {
        let typical = |e: &Expr| -> f64 {
            let mut v: Vec<f64> = e
                .eval(data)
                .into_iter()
                .filter(|x| x.is_finite())
                .map(f64::abs)
                .collect();
            if v.is_empty() {
                return 0.0;
            }
            let mid = v.len() / 2;
            *v.select_nth_unstable_by(mid, f64::total_cmp).1
        };
        match self {
            Expr::Col(_) | Expr::Num(_) => false,
            Expr::F1(_, e) | Expr::W(_, e, _) | Expr::Motif(e, _) => e.lopsided(data),
            Expr::Corr(a, b, _) => a.lopsided(data) || b.lopsided(data),
            Expr::F2(op, a, b) => {
                if a.lopsided(data) || b.lopsided(data) {
                    return true;
                }
                if matches!(op, Op2::Mul | Op2::Div) {
                    return false;
                }
                let (x, y) = (typical(a), typical(b));
                let ratio = x.max(y) / x.min(y).max(1e-300);
                // Gap compares levels (an average to the price): those are alike.
                ratio > if *op == Op2::Gap { 3.0 } else { 8.0 }
            }
        }
    }

    /// Whether it takes a % change, gap or log of something that isn't always positive
    /// (a change from near zero explodes: meaningless).
    pub fn ill_posed(&self, data: &dyn Columns) -> bool {
        let positive = |e: &Expr| {
            e.eval(data)
                .iter()
                .filter(|x| x.is_finite())
                .all(|x| *x > 0.0)
        };
        match self {
            Expr::Col(_) | Expr::Num(_) => false,
            Expr::W(OpW::Pct, e, _) | Expr::F1(Op1::Log, e) => !positive(e) || e.ill_posed(data),
            Expr::F2(Op2::Gap, a, b) => {
                !positive(a) || !positive(b) || a.ill_posed(data) || b.ill_posed(data)
            }
            Expr::F1(_, e) | Expr::W(_, e, _) | Expr::Motif(e, _) => e.ill_posed(data),
            Expr::F2(_, a, b) | Expr::Corr(a, b, _) => a.ill_posed(data) || b.ill_posed(data),
        }
    }

    /// The longest stretch of history it looks back over.
    pub fn reach(&self) -> usize {
        match self {
            Expr::Col(_) | Expr::Num(_) => 0,
            Expr::F1(_, e) => e.reach(),
            Expr::Motif(e, m) => e.reach() + m.len(),
            Expr::Corr(a, b, n) => a.reach().max(b.reach()) + n,
            Expr::W(op, e, n) => {
                e.reach()
                    + match op {
                        OpW::Ema | OpW::Rsi => 3 * n,
                        _ => *n,
                    }
            }
            Expr::F2(_, a, b) => a.reach().max(b.reach()),
        }
    }

    /// Its value at every row (NaN where it isn't known: the warm-up, missing data).
    pub fn eval(&self, data: &dyn Columns) -> Vec<f64> {
        let rows = data.rows();
        match self {
            Expr::Col(c) => data
                .column(c)
                .map_or_else(|| vec![f64::NAN; rows], <[f64]>::to_vec),
            Expr::Num(v) => vec![*v; rows],
            Expr::F1(Op1::Run, e) => run(&e.eval(data)),
            Expr::Motif(e, m) => motif(&e.eval(data), m),
            Expr::Corr(a, b, n) => corr(&a.eval(data), &b.eval(data), (*n).max(3)),
            Expr::F1(op, e) => {
                let mut v = e.eval(data);
                for x in &mut v {
                    *x = match op {
                        Op1::Abs => x.abs(),
                        Op1::Neg => -*x,
                        Op1::Sign => {
                            if *x > 0.0 {
                                1.0
                            } else if *x < 0.0 {
                                -1.0
                            } else if x.is_nan() {
                                f64::NAN
                            } else {
                                0.0
                            }
                        }
                        Op1::Log => {
                            if *x > 0.0 {
                                x.ln()
                            } else {
                                f64::NAN
                            }
                        }
                        Op1::Run => unreachable!("worked out above"),
                    }
                }
                v
            }
            Expr::W(op, e, n) => window(*op, &e.eval(data), (*n).max(1)),
            Expr::F2(op, a, b) => {
                let (a, b) = (a.eval(data), b.eval(data));
                a.iter()
                    .zip(&b)
                    .map(|(x, y)| match op {
                        Op2::Add => x + y,
                        Op2::Sub => x - y,
                        Op2::Mul => x * y,
                        Op2::Div if *y != 0.0 => x / y,
                        Op2::Gap if *y != 0.0 => (x / y - 1.0) * 1e4,
                        Op2::Div | Op2::Gap => f64::NAN,
                    })
                    .collect()
            }
        }
    }
}

/// The streak: +n after n rises in a row, -n after n falls (0 when unchanged).
fn run(v: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(v.len());
    let mut streak = 0.0f64;
    for i in 0..v.len() {
        if i == 0 || !v[i].is_finite() || !v[i - 1].is_finite() {
            streak = 0.0;
            out.push(f64::NAN);
            continue;
        }
        streak = match v[i].partial_cmp(&v[i - 1]) {
            Some(std::cmp::Ordering::Greater) => streak.max(0.0) + 1.0,
            Some(std::cmp::Ordering::Less) => streak.min(0.0) - 1.0,
            _ => 0.0,
        };
        out.push(streak);
    }
    out
}

/// 1 where the last moves had the pattern's shape (`+` up, `-` down, `=` unchanged,
/// oldest first), else 0.
fn motif(v: &[f64], pattern: &str) -> Vec<f64> {
    let p: Vec<u8> = pattern.bytes().collect();
    let k = p.len();
    (0..v.len())
        .map(|i| {
            if k == 0 || i < k {
                return f64::NAN;
            }
            let mut hit = true;
            for (j, want) in p.iter().enumerate() {
                // Move j of the k: from point i-k+j to i-k+j+1.
                let (a, b) = (v[i - k + j], v[i - k + j + 1]);
                if !a.is_finite() || !b.is_finite() {
                    return f64::NAN;
                }
                let got = if b > a {
                    b'+'
                } else if b < a {
                    b'-'
                } else {
                    b'='
                };
                if got != *want {
                    hit = false;
                }
            }
            f64::from(u8::from(hit))
        })
        .collect()
}

/// How a and b moved together over the last n points: the correlation of their
/// point-to-point changes, -1 to 1.
fn corr(a: &[f64], b: &[f64], n: usize) -> Vec<f64> {
    let d = |v: &[f64], i: usize| if i == 0 { f64::NAN } else { v[i] - v[i - 1] };
    let len = a.len().min(b.len());
    (0..len)
        .map(|i| {
            if i < n {
                return f64::NAN;
            }
            let (mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for j in i + 1 - n..=i {
                let (x, y) = (d(a, j), d(b, j));
                if !x.is_finite() || !y.is_finite() {
                    return f64::NAN;
                }
                sx += x;
                sy += y;
                sxx += x * x;
                syy += y * y;
                sxy += x * y;
            }
            let nf = n as f64;
            let cov = sxy / nf - sx / nf * sy / nf;
            let vx = sxx / nf - (sx / nf).powi(2);
            let vy = syy / nf - (sy / nf).powi(2);
            if vx <= 0.0 || vy <= 0.0 {
                0.0
            } else {
                (cov / (vx * vy).sqrt()).clamp(-1.0, 1.0)
            }
        })
        .collect()
}

/// Rolling sums over the last n finite values; `None` until all n are finite.
struct Rolling {
    n: usize,
    base: Option<f64>,
    sum: f64,
    sq: f64,
    bad: usize,
}

impl Rolling {
    fn new(n: usize) -> Self {
        Rolling {
            n,
            base: None,
            sum: 0.0,
            sq: 0.0,
            bad: 0,
        }
    }

    /// Add v (leaving `old`, the value n back, when there is one): (mean, sd, v − base).
    fn push(&mut self, v: f64, old: Option<f64>, filled: bool) -> Option<(f64, f64)> {
        // Measured from the first value, so big numbers (prices) keep their precision.
        let base = *self.base.get_or_insert(if v.is_finite() { v } else { 0.0 });
        let mut add = |x: f64, sign: f64| {
            if x.is_finite() {
                let d = x - base;
                self.sum += sign * d;
                self.sq += sign * d * d;
            } else if sign > 0.0 {
                self.bad += 1;
            } else {
                self.bad -= 1;
            }
        };
        add(v, 1.0);
        if let Some(o) = old {
            add(o, -1.0);
        }
        if !filled || self.bad > 0 {
            return None;
        }
        let n = self.n as f64;
        let mean = self.sum / n;
        let sd = (self.sq / n - mean * mean).max(0.0).sqrt();
        Some((mean + base, sd))
    }
}

fn window(op: OpW, v: &[f64], n: usize) -> Vec<f64> {
    let len = v.len();
    let nan = f64::NAN;
    let back = |i: usize| if i >= n { v[i - n] } else { nan };
    match op {
        OpW::Lag => (0..len).map(back).collect(),
        OpW::Diff => (0..len).map(|i| v[i] - back(i)).collect(),
        OpW::Pct => (0..len)
            .map(|i| {
                let b = back(i);
                if b != 0.0 {
                    (v[i] / b - 1.0) * 1e4
                } else {
                    nan
                }
            })
            .collect(),
        OpW::Mean | OpW::Std | OpW::Z => {
            let n = n.max(2);
            let mut r = Rolling::new(n);
            (0..len)
                .map(|i| {
                    let old = (i >= n).then(|| v[i - n]);
                    match r.push(v[i], old, i + 1 >= n) {
                        None => nan,
                        Some((mean, sd)) => match op {
                            OpW::Mean => mean,
                            OpW::Std => sd,
                            _ if sd > 0.0 => (v[i] - mean) / sd,
                            _ => 0.0,
                        },
                    }
                })
                .collect()
        }
        OpW::Slope => {
            // Least-squares slope over the last n points, per point.
            let n = n.max(2);
            let nf = n as f64;
            let sx = nf * (nf - 1.0) / 2.0;
            let sxx = (nf - 1.0) * nf * (2.0 * nf - 1.0) / 6.0;
            let den = nf * sxx - sx * sx;
            (0..len)
                .map(|i| {
                    if i + 1 < n {
                        return nan;
                    }
                    let w = &v[i + 1 - n..=i];
                    let (mut sy, mut sxy) = (0.0, 0.0);
                    for (k, y) in w.iter().enumerate() {
                        sy += y;
                        sxy += k as f64 * y;
                    }
                    (nf * sxy - sx * sy) / den
                })
                .collect()
        }
        OpW::Max | OpW::Min | OpW::Pos | OpW::SinceHigh | OpW::SinceLow => {
            let n = n.max(2);
            let (mut hi, mut lo): (VecDeque<usize>, VecDeque<usize>) =
                (VecDeque::new(), VecDeque::new());
            let mut bad = 0usize;
            (0..len)
                .map(|i| {
                    let x = v[i];
                    if x.is_finite() {
                        while hi.back().is_some_and(|&j| v[j] <= x) {
                            hi.pop_back();
                        }
                        hi.push_back(i);
                        while lo.back().is_some_and(|&j| v[j] >= x) {
                            lo.pop_back();
                        }
                        lo.push_back(i);
                    } else {
                        bad += 1;
                    }
                    if i >= n && !v[i - n].is_finite() {
                        bad -= 1;
                    }
                    while hi.front().is_some_and(|&j| j + n <= i) {
                        hi.pop_front();
                    }
                    while lo.front().is_some_and(|&j| j + n <= i) {
                        lo.pop_front();
                    }
                    if i + 1 < n || bad > 0 {
                        return nan;
                    }
                    let (h, l) = (v[hi[0]], v[lo[0]]);
                    match op {
                        OpW::Max => h,
                        OpW::Min => l,
                        OpW::SinceHigh => (i - hi[0]) as f64,
                        OpW::SinceLow => (i - lo[0]) as f64,
                        _ if h > l => (x - l) / (h - l) * 100.0,
                        _ => 50.0,
                    }
                })
                .collect()
        }
        OpW::Ema => {
            let k = 2.0 / (n as f64 + 1.0);
            let (mut prev, mut seen) = (nan, 0usize);
            v.iter()
                .map(|&x| {
                    if !x.is_finite() {
                        return nan;
                    }
                    prev = if prev.is_finite() {
                        x * k + prev * (1.0 - k)
                    } else {
                        x
                    };
                    seen += 1;
                    if seen >= n { prev } else { nan }
                })
                .collect()
        }
        OpW::Rsi => {
            let nf = n as f64;
            let (mut gain, mut loss, mut seen) = (0.0, 0.0, 0usize);
            (0..len)
                .map(|i| {
                    if i == 0 || !v[i].is_finite() || !v[i - 1].is_finite() {
                        return nan;
                    }
                    let d = v[i] - v[i - 1];
                    let (g, l) = (d.max(0.0), (-d).max(0.0));
                    seen += 1;
                    if seen <= n {
                        gain += g / nf;
                        loss += l / nf;
                        return nan;
                    }
                    gain = (gain * (nf - 1.0) + g) / nf;
                    loss = (loss * (nf - 1.0) + l) / nf;
                    if loss == 0.0 {
                        100.0
                    } else {
                        100.0 - 100.0 / (1.0 + gain / loss)
                    }
                })
                .collect()
        }
    }
}

// ───────────── text ─────────────

fn is_word(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_alphabetic() || f == '_')
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && !OPS1.iter().any(|o| o.1 == s)
        && !OPSW.iter().any(|o| o.1 == s)
        && !["gap", "corr", "motif"].contains(&s)
}

impl std::fmt::Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Expr::Col(c) if is_word(c) => write!(f, "{c}"),
            Expr::Col(c) => write!(f, "{c:?}"),
            Expr::Num(v) => write!(f, "{v}"),
            Expr::F1(op, e) => {
                let name = OPS1.iter().find(|o| o.0 == *op).expect("named").1;
                write!(f, "{name}({e})")
            }
            Expr::W(op, e, n) => {
                let name = OPSW.iter().find(|o| o.0 == *op).expect("named").1;
                write!(f, "{name}({e}, {n})")
            }
            Expr::F2(Op2::Gap, a, b) => write!(f, "gap({a}, {b})"),
            Expr::Corr(a, b, n) => write!(f, "corr({a}, {b}, {n})"),
            Expr::Motif(e, m) => write!(f, "motif({e}, \"{m}\")"),
            Expr::F2(op, a, b) => {
                let sym = match op {
                    Op2::Add => "+",
                    Op2::Sub => "-",
                    Op2::Mul => "*",
                    _ => "/",
                };
                // Brackets around a sum inside a product, and around anything on the
                // right that binds as loosely (it's read left to right).
                let level = |o: &Op2| u8::from(matches!(o, Op2::Mul | Op2::Div));
                let wrap = |e: &Expr, right: bool| match e {
                    Expr::F2(Op2::Gap, ..) => false,
                    Expr::F2(inner, ..) => {
                        level(inner) < level(op) || (right && level(inner) == level(op))
                    }
                    _ => false,
                };
                let side = |e: &Expr, right: bool| {
                    if wrap(e, right) {
                        format!("({e})")
                    } else {
                        e.to_string()
                    }
                };
                write!(f, "{} {sym} {}", side(a, false), side(b, true))
            }
        }
    }
}

/// Read a formula (`z(pct(price, 5), 50)`).
pub fn parse(text: &str) -> Result<Expr, String> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    let e = p.sum()?;
    p.space();
    if p.i < p.s.len() {
        return Err(format!("formula '{text}': unexpected '{}'", &text[p.i..]));
    }
    Ok(e)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.space();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!("formula: expected '{}'", c as char))
        }
    }

    fn sum(&mut self) -> Result<Expr, String> {
        let mut e = self.product()?;
        loop {
            if self.eat(b'+') {
                e = Expr::f2(Op2::Add, e, self.product()?);
            } else if self.eat(b'-') {
                e = Expr::f2(Op2::Sub, e, self.product()?);
            } else {
                return Ok(e);
            }
        }
    }

    fn product(&mut self) -> Result<Expr, String> {
        let mut e = self.atom()?;
        loop {
            if self.eat(b'*') {
                e = Expr::f2(Op2::Mul, e, self.atom()?);
            } else if self.eat(b'/') {
                e = Expr::f2(Op2::Div, e, self.atom()?);
            } else {
                return Ok(e);
            }
        }
    }

    fn atom(&mut self) -> Result<Expr, String> {
        self.space();
        let start = self.i;
        match self.s.get(self.i) {
            None => Err("formula: it ends too soon".into()),
            Some(b'(') => {
                self.i += 1;
                let e = self.sum()?;
                self.expect(b')')?;
                Ok(e)
            }
            Some(b'-') => {
                self.i += 1;
                Ok(Expr::F1(Op1::Neg, Box::new(self.atom()?)))
            }
            Some(b'"') => {
                self.i += 1;
                while self.s.get(self.i).is_some_and(|c| *c != b'"') {
                    self.i += 1;
                }
                let name = String::from_utf8_lossy(&self.s[start + 1..self.i]).into_owned();
                self.expect(b'"')?;
                Ok(Expr::Col(name))
            }
            Some(c) if c.is_ascii_digit() || *c == b'.' => {
                while self
                    .s
                    .get(self.i)
                    .is_some_and(|c| c.is_ascii_digit() || *c == b'.' || *c == b'e')
                {
                    self.i += 1;
                }
                let t = String::from_utf8_lossy(&self.s[start..self.i]);
                t.parse()
                    .map(Expr::Num)
                    .map_err(|_| format!("formula: '{t}' isn't a number"))
            }
            Some(_) => {
                while self.s.get(self.i).is_some_and(|c| {
                    c.is_ascii_alphanumeric() || *c == b'_' || *c == b'.' || *c >= 0x80
                }) {
                    self.i += 1;
                }
                let word = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
                if word.is_empty() {
                    return Err(format!("formula: unexpected '{}'", self.s[self.i] as char));
                }
                if !self.eat(b'(') {
                    return Ok(Expr::Col(word));
                }
                let arg = self.sum()?;
                if let Some(op) = OPS1.iter().find(|o| o.1 == word) {
                    self.expect(b')')?;
                    return Ok(Expr::F1(op.0, Box::new(arg)));
                }
                self.expect(b',')?;
                if word == "gap" {
                    let b = self.sum()?;
                    self.expect(b')')?;
                    return Ok(Expr::f2(Op2::Gap, arg, b));
                }
                if word == "motif" {
                    self.expect(b'"')?;
                    let from = self.i;
                    while self.s.get(self.i).is_some_and(|c| b"+-=".contains(c)) {
                        self.i += 1;
                    }
                    let m = String::from_utf8_lossy(&self.s[from..self.i]).into_owned();
                    if m.is_empty() || self.s.get(self.i) != Some(&b'"') {
                        return Err(
                            "formula: motif(x, \"++-\") takes a shape of + (up), - (down) and = (unchanged)"
                                .into(),
                        );
                    }
                    self.i += 1;
                    self.expect(b')')?;
                    return Ok(Expr::Motif(Box::new(arg), m));
                }
                let corr_b = if word == "corr" {
                    let b = self.sum()?;
                    self.expect(b',')?;
                    Some(b)
                } else {
                    None
                };
                let op = OPSW.iter().find(|o| o.1 == word);
                if op.is_none() && corr_b.is_none() {
                    return Err(format!("formula: there's no function called {word}"));
                }
                self.space();
                let n_start = self.i;
                while self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
                    self.i += 1;
                }
                let n: usize = String::from_utf8_lossy(&self.s[n_start..self.i])
                    .parse()
                    .map_err(|_| format!("formula: {word}(…, n) needs a whole number of points"))?;
                self.expect(b')')?;
                match (corr_b, op) {
                    (Some(b), _) => Ok(Expr::Corr(Box::new(arg), Box::new(b), n.max(3))),
                    (None, Some(op)) => Ok(Expr::w(op.0, arg, n.max(1))),
                    (None, None) => unreachable!("checked above"),
                }
            }
        }
    }
}

// ───────────── making new ones ─────────────

/// A small, fast random number source (xorshift).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// 0..n
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() % 10_000) as f64 / 10_000.0 < p
    }

    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

/// What new formulas are made from.
pub struct Maker {
    /// The series (the price first: it's picked more often).
    pub cols: Vec<String>,
    /// Window lengths to try.
    pub windows: Vec<usize>,
}

/// Operations that take any series to something comparable over time (a change, a
/// distance from normal…): used at the top of new formulas.
const SCALE_FREE: [OpW; 6] = [OpW::Pct, OpW::Z, OpW::Pos, OpW::Rsi, OpW::Diff, OpW::Slope];

impl Maker {
    pub fn new(cols: Vec<String>, rows: usize) -> Maker {
        let windows = [2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377]
            .into_iter()
            .filter(|w| w * 6 <= rows)
            .collect();
        Maker { cols, windows }
    }

    fn window(&self, rng: &mut Rng) -> usize {
        *rng.pick(&self.windows)
    }

    fn leaf(&self, rng: &mut Rng) -> Expr {
        if rng.chance(0.5) || self.cols.len() == 1 {
            Expr::Col(self.cols[0].clone())
        } else {
            Expr::Col(rng.pick(&self.cols[1..]).clone())
        }
    }

    /// A shape of 2 to 6 moves.
    fn shape(&self, rng: &mut Rng) -> String {
        (0..2 + rng.below(5))
            .map(|_| *rng.pick(&['+', '+', '-', '-', '=']))
            .collect()
    }

    /// A pattern: a shape of the price's moves (or something's), a streak, or how two
    /// series move together.
    fn pattern(&self, rng: &mut Rng) -> Expr {
        match rng.below(4) {
            0 | 1 => Expr::Motif(Box::new(self.leaf(rng)), self.shape(rng)),
            2 => Expr::F1(Op1::Run, Box::new(self.leaf(rng))),
            _ => Expr::Corr(
                Box::new(self.leaf(rng)),
                Box::new(Expr::w(OpW::Lag, self.leaf(rng), self.window(rng))),
                self.window(rng).max(5),
            ),
        }
    }

    /// Any formula up to `depth` deep.
    fn any(&self, rng: &mut Rng, depth: usize) -> Expr {
        if depth <= 1 || rng.chance(0.25) {
            return self.leaf(rng);
        }
        if rng.chance(0.1) {
            return self.pattern(rng);
        }
        match rng.below(10) {
            0..=5 => {
                let op = OPSW[rng.below(OPSW.len())].0;
                Expr::w(op, self.any(rng, depth - 1), self.window(rng))
            }
            6 => Expr::F1(
                OPS1[rng.below(OPS1.len())].0,
                Box::new(self.any(rng, depth - 1)),
            ),
            _ => {
                let op = *rng.pick(&[Op2::Add, Op2::Sub, Op2::Mul, Op2::Div, Op2::Gap]);
                Expr::f2(op, self.any(rng, depth - 1), self.any(rng, depth - 1))
            }
        }
    }

    /// A window at least `k` rungs longer than `n` (or the longest).
    fn longer(&self, n: usize, k: usize) -> usize {
        let i = self.windows.iter().position(|w| *w >= n).unwrap_or(0);
        self.windows[(i + k).min(self.windows.len() - 1)]
    }

    /// Something scale-free about a series: a shallow random formula, made comparable
    /// over time.
    fn signal(&self, rng: &mut Rng) -> Expr {
        for _ in 0..20 {
            let inner = if rng.chance(0.5) {
                self.leaf(rng)
            } else {
                self.any(rng, 3)
            };
            let e = Expr::w(*rng.pick(&SCALE_FREE), inner, self.window(rng));
            if e.sensible() {
                return e;
            }
        }
        Expr::w(OpW::Pct, self.leaf(rng), self.window(rng))
    }

    /// A formula in a shape traders use, with random parts: momentum against
    /// volatility, a divergence, a signal taken only with the trend, mean reversion
    /// that depends on how calm it is, an acceleration, two timeframes against each
    /// other, a pattern in context.
    pub fn template(&self, rng: &mut Rng) -> Expr {
        let p = || Expr::col(&self.cols[0]);
        let n = self.window(rng);
        let m = self.longer(n, 1 + rng.below(3));
        let returns = || Expr::w(OpW::Pct, p(), 1);
        let x = match rng.below(8) {
            // How far it went, in its usual steps.
            0 => Expr::f2(
                Op2::Div,
                Expr::w(OpW::Pct, p(), n),
                Expr::w(OpW::Std, returns(), m),
            ),
            // The price's trend and a signal's trend disagree.
            1 => {
                let k = self.window(rng);
                let sig = Expr::w(*rng.pick(&[OpW::Rsi, OpW::Pos, OpW::Z]), self.leaf(rng), k);
                Expr::f2(
                    Op2::Sub,
                    Expr::w(OpW::Z, Expr::w(OpW::Slope, p(), n), m),
                    Expr::w(OpW::Z, Expr::w(OpW::Slope, sig, n), m),
                )
            }
            // A signal, only with the trend.
            2 => Expr::f2(
                Op2::Mul,
                Expr::w(OpW::Z, self.signal(rng), n),
                Expr::F1(Op1::Sign, Box::new(Expr::w(OpW::Slope, p(), m))),
            ),
            // Stretched from normal, weighted by how calm or wild it is.
            3 => Expr::f2(
                Op2::Mul,
                Expr::w(OpW::Z, p(), n),
                Expr::w(OpW::Pos, Expr::w(OpW::Std, returns(), n), m),
            ),
            // Speeding up or slowing down.
            4 => Expr::w(OpW::Diff, Expr::w(OpW::Pct, p(), n), self.window(rng)),
            // Where it is in a short range against a long one.
            5 => Expr::f2(
                Op2::Sub,
                Expr::w(OpW::Pos, p(), n),
                Expr::w(OpW::Pos, p(), m),
            ),
            // Another series' surprise, in the direction the price is going.
            6 => Expr::f2(
                Op2::Mul,
                Expr::w(OpW::Z, self.leaf(rng), n),
                Expr::F1(Op1::Sign, Box::new(Expr::w(OpW::Pct, p(), m))),
            ),
            // How often a shape came up lately, signed by where the price stands.
            _ => Expr::f2(
                Op2::Mul,
                Expr::w(OpW::Mean, self.pattern(rng), n),
                Expr::w(OpW::Z, p(), m),
            ),
        };
        // Sometimes smoothed or standardised once more.
        match rng.below(4) {
            0 => Expr::w(OpW::Ema, x, *rng.pick(&[2, 3, 5])),
            1 => Expr::w(OpW::Z, x, self.longer(m, 1)),
            _ => x,
        }
    }

    /// Two formulas that work, combined: their sum or product when both are
    /// standardised (together, or each one only matters when the other is high), one
    /// taken only when the other's sign agrees, how one follows the other, or how fast
    /// one is changing.
    pub fn compose(&self, a: &Expr, b: &Expr, rng: &mut Rng) -> Expr {
        let n = self.window(rng);
        let z = |e: &Expr| Expr::w(OpW::Z, e.clone(), n);
        match rng.below(5) {
            0 => Expr::f2(Op2::Add, z(a), z(b)),
            1 => Expr::f2(Op2::Mul, z(a), z(b)),
            2 => {
                let gate = if b.centred() { b.clone() } else { z(b) };
                Expr::f2(Op2::Mul, z(a), Expr::F1(Op1::Sign, Box::new(gate)))
            }
            3 => Expr::Corr(Box::new(a.clone()), Box::new(b.clone()), n.max(5)),
            _ => Expr::w(OpW::Z, Expr::w(OpW::Diff, a.clone(), self.window(rng)), n),
        }
    }

    /// A new formula, usually something comparable over time at the top.
    pub fn random(&self, rng: &mut Rng) -> Expr {
        if rng.chance(0.3) {
            return self.template(rng);
        }
        let depth = 2 + rng.below(4);
        // Patterns as they are, or smoothed (how often the shape came up lately).
        if rng.chance(0.25) {
            let p = self.pattern(rng);
            return if rng.chance(0.4) {
                Expr::w(OpW::Mean, p, self.window(rng).max(5))
            } else {
                p
            };
        }
        if rng.chance(0.75) {
            let op = *rng.pick(&SCALE_FREE);
            Expr::w(op, self.any(rng, depth - 1), self.window(rng))
        } else if rng.chance(0.5) {
            // One thing against another.
            let op = *rng.pick(&[Op2::Sub, Op2::Gap, Op2::Div]);
            let a = Expr::w(
                *rng.pick(&SCALE_FREE),
                self.any(rng, depth - 2),
                self.window(rng),
            );
            let b = Expr::w(
                *rng.pick(&SCALE_FREE),
                self.any(rng, depth - 2),
                self.window(rng),
            );
            Expr::f2(op, a, b)
        } else {
            self.any(rng, depth)
        }
    }

    /// A changed copy: a part replaced, a window resized, or an operation swapped.
    pub fn mutate(&self, e: &Expr, rng: &mut Rng) -> Expr {
        let parts = e.size();
        let target = rng.below(parts);
        let mut k = 0;
        self.mutate_at(e, target, &mut k, rng)
    }

    fn mutate_at(&self, e: &Expr, target: usize, k: &mut usize, rng: &mut Rng) -> Expr {
        let here = *k == target;
        *k += 1;
        if here {
            return match e {
                Expr::W(op, inner, n) if rng.chance(0.6) => {
                    if rng.chance(0.6) {
                        // A nearby window.
                        let i = self.windows.iter().position(|w| w >= n).unwrap_or(0);
                        let j = if rng.chance(0.5) {
                            i.saturating_sub(1)
                        } else {
                            (i + 1).min(self.windows.len() - 1)
                        };
                        Expr::W(*op, inner.clone(), self.windows[j])
                    } else {
                        Expr::W(OPSW[rng.below(OPSW.len())].0, inner.clone(), *n)
                    }
                }
                Expr::F2(_, a, b) if rng.chance(0.5) => {
                    let op = *rng.pick(&[Op2::Add, Op2::Sub, Op2::Mul, Op2::Div, Op2::Gap]);
                    Expr::F2(op, a.clone(), b.clone())
                }
                // A shape a move longer, shorter, or with one move changed.
                Expr::Motif(inner, m) if rng.chance(0.8) => {
                    let mut m: Vec<char> = m.chars().collect();
                    match rng.below(3) {
                        0 if m.len() < 8 => m.insert(0, *rng.pick(&['+', '-', '='])),
                        1 if m.len() > 2 => {
                            m.remove(0);
                        }
                        _ => {
                            let i = rng.below(m.len());
                            m[i] = *rng.pick(&['+', '-', '=']);
                        }
                    }
                    Expr::Motif(inner.clone(), m.into_iter().collect())
                }
                Expr::Corr(a, b, n) if rng.chance(0.6) => {
                    let i = self.windows.iter().position(|w| w >= n).unwrap_or(0);
                    let j = (i + 1).min(self.windows.len() - 1);
                    let n = if rng.chance(0.5) {
                        self.windows[i.saturating_sub(1)]
                    } else {
                        self.windows[j]
                    };
                    Expr::Corr(a.clone(), b.clone(), n.max(5))
                }
                Expr::Col(_) => self.leaf(rng),
                _ => self.any(rng, 3),
            };
        }
        match e {
            Expr::Col(_) | Expr::Num(_) => e.clone(),
            Expr::Motif(inner, m) => {
                Expr::Motif(Box::new(self.mutate_at(inner, target, k, rng)), m.clone())
            }
            Expr::Corr(a, b, n) => {
                let a = self.mutate_at(a, target, k, rng);
                let b = self.mutate_at(b, target, k, rng);
                Expr::Corr(Box::new(a), Box::new(b), *n)
            }
            Expr::F1(op, inner) => Expr::F1(*op, Box::new(self.mutate_at(inner, target, k, rng))),
            Expr::W(op, inner, n) => {
                Expr::W(*op, Box::new(self.mutate_at(inner, target, k, rng)), *n)
            }
            Expr::F2(op, a, b) => {
                let a = self.mutate_at(a, target, k, rng);
                let b = self.mutate_at(b, target, k, rng);
                Expr::F2(*op, Box::new(a), Box::new(b))
            }
        }
    }

    /// `a` with one of its parts swapped for one of `b`'s.
    pub fn cross(&self, a: &Expr, b: &Expr, rng: &mut Rng) -> Expr {
        let donor = nth(b, rng.below(b.size())).clone();
        let target = rng.below(a.size());
        let mut k = 0;
        replace(a, target, &donor, &mut k)
    }
}

fn nth(e: &Expr, target: usize) -> &Expr {
    fn go<'a>(e: &'a Expr, target: usize, k: &mut usize) -> Option<&'a Expr> {
        if *k == target {
            return Some(e);
        }
        *k += 1;
        match e {
            Expr::Col(_) | Expr::Num(_) => None,
            Expr::F1(_, i) | Expr::W(_, i, _) | Expr::Motif(i, _) => go(i, target, k),
            Expr::F2(_, a, b) | Expr::Corr(a, b, _) => {
                go(a, target, k).or_else(|| go(b, target, k))
            }
        }
    }
    go(e, target, &mut 0).unwrap_or(e)
}

fn replace(e: &Expr, target: usize, with: &Expr, k: &mut usize) -> Expr {
    if *k == target {
        *k += e.size();
        return with.clone();
    }
    *k += 1;
    match e {
        Expr::Col(_) | Expr::Num(_) => e.clone(),
        Expr::F1(op, i) => Expr::F1(*op, Box::new(replace(i, target, with, k))),
        Expr::W(op, i, n) => Expr::W(*op, Box::new(replace(i, target, with, k)), *n),
        Expr::Motif(i, m) => Expr::Motif(Box::new(replace(i, target, with, k)), m.clone()),
        Expr::Corr(a, b, n) => {
            let a = replace(a, target, with, k);
            let b = replace(b, target, with, k);
            Expr::Corr(Box::new(a), Box::new(b), *n)
        }
        Expr::F2(op, a, b) => {
            let a = replace(a, target, with, k);
            let b = replace(b, target, with, k);
            Expr::F2(*op, Box::new(a), Box::new(b))
        }
    }
}

/// The usual indicators, as formulas: where a search starts from.
pub fn classics(
    price: &str,
    book: Option<(&str, &str)>,
    others: &[(String, bool)],
    rows: usize,
) -> Vec<Expr> {
    let p = || Expr::col(price);
    let fits = |n: usize| n * 6 <= rows;
    let mut v = Vec::new();
    for n in [5, 15, 50, 150].into_iter().filter(|n| fits(*n)) {
        v.push(Expr::w(OpW::Pct, p(), n));
        v.push(Expr::w(OpW::Z, p(), n));
        v.push(Expr::w(OpW::Pos, p(), n));
        if n >= 15 {
            v.push(Expr::w(OpW::Std, Expr::w(OpW::Pct, p(), 1), n));
            v.push(Expr::w(
                OpW::Mean,
                Expr::F1(Op1::Sign, Box::new(Expr::w(OpW::Diff, p(), 1))),
                n,
            ));
        }
    }
    for n in [3, 10, 40].into_iter().filter(|n| fits(4 * n)) {
        v.push(Expr::f2(
            Op2::Gap,
            Expr::w(OpW::Ema, p(), n),
            Expr::w(OpW::Ema, p(), 4 * n),
        ));
    }
    for n in [7, 14].into_iter().filter(|n| fits(*n)) {
        v.push(Expr::w(OpW::Rsi, p(), n));
    }
    if let Some((bid, ask)) = book {
        v.push(Expr::f2(Op2::Gap, Expr::col(ask), Expr::col(bid)));
        // Where the price sits between bid and ask.
        v.push(Expr::f2(
            Op2::Div,
            Expr::f2(Op2::Sub, p(), Expr::col(bid)),
            Expr::f2(Op2::Sub, Expr::col(ask), Expr::col(bid)),
        ));
    }
    for (name, price_like) in others {
        if *price_like {
            v.push(Expr::f2(Op2::Gap, Expr::col(name), p()));
        } else {
            v.push(Expr::col(name));
            if fits(100) {
                v.push(Expr::w(OpW::Z, Expr::col(name), 100));
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Cols(Vec<(String, Vec<f64>)>);

    impl Columns for Cols {
        fn column(&self, name: &str) -> Option<&[f64]> {
            self.0.iter().find(|c| c.0 == name).map(|c| c.1.as_slice())
        }
        fn rows(&self) -> usize {
            self.0[0].1.len()
        }
    }

    #[test]
    fn reads_and_writes_formulas() {
        for text in [
            "z(pct(price, 5), 50)",
            "gap(ema(price, 3), ema(price, 12))",
            "(price - bid) / (ask - bid)",
            "pct(feed.0.price, 5) - z(\"my volume\", 20) * 2",
            "a * (b * c) - (d - e)",
            "mean(sign(diff(price, 1)), 15)",
            "neg(abs(slope(price, 8)))",
            "motif(price, \"++-=\")",
            "mean(motif(price, \"--\"), 13) + run(price)",
            "corr(price, lag(volume, 3), 21)",
            "since_high(price, 55) - since_low(price, 55)",
        ] {
            let e = parse(text).unwrap();
            assert_eq!(e.to_string(), text);
            assert_eq!(parse(&e.to_string()).unwrap(), e);
        }
        assert!(parse("z(price)").is_err());
        assert!(parse("nope(price, 3)").is_err());
        assert_eq!(parse("a - (b - c)").unwrap().to_string(), "a - (b - c)");
    }

    #[test]
    fn windows_only_look_back() {
        let price: Vec<f64> = (0..40).map(|i| 100.0 + (i as f64 * 0.7).sin()).collect();
        let data = Cols(vec![("price".into(), price.clone())]);
        for text in [
            "pct(price, 3)",
            "z(price, 5)",
            "pos(price, 5)",
            "ema(price, 4)",
            "rsi(price, 4)",
            "std(price, 4)",
            "slope(price, 4)",
            "max(price, 4)",
            "motif(price, \"+-\")",
            "run(price)",
            "since_high(price, 6)",
            "corr(price, lag(price, 2), 6)",
        ] {
            let full = parse(text).unwrap().eval(&data);
            // The same values when the future isn't there yet.
            let cut = Cols(vec![("price".into(), price[..25].to_vec())]);
            let part = parse(text).unwrap().eval(&cut);
            for i in 0..25 {
                assert!(
                    full[i] == part[i] || (full[i].is_nan() && part[i].is_nan()),
                    "{text} at {i}"
                );
            }
            assert!(full[30].is_finite(), "{text}");
        }
        let pos = parse("pos(price, 5)").unwrap().eval(&data);
        assert!(
            pos.iter()
                .filter(|v| v.is_finite())
                .all(|v| (0.0..=100.0).contains(v))
        );
        let x = Cols(vec![("x".into(), vec![1.0, 2.0, 3.0, 2.0, 2.0, 5.0])]);
        let m = parse("motif(x, \"+-=\")").unwrap().eval(&x);
        assert_eq!(
            &m[3..],
            [0.0, 1.0, 0.0],
            "up, down, unchanged ends at point 4"
        );
        assert_eq!(
            &parse("run(x)").unwrap().eval(&x)[1..],
            [1.0, 2.0, -1.0, 0.0, 1.0]
        );
        assert_eq!(parse("since_high(x, 4)").unwrap().eval(&x)[4], 2.0);
        assert!(parse("motif(x, \"up\")").is_err());
        let slope = parse("slope(x, 3)")
            .unwrap()
            .eval(&Cols(vec![("x".into(), vec![1.0, 3.0, 5.0, 7.0])]));
        assert!((slope[3] - 2.0).abs() < 1e-9);
    }

    #[test]
    fn shapes_ignore_windows_and_series() {
        let a = parse("slope(rsi(price, 55), 2)").unwrap();
        let b = parse("slope(rsi(volume, 13), 8)").unwrap();
        assert_eq!(a.shape(), b.shape());
        assert_eq!(a.shape(), "slope(rsi(_, _), _)");
        assert_eq!(a.head(), "slope");
        assert_ne!(a.shape(), parse("slope(z(price, 55), 2)").unwrap().shape());
    }

    #[test]
    fn nonsense_never_gets_tried() {
        let ok = |t: &str| parse(t).unwrap().sensible();
        // Mixed scales, dividing by something that crosses zero, a % change of
        // something that isn't a level, the sign of something always positive.
        for bad in [
            "price + rsi(price, 14)",
            "pct(z(price, 20), 5)",
            "pct(price, 5) / z(price, 20)",
            "sign(rsi(price, 14))",
            "price * price",
            "price * z(price, 20)",
            "motif(price, \"++\") / price",
            "log(diff(price, 3))",
            "gap(z(price, 5), price)",
            "3 + 4",
        ] {
            assert!(!ok(bad), "{bad}");
        }
        for good in [
            "pct(price, 15) / std(pct(price, 1), 50)",
            "z(slope(price, 8), 50) - z(slope(rsi(price, 14), 8), 50)",
            "z(pct(volume, 5), 20) * sign(slope(price, 50))",
            "z(price, 20) * pos(std(pct(price, 1), 20), 100)",
            "pos(price, 8) - pos(price, 89)",
            "gap(ema(price, 3), ema(price, 12))",
            "mean(motif(price, \"++-\"), 13) * z(price, 34)",
            "rsi(price, 14) - 50",
        ] {
            assert!(ok(good), "{good}");
        }
        // The usual indicators make sense too.
        for c in classics("price", None, &[("volume".into(), false)], 5000) {
            assert!(c.sensible(), "{c}");
        }
    }

    #[test]
    fn makes_deep_formulas_that_make_sense() {
        let m = Maker::new(vec!["price".into(), "volume".into()], 5000);
        let mut rng = Rng::new(9);
        let mut deep = 0;
        for _ in 0..300 {
            let t = m.template(&mut rng);
            assert!(t.sensible(), "{t}");
            assert_eq!(parse(&t.to_string()).unwrap(), t, "{t}");
            let (a, b) = (m.template(&mut rng), m.random(&mut rng));
            let c = m.compose(&a, &b, &mut rng);
            if a.sensible() && b.sensible() {
                assert!(c.sensible(), "{a} · {b} → {c}");
            }
            if c.depth() >= 6 {
                deep += 1;
            }
        }
        assert!(deep > 100, "{deep} deep ones");
    }

    #[test]
    fn makes_valid_new_formulas() {
        let m = Maker::new(vec!["price".into(), "volume".into()], 5000);
        let mut rng = Rng::new(42);
        for _ in 0..500 {
            let a = m.random(&mut rng);
            let b = m.mutate(&a, &mut rng);
            let c = m.cross(&a, &b, &mut rng);
            for e in [a, b, c] {
                assert_eq!(parse(&e.to_string()).unwrap(), e, "{e}");
                assert!(e.columns().iter().all(|c| c == "price" || c == "volume"));
            }
        }
    }
}
