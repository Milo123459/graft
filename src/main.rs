//! graft — graph anything in your terminal.

mod backtest;
mod book;
mod builder;
mod cli;
mod data;
mod discover;
mod explain;
mod export;
mod formula;
mod history;
mod interactive;
mod kitty;
mod lab;
mod lines;
mod marks;
mod notify;
mod params;
mod parse;
mod profile;
mod recent;
mod remote;
mod render;
mod script;
mod session;
mod setup;
#[cfg(test)]
mod snapshots;
mod source;
mod sources;
mod stats;
mod stream;
mod suggest;
mod template;
mod time;
mod transform;
mod tui;
mod tutorial;
mod ui;
mod units;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};

use crate::cli::Cli;
use crate::render::ChartOpts;
use crate::session::Session;
use crate::source::{Event, SourceSpec};
use crate::stats::ReportOpts;

/// Exit status when an `--alert` condition holds.
const ALERT_EXIT: u8 = 3;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        // `graft ... | head` closing the pipe early isn't an error.
        Err(e)
            if e.chain().any(|c| {
                c.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            }) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("graft: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let args = profile::expand(std::env::args_os().collect())?;
    let cli = Cli::parse_from(&args);
    // -p in a cluster of short flags (`-1p name`) isn't expanded: say so.
    if let Some(name) = &cli.profile {
        anyhow::bail!(
            "give -p on its own: `-p {name}` (it can't be combined with other short flags)"
        );
    }
    let (args, cli) = promote_dashboard(args, cli)?;

    if let Some(shell) = cli.completions {
        clap_complete::generate(shell, &mut Cli::command(), "graft", &mut std::io::stdout());
        return Ok(ExitCode::SUCCESS);
    }
    if cli.man {
        clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
        return Ok(ExitCode::SUCCESS);
    }
    if cli.list_profiles {
        std::io::stdout().write_all(profile::list()?.as_bytes())?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(name) = &cli.save_profile {
        let path = profile::save(name, &args)?;
        eprintln!("graft: saved profile '{name}' to {}", path.display());
    }

    let stdout_tty = std::io::stdout().is_terminal();
    if cli.tutorial {
        if !stdout_tty {
            anyhow::bail!("--tutorial needs a terminal");
        }
        let (setup, live) = tutorial::start()?;
        return interactive::run(interactive::Start::Viewer(Box::new(setup), live));
    }
    let color = stdout_tty && !cli.no_color;
    let interactive =
        !cli.once && !cli.no_chart && cli.explain.is_none() && cli.export.is_empty() && stdout_tty;
    let no_source = !cli.has_source();

    // The format is never guessed: without one, the builder offers suggestions to pick
    // from (interactive), or the error lists them as commands.
    let open_builder =
        cli.build || !cli.has_parser() || (no_source && std::io::stdin().is_terminal());
    if !interactive && !cli.has_parser() {
        return Err(no_format_error(&cli, interactive));
    }
    if interactive && open_builder {
        // Piped data can still be shaped in the builder.
        let live = (no_source && !std::io::stdin().is_terminal())
            .then(|| interactive::Live::new(SourceSpec::Stdin));
        return interactive::run(interactive::Start::Builder(Box::new(cli), live));
    }

    let spec = setup::source_spec(&cli, interactive)?;
    let mut setup = setup::build(cli, spec.clone(), color)?;
    let rx = source::spawn(spec.clone());

    if let Some(limit) = setup.cli.explain {
        explain::run(&mut setup.session, rx, limit, color)?;
        return Ok(ExitCode::SUCCESS);
    }
    if interactive {
        let live = interactive::Live::from_rx(spec, rx);
        return interactive::run(interactive::Start::Viewer(Box::new(setup), live));
    }
    let Setup {
        cli,
        mut session,
        opts,
        report,
        spec,
        timeframe,
    } = setup;
    run_once(
        &cli,
        &mut session,
        rx,
        &opts,
        &report,
        &spec,
        color,
        timeframe,
    )
}

use crate::setup::Setup;

/// Dashboards with no main source: see `sources::promote`.
fn promote_dashboard(
    args: Vec<std::ffi::OsString>,
    cli: Cli,
) -> Result<(Vec<std::ffi::OsString>, Cli)> {
    let strings: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match sources::promote(&cli, &strings)? {
        Some(new) => {
            let cli = Cli::try_parse_from(&new).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((new.into_iter().map(Into::into).collect(), cli))
        }
        None => Ok((args, cli)),
    }
}

/// "No format given", listing the formats that fit a sample of the input.
/// The first records of a stream (up to 50, or what came in 8 seconds).
/// A stream's first messages, to see what it sends: up to 50, listening a second
/// after the first (a busy feed fills that at once; a quiet one isn't waited on), or
/// 8 seconds for one that sends nothing.
pub fn sample_stream(spec: SourceSpec) -> Vec<String> {
    first_messages(&source::spawn(spec))
}

/// The first messages from `rx` (as `sample_stream` takes them).
pub fn first_messages(rx: &std::sync::mpsc::Receiver<source::Event>) -> Vec<String> {
    use std::time::{Duration, Instant};
    let mut end = Instant::now() + Duration::from_secs(8);
    let mut lines = Vec::new();
    while lines.len() < 50 {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(source::Event::Line(l, _)) => {
                if lines.is_empty() {
                    end = end.min(Instant::now() + Duration::from_secs(1));
                }
                lines.push(l);
            }
            Ok(source::Event::Error(_) | source::Event::Eof) | Err(_) => break,
            Ok(source::Event::SampleDone) => {}
        }
    }
    lines
}

fn no_format_error(cli: &Cli, interactive: bool) -> anyhow::Error {
    let what = cli
        .cmd
        .as_deref()
        .map(|c| format!("`{c}`"))
        .or_else(|| cli.stream.clone())
        .or_else(|| cli.input.as_ref().map(|p| p.display().to_string()))
        .unwrap_or_else(|| "stdin".into());
    let mut msg =
        format!("no format given for {what}. graft doesn't guess: choose how to parse it");
    let sample: Vec<String> = match setup::source_spec(cli, interactive) {
        Ok(SourceSpec::File { path, .. }) => std::fs::read_to_string(&path)
            .map(|s| s.lines().take(300).map(String::from).collect())
            .unwrap_or_default(),
        Ok(SourceSpec::Command { cmd, ssh, .. }) => {
            builder::sample_command_blocking(&cmd, ssh.as_deref()).unwrap_or_default()
        }
        Ok(SourceSpec::Remote { host, path, .. }) => builder::sample_command_blocking(
            &format!("head -n 300 {}", cli::shell_quote(&path)),
            Some(&host),
        )
        .unwrap_or_default(),
        Ok(spec @ SourceSpec::Stream { .. }) => sample_stream(spec),
        Ok(SourceSpec::Stdin) => std::io::stdin()
            .lines()
            .map_while(Result::ok)
            .take(300)
            .collect(),
        Err(_) => Vec::new(),
    };
    let from_command = cli.cmd.is_some();
    let script = std::rc::Rc::new(
        script::ScriptHost::new(None, cli.binary_units).expect("plain script host"),
    );
    let suggestions = suggest::suggest(&sample, cli, from_command, &script);
    if suggestions.is_empty() {
        msg.push_str(
            " with -F/--format, --regex, --kv, --csv, --json or --columns (--auto to guess)",
        );
    } else {
        msg.push_str(". Formats that fit:\n");
        for s in suggestions.iter().take(4) {
            msg.push_str(&format!(
                "  {}\n      ({} of {} sample lines)\n",
                s.cli.command_line(),
                s.matched,
                s.total
            ));
        }
        msg.push_str(
            "Run without --once in a terminal to pick one interactively, or add --auto to guess.",
        );
    }
    anyhow::anyhow!(msg)
}

/// Terminal size, from the tty or $COLUMNS/$LINES.
fn terminal_size() -> Option<(u16, u16)> {
    ratatui::crossterm::terminal::size()
        .ok()
        .filter(|&(w, h)| w > 0 && h > 0)
        .or_else(|| {
            let env = |k: &str| std::env::var(k).ok()?.parse::<u16>().ok();
            Some((env("COLUMNS")?, env("LINES")?))
        })
}

#[allow(clippy::too_many_arguments)]
fn run_once(
    cli: &Cli,
    session: &mut Session,
    rx: std::sync::mpsc::Receiver<Event>,
    opts: &ChartOpts,
    report: &ReportOpts,
    spec: &SourceSpec,
    color: bool,
    timeframe: transform::Timeframe,
) -> Result<ExitCode> {
    let cli = &cli.with_params();
    // A stream never ends: take --count records of it (1 by default).
    let mut records_left =
        matches!(spec, SourceSpec::Stream { .. }).then(|| cli.count.unwrap_or(1));
    for ev in rx {
        match ev {
            Event::Line(line, meta) => {
                let _ = session.feed(&line, &meta);
                if let Some(n) = records_left.as_mut() {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        break;
                    }
                }
            }
            Event::Error(e) => eprintln!("graft: {e}"),
            Event::SampleDone => {}
            Event::Eof => break,
        }
    }
    let mut full = session.base_dataset();
    if let Some(name) = &cli.as_name {
        sources::rename(name, &mut full);
    }
    // --merge sources: read each to the end and add its series.
    let mut merged: Vec<String> = Vec::new();
    for spec in &cli.merge {
        let m = sources::Merge::parse(spec)?;
        let mut sub = sources::Sub::open_merge(&m, false);
        sub.load_to_end(&transform::Timeframe::default());
        if let Some(e) = &sub.view.error {
            eprintln!("graft: {}: {e}", m.name);
        }
        let mut d = sub.merged();
        if full.series.is_empty() {
            full.x_kind = d.x_kind;
        }
        merged.extend(d.series.iter().map(|s| s.name.clone()));
        full.series.append(&mut d.series);
    }
    session.add_custom_lines(&mut full);
    for e in session.line_errors.borrow().iter() {
        eprintln!("graft: {e}");
    }
    timeframe.apply(&mut full);
    let mut ds = session.select(&full);
    for name in merged {
        if !ds.series.iter().any(|s| s.name == name)
            && let Some(s) = full.get(&name)
        {
            ds.series.push(s.clone());
        }
    }
    if ds.is_empty() {
        eprintln!(
            "graft: no data parsed from {} ({} lines read); try --explain to see how lines are parsed",
            spec.describe(),
            session.lines
        );
        return Ok(ExitCode::FAILURE);
    }
    // Markers from --mark and the script's mark_when().
    let rules: Vec<marks::MarkRule> = cli
        .mark
        .iter()
        .filter_map(|m| marks::MarkRule::parse(m).ok())
        .chain(session.script.take_mark_rules())
        .collect();
    let (placed, errors) = marks::evaluate(&session.script, &rules, &ds);
    for e in errors {
        eprintln!("graft: {e}");
    }
    let mut opts = ChartOpts {
        marks: placed,
        ..opts.clone()
    };
    // Titles can say the source's settings: --title '{coin} {interval}'.
    let params = params::parse_all(&cli.param);
    opts.title = params::fill(&opts.title, &params);
    if let Some(p) = opts.panes.as_mut() {
        for t in p.panes.iter_mut().filter_map(|q| q.title.as_mut()) {
            *t = params::fill(t, &params);
        }
    }
    let opts = &opts;

    let mut out = std::io::stdout().lock();
    if !cli.no_chart {
        let term = terminal_size();
        let width = cli.width.or(term.map(|t| t.0)).unwrap_or(100).max(20);
        let reserved = if cli.stats || cli.stats_template.is_some() {
            ds.series.len() as u16 * 2 + 3
        } else {
            0
        } + 1;
        let height = cli
            .height
            .unwrap_or_else(|| term.map_or(20, |t| t.1.saturating_sub(reserved).clamp(10, 40)))
            .max(5);
        // Panes with their own source: read those too.
        let views: Vec<render::SourceView> = sources::wanted(opts)
            .iter()
            .map(|name| {
                let mut sub = sources::Sub::open(name, false, cli.ssh.as_deref(), &cli.source);
                sub.load_to_end(&timeframe);
                sub.view
            })
            .collect();
        let buf = render::render_with_sources(&ds, Some(&full), opts, width, height, &views);
        out.write_all(render::ansi::to_ansi(&buf, color).as_bytes())?;
    }
    // A stats template or JSON format implies --stats.
    if cli.stats || cli.stats_template.is_some() || cli.stats_format == cli::StatsFormat::Json {
        let width = cli
            .width
            .map(usize::from)
            .or(terminal_size().map(|t| usize::from(t.0)));
        let report = ReportOpts {
            width,
            ..report.clone()
        };
        out.write_all(stats::report(&ds, &report)?.as_bytes())?;
        if cli.stats_format == cli::StatsFormat::Text && !opts.marks.is_empty() {
            writeln!(out, "\nmarkers:")?;
            for m in &opts.marks {
                writeln!(out, "  {}  {}", ds.fmt_x_full(m.x), m.label)?;
            }
        }
    }
    out.flush()?;
    for path in &cli.export {
        export::export(path, &ds, opts)?;
        eprintln!("graft: wrote {}", path.display());
    }

    session.script.set_data(full, None);
    let mut alerted = false;
    for alert in &cli.alert {
        if session
            .script
            .eval_bool(alert)
            .with_context(|| format!("--alert '{alert}'"))?
        {
            eprintln!("graft: ALERT: {alert}");
            alerted = true;
            for e in notify::send(&cli.notify, &opts.title, &format!("ALERT: {alert}")) {
                eprintln!("graft: {e}");
            }
        }
    }
    Ok(if alerted {
        ExitCode::from(ALERT_EXIT)
    } else {
        ExitCode::SUCCESS
    })
}
