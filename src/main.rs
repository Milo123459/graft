//! graft — graph anything in your terminal.

mod cli;
mod data;
mod explain;
mod export;
mod parse;
mod profile;
mod render;
mod script;
mod session;
mod source;
mod stats;
mod time;
mod transform;
mod tui;
mod units;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::rc::Rc;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use ratatui::style::Color;

use crate::cli::Cli;
use crate::render::{ChartOpts, DEFAULT_PALETTE, HLine};
use crate::script::ScriptHost;
use crate::session::Session;
use crate::source::{Event, SourceSpec};
use crate::stats::ReportOpts;
use crate::units::parse_value;

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
    let spec = source_spec(&cli, interactive)?;

    let script = Rc::new(ScriptHost::new(cli.script.as_deref(), cli.binary_units)?);
    if cli.parse_fn && !script.has_fn("parse") {
        bail!("--parse-fn: the script doesn't define `fn parse(line)`");
    }
    let mut session = Session::new(&cli, script.clone())?;
    let opts = chart_opts(&cli, &spec)?;
    let report = report_opts(&cli, color)?;
    let rx = source::spawn(spec.clone());

    if let Some(limit) = cli.explain {
        explain::run(&mut session, rx, limit, color)?;
        return Ok(ExitCode::SUCCESS);
    }
    if interactive {
        let app = tui::App::new(
            session,
            rx,
            opts,
            report,
            cli.alert.clone(),
            spec,
            cli.stats,
        );
        tui::run(app)?;
        return Ok(ExitCode::SUCCESS);
    }
    run_once(&cli, &mut session, rx, &opts, &report, &spec, color)
}

fn source_spec(cli: &Cli, interactive: bool) -> Result<SourceSpec> {
    Ok(match (&cli.cmd, &cli.input) {
        (Some(cmd), _) => {
            // Non-interactive runs take one sample unless told otherwise.
            let count = cli.count.or((!interactive).then_some(1));
            SourceSpec::Command {
                cmd: cmd.clone(),
                every: cli.every,
                count,
            }
        }
        (None, Some(p)) if p.as_os_str() == "-" => SourceSpec::Stdin,
        (None, Some(p)) => {
            if !p.exists() {
                bail!("{}: no such file", p.display());
            }
            SourceSpec::File {
                path: p.clone(),
                follow: cli.follow && interactive,
            }
        }
        (None, None) if !std::io::stdin().is_terminal() => SourceSpec::Stdin,
        (None, None) => bail!("no input: give a FILE, pipe data in, or use --cmd (see --help)"),
    })
}

fn value_arg(s: &str, what: &str, binary: bool) -> Result<f64> {
    parse_value(s, binary)
        .map(|v| v.0)
        .with_context(|| format!("{what}: '{s}' is not a number"))
}

fn parse_color(s: &str) -> Result<Color> {
    Color::from_str(s.trim()).map_err(|_| {
        anyhow::anyhow!("unknown colour '{s}' (try a name like `cyan`, `#ff8800` or 0-255)")
    })
}

fn chart_opts(cli: &Cli, spec: &SourceSpec) -> Result<ChartOpts> {
    let b = cli.binary_units;
    let mut palette = cli
        .colors
        .iter()
        .map(|c| parse_color(c))
        .collect::<Result<Vec<_>>>()?;
    palette.extend(DEFAULT_PALETTE);
    let hlines = cli
        .hline
        .iter()
        .map(|spec| {
            let mut parts = spec.splitn(3, ':');
            let value = value_arg(parts.next().unwrap_or(""), "--hline", b)?;
            let color = parts
                .next()
                .filter(|c| !c.is_empty())
                .map(parse_color)
                .transpose()?;
            let label = parts.next().map(String::from);
            Ok(HLine {
                value,
                color,
                label,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ChartOpts {
        kind: cli.kind,
        marker: cli.marker,
        title: cli.title.clone().unwrap_or_else(|| spec.describe()),
        palette,
        legend: cli.legend,
        log: cli.log,
        ymin: cli
            .ymin
            .as_deref()
            .map(|v| value_arg(v, "--ymin", b))
            .transpose()?,
        ymax: cli
            .ymax
            .as_deref()
            .map(|v| value_arg(v, "--ymax", b))
            .transpose()?,
        hlines,
        bins: cli.bins.max(1),
        split: cli.split,
    })
}

fn report_opts(cli: &Cli, color: bool) -> Result<ReportOpts> {
    Ok(ReportOpts {
        per: cli
            .per
            .as_deref()
            .map(time::parse_duration_secs)
            .transpose()
            .context("--per")?,
        target: cli
            .target
            .as_deref()
            .map(|v| value_arg(v, "--target", cli.binary_units))
            .transpose()?,
        color,
    })
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
        let reserved = if cli.stats {
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
    if cli.stats {
        out.write_all(stats::report(&ds, report).as_bytes())?;
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
