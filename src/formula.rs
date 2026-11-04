//! Formulas over series: a small language that discovery writes (and evolves), and
//! that charts can use (`formula("z(pct(price, 5) - pct(volume, 20), 50)")`).
//!
//! - a series by name (`price`, `"feed.0.bid"` quoted when it isn't a plain word), or
//!   a number;
//! - over the last n points: `lag diff pct mean ema std max min z pos rsi slope`
//!   (`pct` is the change in bp, `z` the distance from the mean in standard
//!   deviations, `pos` the place in the range 0–100);
//! - `abs neg sign log`, `a + b`, `a - b`, `a * b`, `a / b`, `gap(a, b)` (a relative
//!   to b, in bp).
//!
//! Every value uses only points up to its own: no peeking ahead.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op1 {
    Abs,
    Neg,
    Sign,
    Log,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op2 {
    Add,
    Sub,
    Mul,
    Div,
    Gap,
}

const OPS1: [(Op1, &str); 4] = [
    (Op1::Abs, "abs"),
    (Op1::Neg, "neg"),
    (Op1::Sign, "sign"),
    (Op1::Log, "log"),
];
const OPSW: [(OpW, &str); 12] = [
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
];

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Col(String),
    Num(f64),
    F1(Op1, Box<Expr>),
    W(OpW, Box<Expr>, usize),
    F2(Op2, Box<Expr>, Box<Expr>),
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

    /// How many parts it has (simpler formulas are preferred).
    pub fn size(&self) -> usize {
        match self {
            Expr::Col(_) | Expr::Num(_) => 1,
            Expr::F1(_, e) | Expr::W(_, e, _) => 1 + e.size(),
            Expr::F2(_, a, b) => 1 + a.size() + b.size(),
        }
    }

    pub fn depth(&self) -> usize {
        match self {
            Expr::Col(_) | Expr::Num(_) => 1,
            Expr::F1(_, e) | Expr::W(_, e, _) => 1 + e.depth(),
            Expr::F2(_, a, b) => 1 + a.depth().max(b.depth()),
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
            Expr::F1(_, e) | Expr::W(_, e, _) => e.walk(f),
            Expr::F2(_, a, b) => {
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
            Expr::F1(_, e) | Expr::W(_, e, _) => e.lopsided(data),
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

    /// The longest stretch of history it looks back over.
    pub fn reach(&self) -> usize {
        match self {
            Expr::Col(_) | Expr::Num(_) => 0,
            Expr::F1(_, e) => e.reach(),
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
        OpW::Max | OpW::Min | OpW::Pos => {
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
        && s != "gap"
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
                let Some(op) = OPSW.iter().find(|o| o.1 == word) else {
                    return Err(format!("formula: there's no function called {word}"));
                };
                self.space();
                let n_start = self.i;
                while self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
                    self.i += 1;
                }
                let n: usize = String::from_utf8_lossy(&self.s[n_start..self.i])
                    .parse()
                    .map_err(|_| format!("formula: {word}(…, n) needs a whole number of points"))?;
                self.expect(b')')?;
                Ok(Expr::w(op.0, arg, n.max(1)))
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

    /// Any formula up to `depth` deep.
    fn any(&self, rng: &mut Rng, depth: usize) -> Expr {
        if depth <= 1 || rng.chance(0.25) {
            return self.leaf(rng);
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

    /// A new formula, usually something comparable over time at the top.
    pub fn random(&self, rng: &mut Rng) -> Expr {
        let depth = 2 + rng.below(3);
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
                Expr::Col(_) => self.leaf(rng),
                _ => self.any(rng, 3),
            };
        }
        match e {
            Expr::Col(_) | Expr::Num(_) => e.clone(),
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
            Expr::F1(_, i) | Expr::W(_, i, _) => go(i, target, k),
            Expr::F2(_, a, b) => go(a, target, k).or_else(|| go(b, target, k)),
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
        let slope = parse("slope(x, 3)")
            .unwrap()
            .eval(&Cols(vec![("x".into(), vec![1.0, 3.0, 5.0, 7.0])]));
        assert!((slope[3] - 2.0).abs() < 1e-9);
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
