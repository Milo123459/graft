//! `--history 'ARGS'`: older data for a chart whose source only sends what's new (a
//! stream, a feed of the latest price). ARGS is another source with its own format,
//! like `--source` (`-c "curl -s …?start={from_ms:1000s}&end={now_ms}" --json …`). Its
//! series go in under the same names, before the source's own first point, so the
//! chart (and discovery) starts with hours of data instead of none.
//!
//! When its command asks for a window (`{from…}` … `{now…}`), it pages back: run for
//! the window ending now and the ones before it, four at once (each the window's length
//! earlier), until `--history-depth` is covered or a page comes back empty. Scrolling back to the oldest data asks for the
//! window before that, like the backfill of a command source.

use anyhow::{Result, bail};

use crate::cli::Cli;
use crate::data::Dataset;
use crate::interactive::Live;
use crate::session::Session;
use crate::source::SourceSpec;

/// Pages it runs at most to cover the depth at the start.
const MAX_PAGES: usize = 500;
/// Pages it fetches at once while filling (an API a second or two away loads 4× as
/// fast).
const PARALLEL: usize = 4;

/// A page being read: what it was asked for (the window ending at `end`) and how many
/// lines of it matched so far.
struct Page {
    live: Live,
    end: f64,
    got: usize,
}

pub struct History {
    session: Session,
    /// Its command (with `{from…}` / `{now…}` still in it), if it's one.
    cmd: Option<(String, Option<String>)>,
    /// How long a window each run asks for (`None`: not known, so pages go one at a
    /// time, each ending at the oldest point so far).
    span: Option<f64>,
    /// The pages being read.
    loading: Vec<Page>,
    /// Where the next page ends, while filling.
    next_end: Option<f64>,
    /// Pages read.
    pub pages: usize,
    /// How far back to go at the start (seconds).
    depth: f64,
    /// When it started (the depth is counted back from here).
    started: f64,
    /// Still paging back to cover the depth.
    filling: bool,
    /// The source has nothing older.
    pub done: bool,
    /// What it has read, as series (kept until it reads more).
    cache: Option<(u64, Dataset)>,
    /// Its own --resample (else it follows the chart's).
    own_resample: bool,
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
        // Points as far apart as the chart's (unless it says otherwise itself), so the
        // two join up.
        let own_resample = cli.resample.is_some();
        let mut setup = crate::setup::build(cli, first.clone(), true)?;
        if !own_resample {
            setup.session.set_resample(chart.resample);
        }
        let span = cmd
            .as_ref()
            .and_then(|(c, _)| crate::params::window_span(c));
        let now = crate::time::now_secs();
        let mut h = History {
            own_resample,
            session: setup.session,
            cmd,
            span,
            loading: vec![Page {
                live: Live::new(first),
                end: now,
                got: 0,
            }],
            next_end: span.map(|s| now - s),
            pages: 0,
            depth,
            started: now,
            filling: true,
            done: false,
            cache: None,
        };
        h.fill();
        Ok(h)
    }

    /// The chart's points changed spacing: its own follow (unless it set its own).
    pub fn set_resample(&mut self, r: Option<crate::cli::Resample>) {
        if !self.own_resample {
            self.session.set_resample(r);
            self.cache = None;
        }
    }

    /// Go back this far (seconds). Further than it has: it pages back for the rest now.
    /// Returns whether it's loading more.
    pub fn set_depth(&mut self, depth: f64) -> bool {
        self.depth = depth;
        if self.covered() >= depth || self.done || !self.pages_back() {
            return self.loading();
        }
        self.filling = true;
        if self.next_end.is_none() || self.span.is_none() {
            self.next_end = self.oldest();
        }
        self.fill();
        self.loading()
    }

    /// Whether its command asks for a window (so it can page back).
    pub fn pages_back(&self) -> bool {
        self.cmd
            .as_ref()
            .is_some_and(|(c, _)| crate::params::has_window(c))
    }

    /// Still paging back to cover the depth (the first load).
    pub fn filling(&self) -> bool {
        self.filling && self.loading()
    }

    pub fn loading(&self) -> bool {
        !self.loading.is_empty()
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

    /// How much time it has, oldest to newest.
    fn covered(&self) -> f64 {
        match (self.oldest(), self.newest()) {
            (Some(a), Some(b)) => b - a,
            _ => 0.0,
        }
    }

    /// Start a page: the window ending at `end`.
    fn launch(&mut self, end: f64) {
        let Some((cmd, ssh)) = &self.cmd else { return };
        let spec = SourceSpec::Command {
            cmd: crate::params::fill_times(cmd, end),
            every: std::time::Duration::from_secs(1),
            count: Some(1),
            ssh: ssh.clone(),
        };
        self.loading.push(Page {
            live: Live::new(spec),
            end,
            got: 0,
        });
    }

    /// While filling: keep up to PARALLEL pages going, back to the depth. When the
    /// window's length isn't known, one at a time from the oldest point.
    fn fill(&mut self) {
        if !self.filling || self.done || !self.pages_back() {
            return;
        }
        // Back to the depth from its newest point (from now until there's one).
        let until = self.newest().unwrap_or(self.started) - self.depth;
        let at_once = if self.span.is_some() { PARALLEL } else { 1 };
        while self.loading.len() < at_once && self.pages + self.loading.len() < MAX_PAGES {
            let end = match (self.span, self.next_end) {
                (Some(_), Some(e)) => e,
                // One at a time: from the oldest point, once the last page is in.
                _ if self.loading.is_empty() => match self.oldest() {
                    Some(o) => o,
                    None => return,
                },
                _ => return,
            };
            if end <= until {
                return;
            }
            self.launch(end);
            self.next_end = self.span.map(|s| end - s);
        }
    }

    /// Read the pages being loaded; when one is done, start the next while the depth
    /// isn't covered. Returns (a page finished, an error or news to show).
    pub fn tick(&mut self) -> (bool, Option<(String, bool)>) {
        if self.loading.is_empty() {
            return (false, None);
        }
        let session = &mut self.session;
        let mut finished = Vec::new();
        let mut error = None;
        for (k, page) in self.loading.iter_mut().enumerate() {
            let got = &mut page.got;
            let errors = page
                .live
                .pump(std::time::Duration::from_millis(10), &mut |line, meta| {
                    if session.feed(line, meta).is_ok() {
                        *got += 1;
                    }
                });
            if let Some(e) = errors.into_iter().last() {
                error = Some(e);
                finished.push(k);
            } else if page.live.eof {
                finished.push(k);
            }
        }
        if finished.is_empty() {
            return (false, None);
        }
        let mut empty = false;
        for k in finished.into_iter().rev() {
            let page = self.loading.remove(k);
            self.pages += 1;
            // Nothing where it asked (and it asked before what it has): that's all.
            empty |= page.got == 0 && self.oldest().is_some_and(|o| page.end <= o + 1.0);
        }
        if let Some(e) = error {
            self.loading.clear();
            self.filling = false;
            return (true, Some((format!("history: {e}"), true)));
        }
        if self.session.lines > 0 && self.session.matched == 0 {
            self.loading.clear();
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
        if empty && self.pages > 1 {
            self.done = true;
        }
        let covered = self.covered();
        let page_s = if self.pages == 1 { "" } else { "s" };
        if self.filling {
            self.fill();
            if self.loading() {
                // How far it's got, page by page, so it's clearly moving.
                return (
                    true,
                    Some((
                        format!(
                            "history: loading… {} of {} ({} page{page_s})",
                            crate::time::fmt_duration(covered),
                            crate::time::fmt_duration(self.depth),
                            self.pages
                        ),
                        false,
                    )),
                );
            }
            self.filling = false;
            let all = if self.done { ", all it has" } else { "" };
            return (
                true,
                Some((
                    format!(
                        "history: {} loaded ({} page{page_s}{all})",
                        crate::time::fmt_duration(covered),
                        self.pages,
                    ),
                    false,
                )),
            );
        }
        if self.loading() {
            return (true, None);
        }
        // Scrolled back for more: say what came.
        if self.done {
            return (true, Some(("history: no older data".into(), false)));
        }
        let back = self.oldest().map_or(String::new(), |o| {
            format!(
                ", back to {} ago",
                crate::time::fmt_ago(crate::time::now_secs() - o)
            )
        });
        (
            true,
            Some((
                format!(
                    "history: older data loaded ({} in all{back})",
                    crate::time::fmt_duration(covered)
                ),
                false,
            )),
        )
    }

    /// Ask for the window before the oldest point (scrolled back to it). False when it
    /// can't: no window in its command, or nothing older. True while pages are loading.
    pub fn more(&mut self) -> bool {
        if self.loading() {
            return true;
        }
        if self.done || !self.pages_back() {
            return false;
        }
        let Some(oldest) = self.oldest() else {
            return false;
        };
        self.launch(oldest);
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
        let mut news = Vec::new();
        while h.loading() && std::time::Instant::now() < end {
            news.extend(h.tick().1.map(|n| n.0));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // It says how far it's got as it goes, then that it's loaded.
        assert!(news.iter().any(|n| n.contains("loading… ")), "{news:?}");
        assert!(news.last().unwrap().contains("loaded ("), "{news:?}");
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
        let mut said = None;
        while h.loading() {
            said = said.or(h.tick().1);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(h.oldest().unwrap() < before);
        let said = said.expect("it says when the older data has come").0;
        assert!(
            said.contains("older data loaded") && said.contains("back to"),
            "{said}"
        );
    }

    #[test]
    fn several_pages_load_at_once_with_no_gaps() {
        // Each page takes 0.3 s and has a point a minute; 8 of them one at a time would
        // take 2.4 s.
        let cmd = r#"-c 'sleep 0.3; start={from:10m}; end={now}; for i in 0 1 2 3 4 5 6 7 8 9; do echo "{\"t\":$((start + i * 60)),\"price\":1}"; done' --json --time-field t"#;
        let started = std::time::Instant::now();
        let mut h = History::open(cmd, &Cli::empty(), 8.0 * 600.0).unwrap();
        while h.loading() && started.elapsed() < std::time::Duration::from_secs(10) {
            h.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1600),
            "{:?}",
            started.elapsed()
        );
        assert!(h.pages >= 8, "{}", h.pages);
        let mut full = Dataset {
            x_kind: XKind::Time,
            series: Vec::new(),
        };
        h.join(&mut full);
        let p = &full.series[0].points;
        assert!(
            p.windows(2).all(|w| w[1].0 - w[0].0 <= 60.0 + 1e-9),
            "a point a minute, no gaps"
        );
        assert!(p.last().unwrap().0 - p[0].0 >= 8.0 * 600.0 - 60.0);
    }

    #[test]
    fn needs_a_source_and_a_format() {
        let chart = Cli::empty();
        assert!(History::open("--json", &chart, 60.0).is_err());
        assert!(History::open("-c 'echo 1'", &chart, 60.0).is_err());
    }
}
