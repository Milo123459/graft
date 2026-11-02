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

fn builtin() -> BTreeMap<String, Profile> {
    let p = |desc: &str, args: &[&str]| Profile {
        description: Some(desc.into()),
        args: args.iter().map(|s| s.to_string()).collect(),
    };
    BTreeMap::from([
        (
            "meminfo".into(),
            p(
                "Free/available/cached memory from /proc/meminfo",
                &[
                    "--cmd",
                    "cat /proc/meminfo",
                    "--every",
                    "1s",
                    "--kv",
                    "--binary-units",
                    "--select",
                    "MemAvailable,MemFree,Cached,Buffers",
                    "--title",
                    "memory",
                ],
            ),
        ),
        (
            "loadavg".into(),
            p(
                "1/5/15 minute load average",
                &[
                    "--cmd",
                    "cat /proc/loadavg",
                    "--every",
                    "2s",
                    "--columns",
                    "1,2,3",
                    "--names",
                    "load1,load5,load15",
                    "--title",
                    "load average",
                ],
            ),
        ),
        (
            "net".into(),
            p(
                "Received bytes per second per interface",
                &[
                    "--cmd",
                    "cat /proc/net/dev",
                    "--every",
                    "1s",
                    "--kv",
                    "--rate",
                    "--unit",
                    "B",
                    "--exclude",
                    "lo",
                    "--title",
                    "network rx",
                ],
            ),
        ),
        (
            "cpu-temp".into(),
            p(
                "CPU temperature (e.g. on a Raspberry Pi), °C",
                &[
                    "--cmd",
                    "cat /sys/class/thermal/thermal_zone0/temp",
                    "--every",
                    "2s",
                    "--columns",
                    "1",
                    "--names",
                    "raw",
                    "--series",
                    "celsius = raw / 1000",
                    "--exclude",
                    "raw",
                    "--title",
                    "CPU temperature (°C)",
                ],
            ),
        ),
    ])
}

pub fn config_path() -> Option<PathBuf> {
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

fn find(name: &str) -> Result<Profile> {
    if let Some(p) = load_file()?.profiles.remove(name) {
        return Ok(p);
    }
    builtin()
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
        out.extend(find(&name)?.args.into_iter().map(OsString::from));
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
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn expands_builtin_profile_first() {
        let out = expand(os(&["graft", "--rate", "-p", "loadavg", "--title", "x"])).unwrap();
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
    fn quoting() {
        assert_eq!(shell_quote("cat /proc/meminfo"), "'cat /proc/meminfo'");
        assert_eq!(shell_quote("--every"), "--every");
    }
}
