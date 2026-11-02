//! Turning a [`Cli`] into everything a run needs: the source, a parsing session,
//! chart options and analysis settings.

use std::io::IsTerminal;
use std::rc::Rc;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use ratatui::style::Color;

use crate::cli::Cli;
use crate::render::{ChartOpts, DEFAULT_PALETTE, HLine, Horizon};
use crate::script::ScriptHost;
use crate::session::Session;
use crate::source::SourceSpec;
use crate::stats::{Analysis, ReportOpts, Targets};
use crate::time::TimeParser;
use crate::transform::Timeframe;
use crate::units::parse_value;

pub struct Setup {
    pub timeframe: Timeframe,
    pub cli: Cli,
    pub session: Session,
    pub opts: ChartOpts,
    pub report: ReportOpts,
    pub spec: SourceSpec,
}

/// Build everything for `cli`, reading from `spec`.
pub fn build(cli: Cli, spec: SourceSpec, color: bool) -> Result<Setup> {
    let script = Rc::new(ScriptHost::new(cli.script.as_deref(), cli.binary_units)?);
    if cli.parse_fn && !script.has_fn("parse") {
        bail!("--parse-fn: the script doesn't define `fn parse(line)`");
    }
    let analysis = analysis(&cli)?;
    let session = Session::new(&cli, script)?;
    let opts = chart_opts(&cli, &spec, analysis.clone())?;
    let report = ReportOpts {
        analysis,
        color,
        format: cli.stats_format,
        template: cli.stats_template.clone(),
        width: None,
    };
    if let Some(t) = &report.template {
        // Validate the template up front with a dummy report.
        validate_stats_template(t)?;
    }
    let timeframe = timeframe(&cli)?;
    Ok(Setup {
        timeframe,
        cli,
        session,
        opts,
        report,
        spec,
    })
}

fn validate_stats_template(t: &str) -> Result<()> {
    use crate::data::{Dataset, Series, XKind};
    let s = Series {
        name: "x".into(),
        unit: Default::default(),
        points: vec![(0.0, 1.0), (1.0, 2.0)],
    };
    let ds = Dataset {
        x_kind: XKind::Index,
        series: vec![s],
    };
    let r = Analysis::default()
        .report(&ds, &ds.series[0])
        .context("internal: no report")?;
    r.fill(t).map(|_| ())
}

pub fn source_spec(cli: &Cli, interactive: bool) -> Result<SourceSpec> {
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
        (None, None) => bail!(
            "no input: give a FILE, pipe data in, use --cmd, or run `graft` in a terminal to open the builder"
        ),
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

pub fn timeframe(cli: &Cli) -> Result<Timeframe> {
    let tp = crate::time::TimeParser::new(cli.time_format.clone());
    let at = |s: &Option<String>, what: &str| -> Result<Option<f64>> {
        s.as_deref()
            .map(|v| tp.parse(v).or_else(|| TimeParser::default().parse(v)).with_context(|| format!("{what}: can't read '{v}' as a time (try 2026-10-15 or \"2026-10-15 08:00\")")))
            .transpose()
    };
    Ok(Timeframe {
        last: cli.last,
        since: at(&cli.since, "--since")?,
        until: at(&cli.until, "--until")?,
    })
}

pub fn analysis(cli: &Cli) -> Result<Analysis> {
    Ok(Analysis {
        model: cli.fit,
        window: cli.fit_window,
        targets: Targets::parse(&cli.target, cli.binary_units)?,
        per: cli
            .per
            .as_deref()
            .map(crate::time::parse_duration_secs)
            .transpose()
            .context("--per")?,
    })
}

pub fn chart_opts(cli: &Cli, spec: &SourceSpec, analysis: Analysis) -> Result<ChartOpts> {
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
    let mut trends = if cli.trend.is_empty() && cli.trend_only {
        vec![crate::cli::TrendStyle::Avg]
    } else {
        cli.trend.clone()
    };
    let mut show_raw = !cli.trend_only;
    // Side by side without --right-panel: the trends go right, the live data left.
    let right = match &cli.right_panel {
        Some(lines) => lines.clone(),
        None if cli.side_by_side && !trends.is_empty() => {
            show_raw = true;
            crate::cli::Lines {
                live: false,
                trends: std::mem::take(&mut trends),
            }
        }
        None => crate::cli::Lines::default_right(),
    };
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
        x_format: cli.x_format.clone(),
        trends,
        trend_window: cli.trend_window,
        show_raw,
        side_by_side: cli.side_by_side || cli.right_panel.is_some(),
        right,
        panes: crate::render::panes::Panes::from_args(&cli.pane, cli.layout.as_deref())
            .map_err(anyhow::Error::msg)?,
        show_targets: !cli.no_targets,
        analysis,
        forecast: cli
            .forecast
            .as_deref()
            .map(Horizon::parse)
            .transpose()
            .context("--forecast")?,
    })
}
