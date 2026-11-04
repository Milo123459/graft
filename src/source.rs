//! Input sources: files (optionally followed), stdin, periodically-run commands and
//! streams (WebSockets, server-sent events: `stream.rs`).
//! Each runs on its own thread and streams lines over a channel.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::Stdio;
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
        /// Run it on this host over ssh.
        ssh: Option<String>,
    },
    /// A file on another machine, read over ssh.
    Remote {
        host: String,
        path: String,
        follow: bool,
    },
    /// A WebSocket or a streaming HTTP response (server-sent events or lines), with
    /// messages to send once connected. One that ends (an HTTP reply that's a
    /// snapshot, not a stream) is fetched again `every` so often.
    Stream {
        url: String,
        send: Vec<String>,
        every: Duration,
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
            Self::Command { cmd, ssh: None, .. } => cmd.clone(),
            Self::Command {
                cmd, ssh: Some(h), ..
            } => format!("{h}: {cmd}"),
            Self::Stream { url, .. } => crate::stream::short(url),
            Self::Remote { host, path, .. } => format!(
                "{host}:{}",
                std::path::Path::new(path)
                    .file_name()
                    .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned())
            ),
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Self::File { follow: true, .. }
                | Self::Remote { follow: true, .. }
                | Self::Stdin
                | Self::Stream { .. }
                | Self::Command { count: None, .. }
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
            SourceSpec::Command {
                cmd,
                every,
                count,
                ssh,
            } => run_command(&cmd, every, count, ssh.as_deref(), &tx),
            SourceSpec::Remote { host, path, follow } => read_remote(&host, &path, follow, &tx),
            SourceSpec::Stream { url, send, every } => crate::stream::run(&url, &send, every, &tx),
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

/// Which file a handle or path is (so a rotated log is noticed).
#[cfg(unix)]
fn meta_id(m: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}

#[cfg(not(unix))]
fn meta_id(_: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn file_id(f: &File) -> (u64, u64) {
    f.metadata().map(|m| meta_id(&m)).unwrap_or_default()
}

fn read_file(path: &PathBuf, follow: bool, tx: &Sender<Event>) -> Result<(), String> {
    let open = || File::open(path).map_err(|e| format!("{}: {e}", path.display()));
    if !follow {
        return read_lines(BufReader::new(open()?), tx, None);
    }
    let first = open()?;
    let mut identity = file_id(&first);
    let mut reader = BufReader::new(first);
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
        // Truncated (shorter than what was read) or rotated (a different file now has
        // the name): start again from the top of what's there now.
        let meta = std::fs::metadata(path).ok();
        let len = meta.as_ref().map_or(pos, |m| m.len());
        let rotated = meta.as_ref().is_some_and(|m| meta_id(m) != identity);
        if (len < pos || rotated)
            && let Ok(f) = File::open(path)
        {
            identity = file_id(&f);
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

/// Stream a remote file through `ssh host cat|tail -F`.
fn read_remote(host: &str, path: &str, follow: bool, tx: &Sender<Event>) -> Result<(), String> {
    let mut child =
        crate::remote::command(Some(host), &crate::remote::read_file_command(path, follow))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run ssh: {e}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    // Read stderr alongside, so a chatty error can't block the stream.
    let errors = thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    read_lines(BufReader::new(stdout), tx, None)?;
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        let stderr = errors.join().unwrap_or_default();
        return Err(
            crate::remote::explain_error(host, &stderr).unwrap_or_else(|| {
                format!(
                    "{host}:{path}: {}",
                    stderr.lines().next().unwrap_or("").trim()
                )
            }),
        );
    }
    Ok(())
}

fn run_command(
    cmd: &str,
    every: Duration,
    count: Option<u64>,
    ssh: Option<&str>,
    tx: &Sender<Event>,
) -> Result<(), String> {
    let mut index = 0;
    loop {
        let start = Instant::now();
        let sample = SampleInfo {
            index,
            time: now_secs(),
        };
        // {now_ms}, {from_ms:6h}…: the window each run asks for, ending now.
        let filled = crate::params::fill_times(cmd, sample.time);
        let cmd = filled.as_str();
        let mut child = crate::remote::command(ssh, cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run `{cmd}`: {e}"))?;
        let stdout = child.stdout.take().expect("piped stdout");
        // Drain stderr alongside stdout: a command that writes a lot to stderr would
        // otherwise block on a full pipe and never finish its stdout.
        let errors = child.stderr.take().map(|mut e| {
            thread::spawn(move || {
                let mut s = String::new();
                let _ = e.read_to_string(&mut s);
                s
            })
        });
        read_lines(BufReader::new(stdout), tx, Some(sample))?;
        let stderr = errors.and_then(|h| h.join().ok()).unwrap_or_default();
        let status = child.wait().map_err(|e| e.to_string())?;
        if !status.success() {
            let detail = stderr.lines().next().unwrap_or("").trim();
            let advice = ssh.and_then(|h| crate::remote::explain_error(h, &stderr));
            let _ = tx.send(Event::Error(advice.unwrap_or_else(|| {
                format!(
                    "`{cmd}` {status}{}{detail}",
                    if detail.is_empty() { "" } else { ": " }
                )
            })));
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
    fn follow_switches_to_a_rotated_file() {
        let dir = std::env::temp_dir().join(format!("graft-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.log");
        std::fs::write(&path, "a=1\n").unwrap();
        let rx = spawn(SourceSpec::File {
            path: path.clone(),
            follow: true,
        });
        let mut lines = Vec::new();
        let mut collect = |until: usize| {
            let end = Instant::now() + Duration::from_secs(3);
            while lines.len() < until && Instant::now() < end {
                if let Ok(Event::Line(l, _)) = rx.recv_timeout(Duration::from_millis(50)) {
                    lines.push(l);
                }
            }
        };
        collect(1);
        // Rotate: the old file moves away, a new one takes the name.
        std::fs::rename(&path, dir.join("app.log.1")).unwrap();
        std::fs::write(&path, "a=2\na=3\n").unwrap();
        collect(3);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(lines, ["a=1", "a=2", "a=3"]);
    }

    #[test]
    fn command_samples() {
        let rx = spawn(SourceSpec::Command {
            cmd: "echo x=1; echo y=2".into(),
            every: Duration::from_millis(10),
            count: Some(2),
            ssh: None,
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
