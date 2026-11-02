//! Panes: the chart area split into several charts, zellij-style. Each pane picks its
//! own series, chart type and lines; zoom, cursor and timeframe are shared so the panes
//! line up in time.
//!
//! On the command line a pane is `--pane 'node1,node2 kind=bar lines=live,avg'` and the
//! arrangement is `--layout '1 | 2 / 3'`: `|` puts panes side by side, `/` stacks them
//! (`|` binds tighter), parentheses group, and `|60%` gives the left part 60%.

use ratatui::layout::Rect;

use crate::cli::{ChartKind, Lines};

/// What one pane shows. `None`/empty fields follow the global settings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pane {
    /// Series in this pane, in order; empty = all of them.
    pub series: Vec<String>,
    pub kind: Option<ChartKind>,
    pub lines: Option<Lines>,
    /// Data from elsewhere: a preset, saved profile or recent chart (`cpu`, `@2`).
    pub source: Option<String>,
}

impl Pane {
    /// `node1,node2 kind=bar lines=live,avg` (`*` or nothing = all series).
    pub fn parse(spec: &str) -> Result<Pane, String> {
        let mut pane = Pane::default();
        for word in spec.split_whitespace() {
            if let Some(v) = word.strip_prefix("kind=") {
                pane.kind = Some(
                    <ChartKind as clap::ValueEnum>::from_str(v, true)
                        .map_err(|_| format!("pane: unknown chart type '{v}' (line, scatter, bar, histogram, sparkline)"))?,
                );
            } else if let Some(v) = word.strip_prefix("source=") {
                pane.source = (!v.is_empty()).then(|| v.to_string());
            } else if let Some(v) = word.strip_prefix("lines=") {
                pane.lines = Some(Lines::parse(v).map_err(|e| format!("pane: {e}"))?);
            } else if word.contains('=') {
                return Err(format!(
                    "pane: unknown setting '{word}' (kind=…, lines=…, source=…)"
                ));
            } else if word != "*" {
                pane.series
                    .extend(word.split(',').filter(|s| !s.is_empty()).map(String::from));
            }
        }
        Ok(pane)
    }

    pub fn to_spec(&self) -> String {
        let mut words = vec![if self.series.is_empty() {
            "*".to_string()
        } else {
            self.series.join(",")
        }];
        if let Some(k) = self.kind {
            words.push(format!("kind={}", k.name()));
        }
        if let Some(l) = &self.lines {
            words.push(format!("lines={}", l.to_arg()));
        }
        if let Some(src) = &self.source {
            words.push(format!("source={src}"));
        }
        words.join(" ")
    }

    pub fn shows(&self, name: &str) -> bool {
        self.series.is_empty() || self.series.iter().any(|s| s == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// `a | b`: side by side.
    Across,
    /// `a / b`: a above b.
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(usize),
    Split {
        split: Split,
        /// Share of the first part, 0..1.
        ratio: f64,
        a: Box<Node>,
        b: Box<Node>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Panes {
    pub root: Node,
    pub panes: Vec<Pane>,
    pub focus: usize,
}

impl Panes {
    pub fn single(pane: Pane) -> Panes {
        Panes {
            root: Node::Leaf(0),
            panes: vec![pane],
            focus: 0,
        }
    }

    /// Two panes side by side (or stacked).
    pub fn pair(left: Pane, right: Pane, split: Split) -> Panes {
        Panes {
            root: Node::Split {
                split,
                ratio: 0.5,
                a: Box::new(Node::Leaf(0)),
                b: Box::new(Node::Leaf(1)),
            },
            panes: vec![left, right],
            focus: 0,
        }
    }

    /// From `--pane` specs and an optional `--layout`. Without a layout, two panes go
    /// side by side and more go in a grid two wide.
    pub fn from_args(specs: &[String], layout: Option<&str>) -> Result<Option<Panes>, String> {
        let mut panes: Vec<Pane> = specs
            .iter()
            .map(|s| Pane::parse(s))
            .collect::<Result<_, _>>()?;
        let root = match layout {
            Some(expr) => {
                let root = parse_layout(expr)?;
                let max = leaves(&root).into_iter().max().unwrap_or(0);
                if panes.len() <= max {
                    panes.resize(max + 1, Pane::default());
                }
                let mut seen = leaves(&root);
                seen.sort_unstable();
                seen.dedup();
                if seen.len() != leaves(&root).len() {
                    return Err("--layout: each pane number may appear once".into());
                }
                // Panes left out of the layout are dropped.
                let keep: Vec<usize> = seen;
                let renumber = |i: usize| keep.iter().position(|k| *k == i).unwrap_or(0);
                let root = map_leaves(root, &renumber);
                panes = keep.iter().map(|&i| panes[i].clone()).collect();
                root
            }
            None if panes.is_empty() => return Ok(None),
            None => grid(panes.len()),
        };
        Ok(Some(Panes {
            root,
            panes,
            focus: 0,
        }))
    }

    pub fn layout_expr(&self) -> String {
        expr(&self.root, true)
    }

    pub fn count(&self) -> usize {
        self.panes.len()
    }

    /// Each pane's area.
    pub fn rects(&self, area: Rect) -> Vec<(usize, Rect)> {
        let mut out = Vec::new();
        place(&self.root, area, &mut out);
        out
    }

    /// Split the focused pane; the new pane copies it and takes the focus.
    pub fn split(&mut self, split: Split) {
        let new = self.panes.len();
        self.panes.push(self.panes[self.focus].clone());
        let focus = self.focus;
        replace_leaf(&mut self.root, focus, &mut |leaf| Node::Split {
            split,
            ratio: 0.5,
            a: Box::new(leaf),
            b: Box::new(Node::Leaf(new)),
        });
        self.focus = new;
    }

    /// Close the focused pane (not the last one).
    pub fn close(&mut self) -> bool {
        if self.panes.len() <= 1 {
            return false;
        }
        let gone = self.focus;
        self.root = remove_leaf(self.root.clone(), gone).expect("another pane remains");
        self.panes.remove(gone);
        self.root = map_leaves(self.root.clone(), &|i| if i > gone { i - 1 } else { i });
        self.focus = gone.min(self.panes.len() - 1);
        true
    }

    /// The pane next to the focused one in a direction.
    pub fn neighbour(&self, area: Rect, dir: Dir) -> Option<usize> {
        let rects = self.rects(area);
        let me = rects.iter().find(|r| r.0 == self.focus)?.1;
        let (cx, cy) = (
            me.x as i32 + me.width as i32 / 2,
            me.y as i32 + me.height as i32 / 2,
        );
        rects
            .iter()
            .filter(|(i, _)| *i != self.focus)
            .filter(|(_, r)| match dir {
                Dir::Left => r.right() <= me.x,
                Dir::Right => r.x >= me.right(),
                Dir::Up => r.bottom() <= me.y,
                Dir::Down => r.y >= me.bottom(),
            })
            .min_by_key(|(_, r)| {
                let (x, y) = (
                    r.x as i32 + r.width as i32 / 2,
                    r.y as i32 + r.height as i32 / 2,
                );
                // Prefer panes in line with this one.
                let (along, across) = match dir {
                    Dir::Left | Dir::Right => ((x - cx).abs(), (y - cy).abs()),
                    Dir::Up | Dir::Down => ((y - cy).abs(), (x - cx).abs()),
                };
                along + 3 * across
            })
            .map(|(i, _)| *i)
    }

    pub fn focus_dir(&mut self, area: Rect, dir: Dir) -> bool {
        match self.neighbour(area, dir) {
            Some(i) => {
                self.focus = i;
                true
            }
            None => false,
        }
    }

    /// Swap the focused pane with its neighbour; the focus moves with it.
    pub fn move_dir(&mut self, area: Rect, dir: Dir) -> bool {
        let Some(other) = self.neighbour(area, dir) else {
            return false;
        };
        self.panes.swap(self.focus, other);
        self.focus = other;
        true
    }

    /// Move a border of the focused pane towards `dir` by `step` (a share of the split):
    /// the edge on that side if it has one, else the opposite edge (zellij-style).
    pub fn resize(&mut self, dir: Dir, step: f64) -> bool {
        let path = path_to(&self.root, self.focus).unwrap_or_default();
        let want = match dir {
            Dir::Left | Dir::Right => Split::Across,
            Dir::Up | Dir::Down => Split::Down,
        };
        let towards_end = matches!(dir, Dir::Right | Dir::Down);
        let splits: Vec<usize> = (0..path.len())
            .rev()
            .filter(|&d| matches!(node_at_mut(&mut self.root.clone(), &path[..d]), Node::Split { split, .. } if *split == want))
            .collect();
        // The edge on the `dir` side: the pane is in the first part when it's the right/bottom edge.
        let depth = splits
            .iter()
            .copied()
            .find(|&d| !path[d] == towards_end)
            .or_else(|| splits.first().copied());
        let Some(depth) = depth else {
            return false;
        };
        if let Node::Split { ratio, .. } = node_at_mut(&mut self.root, &path[..depth]) {
            let delta = if towards_end { step } else { -step };
            *ratio = (*ratio + delta).clamp(0.1, 0.9);
        }
        true
    }

    /// The border under (col, row), as a split to drag: (path, its area).
    pub fn border_at(&self, area: Rect, col: u16, row: u16) -> Option<(Vec<bool>, Rect)> {
        let mut found = None;
        find_border(&self.root, area, col, row, &mut Vec::new(), &mut found);
        found
    }

    /// Drag a border (from `border_at`) to (col, row).
    pub fn drag_border(&mut self, path: &[bool], area: Rect, col: u16, row: u16) {
        if let Node::Split { split, ratio, .. } = node_at_mut(&mut self.root, path) {
            let r = match split {
                Split::Across => {
                    (col.saturating_sub(area.x) as f64 + 0.5) / area.width.max(1) as f64
                }
                Split::Down => {
                    (row.saturating_sub(area.y) as f64 + 0.5) / area.height.max(1) as f64
                }
            };
            *ratio = r.clamp(0.1, 0.9);
        }
    }

    /// Make every split even.
    pub fn equalize(&mut self) {
        fn eq(n: &mut Node) {
            if let Node::Split { ratio, a, b, .. } = n {
                *ratio = 0.5;
                eq(a);
                eq(b);
            }
        }
        eq(&mut self.root);
    }
}

fn grid(n: usize) -> Node {
    let row = |from: usize, len: usize| {
        chain(Split::Across, (from..from + len).map(Node::Leaf).collect())
    };
    if n <= 2 {
        return row(0, n);
    }
    let rows: Vec<Node> = (0..n).step_by(2).map(|i| row(i, (n - i).min(2))).collect();
    chain(Split::Down, rows)
}

/// `a op b op c` with equal shares.
fn chain(split: Split, mut parts: Vec<Node>) -> Node {
    if parts.len() == 1 {
        return parts.pop().unwrap();
    }
    let n = parts.len();
    let first = parts.remove(0);
    Node::Split {
        split,
        ratio: 1.0 / n as f64,
        a: Box::new(first),
        b: Box::new(chain(split, parts)),
    }
}

fn place(node: &Node, area: Rect, out: &mut Vec<(usize, Rect)>) {
    match node {
        Node::Leaf(i) => out.push((*i, area)),
        Node::Split { split, ratio, a, b } => {
            let (ra, rb) = divide(area, *split, *ratio);
            place(a, ra, out);
            place(b, rb, out);
        }
    }
}

fn divide(area: Rect, split: Split, ratio: f64) -> (Rect, Rect) {
    match split {
        Split::Across => {
            let w = ((area.width as f64 * ratio).round() as u16)
                .clamp(1.min(area.width), area.width.saturating_sub(1).max(1));
            (
                Rect { width: w, ..area },
                Rect {
                    x: area.x + w,
                    width: area.width - w,
                    ..area
                },
            )
        }
        Split::Down => {
            let h = ((area.height as f64 * ratio).round() as u16)
                .clamp(1.min(area.height), area.height.saturating_sub(1).max(1));
            (
                Rect { height: h, ..area },
                Rect {
                    y: area.y + h,
                    height: area.height - h,
                    ..area
                },
            )
        }
    }
}

fn find_border(
    node: &Node,
    area: Rect,
    col: u16,
    row: u16,
    path: &mut Vec<bool>,
    found: &mut Option<(Vec<bool>, Rect)>,
) {
    if let Node::Split { split, ratio, a, b } = node {
        let (ra, rb) = divide(area, *split, *ratio);
        let hit = match split {
            // The two border columns either side of the line between the panes.
            Split::Across => {
                (col + 1 == rb.x || col == rb.x) && row >= area.y && row < area.bottom()
            }
            Split::Down => (row + 1 == rb.y || row == rb.y) && col >= area.x && col < area.right(),
        };
        if hit {
            *found = Some((path.clone(), area));
        }
        path.push(false);
        find_border(a, ra, col, row, path, found);
        path.pop();
        path.push(true);
        find_border(b, rb, col, row, path, found);
        path.pop();
    }
}

fn leaves(node: &Node) -> Vec<usize> {
    match node {
        Node::Leaf(i) => vec![*i],
        Node::Split { a, b, .. } => {
            let mut v = leaves(a);
            v.extend(leaves(b));
            v
        }
    }
}

fn map_leaves(node: Node, f: &dyn Fn(usize) -> usize) -> Node {
    match node {
        Node::Leaf(i) => Node::Leaf(f(i)),
        Node::Split { split, ratio, a, b } => Node::Split {
            split,
            ratio,
            a: Box::new(map_leaves(*a, f)),
            b: Box::new(map_leaves(*b, f)),
        },
    }
}

fn replace_leaf(node: &mut Node, id: usize, f: &mut dyn FnMut(Node) -> Node) -> bool {
    match node {
        Node::Leaf(i) if *i == id => {
            let leaf = std::mem::replace(node, Node::Leaf(id));
            *node = f(leaf);
            true
        }
        Node::Leaf(_) => false,
        Node::Split { a, b, .. } => replace_leaf(a, id, f) || replace_leaf(b, id, f),
    }
}

/// The tree without a leaf (its sibling takes the parent's place).
fn remove_leaf(node: Node, id: usize) -> Option<Node> {
    match node {
        Node::Leaf(i) if i == id => None,
        Node::Leaf(_) => Some(node),
        Node::Split { split, ratio, a, b } => match (remove_leaf(*a, id), remove_leaf(*b, id)) {
            (Some(a), Some(b)) => Some(Node::Split {
                split,
                ratio,
                a: Box::new(a),
                b: Box::new(b),
            }),
            (Some(n), None) | (None, Some(n)) => Some(n),
            (None, None) => None,
        },
    }
}

/// Path to a leaf: false = first part, true = second.
fn path_to(node: &Node, id: usize) -> Option<Vec<bool>> {
    match node {
        Node::Leaf(i) => (*i == id).then(Vec::new),
        Node::Split { a, b, .. } => {
            if let Some(mut p) = path_to(a, id) {
                p.insert(0, false);
                Some(p)
            } else {
                let mut p = path_to(b, id)?;
                p.insert(0, true);
                Some(p)
            }
        }
    }
}

fn node_at_mut<'a>(mut node: &'a mut Node, path: &[bool]) -> &'a mut Node {
    for &second in path {
        match node {
            Node::Split { a, b, .. } => node = if second { b } else { a },
            Node::Leaf(_) => break,
        }
    }
    node
}

fn expr(node: &Node, top: bool) -> String {
    match node {
        Node::Leaf(i) => (i + 1).to_string(),
        Node::Split { split, ratio, a, b } => {
            let op = match split {
                Split::Across => "|",
                Split::Down => "/",
            };
            let pct = if (ratio - 0.5).abs() < 0.005 {
                String::new()
            } else {
                // Whole percents, unless that would lose a small share.
                let p = ratio * 100.0;
                if p.round() >= 10.0 && 100.0 - p.round() >= 10.0 {
                    format!("{}%", p.round())
                } else {
                    format!("{}%", (p * 10.0).round() / 10.0)
                }
            };
            let s = format!("{} {op}{pct} {}", expr(a, false), expr(b, false));
            if top { s } else { format!("({s})") }
        }
    }
}

/// Parse `1 | 2 / 3`, `(1 / 2) |60% 3`.
pub fn parse_layout(src: &str) -> Result<Node, String> {
    let tokens = tokenize(src)?;
    let mut pos = 0;
    let node = parse_stack(&tokens, &mut pos)?;
    if pos != tokens.len() {
        return Err(format!(
            "--layout: unexpected '{}' in '{src}'",
            tokens[pos].text()
        ));
    }
    Ok(node)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(usize),
    Op(Split, Option<f64>),
    Open,
    Close,
}

impl Tok {
    fn text(&self) -> String {
        match self {
            Tok::Num(n) => n.to_string(),
            Tok::Op(Split::Across, _) => "|".into(),
            Tok::Op(Split::Down, _) => "/".into(),
            Tok::Open => "(".into(),
            Tok::Close => ")".into(),
        }
    }
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => i += 1,
            '(' => {
                out.push(Tok::Open);
                i += 1;
            }
            ')' => {
                out.push(Tok::Close);
                i += 1;
            }
            '|' | '/' => {
                let split = if c == '|' { Split::Across } else { Split::Down };
                i += 1;
                // Optional share: `|60%`.
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let pct = if i < chars.len() && chars[i] == '%' && i > start {
                    let v: f64 = chars[start..i]
                        .iter()
                        .collect::<String>()
                        .parse()
                        .map_err(|_| "--layout: bad percentage")?;
                    i += 1;
                    if !(v > 0.0 && v < 100.0) {
                        return Err(format!("--layout: {v}% should be between 0% and 100%"));
                    }
                    Some(v / 100.0)
                } else {
                    i = start;
                    None
                };
                out.push(Tok::Op(split, pct));
            }
            d if d.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let n: usize = chars[start..i]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0);
                if n == 0 {
                    return Err("--layout: panes are numbered from 1".into());
                }
                out.push(Tok::Num(n - 1));
            }
            other => {
                return Err(format!(
                    "--layout: unexpected '{other}' (use pane numbers, | side by side, / stacked, parentheses)"
                ));
            }
        }
    }
    Ok(out)
}

fn parse_level(t: &[Tok], pos: &mut usize, split: Split) -> Result<Node, String> {
    let sub = |t: &[Tok], pos: &mut usize| match split {
        Split::Down => parse_level(t, pos, Split::Across),
        Split::Across => parse_atom(t, pos),
    };
    let mut parts = vec![sub(t, pos)?];
    let mut shares: Vec<Option<f64>> = Vec::new();
    while let Some(Tok::Op(s, pct)) = t.get(*pos) {
        if *s != split {
            break;
        }
        shares.push(*pct);
        *pos += 1;
        parts.push(sub(t, pos)?);
    }
    if shares.iter().all(Option::is_none) {
        return Ok(chain(split, parts));
    }
    // `a |20% b |30% c`: each share is the part before it, out of the whole; parts
    // without one split what's left equally.
    let given: f64 = shares.iter().flatten().sum();
    if given >= 1.0 {
        return Err(format!(
            "--layout: the shares add up to {:.0}% — leave room for the last pane",
            given * 100.0
        ));
    }
    let open = parts.len() - shares.iter().flatten().count();
    let rest = (1.0 - given) / open as f64;
    let mut sizes: Vec<f64> = shares.iter().map(|s| s.unwrap_or(rest)).collect();
    sizes.push(rest);
    Ok(chain_sized(split, parts, &sizes))
}

/// `a op b op c` with the given shares of the whole.
fn chain_sized(split: Split, mut parts: Vec<Node>, sizes: &[f64]) -> Node {
    if parts.len() == 1 {
        return parts.pop().unwrap();
    }
    let total: f64 = sizes.iter().sum();
    let first = parts.remove(0);
    Node::Split {
        split,
        ratio: (sizes[0] / total).clamp(0.01, 0.99),
        a: Box::new(first),
        b: Box::new(chain_sized(split, parts, &sizes[1..])),
    }
}

fn parse_stack(t: &[Tok], pos: &mut usize) -> Result<Node, String> {
    parse_level(t, pos, Split::Down)
}

fn parse_atom(t: &[Tok], pos: &mut usize) -> Result<Node, String> {
    match t.get(*pos) {
        Some(Tok::Num(n)) => {
            *pos += 1;
            Ok(Node::Leaf(*n))
        }
        Some(Tok::Open) => {
            *pos += 1;
            let node = parse_stack(t, pos)?;
            match t.get(*pos) {
                Some(Tok::Close) => {
                    *pos += 1;
                    Ok(node)
                }
                _ => Err("--layout: missing ')'".into()),
            }
        }
        Some(other) => Err(format!(
            "--layout: expected a pane number, found '{}'",
            other.text()
        )),
        None => Err("--layout: expected a pane number at the end".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 100, 40)
    }

    #[test]
    fn layout_expressions_round_trip() {
        for src in [
            "1 | 2",
            "1 / 2",
            "(1 | 2) / 3",
            "1 |60% (2 / 3)",
            "1 |33% (2 | 3)",
        ] {
            let node = parse_layout(src).unwrap();
            let p = Panes {
                root: node,
                panes: vec![Pane::default(); 3],
                focus: 0,
            };
            assert_eq!(parse_layout(&p.layout_expr()).unwrap(), p.root, "{src}");
        }
        // `|` binds tighter than `/`.
        let p = Panes::from_args(&[], Some("1 | 2 / 3")).unwrap().unwrap();
        assert_eq!(p.layout_expr(), "(1 | 2) / 3");
        assert_eq!(p.count(), 3);
        let thirds = Panes::from_args(&[], Some("1|2|3")).unwrap().unwrap();
        let widths: Vec<u16> = thirds
            .rects(Rect::new(0, 0, 99, 10))
            .iter()
            .map(|r| r.1.width)
            .collect();
        assert_eq!(widths, [33, 33, 33]);
        // Shares in longer chains: each is the pane before it; the rest split what's left.
        let p = Panes::from_args(&[], Some("1 |20% 2 |30% 3"))
            .unwrap()
            .unwrap();
        let widths: Vec<u16> = p
            .rects(Rect::new(0, 0, 100, 10))
            .iter()
            .map(|r| r.1.width)
            .collect();
        assert_eq!(widths, [20, 30, 50]);
        let p = Panes::from_args(&[], Some("1 |50% 2 | 3"))
            .unwrap()
            .unwrap();
        let widths: Vec<u16> = p
            .rects(Rect::new(0, 0, 100, 10))
            .iter()
            .map(|r| r.1.width)
            .collect();
        assert_eq!(widths, [50, 25, 25]);
        assert!(parse_layout("1 |60% 2 |50% 3").is_err());
        assert!(parse_layout("1 |0% 2").is_err());
        // Small shares (many panes in a row) survive a round trip.
        let many = (1..=12)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        let p = Panes::from_args(&[], Some(&many)).unwrap().unwrap();
        let again = parse_layout(&p.layout_expr()).unwrap();
        let q = Panes {
            root: again,
            panes: p.panes.clone(),
            focus: 0,
        };
        let w = |p: &Panes| -> Vec<u16> {
            p.rects(Rect::new(0, 0, 120, 10))
                .iter()
                .map(|r| r.1.width)
                .collect()
        };
        assert_eq!(w(&p), w(&q));
        assert!(parse_layout("1 | ").is_err());
        assert!(parse_layout("1 + 2").is_err());
        assert!(Panes::from_args(&[], Some("1 | 1")).is_err());
    }

    #[test]
    fn pane_specs() {
        let p = Pane::parse("node1,node2 kind=bar lines=live,avg source=@2").unwrap();
        assert_eq!(p.source.as_deref(), Some("@2"));
        assert_eq!(p.series, ["node1", "node2"]);
        assert_eq!(p.kind, Some(ChartKind::Bar));
        assert_eq!(Pane::parse(&p.to_spec()).unwrap(), p);
        assert_eq!(Pane::parse("*").unwrap(), Pane::default());
        assert!(Pane::parse("kind=pie").is_err());
        // Without a layout: two side by side, more in a grid.
        let specs: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let p = Panes::from_args(&specs, None).unwrap().unwrap();
        assert_eq!(p.layout_expr(), "(1 | 2) / 3");
    }

    #[test]
    fn split_focus_move_resize_close() {
        let mut p = Panes {
            root: Node::Leaf(0),
            panes: vec![Pane::default()],
            focus: 0,
        };
        p.panes[0].kind = Some(ChartKind::Bar);
        p.split(Split::Across);
        assert_eq!((p.count(), p.focus), (2, 1));
        assert_eq!(
            p.panes[1].kind,
            Some(ChartKind::Bar),
            "a new pane copies the old"
        );
        p.panes[1].kind = Some(ChartKind::Line);
        p.split(Split::Down);
        assert_eq!(p.layout_expr(), "1 | (2 / 3)");
        // Focus moves by position.
        assert!(p.focus_dir(area(), Dir::Up));
        assert_eq!(p.focus, 1);
        assert!(p.focus_dir(area(), Dir::Left));
        assert_eq!(p.focus, 0);
        assert!(!p.focus_dir(area(), Dir::Left));
        // Moving swaps contents and keeps the focus on the moved pane.
        assert!(p.move_dir(area(), Dir::Right));
        assert_eq!((p.focus, p.panes[p.focus].kind), (1, Some(ChartKind::Bar)));
        // Resize: pane 1 (left) grows to the right.
        p.focus = 0;
        assert!(p.resize(Dir::Right, 0.1));
        assert_eq!(p.rects(area())[0].1.width, 60);
        // The left pane has no left edge: its right edge moves left instead.
        assert!(p.resize(Dir::Left, 0.1));
        assert_eq!(p.rects(area())[0].1.width, 50);
        assert!(
            !p.resize(Dir::Up, 0.1),
            "nothing above or below the left pane"
        );
        // Drag the border between the columns.
        let col = p.rects(area())[1].1.x;
        let (path, a) = p.border_at(area(), col, 5).unwrap();
        p.drag_border(&path, a, 30, 5);
        assert_eq!(p.rects(area())[0].1.width, 31);
        // Close the middle pane: its sibling takes its place.
        p.focus = 1;
        assert!(p.close());
        assert_eq!(p.layout_expr().replace("31%", "x"), "1 |x 2");
        assert!(p.close());
        assert!(!p.close(), "the last pane stays");
    }
}
