//! Telling you when an `--alert` fires (`--notify`), and copying text to the clipboard.
//!
//! Targets:
//! - `bell`: the terminal bell
//! - `desktop`: a desktop notification (notify-send on Linux, osascript on macOS)
//! - `ntfy:TOPIC` or `ntfy:https://server/TOPIC`: a push to your phone via ntfy
//! - `webhook:URL`: a JSON POST with `text` and `content` (Slack, Discord, …)
//! - `cmd:COMMAND`: run a command, with `$GRAFT_ALERT` and `$GRAFT_TITLE` set
//!
//! HTTP goes through `curl`, so no network code lives in graft itself.

use std::io::Write;
use std::process::{Command, Stdio};

/// Check a `--notify` target.
pub fn parse_target(s: &str) -> Result<String, String> {
    let ok = match s.split_once(':') {
        None => matches!(s, "bell" | "desktop"),
        Some(("ntfy" | "webhook" | "cmd", rest)) => !rest.trim().is_empty(),
        Some(_) => false,
    };
    if ok {
        Ok(s.to_string())
    } else {
        Err(format!(
            "'{s}': use bell, desktop, ntfy:TOPIC, webhook:URL or cmd:COMMAND"
        ))
    }
}

/// What a target would run (`None` for the bell, which is written to the terminal).
fn command_for(target: &str, title: &str, message: &str) -> Option<Command> {
    let (kind, rest) = target.split_once(':').unwrap_or((target, ""));
    match kind {
        "desktop" if cfg!(target_os = "macos") => {
            let mut c = Command::new("osascript");
            c.arg("-e").arg(format!(
                "display notification {} with title {}",
                applescript_string(message),
                applescript_string(title)
            ));
            Some(c)
        }
        "desktop" => {
            let mut c = Command::new("notify-send");
            c.arg(title).arg(message);
            Some(c)
        }
        "ntfy" => {
            let url = if rest.starts_with("http://") || rest.starts_with("https://") {
                rest.to_string()
            } else {
                format!("https://ntfy.sh/{rest}")
            };
            let mut c = Command::new("curl");
            c.args(["-fsS", "-m", "10", "-H"])
                .arg(format!("Title: {title}"))
                .args(["-H", "Tags: chart_with_downwards_trend", "-d"])
                .arg(message)
                .arg(url);
            Some(c)
        }
        "webhook" => {
            let text = format!("{title}: {message}");
            let body = serde_json::json!({ "text": text, "content": text }).to_string();
            let mut c = Command::new("curl");
            c.args([
                "-fsS",
                "-m",
                "10",
                "-H",
                "Content-Type: application/json",
                "-d",
            ])
            .arg(body)
            .arg(rest);
            Some(c)
        }
        "cmd" => {
            let mut c = Command::new("sh");
            c.arg("-c")
                .arg(rest)
                .env("GRAFT_ALERT", message)
                .env("GRAFT_TITLE", title);
            Some(c)
        }
        _ => None,
    }
}

fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Send to every target and wait; returns one error line per failed target.
pub fn send(targets: &[String], title: &str, message: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for t in targets {
        if t == "bell" {
            let mut out = std::io::stderr();
            let _ = out.write_all(b"\x07");
            let _ = out.flush();
            continue;
        }
        let Some(mut c) = command_for(t, title, message) else {
            continue;
        };
        match c
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => errors.push(format!(
                "--notify {t}: {}",
                String::from_utf8_lossy(&o.stderr)
                    .lines()
                    .next()
                    .unwrap_or("failed")
                    .trim()
            )),
            Err(e) => errors.push(format!("--notify {t}: {e}")),
        }
    }
    errors
}

/// Send without waiting (from the viewer).
pub fn send_in_background(targets: Vec<String>, title: String, message: String) {
    std::thread::spawn(move || send(&targets, &title, &message));
}

/// The escape sequence that puts `text` on the clipboard (OSC 52). Supported by most
/// modern terminals, including over SSH; tmux needs `set -g set-clipboard on`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_and_encoding() {
        assert!(parse_target("bell").is_ok() && parse_target("ntfy:disks").is_ok());
        assert!(parse_target("ntfy:").is_err() && parse_target("email:me").is_err());
        assert_eq!(base64(b"graft"), "Z3JhZnQ=");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
        let c = command_for("ntfy:disks", "graft", "node1 < 50GB").unwrap();
        let args: Vec<_> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().unwrap(), "https://ntfy.sh/disks");
        // cmd: targets get the alert in the environment.
        let dir = std::env::temp_dir().join(format!("graft-notify-{}", std::process::id()));
        let file = dir.with_extension("txt");
        let errors = send(
            &[format!(
                "cmd:echo \"$GRAFT_TITLE: $GRAFT_ALERT\" > {}",
                file.display()
            )],
            "disks",
            "node1 < 50GB",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "disks: node1 < 50GB\n"
        );
        let _ = std::fs::remove_file(&file);
        assert_eq!(send(&["cmd:exit 3".into()], "t", "m").len(), 1);
    }
}
