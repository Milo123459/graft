//! End-to-end tests of the `graft` binary in non-interactive mode.

use std::io::Write;
use std::process::{Command, Output, Stdio};

const LOG: &str = "examples/reclaimable.log";
const FMT: &str = "{time} node1: {node1} node2: {node2}";

fn graft(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graft"))
        .args(args)
        .env_remove("NO_COLOR")
        .env(
            "GRAFT_PROFILES",
            std::env::temp_dir().join("graft-test-no-profiles.toml"),
        )
        .output()
        .expect("run graft")
}

fn graft_stdin(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_graft"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run graft");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn once_draws_chart_and_trend() {
    let o = graft(&[
        LOG, "-F", FMT, "--once", "--stats", "--width", "90", "--height", "16",
    ]);
    assert!(o.status.success());
    let out = stdout(&o);
    assert!(!out.contains('\x1b'), "no colour when piped");
    assert!(
        out.contains("node1 58.8 GB") && out.contains("node2 415.7 GB"),
        "{out}"
    );
    assert!(
        out.contains("node2: falling by an average of 7.69 GB/day"),
        "{out}"
    );
    assert_eq!(out.lines().next().unwrap().chars().count(), 90);
}

#[test]
fn alerts_set_exit_status() {
    let fired = graft(&[LOG, "-F", FMT, "--no-chart", "--alert", "node1 < 59GB"]);
    assert_eq!(fired.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&fired.stderr).contains("ALERT: node1 < 59GB"));
    let quiet = graft(&[
        LOG,
        "-F",
        FMT,
        "--no-chart",
        "--alert",
        "last(\"node1\") < 10GB",
    ]);
    assert_eq!(quiet.status.code(), Some(0));
}

#[test]
fn explain_reports_each_line() {
    let out = stdout(&graft(&[LOG, "-F", FMT, "--explain=2"]));
    assert!(
        out.contains("1 ✓ 2026-11-01 04:00 │ node1=59.9 GB  node2=422.6 GB"),
        "{out}"
    );
    assert!(out.contains("29 of 29 lines produced data"), "{out}");
}

#[test]
fn csv_export_round_trips() {
    let path = std::env::temp_dir().join(format!("graft-test-{}.csv", std::process::id()));
    let o = graft(&[
        LOG,
        "-F",
        FMT,
        "--no-chart",
        "--resample",
        "12h:max",
        "-o",
        path.to_str().unwrap(),
    ]);
    assert!(o.status.success());
    let csv = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(csv.lines().next(), Some("time,node1,node2"));
    assert_eq!(
        csv.lines().nth(1),
        Some("2026-11-01 00:00,59900000000,422800000000")
    );
}

#[test]
fn stdin_columns_with_header() {
    let o = graft_stdin(
        &[
            "--columns",
            "all",
            "--once",
            "--no-chart",
            "--stats",
            "-s",
            "free",
        ],
        "r b free\n1 0 100\n2 0 80\n1 1 60\n",
    );
    let out = stdout(&o);
    assert!(
        out.contains("free: falling by an average of 20/sample"),
        "{out}"
    );
}

#[test]
fn command_source() {
    let o = graft(&[
        "-c",
        "echo used=40%; echo free=60%",
        "--kv",
        "--once",
        "-k",
        "bar",
        "--width",
        "40",
        "--height",
        "6",
    ]);
    let out = stdout(&o);
    assert!(out.contains("used") && out.contains("60%"), "{out}");
}

#[test]
fn helpful_error_when_nothing_parses() {
    let o = graft_stdin(&["--once", "--kv"], "hello\nworld\n");
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("--explain"));
}

#[test]
fn bad_arguments_are_reported() {
    let o = graft(&[LOG, "-F", FMT, "--once", "--hline", "lots"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("--hline: 'lots' is not a number"));
    let o = graft(&[LOG, "-F", FMT, "--once", "--series", "oops"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("NAME=EXPR"));
}

#[test]
fn completions_and_man() {
    assert!(stdout(&graft(&["--completions", "zsh"])).contains("#compdef graft"));
    assert!(stdout(&graft(&["--man"])).contains(".TH graft"));
}

#[test]
fn format_template_targets_and_custom_stats_output() {
    let o = graft(&[
        LOG,
        "-F",
        "{time} node1: {node1} node2: {node2}",
        "--no-chart",
        "--target",
        "node1=50GB,node2=200GB",
        "--stats-template",
        "{name}: {trend} → {target} in {eta} ({eta_at})",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let out = stdout(&o);
    assert!(
        out.starts_with("node1: -1.97 GB/day → 50 GB in 3.7 days (2026-11-0"),
        "{out}"
    );
    assert!(
        out.contains("node2: -7.69 GB/day → 200 GB in 27.6 days"),
        "{out}"
    );
}

#[test]
fn stats_json_and_no_default_target() {
    let o = graft(&[
        LOG,
        "-F",
        FMT,
        "--no-chart",
        "--stats",
        "--stats-format",
        "json",
        "--target",
        "none",
        "--fit",
        "exp",
    ]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).expect("valid JSON");
    assert_eq!(v[0]["name"], "node1");
    assert_eq!(v[0]["trend"]["model"], "exp");
    assert!(v[0]["target"].is_null() && v[0]["eta_seconds"].is_null());
}

#[test]
fn forecast_is_drawn_in_once_mode() {
    let out = stdout(&graft(&[
        LOG,
        "-F",
        FMT,
        "--once",
        "--forecast=3d",
        "--target",
        "node1=50GB",
        "-s",
        "node1",
        "--width",
        "80",
        "--height",
        "14",
    ]));
    assert!(
        out.contains("[forecast]") && out.contains("node1 target 50 GB"),
        "{out}"
    );
}

#[test]
fn bad_template_is_reported() {
    let o = graft(&[LOG, "--once", "-F", "{time} {oops"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("unclosed '{'"));
}

#[test]
fn timeframe_flags() {
    let o = graft(&[
        LOG,
        "-F",
        FMT,
        "--no-chart",
        "--stats-template",
        "{name} {n}",
        "--last",
        "6h",
    ]);
    assert_eq!(stdout(&o), "node1 7\nnode2 7\n");
    let o = graft(&[
        LOG,
        "-F",
        FMT,
        "--no-chart",
        "--stats-template",
        "{name} {n}",
        "--since",
        "2026-11-02",
        "--until",
        "2026-11-02 03:00",
    ]);
    assert_eq!(stdout(&o), "node1 4\nnode2 4\n");
    let o = graft(&[LOG, "-F", FMT, "--no-chart", "--since", "yesterday"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("--since: can't read 'yesterday'"));
}

#[test]
fn a_format_is_required_and_suggested() {
    let o = graft(&[LOG, "--once"]);
    assert_eq!(o.status.code(), Some(1));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("no format given for examples/reclaimable.log"),
        "{err}"
    );
    assert!(
        err.contains(
            "graft --format '{time} node1: {node1} node2: {node2}' --once examples/reclaimable.log"
        ),
        "{err}"
    );
    assert!(err.contains("(29 of 29 sample lines)"), "{err}");
    // Guessing is opt-in.
    assert!(
        graft(&[LOG, "--once", "--auto", "--no-chart"])
            .status
            .success()
    );
}

#[test]
fn grep_counts_matching_lines_per_bucket() {
    let log = "2026-11-02 10:00:05 INFO ok\n\
               2026-11-02 10:00:20 ERROR boom\n\
               2026-11-02 10:00:40 ERROR again\n\
               2026-11-02 10:03:10 ERROR later\n\
               1.2.3.4 - - [02/Nov/2026:10:03:30 +0000] \"GET / HTTP/1.1\" 500\n";
    let o = graft_stdin(
        &[
            "--grep",
            "errors=ERROR| 500$",
            "--grep",
            "lines=.",
            "--no-chart",
            "--stats-template",
            "{name} {n} {max} {sum}",
        ],
        log,
    );
    let out = stdout(&o);
    // Buckets 10:00 (2 errors), 10:01 and 10:02 (none), 10:03 (2: ERROR and the 500).
    assert!(
        out.contains("errors 4 2 4"),
        "{out}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(out.contains("lines 4 3 5"), "{out}");
}

/// A stand-in `ssh` that runs the remote command locally (host "denied" fails like a
/// key-less login), first on PATH.
fn fake_ssh_path() -> String {
    let dir = std::env::temp_dir().join(format!("graft-fake-ssh-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("ssh");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         while [ $# -gt 0 ]; do case \"$1\" in -o) shift 2;; --) shift; break;; *) host=$1; shift;; esac; done\n\
         if [ \"$host\" = denied ]; then echo 'denied: Permission denied (publickey).' >&2; exit 255; fi\n\
         exec sh -c \"$*\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn graft_with_path(args: &[&str], path: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graft"))
        .args(args)
        .env("PATH", path)
        .env(
            "GRAFT_PROFILES",
            std::env::temp_dir().join("graft-test-no-profiles.toml"),
        )
        .env(
            "GRAFT_RECENT",
            std::env::temp_dir().join("graft-test-no-recent.toml"),
        )
        .output()
        .expect("run graft")
}

#[test]
fn ssh_runs_commands_and_reads_files_remotely() {
    let path = fake_ssh_path();
    // A command, with shell syntax that must reach the remote shell intact.
    let o = graft_with_path(
        &[
            "--ssh",
            "pi",
            "-c",
            "echo \"used=$((2+3))\"",
            "--kv",
            "--no-chart",
            "--stats-template",
            "{name} {last}",
        ],
        &path,
    );
    assert_eq!(
        stdout(&o),
        "used 5\n",
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    // A file on the remote machine.
    let log = std::fs::canonicalize(LOG).unwrap();
    let o = graft_with_path(
        &[
            "--ssh",
            "pi",
            log.to_str().unwrap(),
            "-F",
            FMT,
            "--no-chart",
            "--stats-template",
            "{name} {n}",
        ],
        &path,
    );
    assert_eq!(
        stdout(&o),
        "node1 29\nnode2 29\n",
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    // Presets pick the remote OS's variant.
    let o = graft_with_path(
        &[
            "-p",
            "load",
            "--ssh",
            "pi",
            "--no-chart",
            "--stats-template",
            "{name}",
        ],
        &path,
    );
    assert!(
        stdout(&o).contains("1 min"),
        "{}{}",
        stdout(&o),
        String::from_utf8_lossy(&o.stderr)
    );
    // Login problems come with advice.
    let o = graft_with_path(
        &["--ssh", "denied", "-c", "uptime", "--kv", "--once"],
        &path,
    );
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("ssh-copy-id denied"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
