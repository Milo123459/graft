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
/// With `ssh`, it runs on that host (presets pick that machine's variant). `defs`: the
/// chart's own named sources (`--source 'NAME = ARGS'`), which come first.
pub fn args_for(name: &str, ssh: Option<&str>, defs: &[String]) -> Result<Vec<String>> {
    if let Some(m) = defs
        .iter()
        .filter_map(|d| Merge::parse(d).ok())
        .find(|m| m.name == name)
    {
        let mut args = m.args;
        if let Some(h) = ssh
            && !args.iter().any(|a| a == "--ssh" || a.starts_with("--ssh="))
        {
            args.extend(["--ssh".to_string(), h.to_string()]);
        }
        return Ok(args);
    }
    if let Some(recent) = crate::recent::find(name) {
        let mut args = recent?.args;
        if let Some(h) = ssh
            && !args.iter().any(|a| a == "--ssh" || a.starts_with("--ssh="))
        {
            args.extend(["--ssh".to_string(), h.to_string()]);
        }
        return Ok(args);
    }
    let mut argv: Vec<std::ffi::OsString> = vec!["graft".into(), "-p".into(), name.into()];
    if let Some(h) = ssh {
        argv.extend(["--ssh".into(), h.into()]);
    }
    let expanded = crate::profile::expand(argv)?;
    Ok(expanded
        .into_iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect())
}

/// The command line a source runs, parsed.
pub fn cli_for(name: &str, ssh: Option<&str>, defs: &[String]) -> Result<Cli> {
    let args = args_for(name, ssh, defs)?;
    <Cli as clap::Parser>::try_parse_from(std::iter::once("graft".to_string()).chain(args))
        .with_context(|| format!("source '{name}'"))
}

/// `--merge`: another source whose series join the chart, named after it.
#[derive(Debug, Clone, PartialEq)]
pub struct Merge {
    pub name: String,
    /// The source's arguments (from a profile, or given inline).
    pub args: Vec<String>,
    pub spec: String,
}

impl Merge {
    /// `NAME` (a profile/preset/recent chart), `NAME = PROFILE`, or `NAME = ARGS…`
    /// with the source's own arguments inline: `binance = -c "curl …" --regex "…"`.
    pub fn parse(spec: &str) -> Result<Merge> {
        let (name, rhs) = match spec.split_once('=') {
            Some((n, r)) if !n.trim().is_empty() && !n.trim().contains(char::is_whitespace) => {
                (n.trim(), r.trim())
            }
            _ => (spec.trim(), spec.trim()),
        };
        if name.is_empty() {
            anyhow::bail!("--merge: give a name, e.g. --merge 'binance = -c \"curl …\" --json'");
        }
        let args = if rhs.starts_with('-') || rhs.contains(char::is_whitespace) {
            split_args(rhs).with_context(|| format!("--merge '{spec}'"))?
        } else {
            args_for(rhs, None, &[]).with_context(|| format!("--merge '{spec}'"))?
        };
        let cli = <Cli as clap::Parser>::try_parse_from(
            std::iter::once("graft".to_string()).chain(args.iter().cloned()),
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "--merge '{spec}': {}",
                e.to_string().lines().next().unwrap_or("")
            )
        })?;
        if cli.cmd.is_none() && cli.input.is_none() {
            anyhow::bail!("--merge '{spec}': no command or file to read");
        }
        if !cli.has_parser() {
            anyhow::bail!("--merge '{spec}': say how to read it (--format, --regex, --json…)");
        }
        Ok(Merge {
            name: name.to_string(),
            args,
            spec: spec.trim().to_string(),
        })
    }

    pub fn cli(&self) -> Cli {
        <Cli as clap::Parser>::parse_from(
            std::iter::once("graft".to_string()).chain(self.args.iter().cloned()),
        )
    }
}

/// Split a command line like a shell would: spaces separate, '…' and "…" quote,
/// backslash escapes (outside single quotes).
pub fn split_args(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut chars = s.chars();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), c) => cur.push(c),
            // In double quotes a backslash only escapes " \\ $ ` (like a shell).
            (Some('"'), '\\') => match chars.next() {
                Some(n @ ('"' | '\\' | '$' | '`')) => cur.push(n),
                Some(n) => {
                    cur.push('\\');
                    cur.push(n);
                }
                None => cur.push('\\'),
            },
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                has = true;
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                has = true;
            }
            (None, c) if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (_, c) => cur.push(c),
        }
    }
    if quote.is_some() {
        anyhow::bail!("unclosed quote");
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// Name a source's series after it: one series takes the name itself, several become
/// `name.series`.
pub fn rename(name: &str, ds: &mut Dataset) {
    let single = ds.series.len() == 1;
    for s in &mut ds.series {
        s.name = if single {
            name.to_string()
        } else {
            format!("{name}.{}", s.name)
        };
    }
}

/// A dashboard of panes that all have their own `source=` and no main source: the
/// first such pane's source becomes the main one (so recents, profiles and the
/// builder work as usual). `args` includes the program name; returns the new
/// arguments, or `None` when there's nothing to do.
pub fn promote(cli: &Cli, args: &[String]) -> Result<Option<Vec<String>>> {
    if cli.cmd.is_some() || cli.input.is_some() || cli.has_parser() {
        return Ok(None);
    }
    // Only --merge sources: the first is the main one, named after itself.
    if let Some(first) = cli.merge.first() {
        let m = Merge::parse(first)?;
        let mut new = vec!["graft".to_string()];
        new.extend(m.args.iter().cloned());
        new.extend(["--as".to_string(), m.name.clone()]);
        let mut rest = args.iter().skip(1).peekable();
        let mut dropped = false;
        while let Some(a) = rest.next() {
            if !dropped && a == "--merge" && rest.peek().is_some_and(|v| *v == first) {
                rest.next();
                dropped = true;
            } else if !dropped && a.strip_prefix("--merge=") == Some(first.as_str()) {
                dropped = true;
            } else {
                new.push(a.clone());
            }
        }
        return Ok(Some(new));
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
    new.extend(args_for(&source, None, &cli.source)?);
    // The rest of the command line, with that pane now showing the main data.
    // (By position: the i-th --pane, written either `--pane SPEC` or `--pane=SPEC`.)
    let mut seen = 0;
    let mut rest = args.iter().skip(1);
    while let Some(a) = rest.next() {
        if a == "--pane" {
            let value = rest.next().cloned().unwrap_or_default();
            new.push(a.clone());
            new.push(if seen == i { first.to_spec() } else { value });
            seen += 1;
        } else if a.starts_with("--pane=") {
            new.push(if seen == i {
                format!("--pane={}", first.to_spec())
            } else {
                a.clone()
            });
            seen += 1;
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
    pub fn open(name: &str, interactive: bool, ssh: Option<&str>, defs: &[String]) -> Self {
        match Self::try_open(name, interactive, ssh, defs) {
            Ok(s) => s,
            Err(e) => Self::failed(name, format!("{e:#}")),
        }
    }

    /// Start a merged source.
    pub fn open_merge(m: &Merge, interactive: bool) -> Self {
        let cli = m.cli();
        match Self::start(&m.name, cli, interactive) {
            Ok(s) => s,
            Err(e) => Self::failed(&m.name, format!("{e:#}")),
        }
    }

    /// Everything it has read, named after it (for merging into another chart).
    pub fn merged(&self) -> Dataset {
        let mut ds = self.session.full_dataset();
        rename(&self.name, &mut ds);
        ds
    }

    /// What's wrong, if anything: an error from its command, or lines that don't fit
    /// its format.
    pub fn problem(&self) -> Option<String> {
        if let Some(e) = &self.view.error {
            return Some(e.clone());
        }
        (self.session.lines > 0 && self.session.matched == 0).then(|| {
            format!(
                "{} lines read, none matched its format (try its command with --explain=3)",
                self.session.lines
            )
        })
    }

    pub fn revision(&self) -> u64 {
        self.session.store.revision
    }

    /// Still sending data (a command that keeps running, a followed file).
    pub fn is_live(&self) -> bool {
        self.live.spec.is_live() && !self.live.eof
    }

    fn try_open(name: &str, interactive: bool, ssh: Option<&str>, defs: &[String]) -> Result<Self> {
        Self::start(name, cli_for(name, ssh, defs)?, interactive)
    }

    fn start(name: &str, cli: Cli, interactive: bool) -> Result<Self> {
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
pub fn sync(
    subs: &mut Vec<Sub>,
    opts: &ChartOpts,
    interactive: bool,
    ssh: Option<&str>,
    defs: &[String],
) {
    let want = wanted(opts);
    subs.retain(|s| want.contains(&s.name));
    for name in want {
        if !subs.iter().any(|s| s.name == name) {
            subs.push(Sub::open(&name, interactive, ssh, defs));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_and_argument_splitting() {
        assert_eq!(
            split_args(r#"-c "curl -s 'https://x/y?a=1'" --regex '"price":"(?P<v>[0-9.]+)"' a\ b"#)
                .unwrap(),
            [
                "-c",
                "curl -s 'https://x/y?a=1'",
                "--regex",
                r#""price":"(?P<v>[0-9.]+)""#,
                "a b"
            ]
        );
        assert!(split_args("'open").is_err());
        assert_eq!(
            split_args(r#"--regex "amount\W+(\d+)\"x""#).unwrap(),
            ["--regex", r#"amount\W+(\d+)"x"#]
        );
        let m = Merge::parse(
            r#"binance = -c "curl -s https://api.binance.com" --regex '"price":"(?P<v>[0-9.]+)"'"#,
        )
        .unwrap();
        assert_eq!(m.name, "binance");
        assert_eq!(
            m.cli().cmd.as_deref(),
            Some("curl -s https://api.binance.com")
        );
        // A preset by name.
        assert_eq!(
            Merge::parse("load").unwrap().cli().cmd.as_deref(),
            Some("cat /proc/loadavg")
        );
        assert!(
            Merge::parse("x = -c 'echo 1'")
                .unwrap_err()
                .to_string()
                .contains("say how to read it")
        );
        let mut ds = Dataset {
            x_kind: crate::data::XKind::Time,
            series: vec![crate::data::Series::new("v", Default::default())],
        };
        rename("binance", &mut ds);
        assert_eq!(ds.series[0].name, "binance");
    }
}
