//! Extra sources for panes: `--pane 'source=cpu'` runs the `cpu` preset (or any saved
//! profile, or a recent chart like `@2`) as its own input, with its own format, next
//! to the main chart.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::cli::Cli;
use crate::data::Dataset;
use crate::interactive::Live;
use crate::render::{ChartOpts, SourceView};
use crate::session::Session;
use crate::transform::Timeframe;

/// The arguments a source name stands for (without changing directory, unlike `-p @`).
pub fn args_for(name: &str) -> Result<Vec<String>> {
    if let Some(recent) = crate::recent::find(name) {
        return Ok(recent?.args);
    }
    let expanded = crate::profile::expand(vec!["graft".into(), "-p".into(), name.into()])?;
    Ok(expanded
        .into_iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect())
}

/// The command line a source runs, parsed.
pub fn cli_for(name: &str) -> Result<Cli> {
    let args = args_for(name)?;
    <Cli as clap::Parser>::try_parse_from(std::iter::once("graft".to_string()).chain(args))
        .with_context(|| format!("source '{name}'"))
}

/// A dashboard of panes that all have their own `source=` and no main source: the
/// first such pane's source becomes the main one (so recents, profiles and the
/// builder work as usual). `args` includes the program name; returns the new
/// arguments, or `None` when there's nothing to do.
pub fn promote(cli: &Cli, args: &[String]) -> Result<Option<Vec<String>>> {
    if cli.cmd.is_some() || cli.input.is_some() || cli.has_parser() {
        return Ok(None);
    }
    let Some((i, mut first)) = cli.pane.iter().enumerate().find_map(|(i, spec)| {
        crate::render::panes::Pane::parse(spec)
            .ok()
            .filter(|p| p.source.is_some())
            .map(|p| (i, p))
    }) else {
        return Ok(None);
    };
    let source = first.source.take().expect("filtered");
    let mut new = vec!["graft".to_string()];
    new.extend(args_for(&source)?);
    // The rest of the command line, with that pane now showing the main data.
    let spec = &cli.pane[i];
    let mut replaced = false;
    for a in args.iter().skip(1) {
        if !replaced && a == spec {
            new.push(first.to_spec());
            replaced = true;
        } else {
            new.push(a.clone());
        }
    }
    Ok(Some(new))
}

/// One running source and what it last produced.
pub struct Sub {
    pub name: String,
    session: Session,
    live: Live,
    built_rev: u64,
    pub view: SourceView,
}

impl Sub {
    /// Start a source (`interactive`: commands keep sampling).
    pub fn open(name: &str, interactive: bool) -> Self {
        match Self::try_open(name, interactive) {
            Ok(s) => s,
            Err(e) => Self::failed(name, format!("{e:#}")),
        }
    }

    fn try_open(name: &str, interactive: bool) -> Result<Self> {
        let cli = cli_for(name)?;
        let spec = crate::setup::source_spec(&cli, interactive)?;
        let setup = crate::setup::build(cli, spec.clone(), true)?;
        Ok(Self {
            name: name.to_string(),
            session: setup.session,
            live: Live::new(spec),
            built_rev: u64::MAX,
            view: SourceView {
                name: name.to_string(),
                ds: Dataset::default(),
                opts: setup.opts,
                error: None,
            },
        })
    }

    fn failed(name: &str, error: String) -> Self {
        // A placeholder that only shows the error.
        let cli = <Cli as clap::Parser>::parse_from(["graft", "--kv"]);
        let session = Session::new(
            &cli,
            std::rc::Rc::new(
                crate::script::ScriptHost::new(None, false).expect("plain script host"),
            ),
        )
        .expect("plain session");
        Self {
            name: name.to_string(),
            session,
            live: Live::closed(),
            built_rev: 0,
            view: SourceView {
                name: name.to_string(),
                ds: Dataset::default(),
                opts: ChartOpts::default(),
                error: Some(error),
            },
        }
    }

    /// Read what has arrived; returns whether the data changed.
    pub fn pump(&mut self, budget: Duration) -> bool {
        let session = &mut self.session;
        let errors = self.live.pump(budget, &mut |line, meta| {
            let _ = session.feed(line, meta);
        });
        if let Some(e) = errors.into_iter().last() {
            self.view.error = Some(e);
        }
        self.session.store.revision != self.built_rev
    }

    /// Rebuild the dataset, cut to the shared timeframe.
    pub fn rebuild(&mut self, timeframe: &Timeframe) {
        let mut full = self.session.full_dataset();
        timeframe.apply(&mut full);
        self.view.ds = self.session.select(&full);
        self.built_rev = self.session.store.revision;
        if !self.view.ds.is_empty() {
            self.view.error = None;
        }
    }

    /// Read until the source ends (for `--once`).
    pub fn load_to_end(&mut self, timeframe: &Timeframe) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !self.live.eof && std::time::Instant::now() < deadline {
            self.pump(Duration::from_millis(50));
        }
        self.rebuild(timeframe);
    }
}

/// Source names the panes use, in order, without repeats.
pub fn wanted(opts: &ChartOpts) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in opts.panes.iter().flat_map(|p| p.panes.iter()) {
        if let Some(s) = &p.source
            && !out.contains(s)
        {
            out.push(s.clone());
        }
    }
    out
}

/// Keep `subs` in step with the panes: start new sources, stop unused ones.
pub fn sync(subs: &mut Vec<Sub>, opts: &ChartOpts, interactive: bool) {
    let want = wanted(opts);
    subs.retain(|s| want.contains(&s.name));
    for name in want {
        if !subs.iter().any(|s| s.name == name) {
            subs.push(Sub::open(&name, interactive));
        }
    }
}
