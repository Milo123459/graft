//! Input sources: files (optionally followed), stdin, and periodically-run commands.
//! Each runs on its own thread and streams lines over a channel.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::data::{LineMeta, SampleInfo};
use crate::time::now_secs;

#[derive(Debug, Clone, PartialEq)]
pub enum SourceSpec {
    File {
        path: PathBuf,
        follow: bool,
    },
    Stdin,
    Command {
        cmd: String,
        every: Duration,
        count: Option<u64>,
    },
}

impl SourceSpec {
    /// A short name for titles.
    pub fn describe(&self) -> String {
        match self {
            Self::File { path, .. } => path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            Self::Stdin => "stdin".into(),
            Self::Command { cmd, .. } => cmd.clone(),
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Self::File { follow: true, .. } | Self::Stdin | Self::Command { count: None, .. }
        )
    }
}

#[derive(Debug)]
pub enum Event {
    Line(String, LineMeta),
    /// Separates `--cmd` runs.
    SampleDone,
    Error(String),
    Eof,
}

pub fn spawn(spec: SourceSpec) -> Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = match spec {
            SourceSpec::File { path, follow } => read_file(&path, follow, &tx),
            SourceSpec::Stdin => read_lines(std::io::stdin().lock(), &tx, None),
            SourceSpec::Command { cmd, every, count } => run_command(&cmd, every, count, &tx),
        };
        if let Err(e) = result {
            let _ = tx.send(Event::Error(e));
        }
        let _ = tx.send(Event::Eof);
    });
    rx
}

fn meta(sample: Option<SampleInfo>) -> LineMeta {
    LineMeta {
        arrival: now_secs(),
        sample,
    }
}

/// Send complete lines; returns Err only if the receiver hung up.
fn read_lines(
    mut r: impl BufRead,
    tx: &Sender<Event>,
    sample: Option<SampleInfo>,
) -> Result<(), String> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf)
                    .trim_end_matches(['\n', '\r'])
                    .to_string();
                if tx.send(Event::Line(line, meta(sample))).is_err() {
                    return Ok(());
                }
            }
            Err(e) => return Err(format!("read error: {e}")),
        }
    }
}

fn read_file(path: &PathBuf, follow: bool, tx: &Sender<Event>) -> Result<(), String> {
    let open = || File::open(path).map_err(|e| format!("{}: {e}", path.display()));
    if !follow {
        return read_lines(BufReader::new(open()?), tx, None);
    }
    let mut reader = BufReader::new(open()?);
    let mut pos = 0u64;
    let mut partial = Vec::new();
    loop {
        let mut chunk = Vec::new();
        let n = reader
            .read_until(b'\n', &mut chunk)
            .map_err(|e| format!("read error: {e}"))?;
        if n > 0 {
            pos += n as u64;
            partial.extend_from_slice(&chunk);
            if partial.ends_with(b"\n") {
                let line = String::from_utf8_lossy(&partial)
                    .trim_end_matches(['\n', '\r'])
                    .to_string();
                partial.clear();
                if tx.send(Event::Line(line, meta(None))).is_err() {
                    return Ok(());
                }
            }
            continue;
        }
        thread::sleep(Duration::from_millis(250));
        // Handle truncation / rotation: start again from the top.
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(pos);
        if len < pos
            && let Ok(f) = File::open(path)
        {
            reader = BufReader::new(f);
            pos = 0;
            partial.clear();
        }
        // Detect a closed channel even when the file is idle.
        if tx.send(Event::SampleDone).is_err() {
            return Ok(());
        }
    }
}

fn run_command(
    cmd: &str,
    every: Duration,
    count: Option<u64>,
    tx: &Sender<Event>,
) -> Result<(), String> {
    let mut index = 0;
    loop {
        let start = Instant::now();
        let sample = SampleInfo {
            index,
            time: now_secs(),
        };
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run `{cmd}`: {e}"))?;
        let stdout = child.stdout.take().expect("piped stdout");
        read_lines(BufReader::new(stdout), tx, Some(sample))?;
        let mut stderr = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut stderr);
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        if !status.success() {
            let detail = stderr.lines().next().unwrap_or("").trim();
            let _ = tx.send(Event::Error(format!(
                "`{cmd}` {status}{}{detail}",
                if detail.is_empty() { "" } else { ": " }
            )));
        }
        if tx.send(Event::SampleDone).is_err() {
            return Ok(());
        }
        index += 1;
        if count.is_some_and(|c| index >= c) {
            return Ok(());
        }
        thread::sleep(every.saturating_sub(start.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn follow_picks_up_appended_lines() {
        let path = std::env::temp_dir().join(format!("graft-follow-{}.log", std::process::id()));
        std::fs::write(&path, "a=1\n").unwrap();
        let rx = spawn(SourceSpec::File {
            path: path.clone(),
            follow: true,
        });
        let next_line = || loop {
            match rx.recv_timeout(Duration::from_secs(3)).expect("event") {
                Event::Line(l, _) => return l,
                Event::SampleDone => continue,
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(next_line(), "a=1");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(f, "a=").unwrap();
        f.flush().unwrap();
        thread::sleep(Duration::from_millis(400));
        writeln!(f, "2").unwrap();
        assert_eq!(next_line(), "a=2", "partial lines are joined");
        std::fs::write(&path, "a=9\n").unwrap();
        assert_eq!(next_line(), "a=9", "truncation restarts from the top");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn command_samples() {
        let rx = spawn(SourceSpec::Command {
            cmd: "echo x=1; echo y=2".into(),
            every: Duration::from_millis(10),
            count: Some(2),
        });
        let events: Vec<Event> = rx.iter().collect();
        let lines: Vec<(String, u64)> = events
            .iter()
            .filter_map(|e| match e {
                Event::Line(l, m) => Some((l.clone(), m.sample.unwrap().index)),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines,
            [
                ("x=1".into(), 0),
                ("y=2".into(), 0),
                ("x=1".into(), 1),
                ("y=2".into(), 1)
            ]
        );
        assert!(matches!(events.last(), Some(Event::Eof)));
    }
}
