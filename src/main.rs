//! graft — graph anything in your terminal.

mod builder;
mod cli;
mod data;
mod explain;
mod export;
mod interactive;
mod parse;
mod profile;
mod render;
mod script;
mod session;
mod setup;
mod source;
mod stats;
mod suggest;
mod template;
mod time;
mod transform;
mod tui;
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
    let color = stdout_tty && !cli.no_color;
    let interactive =
        !cli.once && !cli.no_chart && cli.explain.is_none() && cli.export.is_empty() && stdout_tty;
    let no_source = cli.cmd.is_none() && cli.input.is_none();

    if interactive && (cli.build || (no_source && std::io::stdin().is_terminal())) {
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
    } = setup;
    run_once(&cli, &mut session, rx, &opts, &report, &spec, color)
}

use crate::setup::Setup;

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

fn run_once(
    cli: &Cli,
    session: &mut Session,
    rx: std::sync::mpsc::Receiver<Event>,
    opts: &ChartOpts,
    report: &ReportOpts,
    spec: &SourceSpec,
    color: bool,
) -> Result<ExitCode> {
    for ev in rx {
        match ev {
            Event::Line(line, meta) => {
                let _ = session.feed(&line, &meta);
            }
            Event::Error(e) => eprintln!("graft: {e}"),
            Event::SampleDone => {}
            Event::Eof => break,
        }
    }
    let full = session.full_dataset();
    let ds = session.select(&full);
    if ds.is_empty() {
        eprintln!(
            "graft: no data parsed from {} ({} lines read); try --explain to see how lines are parsed",
            spec.describe(),
            session.lines
        );
        return Ok(ExitCode::FAILURE);
    }

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
        let buf = render::render_to_buffer(&ds, opts, width, height);
        out.write_all(render::ansi::to_ansi(&buf, color).as_bytes())?;
    }
    // A stats template or JSON format implies --stats.
    if cli.stats || cli.stats_template.is_some() || cli.stats_format == cli::StatsFormat::Json {
        out.write_all(stats::report(&ds, report)?.as_bytes())?;
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
        }
    }
    Ok(if alerted {
        ExitCode::from(ALERT_EXIT)
    } else {
        ExitCode::SUCCESS
    })
}
