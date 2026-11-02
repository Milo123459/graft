//! Unit-aware number parsing and human-friendly formatting.
//!
//! Values such as `59.9 GB`, `1.2G`, `512 kB`, `30ms` or `15%` are normalised to a
//! base unit (bytes, seconds) so series can be compared, transformed and analysed,
//! then formatted back into a readable form for display.

use std::fmt;

/// The physical dimension of a value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Dim {
    /// A plain number with no known unit.
    #[default]
    None,
    /// Bytes. `binary` selects KiB/MiB/... (1024) for display.
    Bytes { binary: bool },
    /// Seconds.
    Seconds,
    /// Any other suffix, kept verbatim (`%`, `°C`, `req`, ...).
    Other(String),
    /// Money: the symbol goes in front, with cents (`$3,456.78`).
    Money(String),
}

/// The unit of a series: a dimension, optionally per second (after `--rate`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Unit {
    pub dim: Dim,
    pub rate: bool,
}

impl Unit {
    pub fn new(dim: Dim) -> Self {
        Self { dim, rate: false }
    }

    pub fn is_none(&self) -> bool {
        self.dim == Dim::None && !self.rate
    }

    /// Format a value in this unit, e.g. `58.8 GB`, `1.2 ms`, `3.4k/s`.
    pub fn format(&self, v: f64) -> String {
        let base = match &self.dim {
            Dim::None => fmt_compact(v),
            Dim::Bytes { binary } => fmt_bytes(v, *binary),
            Dim::Seconds => fmt_seconds(v),
            Dim::Other(s) => format!("{}{}", fmt_compact(v), s),
            Dim::Money(sym) => fmt_money(v, sym),
        };
        if self.rate { format!("{base}/s") } else { base }
    }

    /// The multiplier of the display unit used for a value of this size (e.g. 1e9
    /// for "GB"), so axis ticks can be round in the unit they're shown in.
    pub fn tick_scale(&self, v: f64) -> f64 {
        let a = v.abs();
        match &self.dim {
            Dim::Bytes { binary } => {
                let base: f64 = if *binary { 1024.0 } else { 1000.0 };
                let mut scale = 1.0;
                while a >= scale * base && scale < base.powi(6) {
                    scale *= base;
                }
                scale
            }
            Dim::Seconds => match a {
                a if a < 1e-6 => 1e-9,
                a if a < 1e-3 => 1e-6,
                a if a < 1.0 => 1e-3,
                a if a < 120.0 => 1.0,
                a if a < 7200.0 => 60.0,
                a if a < 172_800.0 => 3600.0,
                _ => 86400.0,
            },
            Dim::None if a >= 1e12 => 1e12,
            Dim::None if a >= 1e9 => 1e9,
            Dim::None if a >= 1e6 => 1e6,
            _ => 1.0,
        }
    }

    /// Short label for axis titles / tables.
    pub fn label(&self) -> String {
        let base = match &self.dim {
            Dim::None => String::new(),
            Dim::Bytes { .. } => "bytes".into(),
            Dim::Seconds => "seconds".into(),
            Dim::Other(s) | Dim::Money(s) => s.clone(),
        };
        match (base.is_empty(), self.rate) {
            (true, true) => "per second".into(),
            (false, true) => format!("{base}/s"),
            _ => base,
        }
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// Look up a unit suffix, returning its dimension and multiplier to the base unit.
///
/// SI byte prefixes are decimal (1 kB = 1000 B) unless `binary` is set, in which
/// case they're treated as 1024-based (the convention used by `/proc/meminfo`,
/// `df -h`, `free -h`, ...). IEC prefixes (KiB, MiB, ...) are always 1024-based.
pub fn lookup_unit(s: &str, binary: bool) -> Option<(Dim, f64)> {
    let si = |exp: i32| {
        if binary {
            1024f64.powi(exp)
        } else {
            1000f64.powi(exp)
        }
    };
    let bytes = |mult: f64| Some((Dim::Bytes { binary }, mult));
    match s {
        "B" | "b" | "bytes" | "byte" => bytes(1.0),
        "k" | "K" | "kB" | "KB" | "kb" => bytes(si(1)),
        "M" | "MB" | "mb" => bytes(si(2)),
        "G" | "GB" | "gb" => bytes(si(3)),
        "T" | "TB" | "tb" => bytes(si(4)),
        "P" | "PB" | "pb" => bytes(si(5)),
        "Ki" | "KiB" => Some((Dim::Bytes { binary: true }, 1024f64)),
        "Mi" | "MiB" => Some((Dim::Bytes { binary: true }, 1024f64.powi(2))),
        "Gi" | "GiB" => Some((Dim::Bytes { binary: true }, 1024f64.powi(3))),
        "Ti" | "TiB" => Some((Dim::Bytes { binary: true }, 1024f64.powi(4))),
        "Pi" | "PiB" => Some((Dim::Bytes { binary: true }, 1024f64.powi(5))),
        "ns" => Some((Dim::Seconds, 1e-9)),
        "us" | "µs" | "μs" => Some((Dim::Seconds, 1e-6)),
        "ms" => Some((Dim::Seconds, 1e-3)),
        "s" | "sec" | "secs" => Some((Dim::Seconds, 1.0)),
        "min" | "mins" => Some((Dim::Seconds, 60.0)),
        "h" | "hr" | "hrs" => Some((Dim::Seconds, 3600.0)),
        "%" => Some((Dim::Other("%".into()), 1.0)),
        "$" | "USD" | "usd" | "USDT" | "USDC" => Some((Dim::Money("$".into()), 1.0)),
        "€" | "EUR" | "eur" => Some((Dim::Money("€".into()), 1.0)),
        "£" | "GBP" | "gbp" => Some((Dim::Money("£".into()), 1.0)),
        _ => None,
    }
}

/// `$3,456.78`; small amounts keep more digits (`$0.01234`), big ones are compact
/// (`$1.23M`).
pub fn fmt_money(v: f64, sym: &str) -> String {
    let sign = if v < 0.0 { "-" } else { "" };
    let a = v.abs();
    if a >= 1e6 {
        let (d, s) = if a >= 1e12 {
            (1e12, "T")
        } else if a >= 1e9 {
            (1e9, "B")
        } else {
            (1e6, "M")
        };
        return format!(
            "{sign}{sym}{}{s}",
            fmt_sig(((a / d) * 100.0).round() / 100.0)
        );
    }
    if a > 0.0 && a < 1.0 {
        // Four significant digits.
        let decimals = (3 - a.log10().floor() as i32).max(2) as usize;
        return format!("{sign}{sym}{a:.decimals$}");
    }
    let cents = format!("{a:.2}");
    let (int, frac) = cents.split_once('.').unwrap_or((&cents, "00"));
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{sign}{sym}{grouped}.{frac}")
}

/// Split a leading number off `s`, returning the number and the remaining text.
pub fn split_number(s: &str) -> Option<(f64, &str)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'+') {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    let mantissa = &s[digits_start..i];
    if mantissa.is_empty() || mantissa == "." {
        return None;
    }
    // Optional exponent, only consumed if it is well formed.
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'-' || bytes[j] == b'+') {
            j += 1;
        }
        let exp_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            i = j;
        }
    }
    let n: f64 = s[..i].parse().ok()?;
    // 1e400 and friends: not a number anyone means.
    n.is_finite().then_some((n, &s[i..]))
}

/// Parse a complete value with an optional unit: `59.9 GB`, `1.2G`, `42`, `15%`.
///
/// Unknown suffixes are kept as [`Dim::Other`].
pub fn parse_value(s: &str, binary: bool) -> Option<(f64, Unit)> {
    let s = s.trim();
    // Money with the symbol in front: `$12.50`, `-€3`.
    let (sign, unsigned) = match s.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", s),
    };
    for sym in ['$', '€', '£'] {
        if let Some(rest) = unsigned.strip_prefix(sym) {
            let (n, tail) = split_number(rest.trim_start())?;
            if !tail.trim().is_empty() {
                return None;
            }
            let n = if sign == "-" { -n } else { n };
            return Some((n, Unit::new(Dim::Money(sym.to_string()))));
        }
    }
    let (n, rest) = split_number(s)?;
    apply_unit(n, rest, binary)
}

/// Combine a number with a unit suffix (which may be empty).
pub fn apply_unit(n: f64, suffix: &str, binary: bool) -> Option<(f64, Unit)> {
    let suffix = suffix.trim();
    if suffix.is_empty() {
        return Some((n, Unit::default()));
    }
    let (suffix, rate) = match suffix.strip_suffix("/s") {
        Some(s) => (s, true),
        None => (suffix, false),
    };
    if let Some((dim, mult)) = lookup_unit(suffix, binary) {
        return Some((n * mult, Unit { dim, rate }));
    }
    // A unit starts with a letter or a unit sign: `,234` (a thousands separator) or
    // `:00` (a time) aren't units.
    let unit_like = suffix.chars().next().is_some_and(|c| {
        c.is_alphabetic() || matches!(c, '%' | '°' | 'µ' | 'μ' | '$' | '€' | '£' | '\'')
    });
    if !unit_like || suffix.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    Some((
        n,
        Unit {
            dim: Dim::Other(suffix.to_string()),
            rate,
        },
    ))
}

/// Format a number compactly with up to 4 significant digits.
pub fn fmt_compact(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    let a = v.abs();
    if a >= 1e15 || (a > 0.0 && a < 1e-3) {
        return format!("{v:.2e}");
    }
    if a >= 1e6 {
        let (div, suffix) = if a >= 1e12 {
            (1e12, "T")
        } else if a >= 1e9 {
            (1e9, "G")
        } else {
            (1e6, "M")
        };
        return format!("{}{suffix}", fmt_sig(v / div));
    }
    fmt_sig(v)
}

/// Roughly 3–4 significant digits, trailing zeros trimmed.
pub fn fmt_sig(v: f64) -> String {
    let a = v.abs();
    let decimals = if a == 0.0 || a >= 1000.0 {
        0
    } else if a >= 10.0 {
        1
    } else if a >= 1.0 {
        2
    } else {
        3
    };
    let s = format!("{v:.decimals$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    };
    if s == "-0" { "0".into() } else { s }
}

fn fmt_bytes(v: f64, binary: bool) -> String {
    let (base, units): (f64, &[&str]) = if binary {
        (1024.0, &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"])
    } else {
        (1000.0, &["B", "kB", "MB", "GB", "TB", "PB", "EB"])
    };
    let mut x = v;
    let mut i = 0;
    while x.abs() >= base && i < units.len() - 1 {
        x /= base;
        i += 1;
    }
    format!("{} {}", fmt_sig(x), units[i])
}

fn fmt_seconds(v: f64) -> String {
    let a = v.abs();
    let (div, unit) = if a == 0.0 {
        (1.0, "s")
    } else if a < 1e-6 {
        (1e-9, "ns")
    } else if a < 1e-3 {
        (1e-6, "µs")
    } else if a < 1.0 {
        (1e-3, "ms")
    } else if a < 120.0 {
        (1.0, "s")
    } else if a < 7200.0 {
        (60.0, "min")
    } else if a < 172_800.0 {
        (3600.0, "h")
    } else {
        (86400.0, "d")
    };
    format!("{} {unit}", fmt_sig(v / div))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_must_look_like_units() {
        assert_eq!(parse_value("1,234", false), None);
        assert_eq!(parse_value("05:00", false), None);
        assert_eq!(parse_value("1e400", false), None);
        assert_eq!(parse_value("12ms", false).unwrap().0, 0.012);
        let (v, u) = parse_value("$12.50", false).unwrap();
        assert_eq!((v, u.format(v)), (12.5, "$12.50".to_string()));
        assert_eq!(parse_value("-€3", false).unwrap().0, -3.0);
    }

    #[test]
    fn money() {
        let usd = Unit::new(lookup_unit("USD", false).unwrap().0);
        assert_eq!(usd.format(3456.781), "$3,456.78");
        assert_eq!(usd.format(-12.5), "-$12.50");
        assert_eq!(usd.format(0.01234), "$0.01234");
        assert_eq!(usd.format(0.5), "$0.5000");
        assert_eq!(usd.format(1_234_567.0), "$1.23M");
        assert_eq!(
            Unit::new(lookup_unit("EUR", false).unwrap().0).format(1000.0),
            "€1,000.00"
        );
    }

    #[test]
    fn parses_values_with_units() {
        assert_eq!(parse_value("59.9 GB", false).unwrap().0, 59.9e9);
        assert_eq!(parse_value("1G", true).unwrap().0, 1024f64.powi(3));
        assert_eq!(parse_value("512 kB", true).unwrap().0, 512.0 * 1024.0);
        let (v, u) = parse_value("30ms", false).unwrap();
        assert!((v - 0.03).abs() < 1e-12);
        assert_eq!(u.dim, Dim::Seconds);
        assert_eq!(
            parse_value("15%", false).unwrap().1.dim,
            Dim::Other("%".into())
        );
        assert_eq!(parse_value("42", false).unwrap(), (42.0, Unit::default()));
        assert_eq!(parse_value("-1.5e3", false).unwrap().0, -1500.0);
        assert!(parse_value("abc", false).is_none());
        assert!(parse_value("12 two words", false).is_none());
    }

    #[test]
    fn split_number_leaves_rest() {
        assert_eq!(split_number("12.5GB rest"), Some((12.5, "GB rest")));
        assert_eq!(split_number("3e"), Some((3.0, "e")));
        assert_eq!(split_number(".5"), Some((0.5, "")));
        assert_eq!(split_number("-"), None);
    }

    #[test]
    fn tick_scales() {
        assert_eq!(
            Unit::new(Dim::Bytes { binary: false }).tick_scale(758e9),
            1e9
        );
        assert_eq!(
            Unit::new(Dim::Bytes { binary: true }).tick_scale(2048.0),
            1024.0
        );
        assert_eq!(Unit::new(Dim::Seconds).tick_scale(0.25), 1e-3);
        assert_eq!(Unit::default().tick_scale(42.0), 1.0);
    }

    #[test]
    fn formats_humanely() {
        let gb = Unit::new(Dim::Bytes { binary: false });
        assert_eq!(gb.format(58.8e9), "58.8 GB");
        assert_eq!(gb.format(422.6e9), "422.6 GB");
        let gib = Unit::new(Dim::Bytes { binary: true });
        assert_eq!(gib.format(1024.0 * 1024.0 * 1.5), "1.5 MiB");
        assert_eq!(Unit::new(Dim::Seconds).format(0.0012), "1.2 ms");
        assert_eq!(fmt_compact(1_234_567.0), "1.23M");
        assert_eq!(fmt_compact(0.5), "0.5");
        let rate = Unit {
            dim: Dim::Bytes { binary: false },
            rate: true,
        };
        assert_eq!(rate.format(2000.0), "2 kB/s");
    }
}
