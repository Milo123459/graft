//! `--book BIDS,ASKS[:LEVELS]`: a stream that sends an order book once and then only
//! what changed (Lighter's `order_book/N`, …), kept as the whole book and read out
//! every `--every` as one line with the best levels on each side.
//!
//! A message with the book's lists in it is a snapshot when it's the first, or its
//! `type` says `subscribed` or `snapshot` (a reconnect sends a new one); otherwise each
//! level in it is a change: its new size, or 0 to take it out. Other messages (a
//! `connected` greeting, …) go through untouched. The line read out is the message with
//! its lists replaced by the book's best LEVELS (20) and `"type":"snapshot/book"`, so a
//! recording of them (`--record`) reads back as the same book.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub struct BookSpec {
    /// Where each side's list is, as JSON keys (`order_book.bids`).
    pub bids: Vec<String>,
    pub asks: Vec<String>,
    /// How many levels a side to read out.
    pub levels: usize,
}

impl BookSpec {
    /// `order_book.bids,order_book.asks` or `…:50`.
    pub fn parse(text: &str) -> Result<BookSpec, String> {
        let (paths, levels) = match text.rsplit_once(':') {
            Some((p, n)) => (
                p,
                n.trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("--book: '{n}' isn't a number of levels"))?,
            ),
            None => (text, 20),
        };
        let Some((b, a)) = paths.split_once(',') else {
            return Err(format!(
                "--book: give the bids' and the asks' lists, e.g. order_book.bids,order_book.asks (got '{text}')"
            ));
        };
        let keys = |p: &str| -> Vec<String> {
            p.trim()
                .split('.')
                .filter(|k| !k.is_empty())
                .map(str::to_string)
                .collect()
        };
        let (bids, asks) = (keys(b), keys(a));
        if bids.is_empty() || asks.is_empty() {
            return Err(format!("--book: '{text}' needs both lists"));
        }
        Ok(BookSpec { bids, asks, levels })
    }
}

/// A price, ordered (prices are finite).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Price(f64);

impl Eq for Price {}

impl PartialOrd for Price {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Price {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// How the source writes a level: `{"price": …, "size": …}` (with its own key names)
/// or `[price, size]`.
#[derive(Debug, Clone, PartialEq)]
enum Style {
    Object { price: String, size: String },
    Pair,
}

const PRICE_KEYS: [&str; 4] = ["price", "px", "p", "rate"];
const SIZE_KEYS: [&str; 7] = ["size", "sz", "qty", "quantity", "q", "amount", "volume"];

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

/// A level's price and size, and how it was written.
fn level(v: &Value) -> Option<(f64, f64, Style)> {
    match v {
        Value::Object(m) => {
            let pk = PRICE_KEYS.iter().find(|k| m.contains_key(**k))?;
            let sk = SIZE_KEYS.iter().find(|k| m.contains_key(**k))?;
            Some((
                number(&m[*pk])?,
                number(&m[*sk])?,
                Style::Object {
                    price: pk.to_string(),
                    size: sk.to_string(),
                },
            ))
        }
        Value::Array(a) if a.len() >= 2 => Some((number(&a[0])?, number(&a[1])?, Style::Pair)),
        _ => None,
    }
}

fn get<'a>(v: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, k| v.get(k))
}

fn set(v: &mut Value, path: &[String], to: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut at = v;
    for k in parents {
        let Some(next) = at.get_mut(k) else { return };
        at = next;
    }
    if let Value::Object(m) = at {
        m.insert(last.clone(), to);
    }
}

pub struct Book {
    spec: BookSpec,
    bids: BTreeMap<Price, f64>,
    asks: BTreeMap<Price, f64>,
    style: Style,
    /// A snapshot has come (changes before one mean nothing).
    started: bool,
    /// When it was last read out, and how often to.
    last_out: f64,
    every: f64,
}

impl Book {
    pub fn new(spec: BookSpec, every: f64) -> Book {
        Book {
            spec,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            style: Style::Object {
                price: "price".into(),
                size: "size".into(),
            },
            started: false,
            last_out: f64::NEG_INFINITY,
            every,
        }
    }

    /// Take in a message read at `now` (seconds). Returns the line to parse: the book
    /// read out (a snapshot, or a change when one's due), the message itself when it
    /// isn't the book's, or `None` (a change, kept for the next read-out).
    pub fn apply(&mut self, line: &str, now: f64) -> Option<String> {
        let Ok(mut v) = serde_json::from_str::<Value>(line) else {
            return Some(line.to_string());
        };
        let (b, a) = (get(&v, &self.spec.bids), get(&v, &self.spec.asks));
        let lists = |x: Option<&Value>| x.and_then(Value::as_array).cloned().unwrap_or_default();
        if b.and_then(Value::as_array).is_none() && a.and_then(Value::as_array).is_none() {
            return Some(line.to_string());
        }
        let (b, a) = (lists(b), lists(a));
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let snapshot = !self.started || kind.contains("subscribed") || kind.contains("snapshot");
        if snapshot {
            self.bids.clear();
            self.asks.clear();
        }
        self.started = true;
        for (list, side) in [(&b, &mut self.bids), (&a, &mut self.asks)] {
            for l in list {
                let Some((p, s, style)) = level(l) else {
                    continue;
                };
                self.style = style;
                if s > 0.0 {
                    side.insert(Price(p), s);
                } else {
                    side.remove(&Price(p));
                }
            }
        }
        if !snapshot && now - self.last_out < self.every {
            return None;
        }
        self.last_out = now;
        let write = |p: f64, s: f64| match &self.style {
            Style::Object { price, size } => {
                let mut m = Map::new();
                m.insert(price.clone(), p.into());
                m.insert(size.clone(), s.into());
                Value::Object(m)
            }
            Style::Pair => Value::Array(vec![p.into(), s.into()]),
        };
        let n = self.spec.levels;
        let bids: Vec<Value> = self
            .bids
            .iter()
            .rev()
            .take(n)
            .map(|(p, s)| write(p.0, *s))
            .collect();
        let asks: Vec<Value> = self
            .asks
            .iter()
            .take(n)
            .map(|(p, s)| write(p.0, *s))
            .collect();
        set(&mut v, &self.spec.bids, Value::Array(bids));
        set(&mut v, &self.spec.asks, Value::Array(asks));
        if let Value::Object(m) = &mut v {
            m.insert("type".into(), "snapshot/book".into());
        }
        Some(v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lighter(kind: &str, ts: u64, bids: &[(&str, &str)], asks: &[(&str, &str)]) -> String {
        let side = |l: &[(&str, &str)]| -> Vec<Value> {
            l.iter()
                .map(|(p, s)| serde_json::json!({"price": p, "size": s}))
                .collect()
        };
        serde_json::json!({
            "channel": "order_book:1",
            "order_book": {"code": 0, "bids": side(bids), "asks": side(asks)},
            "timestamp": ts,
            "type": kind,
        })
        .to_string()
    }

    /// Each side's (price, size), best first.
    type Sides = (Vec<(f64, f64)>, Vec<(f64, f64)>);

    fn sides(line: &str) -> Sides {
        let v: Value = serde_json::from_str(line).unwrap();
        let read = |k: &str| {
            v["order_book"][k]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| (number(&l["price"]).unwrap(), number(&l["size"]).unwrap()))
                .collect()
        };
        (read("bids"), read("asks"))
    }

    #[test]
    fn keeps_the_book_from_a_snapshot_and_its_changes() {
        let spec = BookSpec::parse("order_book.bids,order_book.asks:2").unwrap();
        assert_eq!(spec.levels, 2);
        let mut book = Book::new(spec, 1.0);
        // Not the book's: through as it is.
        let hello = r#"{"type":"connected"}"#;
        assert_eq!(book.apply(hello, 0.0).as_deref(), Some(hello));
        // The snapshot is read out at once, best first, LEVELS a side.
        let snap = lighter(
            "subscribed/order_book",
            1,
            &[("99", "1"), ("100", "2"), ("98", "5")],
            &[("102", "3"), ("101", "1")],
        );
        let out = book.apply(&snap, 0.0).unwrap();
        assert_eq!(
            sides(&out),
            (
                vec![(100.0, 2.0), (99.0, 1.0)],
                vec![(101.0, 1.0), (102.0, 3.0)]
            )
        );
        assert!(out.contains(r#""type":"snapshot/book""#) && out.contains(r#""timestamp":1"#));
        // Changes: a new size, a level gone (0), a new one. Kept until a read-out's due.
        let change = lighter(
            "update/order_book",
            2,
            &[("100", "0.00000"), ("99.5", "4")],
            &[("101", "7")],
        );
        assert_eq!(book.apply(&change, 0.5), None);
        let change = lighter("update/order_book", 3, &[], &[("101", "0")]);
        let out = book.apply(&change, 1.2).unwrap();
        assert_eq!(
            sides(&out),
            (vec![(99.5, 4.0), (99.0, 1.0)], vec![(102.0, 3.0)])
        );
        // What it read out is a snapshot itself: read back (a recording), it's the
        // same book, whatever came before.
        let mut again = Book::new(
            BookSpec::parse("order_book.bids,order_book.asks:2").unwrap(),
            1.0,
        );
        let _ = again.apply(&snap, 0.0);
        assert_eq!(sides(&again.apply(&out, 0.0).unwrap()), sides(&out));
        // A reconnect's new snapshot starts the book over.
        let snap2 = lighter("subscribed/order_book", 4, &[("90", "1")], &[("91", "1")]);
        assert_eq!(
            sides(&book.apply(&snap2, 1.3).unwrap()),
            (vec![(90.0, 1.0)], vec![(91.0, 1.0)])
        );
    }

    #[test]
    fn a_session_reads_the_book_out_and_records_what_it_read() {
        let dir = std::env::temp_dir().join(format!("graft-book-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rec = dir.join("book.jsonl");
        let cli = <crate::cli::Cli as clap::Parser>::parse_from([
            "graft",
            "--json",
            "--time-field",
            "timestamp",
            "--book",
            "order_book.bids,order_book.asks:3",
            "--series",
            "bid = order_book_bids_0_price",
            "--record",
            rec.to_str().unwrap(),
        ]);
        let script = std::rc::Rc::new(crate::script::ScriptHost::new(None, false).unwrap());
        let mut s = crate::session::Session::new(&cli, script).unwrap();
        let at = |t: f64| crate::data::LineMeta {
            arrival: t,
            sample: None,
        };
        let snap = lighter(
            "subscribed/order_book",
            1_000,
            &[("100", "1")],
            &[("101", "1")],
        );
        assert!(s.feed(&snap, &at(0.0)).is_ok());
        // A change before the next read-out (every second): kept, not a point.
        let change = lighter("update/order_book", 1_500, &[("100.5", "2")], &[]);
        assert!(s.feed(&change, &at(0.5)).is_err());
        let change = lighter(
            "update/order_book",
            2_100,
            &[],
            &[("101", "0"), ("102", "1")],
        );
        assert!(s.feed(&change, &at(1.1)).is_ok());
        let ds = s.base_dataset();
        let bid = &ds.get("bid").unwrap().points;
        assert_eq!(bid.iter().map(|p| p.1).collect::<Vec<_>>(), [100.0, 100.5]);
        // What's recorded is what was read out (two lines, each the whole book).
        drop(s);
        let lines = std::fs::read_to_string(&rec).unwrap();
        assert_eq!(lines.lines().count(), 2, "{lines}");
        assert!(lines.lines().all(|l| l.contains("snapshot/book")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn levels_written_as_pairs_stay_pairs() {
        let mut book = Book::new(BookSpec::parse("b,a").unwrap(), 0.0);
        let out = book
            .apply(r#"{"b":[["100","1"],["99","2"]],"a":[["101","3"]]}"#, 0.0)
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["b"][0], serde_json::json!([100.0, 1.0]));
        assert_eq!(v["a"][0], serde_json::json!([101.0, 3.0]));
        assert!(BookSpec::parse("order_book.bids").is_err());
        assert!(BookSpec::parse("b,a:zero").is_err());
    }
}
