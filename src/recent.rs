//! Recently viewed charts, so `graft` on its own can offer them again.
//!
//! Each file or command keeps one entry (its latest view: format, panes, lines…),
//! newest first, in `~/.local/state/graft/recent.toml`. They can also be reopened
//! with `graft -p @` (the latest) or `graft -p @2`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::Cli;

/// How many charts are remembered.
const KEEP: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recent {
    /// Arguments (after `graft`), with paths made absolute.
    pub args: Vec<String>,
    /// Where it was run: commands may depend on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Unix seconds when last viewed.
    pub at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecentFile {
    #[serde(default)]
    recent: Vec<Recent>,
}

#[cfg(test)]
thread_local! {
    pub static TEST_PATH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn path() -> Option<PathBuf> {
    // Tests only ever see a file they set up.
    #[cfg(test)]
    return TEST_PATH.with(|t| t.borrow().clone());
    #[cfg(not(test))]
    {
        recent_path_from_env()
    }
}

#[cfg(not(test))]
fn recent_path_from_env() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GRAFT_RECENT") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state"))
        })?;
    Some(base.join("graft").join("recent.toml"))
}

fn load() -> Vec<Recent> {
    load_checked().unwrap_or_default()
}

/// The list, or an error if the file is there but can't be read (so it isn't
/// overwritten — and lost — by the next chart).
fn load_checked() -> Result<Vec<Recent>> {
    let Some(p) = path() else {
        return Ok(Vec::new());
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok(toml::from_str::<RecentFile>(&s)
            .with_context(|| format!("{} is damaged; not updating it", p.display()))?
            .recent),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

fn store(recent: Vec<Recent>) -> Result<()> {
    let path = path().context("can't determine a state directory (set HOME)")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&RecentFile { recent })?)
        .with_context(|| format!("writing {}", path.display()))
}

/// Newest first.
pub fn list() -> Vec<Recent> {
    load()
}

/// A copy with file paths made absolute, so it works from any directory.
pub fn portable(cli: &Cli) -> Cli {
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut c = cli.clone();
    // Remote paths are the remote machine's business.
    if let Some(p) = &cli.input
        && p.as_os_str() != "-"
        && cli.ssh.is_none()
    {
        c.input = Some(abs(p));
    }
    c.script = cli.script.as_deref().map(abs);
    c.export = Vec::new();
    c.save_profile = None;
    c
}

/// Same file or command: the newer view replaces the older.
fn same_source(a: &Cli, b: &Cli) -> bool {
    a.input == b.input && a.cmd == b.cmd && a.ssh == b.ssh
}

/// Remember a chart that was viewed. Piped input can't be reopened, so it isn't kept.
pub fn remember(cli: &Cli) {
    let _ = try_remember(cli);
}

fn try_remember(cli: &Cli) -> Result<()> {
    let from_stdin = cli.cmd.is_none() && cli.input.as_ref().is_none_or(|p| p.as_os_str() == "-");
    if from_stdin || !cli.has_parser() {
        return Ok(());
    }
    let cli = portable(cli);
    let entry = Recent {
        args: cli.to_args(),
        cwd: cli
            .cmd
            .as_ref()
            .and_then(|_| std::env::current_dir().ok())
            .map(|d| d.display().to_string()),
        at: chrono::Local::now().timestamp(),
    };
    let mut all = load_checked()?;
    all.retain(|r| r.cli().is_none_or(|c| !same_source(&c, &cli)));
    all.insert(0, entry);
    all.truncate(KEEP);
    store(all)
}

/// Remove the `n`th (0-based) entry.
pub fn forget(n: usize) -> Result<()> {
    let mut all = load();
    if n < all.len() {
        all.remove(n);
        store(all)?;
    }
    Ok(())
}

/// `@`, `@1`, `@2`… → the arguments of that recent chart.
pub fn find(name: &str) -> Option<Result<Recent>> {
    let n = name.strip_prefix('@')?;
    let n: usize = if n.is_empty() {
        1
    } else {
        match n.parse() {
            Ok(n) if n >= 1 => n,
            _ => {
                return Some(Err(anyhow::anyhow!(
                    "'{name}': recent charts are @1, @2, … (see --list-profiles)"
                )));
            }
        }
    };
    Some(
        load()
            .into_iter()
            .nth(n - 1)
            .with_context(|| format!("no recent chart {name} (see --list-profiles)")),
    )
}

impl Recent {
    pub fn cli(&self) -> Option<Cli> {
        <Cli as clap::Parser>::try_parse_from(
            std::iter::once("graft".to_string()).chain(self.args.iter().cloned()),
        )
        .ok()
    }

    /// Run commands where they were first run.
    pub fn enter_dir(&self) {
        if let Some(d) = &self.cwd
            && Path::new(d).is_dir()
        {
            let _ = std::env::set_current_dir(d);
        }
    }

    /// A short name: the file name, or the command.
    pub fn title(&self) -> String {
        let Some(cli) = self.cli() else {
            return self.args.join(" ");
        };
        if let Some(t) = &cli.title {
            return t.clone();
        }
        let host = cli
            .ssh
            .as_ref()
            .map(|h| format!("{h}: "))
            .unwrap_or_default();
        if let Some(c) = &cli.cmd {
            return format!("{host}{c}");
        }
        let name = cli
            .input
            .as_ref()
            .map(|p| {
                p.file_name().map_or_else(
                    || p.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                )
            })
            .unwrap_or_default();
        format!("{host}{name}")
    }

    /// "2h ago · 3 panes · bar".
    pub fn summary(&self, now: i64) -> String {
        let ago = (now - self.at).max(0) as f64;
        let mut parts = vec![if ago < 60.0 {
            "just now".to_string()
        } else {
            format!("{} ago", crate::time::fmt_ago(ago))
        }];
        if let Some(cli) = self.cli() {
            if let Some(dir) = cli.input.as_ref().and_then(|p| p.parent()) {
                parts.push(dir.display().to_string());
            }
            if !cli.pane.is_empty() {
                parts.push(format!("{} panes", cli.pane.len()));
            } else if cli.side_by_side || cli.right_panel.is_some() {
                parts.push("side by side".into());
            }
            if cli.kind != crate::cli::ChartKind::Line {
                parts.push(cli.kind.name().into());
            }
            if !cli.trend.is_empty() {
                parts.push(
                    cli.trend
                        .iter()
                        .map(|t| t.label())
                        .collect::<Vec<_>>()
                        .join(" + "),
                );
            }
            if let Some(w) = cli.last {
                parts.push(format!("last {}", crate::cli::fmt_window(w)));
            }
        }
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_file<T>(f: impl FnOnce() -> T) -> T {
        let p = std::env::temp_dir().join(format!(
            "graft-recent-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&p);
        TEST_PATH.with(|t| *t.borrow_mut() = Some(p.clone()));
        let out = f();
        TEST_PATH.with(|t| *t.borrow_mut() = None);
        let _ = std::fs::remove_file(&p);
        out
    }

    fn cli(args: &[&str]) -> Cli {
        <Cli as clap::Parser>::parse_from(args)
    }

    #[test]
    fn a_damaged_list_is_left_alone() {
        with_file(|| {
            let p = path().unwrap();
            std::fs::write(&p, "[[recent]]\nargs = [\"x\"]\nat = \"yesterday\"\n").unwrap();
            remember(&cli(&["graft", "-c", "uptime", "--kv"]));
            assert!(
                std::fs::read_to_string(&p).unwrap().contains("yesterday"),
                "not overwritten"
            );
        });
    }

    #[test]
    fn newest_first_one_per_source_and_reopenable() {
        with_file(|| {
            remember(&cli(&[
                "graft",
                "examples/reclaimable.log",
                "-F",
                "{time} node1: {a}",
            ]));
            remember(&cli(&["graft", "-c", "uptime", "--kv"]));
            // Piped input and charts without a format aren't kept.
            remember(&cli(&["graft", "--kv"]));
            remember(&cli(&["graft", "x.log"]));
            // The same file again replaces its entry and moves to the top.
            remember(&cli(&[
                "graft",
                "examples/reclaimable.log",
                "-F",
                "{time} node1: {a}",
                "-k",
                "bar",
            ]));
            let all = list();
            assert_eq!(all.len(), 2);
            assert_eq!(all[0].title(), "reclaimable.log");
            assert!(
                Path::new(all[0].args.last().unwrap()).is_absolute(),
                "{:?}",
                all[0].args
            );
            assert!(all[0].summary(all[0].at).starts_with("just now ·"));
            assert!(
                all[0].summary(all[0].at + 7200).contains("2h ago")
                    && all[0].summary(0).contains("bar")
            );
            assert_eq!(all[1].title(), "uptime");
            assert!(all[1].cwd.is_some(), "commands remember where they ran");
            // -p @ / @2
            assert_eq!(find("@").unwrap().unwrap(), all[0]);
            assert_eq!(find("@2").unwrap().unwrap(), all[1]);
            assert!(find("@3").unwrap().is_err() && find("@x").unwrap().is_err());
            assert!(find("disks").is_none(), "plain names are profiles");
            forget(0).unwrap();
            assert_eq!(list()[0].title(), "uptime");
        });
    }
}
