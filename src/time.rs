//! Timestamp detection/parsing, durations and x-axis formatting.
//!
//! Timestamps are stored as `f64` seconds since the Unix epoch. Timestamps without
//! an explicit offset are treated as wall-clock time and kept as-is (they're
//! never shifted to UTC), so what you see on the axis matches what's in the log.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use regex::Regex;

/// Patterns recognised at the start of a line when no `--time-format` is given.
static AUTO_PATTERNS: LazyLock<Vec<(Regex, AutoKind)>> = LazyLock::new(|| {
    let p = |re: &str, kind| (Regex::new(re).expect("valid timestamp regex"), kind);
    vec![
        // 2026-11-01 04:00, 2026-11-01T04:00:00.123Z, 2026/11/01 04:00:00+01:00
        p(
            r"^\s*\[?((\d{4})[-/](\d{2})[-/](\d{2})[T ](\d{2}):(\d{2})(?::(\d{2})(?:[.,](\d{1,9}))?)?(?:\s?(Z|[+-]\d{2}:?\d{2}))?)\]?",
            AutoKind::DateTime,
        ),
        // Nov  2 08:00:01 (syslog, no year)
        p(
            r"^\s*\[?([A-Z][a-z]{2}\s+\d{1,2}\s+\d{2}:\d{2}:\d{2})\]?",
            AutoKind::Syslog,
        ),
        // 02/Nov/2026:10:03:30 +0000 (web server access logs)
        p(
            r"^\s*\[?(\d{2}/[A-Z][a-z]{2}/\d{4}:\d{2}:\d{2}:\d{2}(?:\s[+-]\d{4})?)\]?",
            AutoKind::Clf,
        ),
        // 08:00:01 (time of day only, e.g. `sar`, `vmstat -t`)
        p(
            r"^\s*\[?(\d{2}:\d{2}:\d{2}(?:\.\d+)?)\]?(?:\s|$)",
            AutoKind::TimeOfDay,
        ),
        // 2026-11-01 (date only)
        p(r"^\s*\[?(\d{4}-\d{2}-\d{2})\]?(?:\s|$)", AutoKind::Date),
        // 1762000000 or 1762000000.123 or 1762000000123 (epoch s / ms)
        p(
            r"^\s*\[?(\d{10}(?:\.\d+)?|\d{13})\]?(?:\s|$)",
            AutoKind::Epoch,
        ),
    ]
});

#[derive(Debug, Clone, Copy)]
enum AutoKind {
    DateTime,
    Syslog,
    Clf,
    TimeOfDay,
    Date,
    Epoch,
}

/// Parses timestamps, either with an explicit strftime format or by auto-detection.
#[derive(Debug, Clone, Default)]
pub struct TimeParser {
    format: Option<String>,
}

impl TimeParser {
    pub fn new(format: Option<String>) -> Self {
        Self { format }
    }

    /// Find a timestamp at the start of `line`. Returns its value and the byte length
    /// of the consumed prefix (so callers can strip it before further parsing).
    pub fn detect(&self, line: &str) -> Option<(f64, usize)> {
        if let Some(fmt) = &self.format {
            let trimmed = line.trim_start();
            let lead = line.len() - trimmed.len();
            let (ts, rest) = parse_with_format_prefix(trimmed, fmt)?;
            return Some((ts, lead + trimmed.len() - rest.len()));
        }
        AUTO_PATTERNS.iter().find_map(|(re, kind)| {
            let caps = re.captures(line)?;
            let ts = match kind {
                AutoKind::DateTime => parse_iso(&caps)?,
                _ => parse_auto(caps.get(1)?.as_str(), *kind)?,
            };
            Some((ts, caps.get(0)?.end()))
        })
    }

    /// Parse an entire string as a timestamp (e.g. a regex capture or CSV field).
    pub fn parse(&self, s: &str) -> Option<f64> {
        let s = s.trim();
        if let Some(fmt) = &self.format {
            let (ts, rest) = parse_with_format_prefix(s, fmt)?;
            return rest.trim().is_empty().then_some(ts);
        }
        if let Some((ts, len)) = self.detect(s)
            && s[len..].trim().is_empty()
        {
            return Some(ts);
        }
        // Bare epoch numbers of any size.
        let n: f64 = s.parse().ok()?;
        Some(if n > 1e11 { n / 1000.0 } else { n })
    }
}

fn naive_secs(dt: NaiveDateTime) -> f64 {
    dt.and_utc().timestamp_micros() as f64 / 1e6
}

/// Offset-aware timestamps are converted into local wall-clock time so they line up
/// with naive timestamps and with sample times from `--cmd`.
fn aware_secs<Tz: chrono::TimeZone>(dt: DateTime<Tz>) -> f64 {
    naive_secs(dt.with_timezone(&Local).naive_local())
}

fn parse_with_format_prefix<'a>(s: &'a str, fmt: &str) -> Option<(f64, &'a str)> {
    if let Ok((dt, rest)) = DateTime::parse_and_remainder(s, fmt) {
        return Some((aware_secs(dt), rest));
    }
    if let Ok((dt, rest)) = NaiveDateTime::parse_and_remainder(s, fmt) {
        return Some((naive_secs(dt), rest));
    }
    if let Ok((d, rest)) = NaiveDate::parse_and_remainder(s, fmt) {
        return Some((naive_secs(d.and_time(NaiveTime::MIN)), rest));
    }
    if let Ok((t, rest)) = NaiveTime::parse_and_remainder(s, fmt) {
        return Some((naive_secs(Local::now().date_naive().and_time(t)), rest));
    }
    None
}

/// Fast path for ISO-like timestamps, built straight from the regex captures.
fn parse_iso(c: &regex::Captures) -> Option<f64> {
    let num = |i: usize| c.get(i).map_or(Some(0), |m| m.as_str().parse::<u32>().ok());
    let date = NaiveDate::from_ymd_opt(num(2)? as i32, num(3)?, num(4)?)?;
    let nanos = c.get(8).map_or(Some(0), |m| {
        let digits = m.as_str();
        digits
            .parse::<u32>()
            .ok()
            .map(|n| n * 10u32.pow(9 - digits.len() as u32))
    })?;
    let dt = date.and_hms_nano_opt(num(5)?, num(6)?, num(7)?, nanos)?;
    match c.get(9).map(|m| m.as_str()) {
        None => Some(naive_secs(dt)),
        Some("Z") => Some(aware_secs(dt.and_utc())),
        Some(off) => {
            let sign = if off.starts_with('-') { -1 } else { 1 };
            let digits: String = off[1..].chars().filter(char::is_ascii_digit).collect();
            let (h, m): (i32, i32) = (
                digits.get(..2)?.parse().ok()?,
                digits.get(2..)?.parse().ok()?,
            );
            let tz = chrono::FixedOffset::east_opt(sign * (h * 3600 + m * 60))?;
            Some(aware_secs(dt.and_local_timezone(tz).single()?))
        }
    }
}

fn parse_auto(s: &str, kind: AutoKind) -> Option<f64> {
    match kind {
        AutoKind::DateTime => unreachable!("handled by parse_iso"),
        AutoKind::Syslog => {
            let year = Local::now().year();
            let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
            NaiveDateTime::parse_from_str(&format!("{year} {s}"), "%Y %b %d %H:%M:%S")
                .ok()
                .map(naive_secs)
        }
        AutoKind::Clf => DateTime::parse_from_str(s, "%d/%b/%Y:%H:%M:%S %z")
            .ok()
            .map(aware_secs)
            .or_else(|| {
                NaiveDateTime::parse_from_str(s, "%d/%b/%Y:%H:%M:%S")
                    .ok()
                    .map(naive_secs)
            }),
        AutoKind::TimeOfDay => {
            let t = NaiveTime::parse_from_str(s, "%H:%M:%S%.f").ok()?;
            Some(naive_secs(Local::now().date_naive().and_time(t)))
        }
        AutoKind::Date => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .ok()
            .map(|d| naive_secs(d.and_time(NaiveTime::MIN))),
        AutoKind::Epoch => {
            let n: f64 = s.parse().ok()?;
            let secs = if s.len() == 13 && !s.contains('.') {
                n / 1000.0
            } else {
                n
            };
            // Epochs are UTC; convert to local wall-clock for consistency.
            let dt = DateTime::from_timestamp_micros((secs * 1e6) as i64)?;
            Some(aware_secs(dt))
        }
    }
}

/// Current local wall-clock time, in the same representation as parsed timestamps.
pub fn now_secs() -> f64 {
    naive_secs(Local::now().naive_local())
}

fn to_naive(secs: f64) -> Option<NaiveDateTime> {
    DateTime::from_timestamp_micros((secs * 1e6).round() as i64).map(|d| d.naive_utc())
}

/// Format a timestamp for display, choosing precision based on the visible `span`.
pub fn format_time(secs: f64, span: f64) -> String {
    let Some(dt) = to_naive(secs) else {
        return format!("{secs}");
    };
    let fmt = if span < 120.0 {
        "%H:%M:%S"
    } else if span < 86_400.0 {
        if dt.second() == 0 {
            "%H:%M"
        } else {
            "%H:%M:%S"
        }
    } else if span < 86_400.0 * 300.0 {
        "%m-%d %H:%M"
    } else {
        "%Y-%m-%d"
    };
    dt.format(fmt).to_string()
}

/// Format a timestamp with a strftime format.
pub fn format_time_with(secs: f64, fmt: &str) -> String {
    use std::fmt::Write;
    let Some(dt) = to_naive(secs) else {
        return format!("{secs}");
    };
    let mut out = String::new();
    // An invalid format string makes chrono's Display fail; fall back gracefully.
    if write!(out, "{}", dt.format(fmt)).is_err() {
        return format_time_full(secs);
    }
    out
}

/// Full, unambiguous timestamp format.
pub fn format_time_full(secs: f64) -> String {
    match to_naive(secs) {
        Some(dt) if dt.second() == 0 && dt.nanosecond() == 0 => {
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        Some(dt) if dt.nanosecond() == 0 => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        None => format!("{secs}"),
    }
}

/// Parse a duration like `500ms`, `1s`, `10m`, `1.5h`, `2d`, `1w`. A bare number is seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let secs = parse_duration_secs(s)?;
    Duration::try_from_secs_f64(secs).with_context(|| format!("invalid duration '{s}'"))
}

pub fn parse_duration_secs(s: &str) -> Result<f64> {
    let s = s.trim();
    let (n, unit) =
        crate::units::split_number(s).with_context(|| format!("invalid duration '{s}'"))?;
    let mult = match unit.trim() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "ms" => 1e-3,
        "us" | "µs" => 1e-6,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86400.0,
        "w" | "week" | "weeks" => 604_800.0,
        other => bail!("unknown duration unit '{other}' in '{s}' (use ms, s, m, h, d or w)"),
    };
    if n < 0.0 {
        bail!("duration '{s}' must not be negative");
    }
    Ok(n * mult)
}

/// Human description of a span of seconds: `45 s`, `12 min`, `3.4 hours`, `9.6 days`.
pub fn fmt_duration(secs: f64) -> String {
    let a = secs.abs();
    let (v, unit) = if a < 120.0 {
        (secs, "s")
    } else if a < 7200.0 {
        (secs / 60.0, "min")
    } else if a < 172_800.0 {
        (secs / 3600.0, "hours")
    } else if a < 86_400.0 * 730.0 {
        (secs / 86400.0, "days")
    } else {
        (secs / (86400.0 * 365.25), "years")
    };
    format!(
        "{} {unit}",
        crate::units::fmt_sig((v * 10.0).round() / 10.0)
    )
}

/// Compact, re-parseable duration: `500ms`, `1s`, `10m`, `1.5h`, `2d`.
pub fn fmt_duration_compact(secs: f64) -> String {
    let units = [
        (86400.0, "d"),
        (3600.0, "h"),
        (60.0, "m"),
        (1.0, "s"),
        (1e-3, "ms"),
    ];
    for (mult, unit) in units {
        let v = secs / mult;
        if v >= 1.0 {
            let text = crate::units::fmt_sig(v);
            if text
                .parse::<f64>()
                .is_ok_and(|t| (t * mult - secs).abs() <= 1e-9 * secs.max(1.0))
            {
                return format!("{text}{unit}");
            }
        }
    }
    format!("{secs}s")
}

/// A named time unit for expressing slopes, e.g. "per day".
pub fn per_label(secs: f64) -> String {
    match secs {
        1.0 => "s".into(),
        60.0 => "min".into(),
        3600.0 => "hour".into(),
        86400.0 => "day".into(),
        604_800.0 => "week".into(),
        s => fmt_duration(s),
    }
}

/// Pick a natural slope unit for a time span.
pub fn auto_per(span: f64) -> f64 {
    if span >= 86_400.0 {
        86_400.0
    } else if span >= 7200.0 {
        3600.0
    } else if span >= 120.0 {
        60.0
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_log_timestamps() {
        let t = TimeParser::default();
        let (ts, len) = t.detect("[02/Nov/2026:10:03:30 +0000] GET /").unwrap();
        assert_eq!(len, "[02/Nov/2026:10:03:30 +0000]".len());
        let expected =
            aware_secs(DateTime::parse_from_rfc3339("2026-11-02T10:03:30+00:00").unwrap());
        assert_eq!(ts, expected);
    }

    #[test]
    fn detects_iso_like_prefix() {
        let tp = TimeParser::default();
        let line = "2026-11-01 04:00  node1: 59.9 GB";
        let (ts, len) = tp.detect(line).unwrap();
        assert_eq!(&line[len..], "  node1: 59.9 GB");
        assert_eq!(format_time_full(ts), "2026-11-01 04:00");
        let (ts2, _) = tp.detect("2026-11-01T05:00:00 x=1").unwrap();
        assert_eq!(ts2 - ts, 3600.0);
    }

    #[test]
    fn custom_format() {
        let tp = TimeParser::new(Some("%d/%m/%Y %H:%M".into()));
        let (ts, len) = tp.detect("01/11/2026 04:00 v=1").unwrap();
        assert_eq!(format_time_full(ts), "2026-11-01 04:00");
        assert_eq!(len, 16);
        assert!(tp.parse("01/11/2026 04:00").is_some());
    }

    #[test]
    fn does_not_mistake_numbers_for_times() {
        let tp = TimeParser::default();
        assert!(tp.detect("12 34 56").is_none());
        assert!(tp.detect("node1: 5").is_none());
    }

    #[test]
    fn compact_durations_round_trip() {
        for s in [
            0.5,
            1.0,
            1.5,
            60.0,
            600.0,
            3600.0,
            5400.0,
            86400.0,
            7.0 * 86400.0,
            90.0,
        ] {
            let text = fmt_duration_compact(s);
            assert_eq!(parse_duration_secs(&text).unwrap(), s, "{text}");
        }
        assert_eq!(fmt_duration_compact(600.0), "10m");
        assert_eq!(fmt_duration_compact(0.5), "500ms");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("1.5h").unwrap(), 5400.0);
        assert_eq!(parse_duration_secs("500ms").unwrap(), 0.5);
        assert_eq!(parse_duration_secs("10").unwrap(), 10.0);
        assert!(parse_duration_secs("3 fortnights").is_err());
        assert_eq!(fmt_duration(86400.0 * 9.62), "9.6 days");
    }
}
