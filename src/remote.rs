//! `--ssh HOST`: run commands (or read files) on another machine.
//!
//! One SSH connection is opened and reused (OpenSSH connection sharing), so sampling a
//! command every second doesn't log in every second. Authentication must work without
//! a prompt (keys or an agent): graft owns the terminal.

use std::process::Command;

/// The `ssh` options graft uses: no prompts, a shared connection kept for two minutes.
fn ssh_options() -> Vec<String> {
    // The shared connection's socket: in a directory only this user can write to.
    let control = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".ssh")))
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
        .join("graft-ssh-%C");
    [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ControlMaster=auto",
        "-o",
        &format!("ControlPath={}", control.display()),
        "-o",
        "ControlPersist=120",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// A process running `cmd` through the shell, here or on `host`.
pub fn command(host: Option<&str>, cmd: &str) -> Command {
    match host {
        Some(host) => {
            let mut c = Command::new("ssh");
            // The remote shell runs `cmd` as given.
            c.args(ssh_options()).arg(host).arg("--").arg(cmd);
            c
        }
        None => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(cmd);
            c
        }
    }
}

/// The command that reads a remote file: once, or following it like `tail -f`.
pub fn read_file_command(path: &str, follow: bool) -> String {
    let p = crate::cli::shell_quote(path);
    if follow {
        format!("tail -n +1 -F {p}")
    } else {
        format!("cat {p}")
    }
}

/// The remote machine's OS, as `std::env::consts::OS` names it ("linux", "macos").
pub fn remote_os(host: &str) -> Option<&'static str> {
    let out = command(Some(host), "uname -s").output().ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "Linux" => Some("linux"),
        "Darwin" => Some("macos"),
        "FreeBSD" => Some("freebsd"),
        _ => None,
    }
}

/// Turn ssh's own failure messages into advice.
pub fn explain_error(host: &str, stderr: &str) -> Option<String> {
    let s = stderr.to_lowercase();
    if s.contains("permission denied") {
        Some(format!(
            "ssh {host}: login needs a password or passphrase, and graft can't ask for one. \
             Set up key login (ssh-copy-id {host}) or load your key into ssh-agent (ssh-add), then try again."
        ))
    } else if s.contains("could not resolve hostname") {
        Some(format!(
            "ssh {host}: unknown host (check the name, or add it to ~/.ssh/config)"
        ))
    } else if s.contains("host key verification failed") {
        Some(format!(
            "ssh {host}: unknown host key; run `ssh {host}` once yourself to accept it"
        ))
    } else if s.contains("connection refused") || s.contains("timed out") {
        Some(format!("ssh {host}: can't connect ({})", stderr.trim()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_run_here_or_over_ssh() {
        let local = command(None, "echo hi");
        assert_eq!(local.get_program(), "sh");
        let remote = command(Some("pi@raspberrypi"), "head -1 /proc/stat");
        assert_eq!(remote.get_program(), "ssh");
        let args: Vec<_> = remote
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.contains(&"BatchMode=yes".to_string())
                && args.contains(&"ControlMaster=auto".to_string())
        );
        assert_eq!(
            &args[args.len() - 3..],
            ["pi@raspberrypi", "--", "head -1 /proc/stat"]
        );
        assert_eq!(
            read_file_command("/var/log/my app.log", true),
            "tail -n +1 -F '/var/log/my app.log'"
        );
        assert!(
            explain_error("pi", "pi@pi: Permission denied (publickey).")
                .unwrap()
                .contains("ssh-copy-id pi")
        );
    }
}
