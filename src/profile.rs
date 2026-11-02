//! Named argument profiles, stored in `~/.config/graft/profiles.toml`.
//!
//! ```toml
//! [profiles.reclaim]
//! description = "Reclaimable space per node"
//! args = ["/var/log/reclaimable.log", "--stats", "--hline", "50GB:red:low"]
//! ```

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub args: Vec<String>,
}

/// What a preset needs to work on this machine.
#[derive(Debug, Clone, Copy)]
pub enum Needs {
    File(&'static str),
    Program(&'static str),
    /// `std::env::consts::OS`, e.g. "linux" or "macos".
    Os(&'static str),
}

impl Needs {
    fn met(self) -> bool {
        match self {
            Needs::File(p) => std::path::Path::new(p).exists(),
            Needs::Os(os) => std::env::consts::OS == os,
            Needs::Program(name) => std::env::var_os("PATH").is_some_and(|paths| {
                std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
            }),
        }
    }
}

/// A ready-made chart, shown in the quick-start gallery and runnable as `graft -p NAME`.
#[derive(Debug, Clone)]
pub struct Preset {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub args: &'static [&'static str],
    pub needs: &'static [Needs],
}

impl Preset {
    pub fn available(&self) -> bool {
        self.needs.iter().all(|n| n.met())
    }

    pub fn profile(&self) -> Profile {
        Profile {
            description: Some(self.description.into()),
            args: self.args.iter().map(|s| s.to_string()).collect(),
        }
    }
}

const CPU_TOTAL: &str = "(user + nice + system + idle + iowait + irq + softirq + steal)";

/// Built-in presets. Every one spells out its format — nothing is guessed.
/// Some have per-OS variants (Linux reads /proc; macOS uses its own tools).
pub fn presets() -> Vec<Preset> {
    // Leak the two CPU expressions once so they can live in a 'static table.
    let busy: &'static str = Box::leak(
        format!("busy:% = (user + nice + system + irq + softirq + steal) / {CPU_TOTAL} * 100")
            .into_boxed_str(),
    );
    let iowait: &'static str =
        Box::leak(format!("iowait:% = iowait / {CPU_TOTAL} * 100").into_boxed_str());
    let cpu_args: &'static [&'static str] = Box::leak(
        vec![
            "--cmd",
            "head -1 /proc/stat",
            "--every",
            "1s",
            "--format",
            "cpu {user} {nice} {system} {idle} {iowait} {irq} {softirq} {steal}",
            "--rate",
            "--series",
            busy,
            "--series",
            iowait,
            "--select",
            "busy,iowait",
            "--ymin",
            "0",
            "--ymax",
            "100",
            "--title",
            "CPU usage",
        ]
        .into_boxed_slice(),
    );
    #[rustfmt::skip]
    let presets = vec![
        // ── Linux ──
        Preset {
            name: "cpu", title: "CPU usage", description: "busy % and I/O wait %, every second",
            args: cpu_args,
            needs: &[Needs::File("/proc/stat"), Needs::Program("head")],
        },
        Preset {
            name: "memory", title: "Memory", description: "used, available and cached memory",
            args: &["--cmd", "cat /proc/meminfo", "--every", "2s", "--format", "{key}: {value}", "--binary-units",
                "--series", "used:B = MemTotal - MemAvailable", "--select", "used,MemAvailable,Cached", "--ymin", "0", "--title", "Memory"],
            needs: &[Needs::File("/proc/meminfo"), Needs::Program("cat")],
        },
        Preset {
            name: "load", title: "Load average", description: "1, 5 and 15 minute load",
            args: &["--cmd", "cat /proc/loadavg", "--every", "2s", "--columns", "1,2,3", "--names", "1 min,5 min,15 min",
                "--ymin", "0", "--title", "Load average"],
            needs: &[Needs::File("/proc/loadavg"), Needs::Program("cat")],
        },
        Preset {
            name: "network", title: "Network traffic", description: "received / sent bytes per second, per interface",
            args: &["--cmd", "cat /proc/net/dev", "--every", "1s",
                "--format", "{key}:{_:/\\s*/}{rx} {_:word} {_:word} {_:word} {_:word} {_:word} {_:word} {_:word} {tx}",
                "--rate", "--unit", "B", "--exclude", "lo.*", "--title", "Network traffic"],
            needs: &[Needs::File("/proc/net/dev"), Needs::Program("cat")],
        },
        Preset {
            name: "temperature", title: "CPU temperature", description: "°C from the thermal sensor (e.g. Raspberry Pi)",
            args: &["--cmd", "cat /sys/class/thermal/thermal_zone0/temp", "--every", "2s", "--columns", "1", "--names", "raw",
                "--series", "temp:°C = raw / 1000", "--exclude", "raw", "--title", "CPU temperature"],
            needs: &[Needs::File("/sys/class/thermal/thermal_zone0/temp"), Needs::Program("cat")],
        },
        Preset {
            name: "ping", title: "Ping latency", description: "round-trip time to 1.1.1.1 every 2 s",
            args: &["--cmd", "ping -c 1 -W 2 1.1.1.1", "--every", "2s", "--format", "time={latency}", "--title", "Ping 1.1.1.1"],
            needs: &[Needs::Os("linux"), Needs::Program("ping")],
        },
        // ── macOS ──
        Preset {
            name: "cpu", title: "CPU usage", description: "busy, user and system % (from top), every 2 s",
            args: &["--cmd", "top -l 2 -n 0 -s 1 | grep 'CPU usage' | tail -1", "--every", "2s",
                "--format", "CPU usage: {user} user, {sys} sys, {idle} idle",
                "--series", "busy:% = user + sys", "--select", "busy,user,sys", "--ymin", "0", "--ymax", "100", "--title", "CPU usage"],
            needs: &[Needs::Os("macos"), Needs::Program("top")],
        },
        Preset {
            name: "memory", title: "Memory", description: "free memory % (from memory_pressure)",
            args: &["--cmd", "memory_pressure | tail -1", "--every", "5s", "--format", "percentage: {free}",
                "--ymin", "0", "--ymax", "100", "--title", "Memory"],
            needs: &[Needs::Os("macos"), Needs::Program("memory_pressure")],
        },
        Preset {
            name: "load", title: "Load average", description: "1, 5 and 15 minute load",
            args: &["--cmd", "sysctl -n vm.loadavg", "--every", "2s", "--columns", "2,3,4", "--names", "1 min,5 min,15 min",
                "--ymin", "0", "--title", "Load average"],
            needs: &[Needs::Os("macos"), Needs::Program("sysctl")],
        },
        Preset {
            name: "network", title: "Network traffic", description: "received / sent bytes per second on en0",
            args: &["--cmd", "netstat -I en0 -b | grep Link", "--every", "1s",
                "--format", "{_:word} {_:word} {_:word} {_:word} {_:word} {_:word} {rx} {_:word} {_:word} {tx}",
                "--rate", "--unit", "B", "--title", "Network en0"],
            needs: &[Needs::Os("macos"), Needs::Program("netstat")],
        },
        Preset {
            name: "ping", title: "Ping latency", description: "round-trip time to 1.1.1.1 every 2 s",
            args: &["--cmd", "ping -c 1 -t 2 1.1.1.1", "--every", "2s", "--format", "time={latency}", "--title", "Ping 1.1.1.1"],
            needs: &[Needs::Os("macos"), Needs::Program("ping")],
        },
        // ── both ──
        Preset {
            name: "disk", title: "Disk usage", description: "% used per mounted filesystem",
            args: &["--cmd", "df -kP", "--every", "30s", "--format", "{_:word} {_:word} {_:word} {_:word} {value} {key}", "--kind", "bar",
                "--exclude", "/dev*,/sys*,/run*,/proc*,/snap*,/private/var/vm,/System/Volumes/VM,/System/Volumes/Preboot,/System/Volumes/Update*,/System/Volumes/xarts,/System/Volumes/iSCPreboot,/System/Volumes/Hardware",
                "--title", "Disk usage"],
            needs: &[Needs::Program("df")],
        },
        Preset {
            name: "processes", title: "Processes", description: "number of running processes",
            args: &["--cmd", "ps -A -o pid= | wc -l", "--every", "2s", "--columns", "1", "--names", "processes", "--title", "Processes"],
            needs: &[Needs::Program("ps"), Needs::Program("wc")],
        },
    ];
    presets
}

/// Presets by name; where there are per-OS variants, the one that works here wins.
fn builtin() -> BTreeMap<String, Profile> {
    builtin_for(None)
}

/// Presets for this machine, or (`Some(os)`) for a remote one reached over ssh, where
/// only the OS is known: its files and programs are assumed to be there.
fn builtin_for(remote_os: Option<&str>) -> BTreeMap<String, Profile> {
    let mut out: BTreeMap<String, (bool, Profile)> = BTreeMap::new();
    for p in presets() {
        let ok = match remote_os {
            None => p.available(),
            Some(os) => p.needs.iter().all(|n| match n {
                Needs::Os(o) => *o == os,
                // /proc and /sys mean Linux.
                Needs::File(f) if f.starts_with("/proc") || f.starts_with("/sys") => os == "linux",
                _ => true,
            }),
        };
        if out.get(p.name).is_none_or(|(had, _)| !had && ok) {
            out.insert(p.name.to_string(), (ok, p.profile()));
        }
    }
    out.into_iter().map(|(k, v)| (k, v.1)).collect()
}

/// Saved profiles, for the gallery.
pub fn saved() -> Vec<(String, Profile)> {
    load_file()
        .map(|f| f.profiles.into_iter().collect())
        .unwrap_or_default()
}

#[cfg(test)]
thread_local! {
    /// Tests point profiles at a file of their own without touching the environment.
    pub static TEST_PROFILES: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

pub fn config_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(p) = TEST_PROFILES.with(|t| t.borrow().clone()) {
        return Some(p);
    }
    if let Some(p) = std::env::var_os("GRAFT_PROFILES") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("graft").join("profiles.toml"))
}

fn load_file() -> Result<ProfileFile> {
    let Some(path) = config_path() else {
        return Ok(ProfileFile::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProfileFile::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn find(name: &str, ssh: Option<&str>) -> Result<Profile> {
    if let Some(p) = load_file()?.profiles.remove(name) {
        return Ok(p);
    }
    // Over ssh, pick the variant for the other machine's OS.
    let remote = ssh.map(|h| crate::remote::remote_os(h).unwrap_or("linux"));
    builtin_for(remote)
        .remove(name)
        .with_context(|| format!("no profile named '{name}' (see --list-profiles)"))
}

/// Replace `-p NAME` / `--profile NAME` with the profile's arguments. Profile
/// arguments come first so explicit flags override them.
pub fn expand(args: Vec<OsString>) -> Result<Vec<OsString>> {
    let mut rest = Vec::with_capacity(args.len());
    let mut profile = None;
    let mut iter = args.into_iter();
    let argv0 = iter.next().unwrap_or_else(|| "graft".into());
    while let Some(arg) = iter.next() {
        let s = arg.to_string_lossy();
        if s == "--" {
            rest.push(arg);
            rest.extend(iter.by_ref());
            break;
        }
        let name = if s == "-p" || s == "--profile" {
            Some(
                iter.next()
                    .context("--profile needs a name")?
                    .to_string_lossy()
                    .into_owned(),
            )
        } else if let Some(v) = s.strip_prefix("--profile=") {
            Some(v.to_string())
        } else {
            s.strip_prefix("-p")
                .filter(|v| !v.is_empty() && !s.starts_with("--"))
                .map(|v| v.to_string())
        };
        match name {
            Some(n) if profile.is_some() => {
                bail!("only one --profile may be given (got another: '{n}')")
            }
            Some(n) => profile = Some(n),
            None => rest.push(arg),
        }
    }
    let mut out = vec![argv0];
    if let Some(name) = profile {
        // `@`, `@2`: a recent chart.
        if let Some(recent) = crate::recent::find(&name) {
            let recent = recent?;
            recent.enter_dir();
            out.extend(recent.args.into_iter().map(OsString::from));
        } else {
            let ssh = rest.iter().enumerate().find_map(|(i, a)| {
                let a = a.to_string_lossy();
                if a == "--ssh" {
                    rest.get(i + 1).map(|h| h.to_string_lossy().into_owned())
                } else {
                    a.strip_prefix("--ssh=").map(String::from)
                }
            });
            out.extend(
                find(&name, ssh.as_deref())?
                    .args
                    .into_iter()
                    .map(OsString::from),
            );
        }
    }
    out.extend(rest);
    Ok(out)
}

/// Save the (already expanded) arguments, minus `--save-profile`, under `name`.
pub fn save(name: &str, args: &[OsString]) -> Result<PathBuf> {
    let path =
        config_path().context("can't determine config directory (set HOME or GRAFT_PROFILES)")?;
    let mut keep = Vec::new();
    let mut iter = args
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = iter.next() {
        if a == "--save-profile" {
            iter.next();
        } else if !a.starts_with("--save-profile=") {
            keep.push(a);
        }
    }
    let mut file = load_file()?;
    let description = file.profiles.get(name).and_then(|p| p.description.clone());
    file.profiles.insert(
        name.to_string(),
        Profile {
            description,
            args: keep,
        },
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, toml::to_string_pretty(&file)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./,:=@%+".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub fn list() -> Result<String> {
    let saved = load_file()?.profiles;
    let mut out = String::new();
    let section = |title: &str, profiles: &BTreeMap<String, Profile>, out: &mut String| {
        if profiles.is_empty() {
            return;
        }
        let _ = writeln!(out, "{title}:");
        for (name, p) in profiles {
            let _ = writeln!(
                out,
                "  {name}{}",
                p.description
                    .as_ref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default()
            );
            let _ = writeln!(
                out,
                "      graft {}",
                p.args
                    .iter()
                    .map(|a| shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    };
    section(
        &format!(
            "saved ({})",
            config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
        &saved,
        &mut out,
    );
    let builtins: BTreeMap<_, _> = builtin()
        .into_iter()
        .filter(|(k, _)| !saved.contains_key(k))
        .collect();
    section("built-in", &builtins, &mut out);
    let recent = crate::recent::list();
    if !recent.is_empty() {
        let now = chrono::Local::now().timestamp();
        let _ = writeln!(
            out,
            "recent (graft -p @ opens the latest, -p @2 the one before…):"
        );
        for (i, r) in recent.iter().enumerate() {
            let _ = writeln!(out, "  @{} {} — {}", i + 1, r.title(), r.summary(now));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_presets_follow_the_remote_os() {
        let mac = builtin_for(Some("macos"));
        assert!(
            mac["cpu"].args.iter().any(|a| a.contains("top -l")),
            "{:?}",
            mac["cpu"].args
        );
        assert!(mac["load"].args.iter().any(|a| a.contains("sysctl")));
        let linux = builtin_for(Some("linux"));
        assert!(
            linux["load"]
                .args
                .iter()
                .any(|a| a.contains("/proc/loadavg"))
        );
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn expands_builtin_profile_first() {
        let out = expand(os(&["graft", "--rate", "-p", "load", "--title", "x"])).unwrap();
        let out: Vec<String> = out
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(out[0], "graft");
        assert_eq!(out[1], "--cmd");
        assert_eq!(&out[out.len() - 3..], ["--rate", "--title", "x"]);
        assert!(expand(os(&["graft", "-p", "a", "-p", "b"])).is_err());
    }

    #[test]
    fn no_profile_is_identity() {
        let args = os(&["graft", "file.log", "--", "-p"]);
        assert_eq!(expand(args.clone()).unwrap(), args);
    }

    #[test]
    fn presets_are_valid() {
        use clap::Parser;
        for p in presets() {
            let cli = crate::cli::Cli::try_parse_from(
                std::iter::once("graft").chain(p.args.iter().copied()),
            )
            .unwrap_or_else(|e| panic!("{}: {e}", p.name));
            assert!(cli.has_parser(), "{} must spell out its format", p.name);
            let script =
                std::rc::Rc::new(crate::script::ScriptHost::new(None, cli.binary_units).unwrap());
            crate::session::Session::new(&cli, script)
                .unwrap_or_else(|e| panic!("{}: {e:#}", p.name));
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("cat /proc/meminfo"), "'cat /proc/meminfo'");
        assert_eq!(shell_quote("--every"), "--every");
    }
}
