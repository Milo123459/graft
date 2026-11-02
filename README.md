# graft

Graph anything in your terminal: log files, command output, CSV or JSON. Charts
resize to fit the terminal, everything is set with command-line flags, and an
interactive viewer lets you zoom, inspect, select ranges and run analysis with
[Rhai](https://rhai.rs) scripts.

```
$ graft reclaimable.log --once --stats --split
┌ reclaimable.log · node1 ─────────────────────────────────────────────────┐
│60 GB     │⠒⠒⠒⠒⠒⠒⠒⠒⠒⠒⠒⠢⡀                                   ┌─────────────┐│
│          │            ⠈⠢⠤⠤⡀                               │node1 58.8 GB││
│          │                ⠘⢄                              └─────────────┘│
│          │                  ⠉⠑⠒⠊⠉⠉⠉⠉⠉⠑⢄                              ⢠⠃  │
│          │                             ⠉⠑⠒⢄                         ⢠⠃   │
│57.4 GB   │                                ⠈⠢⠤⠒⠒⠤⠤⠔⠒⠒⠒⠒⠒⠒⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠉⠁    │
│          └───────────────────────────────────────────────────────────────│
│11-01 04:00                 11-01 13:20    11-01 22:40         11-02 08:00│
└──────────────────────────────────────────────────────────────────────────┘
┌ reclaimable.log · node2 ─────────────────────────────────────────────────┐
│423.3 GB  │⠒⠒⠒⠤⣀⣀⣀⣀⣀⡀                                     ┌──────────────┐│
│          │         ⠈⠑⠤⢄⣀⣀⣀⣀                              │node2 415.7 GB││
│          │                 ⠉⠉⠑⠒⠢⠤⣀                       └──────────────┘│
│          │                        ⠑⠒⠒⠒⠢⠤⢄⣀⣀                          ⢀⣀⣀⣀│
│          │                                 ⠉⠉⠉⠉⠒⠢⢄        ⡠⠒⠒⠒⠒⠉⠉⠉⠉⠉⠉⠁   │
│411.7 GB  │                                        ⠉⠑⠒⠒⠤⠤⠤⠔⠁              │
│          └───────────────────────────────────────────────────────────────│
│11-01 04:00                 11-01 13:20    11-01 22:40         11-02 08:00│
└──────────────────────────────────────────────────────────────────────────┘
series   n      last       min      mean       max         σ  trend/day    R²
node1   29   58.8 GB   57.5 GB   58.5 GB   59.9 GB  882.6 MB   -1.97 GB  0.61
node2   29  415.7 GB  412.2 GB  417.1 GB  422.8 GB   3.14 GB   -7.69 GB  0.73

• node1: falling by an average of 1.97 GB/day (R² 0.61); reaches 0 B in ~29.1 days (around 2026-12-01 10:46)
• node2: falling by an average of 7.69 GB/day (R² 0.73); reaches 0 B in ~53.6 days (around 2026-12-25 23:00)
```

## Install

```sh
cargo install --path .                 # terminal charts + SVG/CSV/JSON export
cargo install --path . --features png  # also PNG export (pulls in resvg)
```

## Quick tour

```sh
graft app.log                          # auto-detects timestamps and key: value pairs
graft app.log --once                   # print the chart and exit (also used when piped)
graft -c 'cat /proc/meminfo' -n 1s -B  # run a command every second, graph it live
some-command | graft                   # graph stdin
graft app.log -f                       # follow a growing file like tail -f
graft -p meminfo                       # run a profile (see --list-profiles)
```

### Getting data in

| Input | Flags |
|---|---|
| File / stdin | `graft FILE`, `graft -`, or pipe into `graft` |
| Follow a file | `-f, --follow` (handles truncation/rotation) |
| Run a command repeatedly | `-c, --cmd 'COMMAND'`, `-n, --every 2s`, `--count N` |
| Keep only recent data | `-w, --window 6h` or `--window 500` (points) |

### Parsing

The default parser looks for `key: value` / `key=value` pairs. If it finds
none, it falls back to whitespace-separated numeric columns and uses a header
row for names if there is one. Timestamps at the start of a line are detected
automatically: ISO-8601 and similar formats, syslog `Nov  2 08:00:01`, a bare
`HH:MM:SS`, and Unix epochs in seconds or milliseconds.

| Format | Flags |
|---|---|
| key/value | `--kv` |
| Regex | `-r '(?P<time>\S+ \S+) .*took (?P<latency>\d+ms)'` |
| Regex, one series per captured name | `-r '(?P<node>node\d): (?P<value>\S+ GB)' -g node` |
| Columns | `--columns 1,3 --names load1,load15` (`-d ,` for another delimiter) |
| CSV | `--csv` (header and time column auto-detected; `--time-field ts`) |
| JSON / NDJSON | `--json` (nested keys become `mem.used`; multi-line documents work) |
| Anything else | `--script parse.rhai --parse-fn` (see below) |
| Custom timestamp format | `-t '%d/%m/%Y %H:%M'` |
| x axis | `-X auto\|time\|index\|arrival` |

Special regex group names:
- `time`: the timestamp.
- `x`: a numeric x value.
- `key` or `series`: the series name.
- `value`: the value.
- `unit`: a unit to apply to the value.

Any other named group becomes its own series.

**Units** are understood everywhere: `59.9 GB`, `1.2G`, `512 kB`, `30ms`, `15%`.
Values are normalised to bytes or seconds and displayed in readable units.
`-B, --binary-units` treats `kB`/`MB`/`G` as 1024-based, which is what
`/proc/meminfo`, `df -h` and `free -h` use. Use `-u B` to give unitless values a unit.

To debug a format, `--explain` shows how each line was parsed and why any line
was skipped.

### Choosing and transforming series

| | |
|---|---|
| `-s, --select 'Mem*,Cached'` / `-e, --exclude lo` | choose series (globs allowed) |
| `--series 'used = MemTotal - MemAvailable'` | derived series (Rhai expression, units allowed) |
| `--rate` | counter → per-second rate |
| `--delta` | change between points |
| `--resample 1h:max` | bucket (mean, min, max, sum, last, count) |
| `--smooth 5` | moving average |

### Charts

| | |
|---|---|
| `-k line\|scatter\|bar\|histogram\|sparkline` | chart type (bar = latest values) |
| `--split` | one panel per series, each with its own y axis |
| `-m braille\|dot\|block\|half-block\|bar` | marker |
| `--log`, `--ymin 0`, `--ymax 500GB` | y axis |
| `--hline 50GB:red:low` | reference lines (repeatable) |
| `--colors cyan,#ff8800,203`, `--title`, `--legend top-left\|none` | styling |
| `--width`, `--height` | size for `--once` (default: fit the terminal) |
| `-o out.svg` / `.png` / `.csv` / `.json` | export (repeatable) |

### Analysis

`-S, --stats` prints, for each series:
- min, max, mean, σ and the latest value
- a least-squares trend, expressed per a natural unit of time (`--per 1h` to choose)
- R², showing how well a straight line fits the data
- a forecast of when the trend reaches 0, or `--target VALUE`

`--alert EXPR` evaluates a Rhai condition and exits with status 3 if it holds,
so graft works in cron jobs and scripts:

```sh
graft /var/log/reclaimable.log --no-chart \
  --alert 'node1 < 50GB' --alert 'slope("node2", "1d") < -10GB' || notify-send "disk"
```

## Interactive viewer

Opens by default when stdout is a terminal. Press `?` for help.

| Key | Action |
|---|---|
| `← →` / `h l` | pan; moves the cursor when it's on |
| `H L` | pan by half a screen |
| `+ -` | zoom in / out |
| `0` / `r` | reset |
| `c` | cursor (shows exact values) |
| `Home` / `End` | cursor to first / last point |
| `v` … `v` | select a range; the stats panel switches to it |
| `1`–`9` / `a` | toggle a series / show all |
| `t` / `m` / `y` / `x` | chart type / marker / log scale / split panels |
| `s` | stats panel |
| `space` | pause live updates |
| `e` | export the current view to SVG |
| `:` | Rhai console |
| `q` | quit |

### The Rhai console and scripts

Inside the console, every series is a variable holding its latest value.
Functions work on the visible range, or on the selection if you've made one:

```
:node2 - node1
:slope("node2", "1d")                       // change per day
:fmt("node2", slope("node2", "1d"))         // …formatted: "-7.69 GB"
:fmt_dur(eta("node1", 50GB))                // time until node1 hits 50 GB
:hline(mean("node2"), "mean"); zoom("12h")
```

Available functions:
- Statistics: `series()`, `first`, `last`, `min`, `max`, `mean`/`avg`, `median`,
  `stddev`, `p95`, `percentile(name, q)`, `sum`, `count`.
- Trends: `slope(name[, "1d"])`, `r2`, `change`, `pct_change`, `span`,
  `eta(name, target)`, `forecast(name, seconds_ahead)`.
- Data: `values`, `xs`.
- Formatting: `fmt(name, v)`, `fmt_dur`, `fmt_time`, `bytes`.
- Viewer actions: `hline(v[, label])`, `clear_hlines()`, `zoom("6h")`,
  `reset()`, `show(name)`, `hide(name)`, `chart("bar")`.
- Output: `print(...)`.

Unit literals such as `50GB`, `200ms` and `1d` work in all expressions.

`--script file.rhai` loads functions and key bindings
([examples/keys.rhai](examples/keys.rhai)):

```rhai
fn daily(name) { fmt(name, slope(name, "1d")) + "/day" }
bind("D", "daily(\"node2\")");
bind("W", "hline(50GB, \"warn\")");
```

It can also define `parse(line)` for formats that need real code
([examples/parse.rhai](examples/parse.rhai)). The function returns a map of
values plus an optional `time`, or `()` to skip the line.

## Profiles

```sh
graft /var/log/reclaimable.log --stats --split --save-profile reclaim
graft -p reclaim                 # later; extra flags override the profile
graft --list-profiles            # saved + built-in: meminfo, loadavg, net, cpu-temp
```

Profiles are stored in `~/.config/graft/profiles.toml`, or wherever
`$GRAFT_PROFILES` points.

## Shell integration

```sh
graft --completions bash > ~/.local/share/bash-completion/completions/graft
graft --man > ~/.local/share/man/man1/graft.1
```
