//! Rendering snapshots: realistic scenarios rendered to colour ANSI, so the look of
//! every screen can be reviewed (and regressions spotted).
//!
//! `GRAFT_SNAPSHOTS=some/dir cargo test snapshots -- --include-ignored` writes each
//! scenario to `some/dir/<name>.ans`; the regular test run just checks that every
//! scenario renders sensibly.

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::Parser;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::builder::Builder;
use crate::cli::Cli;
use crate::interactive::Live;
use crate::render::{ansi::to_ansi, render_to_buffer};
use crate::source::{self, Event};
use crate::tui::App;

const LOG: &str = "examples/reclaimable.log";
const FMT: &str = "{time} node1: {node1} node2: {node2}";

/// A deterministic month of hourly samples shaped like a real disk-space log:
/// node1 drops quickly then wanders, node2 falls steadily with noise.
fn month_log() -> PathBuf {
    let path = std::env::temp_dir().join("graft-snapshot-month.log");
    let mut seed: u64 = 42;
    let mut rnd = || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 33) as f64 / (1u64 << 31) as f64) - 0.5
    };
    let (mut n1, mut n2) = (137.5f64, 724.5f64);
    let start = chrono::NaiveDate::from_ymd_opt(2026, 10, 2)
        .unwrap()
        .and_hms_opt(1, 0, 0)
        .unwrap();
    let mut out = String::new();
    for i in 0..755 {
        if i < 60 {
            n1 -= 1.3 + rnd() * 0.6
        } else {
            n1 += rnd() * 1.2 + 0.01
        }
        n1 = n1.max(48.0);
        n2 -= 0.42 + rnd() * 1.1;
        let t = start + chrono::Duration::hours(i);
        let _ = writeln!(
            out,
            "{}  node1: {n1:.1} GB  node2: {n2:.1} GB",
            t.format("%Y-%m-%d %H:%M")
        );
    }
    std::fs::write(&path, out).unwrap();
    path
}

/// Eight CPU-style percentage series sampled every minute.
fn cpu_log() -> PathBuf {
    let path = std::env::temp_dir().join("graft-snapshot-cpu.log");
    let names = [
        "user", "system", "iowait", "steal", "irq", "softirq", "nice", "guest",
    ];
    let start = chrono::NaiveDate::from_ymd_opt(2026, 11, 2)
        .unwrap()
        .and_hms_opt(9, 0, 0)
        .unwrap();
    let mut out = String::new();
    for i in 0..240 {
        let t = start + chrono::Duration::minutes(i);
        let _ = write!(out, "{}", t.format("%Y-%m-%d %H:%M:%S"));
        for (k, n) in names.iter().enumerate() {
            let v = (10.0 + 8.0 * k as f64) * (1.0 + ((i as f64) / (7.0 + k as f64)).sin() * 0.4);
            let _ = write!(out, " {n}={v:.1}%");
        }
        out.push('\n');
    }
    std::fs::write(&path, out).unwrap();
    path
}

fn cli(args: &[&str]) -> Cli {
    Cli::parse_from(std::iter::once("graft").chain(args.iter().copied()))
}

/// What `--once` prints (chart + stats), in colour.
fn once(args: &[&str], w: u16, h: u16) -> String {
    let cli = cli(args);
    let spec = crate::setup::source_spec(&cli, false).unwrap();
    let mut setup = crate::setup::build(cli, spec.clone(), true).unwrap();
    for ev in source::spawn(spec) {
        match ev {
            Event::Line(l, m) => {
                let _ = setup.session.feed(&l, &m);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let mut full = setup.session.full_dataset();
    setup.timeframe.apply(&mut full);
    let ds = setup.session.select(&full);
    let mut out = to_ansi(&render_to_buffer(&ds, &setup.opts, w, h), true);
    if setup.cli.stats {
        out.push_str(&crate::stats::report(&ds, &setup.report).unwrap());
    }
    out
}

fn viewer(args: &[&str]) -> App {
    let cli = cli(args);
    let spec = crate::setup::source_spec(&cli, true).unwrap();
    let setup = crate::setup::build(cli, spec.clone(), false).unwrap();
    let mut app = App::new(setup, Live::new(spec));
    app.test_settle();
    app
}

fn draw(w: u16, h: u16, f: impl FnOnce(&mut ratatui::Frame)) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(f).unwrap();
    to_ansi(term.backend().buffer(), true)
}

fn screen(app: &App, w: u16, h: u16) -> String {
    draw(w, h, |f| app.draw(f))
}

/// Every scenario: (name, rendered ANSI).
pub fn scenarios() -> Vec<(String, String)> {
    let month = month_log();
    let month = month.to_str().unwrap();
    let cpu = cpu_log();
    let cpu = cpu.to_str().unwrap();
    let mut v: Vec<(String, String)> = Vec::new();
    let mut add = |name: &str, s: String| v.push((name.to_string(), s));

    // --once
    add("once-basic", once(&[LOG, "-F", FMT, "--stats"], 100, 24));
    add(
        "once-split-forecast",
        once(
            &[
                LOG,
                "-F",
                FMT,
                "--stats",
                "--split",
                "--forecast",
                "--target",
                "node1=50GB,node2=200GB",
            ],
            100,
            30,
        ),
    );
    add(
        "once-month-combined",
        once(
            &[
                month,
                "-F",
                FMT,
                "--stats",
                "--forecast",
                "--target",
                "node1=50GB,node2=200GB",
            ],
            150,
            34,
        ),
    );
    add("once-cpu-many", once(&[cpu, "--kv", "--stats"], 120, 30));
    add("once-bar", once(&[cpu, "--kv", "-k", "bar"], 80, 14));
    add(
        "once-histogram",
        once(
            &[month, "-F", FMT, "-k", "histogram", "-s", "node2"],
            100,
            20,
        ),
    );
    add(
        "once-sparkline",
        once(&[cpu, "--kv", "-k", "sparkline"], 100, 26),
    );
    add("once-narrow", once(&[LOG, "-F", FMT, "--stats"], 44, 16));

    // Viewer, desktop size.
    let mut app = viewer(&[month, "-F", FMT]);
    add("view-default", screen(&app, 150, 45));
    app.test_keys("s");
    app.test_cursor_at(0.6);
    add("view-cursor-stats", screen(&app, 150, 45));
    let mut app = viewer(&[
        month,
        "-F",
        FMT,
        "--forecast",
        "--target",
        "node1=50GB,node2=200GB",
        "--stats",
        "--split",
    ]);
    app.test_cursor_at(0.85);
    add("view-split-forecast-cursor", screen(&app, 150, 45));
    app.test_keys("[");
    app.test_keys("[");
    add("view-timeframe", screen(&app, 150, 45));
    let app = viewer(&[month, "-F", FMT, "--split", "--trend=avg,median,p95"]);
    add("view-raw-and-trend", screen(&app, 150, 45));
    let mut app = viewer(&[month, "-F", FMT, "--trend=avg"]);
    app.test_keys("T");
    add("view-lines-menu", screen(&app, 150, 45));
    add("phone-lines-menu", screen(&app, 46, 34));
    app.test_keys("\x1bR");
    add("view-timeframe-menu", screen(&app, 150, 45));
    let mut app = viewer(&[
        month,
        "-F",
        FMT,
        "--side-by-side",
        "--right-panel=live,avg,p95",
    ]);
    app.test_keys("T");
    add("view-lines-menu-side-by-side", screen(&app, 150, 45));
    let mut app = viewer(&[
        month,
        "-F",
        FMT,
        "--pane",
        "node1 lines=live,median",
        "--pane",
        "node2",
        "--pane",
        "* kind=bar lines=live,avg",
        "--layout",
        "(1 / 2) |65% 3",
    ]);
    add("view-panes", screen(&app, 150, 45));
    app.test_keys("w");
    add("view-panes-panel", screen(&app, 150, 45));
    add("phone-panes-panel", screen(&app, 46, 40));
    let app = viewer(&[
        month,
        "-F",
        FMT,
        "--target",
        "node1=50GB,node2=200GB",
        "--pane",
        "* kind=stat",
        "--pane",
        "*",
        "--layout",
        "1 /30% 2",
    ]);
    add("view-stat-dashboard", screen(&app, 150, 45));
    add("phone-stat-dashboard", screen(&app, 46, 40));
    let app = viewer(&[month, "-F", FMT, "--side-by-side", "-k", "bar"]);
    add("view-side-by-side-bar", screen(&app, 150, 30));
    add("phone-timeframe-menu", screen(&app, 46, 34));
    let app = viewer(&[cpu, "--kv", "--trend=avg"]);
    add("view-trend-avg-many", screen(&app, 150, 45));
    let app = viewer(&[
        month,
        "-F",
        FMT,
        "--side-by-side",
        "--trend=avg",
        "--forecast",
    ]);
    add("view-side-by-side", screen(&app, 150, 45));
    add("phone-side-by-side", screen(&app, 46, 34));
    let mut app = viewer(&[cpu, "--kv", "--stats"]);
    add("view-many-series", screen(&app, 150, 45));
    app.test_keys("?");
    add("view-help", screen(&app, 150, 45));
    let mut app = viewer(&[month, "-F", FMT]);
    app.test_keys("v.......................................v");
    add("view-measured", screen(&app, 150, 45));
    let mut app = viewer(&[LOG, "-F", FMT]);
    app.test_keys(":");
    app.test_keys("slope(\"node2\", \"1d\")");
    add("view-console", screen(&app, 120, 30));
    app.test_keys("\x1b:help\n");
    add("view-console-help", screen(&app, 120, 40));

    // Viewer, phone size.
    let mut app = viewer(&[
        month,
        "-F",
        FMT,
        "--forecast",
        "--target",
        "node1=50GB",
        "--stats",
    ]);
    add("phone-view", screen(&app, 46, 30));
    app.test_cursor_at(0.5);
    app.test_keys("x");
    add("phone-split-cursor", screen(&app, 46, 30));

    // Builder.
    let mut b = Builder::new(cli(&[LOG]), None, None);
    add("builder-unset", draw(150, 45, |f| b.draw(f)));
    b.test_keys("\u{2193}");
    add("builder-picked", draw(150, 45, |f| b.draw(f)));
    add("phone-builder", draw(46, 34, |f| b.draw(f)));
    let b = Builder::new(cli(&[]), None, None);
    add("gallery", draw(120, 32, |f| b.draw(f)));
    add("phone-gallery", draw(46, 30, |f| b.draw(f)));
    // With recent charts.
    let file = std::env::temp_dir().join(format!("graft-snap-recent-{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&file);
    crate::recent::TEST_PATH.with(|t| *t.borrow_mut() = Some(file.clone()));
    crate::recent::remember(&cli(&[
        LOG,
        "-F",
        FMT,
        "--pane",
        "node1",
        "--pane",
        "node2 kind=bar",
    ]));
    crate::recent::remember(&cli(&[
        "-c",
        "ping -c 100 1.1.1.1",
        "-F",
        "time={ping}",
        "--trend=median",
    ]));
    let b = Builder::new(cli(&[]), None, None);
    add("gallery-recent", draw(120, 36, |f| b.draw(f)));
    add("phone-gallery-recent", draw(46, 34, |f| b.draw(f)));
    crate::recent::TEST_PATH.with(|t| *t.borrow_mut() = None);
    let _ = std::fs::remove_file(&file);
    v
}

/// Visible text of an ANSI string (escape codes removed).
fn strip(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in s.chars() {
        match (esc, c) {
            (false, '\x1b') => esc = true,
            (true, 'm') => esc = false,
            (true, _) => {}
            (false, c) => out.push(c),
        }
    }
    out
}

#[test]
fn every_scenario_renders() {
    for (name, ansi) in scenarios() {
        let text = strip(&ansi);
        assert!(text.lines().count() > 3, "{name} is nearly empty:\n{text}");
        assert!(
            !text.contains("waiting for data"),
            "{name} has no data:\n{text}"
        );
        assert!(!text.contains("format error"), "{name}:\n{text}");
    }
}

#[test]
#[ignore = "writes snapshot files; set GRAFT_SNAPSHOTS=dir"]
fn write_snapshots() {
    let Ok(dir) = std::env::var("GRAFT_SNAPSHOTS") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    for (name, ansi) in scenarios() {
        std::fs::write(PathBuf::from(&dir).join(format!("{name}.ans")), ansi).unwrap();
    }
}

#[test]
#[ignore = "runs the system dashboard for a few seconds; set GRAFT_SNAPSHOTS=dir"]
fn write_dashboard_snapshot() {
    let Ok(dir) = std::env::var("GRAFT_SNAPSHOTS") else {
        return;
    };
    let args: Vec<String> = ["graft", "-p", "dashboard"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let args: Vec<String> = crate::profile::expand(args.iter().map(Into::into).collect())
        .unwrap()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let c = <Cli as clap::Parser>::parse_from(&args);
    let args = crate::sources::promote(&c, &args).unwrap().unwrap();
    let c = <Cli as clap::Parser>::parse_from(&args);
    let spec = crate::setup::source_spec(&c, true).unwrap();
    let setup = crate::setup::build(c, spec.clone(), false).unwrap();
    let mut app = App::new(setup, Live::new(spec));
    let end = std::time::Instant::now() + std::time::Duration::from_secs(6);
    while std::time::Instant::now() < end {
        app.tick();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::fs::create_dir_all(&dir).unwrap();
    for (name, w, h) in [("view-dashboard", 150, 45), ("phone-dashboard", 46, 40)] {
        std::fs::write(
            PathBuf::from(&dir).join(format!("{name}.ans")),
            screen(&app, w, h),
        )
        .unwrap();
    }
}

/// Render any command line after letting it run: `GRAFT_SNAPSHOT_ARGS` holds the
/// arguments one per line, `GRAFT_SNAPSHOT_SECS` how long to run (default 5).
#[test]
#[ignore = "renders GRAFT_SNAPSHOT_ARGS into GRAFT_SNAPSHOTS"]
fn write_custom_snapshot() {
    let (Ok(dir), Ok(args)) = (
        std::env::var("GRAFT_SNAPSHOTS"),
        std::env::var("GRAFT_SNAPSHOT_ARGS"),
    ) else {
        return;
    };
    let secs: u64 = std::env::var("GRAFT_SNAPSHOT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let args: Vec<String> = std::iter::once("graft".to_string())
        .chain(args.lines().map(String::from))
        .collect();
    let c = <Cli as clap::Parser>::parse_from(&args);
    let spec = crate::setup::source_spec(&c, true).unwrap();
    let setup = crate::setup::build(c, spec.clone(), false).unwrap();
    let mut app = App::new(setup, Live::new(spec));
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < end {
        app.tick();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::fs::create_dir_all(&dir).unwrap();
    for (name, w, h) in [("custom", 150, 45), ("phone-custom", 46, 40)] {
        std::fs::write(PathBuf::from(&dir).join(format!("{name}.ans")), screen(&app, w, h)).unwrap();
    }
}
