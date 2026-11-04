//! `--history 'ARGS'`: older data for a chart whose source only sends what's new (a
//! stream, a feed of the latest price). ARGS is another source with its own format,
//! like `--source` (`-c "curl -s …?start={from_ms:1000s}&end={now_ms}" --json …`). Its
//! series go in under the same names, before the source's own first point, so the
//! chart (and discovery) starts with hours of data instead of none.
//!
//! When its command asks for a window (`{from…}` … `{now…}`), it pages back: run for
//! the window ending now, then for the window before the oldest point it got, and so on
//! until `--history-depth` is covered. Scrolling back to the oldest data asks for the
//! window before that, like the backfill of a command source.

use anyhow::{Result, bail};

use crate::cli::Cli;
use crate::data::Dataset;
use crate::interactive::Live;
use crate::session::Session;
use crate::source::SourceSpec;

/// Pages it runs at most to cover the depth at the start.
const MAX_PAGES: usize = 200;

pub struct History {
    session: Session,
    /// Its command (with `{from…}` / `{now…}` still in it), if it's one.
    cmd: Option<(String, Option<String>)>,
    /// The page being read, and the time it ends at.
    page: Option<(Live, f64)>,
    pub pages: usize,
    /// How far back to go at the start (seconds).
    depth: f64,
    /// Still paging back to cover the depth.
    filling: bool,
    /// The source has nothing older.
    pub done: bool,
    /// What it has read, as series (kept until it reads more).
    cache: Option<(u64, Dataset)>,
}

impl History {
    /// `args` with the chart's `{name}` settings filled in.
    pub fn open(args: &str, chart: &Cli, depth: f64) -> Result<History> {
        let params = crate::params::parse_all(&chart.param);
        let args = crate::params::fill(args, &params);
        let words = crate::sources::split_args(&args)?;
        let cli = <Cli as clap::Parser>::try_parse_from(
            std::iter::once("graft".to_string()).chain(words),
        )
        .map_err(|e| {
            anyhow::anyhow!("--history: {}", e.to_string().lines().next().unwrap_or(""))
        })?;
        if !cli.has_source() {
            bail!("--history: give it a command or file to read, e.g. -c \"curl -s …\"");
        }
        if !cli.has_parser() {
            bail!("--history: give it a format, e.g. --json");
        }
        let spec = crate::setup::source_spec(&cli, true)?;
        let cmd = match &spec {
            SourceSpec::Command { cmd, ssh, .. } => Some((cmd.clone(), ssh.clone())),
            _ => None,
        };
        let first = match &spec {
            // One run (it's filled with the window ending now when it runs).
            SourceSpec::Command {
                cmd, every, ssh, ..
            } => SourceSpec::Command {
                cmd: cmd.clone(),
                every: *every,
                count: Some(1),
                ssh: ssh.clone(),
            },
            SourceSpec::File { path, .. } => SourceSpec::File {
                path: path.clone(),
                follow: false,
            },
            other => other.clone(),
        };
        let setup = crate::setup::build(cli, first.clone(), true)?;
        Ok(History {
            session: setup.session,
            cmd,
            page: Some((Live::new(first), crate::time::now_secs())),
            pages: 0,
            depth,
            filling: true,
            done: false,
            cache: None,
        })
    }

    /// Whether its command asks for a window (so it can page back).
    pub fn pages_back(&self) -> bool {
        self.cmd
            .as_ref()
            .is_some_and(|(c, _)| crate::params::has_window(c))
    }

    /// Still paging back to cover the depth (the first load).
    pub fn filling(&self) -> bool {
        self.filling && self.page.is_some()
    }

    pub fn loading(&self) -> bool {
        self.page.is_some()
    }

    pub fn oldest(&self) -> Option<f64> {
        self.session
            .store
            .series
            .values()
            .filter_map(|s| s.points.first().map(|p| p.0))
            .min_by(f64::total_cmp)
    }

    fn newest(&self) -> Option<f64> {
        self.session
            .store
            .series
            .values()
            .filter_map(|s| s.points.last().map(|p| p.0))
            .max_by(f64::total_cmp)
    }

    /// Read the page being loaded; when it's done, start the next one while the depth
    /// isn't covered. Returns (a page finished, an error or news to show).
    pub fn tick(&mut self) -> (bool, Option<(String, bool)>) {
        let Some((live, end)) = &mut self.page else {
            return (false, None);
        };
        let session = &mut self.session;
        let errors = live.pump(std::time::Duration::from_millis(30), &mut |line, meta| {
            let _ = session.feed(line, meta);
        });
        if let Some(e) = errors.into_iter().last() {
            self.page = None;
            self.filling = false;
            return (true, Some((format!("history: {e}"), true)));
        }
        if !live.eof {
            return (false, None);
        }
        let end = *end;
        self.page = None;
        self.pages += 1;
        let oldest = self.oldest();
        // Nothing older than asked for: that's all there is.
        if oldest.is_none_or(|o| o >= end - 1e-9) && self.pages > 1 {
            self.done = true;
            self.filling = false;
            return (true, Some(("history: no older data".into(), false)));
        }
        if self.session.lines > 0 && self.session.matched == 0 {
            self.filling = false;
            return (
                true,
                Some((
                    format!(
                        "history: {} lines read, none matched its format",
                        self.session.lines
                    ),
                    true,
                )),
            );
        }
        let covered = match (oldest, self.newest()) {
            (Some(a), Some(b)) => b - a,
            _ => 0.0,
        };
        if self.filling && covered < self.depth && self.pages < MAX_PAGES && self.more() {
            return (true, None);
        }
        let news = self.filling.then(|| {
            (
                format!(
                    "history: {} loaded ({} page{})",
                    crate::time::fmt_duration(covered),
                    self.pages,
                    if self.pages == 1 { "" } else { "s" }
                ),
                false,
            )
        });
        self.filling = false;
        (true, news)
    }

    /// Ask for the window before the oldest point (scrolled back to it). False when it
    /// can't: no window in its command, nothing older, or a page already loading.
    pub fn more(&mut self) -> bool {
        if self.page.is_some() {
            return true;
        }
        if self.done || !self.pages_back() {
            return false;
        }
        let (Some((cmd, ssh)), Some(oldest)) = (&self.cmd, self.oldest()) else {
            return false;
        };
        let spec = SourceSpec::Command {
            cmd: crate::params::fill_times(cmd, oldest),
            every: std::time::Duration::from_secs(1),
            count: Some(1),
            ssh: ssh.clone(),
        };
        self.page = Some((Live::new(spec), oldest));
        true
    }

    /// Put its series into `full`: each before the first point of the series with its
    /// name (a series only it has is added).
    pub fn join(&mut self, full: &mut Dataset) {
        let rev = self.session.store.revision;
        if self.cache.as_ref().is_none_or(|c| c.0 != rev) {
            self.cache = Some((rev, self.session.base_dataset()));
        }
        let Some((_, ds)) = &self.cache else { return };
        for h in &ds.series {
            match full.series.iter_mut().find(|s| s.name == h.name) {
                Some(s) => {
                    let first = s.points.first().map_or(f64::INFINITY, |p| p.0);
                    let before = h.points.partition_point(|p| p.0 < first);
                    let mut points = h.points[..before].to_vec();
                    points.dedup_by(|a, b| a.0 == b.0);
                    points.append(&mut s.points);
                    s.points = points;
                }
                None => full.series.push(h.clone()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Series, XKind};

    #[test]
    fn pages_back_until_the_depth_is_covered_and_joins_under_the_live_data() {
        // Each run prints its window ({from:10m} … {now}), a point a minute.
        let cmd = r#"-c 'start={from:10m}; end={now}; for i in 0 1 2 3 4 5 6 7 8 9; do echo "{\"t\":$((start + i * 60)),\"price\":1}"; done; test $end -gt $start' --json --time-field t"#;
        let chart = Cli::empty();
        let mut h = History::open(cmd, &chart, 3600.0).unwrap();
        assert!(h.pages_back());
        let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while h.loading() && std::time::Instant::now() < end {
            h.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let span = h.newest().unwrap() - h.oldest().unwrap();
        assert!(span >= 3600.0, "{span}");
        assert!(h.pages >= 6, "{}", h.pages);
        // Joined under a live series starting at the newest history point.
        let newest = h.newest().unwrap();
        let mut full = Dataset {
            x_kind: XKind::Time,
            series: vec![Series {
                name: "price".into(),
                unit: Default::default(),
                points: vec![(newest, 2.0), (newest + 1.0, 3.0)],
            }],
        };
        h.join(&mut full);
        let p = &full.series[0].points;
        assert!(p.windows(2).all(|w| w[0].0 < w[1].0), "sorted, no repeats");
        assert_eq!(p[p.len() - 2], (newest, 2.0), "the live point wins");
        assert!(p.len() > 60);
        // Scrolled back: one more page.
        let before = h.oldest().unwrap();
        assert!(h.more());
        while h.loading() {
            h.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(h.oldest().unwrap() < before);
    }

    #[test]
    fn needs_a_source_and_a_format() {
        let chart = Cli::empty();
        assert!(History::open("--json", &chart, 60.0).is_err());
        assert!(History::open("-c 'echo 1'", &chart, 60.0).is_err());
    }
}
