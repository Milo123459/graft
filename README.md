# graft

Graph anything in your terminal: log files, command output, CSV or JSON. Charts
resize to fit the terminal and everything can be set with command-line flags.
You can also build a chart interactively: graft suggests formats for your data
and shows a live preview as you edit them. The viewer works with a mouse or a
touchscreen as well as the keyboard, which helps over SSH from a phone. You can
zoom, scrub through values, select ranges, forecast towards your own targets,
and run analysis with [Rhai](https://rhai.rs) scripts.

graft doesn't guess. You always say how to parse your input, either with a
format flag or by picking one of graft's suggestions in the builder. Everything
else can be set explicitly too: timestamps (`-t`), units (`-B`, `-u`), the trend
model (`--fit`), the data it's fitted to (`--fit-window`, `--last`), forecast
targets (`--target`) and the output format (`--stats-template`,
`--stats-format json`).

```
$ graft reclaimable.log -F '{time} node1: {node1} node2: {node2}' --once --stats --split --forecast --target node1=50GB,node2=200GB
┌ reclaimable.log · node1 [forecast] ────────────────────────────────────────┐
│60 GB│⠒⠒⠒⢄⡀                                             ┌──────────────────┐│
│     │    ⠈⠉⠉⠲⣀⣀⣀⡠⠤⠤⠤⠋⣀⡀                                │node1 target 50 GB││
│     │                 ⠈⠉   ⠒⠤⠤⠄                        │node1 58.8 GB     ││
│     │                            ⠉⠉⠉⠒  ⠠⠤⠤⢄            └──────────────────┘│
│     │                                         ⠉⠉⠒⠂  ⠤⠤⢄⣀                   │
│50 GB│                                                     ⠈⠑⠒⠒⠂  ⠤⣀⣀       │
│     │⠉⠉⠁ ⠉⠉⠉ ⠈⠉⠉ ⠈⠉⠉  ⠉⠉⠁ ⠉⠉⠁ ⠈⠉⠉ ⠈⠉⠉ ⠈⠉⠉⠁ ⠉⠉⠁ ⠉⠉⠁ ⠈⠉⠉ ⠈⠉⠉  ⠉⠉⠁ ⠉⠉⠁ ⠁⠉⠉⠐⠒⠒⠢│
│     └──────────────────────────────────────────────────────────────────────│
│               11-02        11-03        11-04        11-05        11-06    │
└────────────────────────────────────────────────────────────────────────────┘
┌ reclaimable.log · node2 [forecast] ────────────────────────────────────────┐
│      │⠒⠤⢄⣀⣀                                                ┌──────────────┐│
│420 GB│     ⠉⠒⠒⠤⢄⣀⣀⠤⠤⠔⠂                                     │node2 415.7 GB││
│      │                ⠉⠉⠉  ⠐⠢⠤⠤                            └──────────────┘│
│400 GB│                           ⠈⠉⠉⠉⠂  ⠤⠤⠤⡀                               │
│      │                                        ⠈⠉⠑⠒  ⠠⠤⢄⣀                   │
│      │                                                     ⠉⠒⠒⠂  ⠤⣀⣀       │
│380 GB│                                                              ⠁  ⠐⠒⠒⠢│
│      └─────────────────────────────────────────────────────────────────────│
│                11-02        11-03        11-04       11-05        11-06    │
└────────────────────────────────────────────────────────────────────────────┘
series   n      last       min      mean  trend/day    R²  forecast
node1   29   58.8 GB   57.5 GB   58.5 GB   -1.97 GB  0.61  50 GB in 3.7 days
node2   29  415.7 GB  412.2 GB  417.1 GB   -7.69 GB  0.73  200 GB in 27.6 days

• node1: falling by an average of 1.97 GB/day (R² 0.61); reaches 50 GB in about 3.7 days (around 2026-11-06 01:32)
• node2: falling by an average of 7.69 GB/day (R² 0.73); reaches 200 GB in about 27.6 days (around 2026-11-29 23:09)
```

## Install

```sh
cargo install --path .                 # terminal charts + SVG/CSV/JSON export
cargo install --path . --features png  # also PNG export (pulls in resvg)
```

## Quick tour

```sh
graft                                  # open the builder: pick a source, get format suggestions
graft app.log                          # no format given: opens the builder with suggestions
graft app.log -F '{time} {_} took {ms}' # graph it with an explicit format template
graft app.log -F '…' --once            # print the chart and exit (also used when piped)
graft -c 'cat /proc/meminfo' -n 1s -B -F '{key}: {value}'   # run a command every second
some-command | graft --columns all     # graph stdin
graft app.log --kv -f                  # follow a growing file like tail -f
graft -p cpu                           # a preset: cpu, memory, load, network, disk, temperature, ping, processes
```

### Getting data in

| Input | Flags |
|---|---|
| File / stdin | `graft FILE`, `graft -`, or pipe into `graft` |
| Follow a file | `-f, --follow` (handles truncation/rotation) |
| Run a command repeatedly | `-c, --cmd 'COMMAND'`, `-n, --every 2s`, `--count N` |
| Keep only recent data | `-w, --window 6h` or `--window 500` (points); older data is dropped from memory |
| Timeframe | `-L, --last 7d` (or `--last 500` points), `--since 2026-10-15`, `--until "2026-10-20 12:00"`: what's shown **and** analysed. The viewer can change it |

### Parsing

**Format templates** (`-F, --format`) are the easiest way to describe a line:

```sh
graft reclaimable.log -F '{time} node1: {node1} node2: {node2}'
graft app.log         -F '[{time:%d/%m/%Y %H:%M}] {_} took {latency}'
graft -c 'df -B1'     -F '{key} {_:word} {used} {avail} {_}'      # one series per filesystem
```

| Placeholder | Matches |
|---|---|
| `{name}` | a number with an optional unit (`59.9 GB`, `12ms`, `40%`), stored as series `name` |
| `{time}` / `{time:%d/%m %H:%M}` | a timestamp, auto-detected or in the strftime format you give |
| `{x}` | a numeric x value |
| `{key}` | the series name, giving one series per distinct value |
| `{_}` / `{_:word}` | any text to skip / one word to skip |
| `{name:int}`, `{name:word}`, `{name:/regex/}` | change what's matched |
| `{{` `}}` | literal braces |

Spaces in a template match any run of whitespace. Series names can be anything,
for example `{Active(anon)}`.

**A format is always required.** With no format flag:
- in a terminal, graft opens the builder with your source loaded and suggests
  formats, and you pick one;
- with `--once`, exports or piped output, it stops and lists the formats that
  fit as ready-to-run commands:

```
$ graft reclaimable.log --once
graft: no format given for reclaimable.log. graft doesn't guess: choose how to parse it. Formats that fit:
  graft --format '{time} node1: {node1} node2: {node2}' --once reclaimable.log
      (29 of 29 sample lines)
  graft --kv --once reclaimable.log
      (29 of 29 sample lines)
  …
```

`--auto` opts in to guessing: `key: value` pairs if there are any, else numeric
columns. Timestamps written at the start of a line in a common shape are
recognised: ISO-8601 and similar, syslog `Nov  2 08:00:01`, a bare `HH:MM:SS`,
and Unix epochs. You can always spell the format out with `{time:%…}` or `-t`.

| Format | Flags |
|---|---|
| Template | `-F '{time} load: {load}'` |
| key/value | `--kv` |
| Guess (opt-in) | `--auto` |
| Regex | `-r '(?P<time>\S+ \S+) .*took (?P<latency>\d+ms)'` |
| Regex, one series per captured name | `-r '(?P<node>node\d): (?P<value>\S+ GB)' -g node` |
| Columns | `--columns 1,3 --names load1,load15`, or `--columns all` (`-d ,` for another delimiter) |
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

To debug a format, `--explain` (or `--explain=N` for the first N lines) shows how each line was parsed and why any line
was skipped.

### Choosing and transforming series

| | |
|---|---|
| `-s, --select 'Mem*,Cached'` / `-e, --exclude lo` | choose series (globs allowed) |
| `--series 'used = MemTotal - MemAvailable'` | derived series (Rhai expression, unit literals allowed); `--series 'busy:% = …'` sets its unit. Computed after `--rate`/`--delta`, so ratios of rates work |
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

### Analysis and forecasting

`-S, --stats` prints, for each series:
- min, max, mean, σ and the latest value
- the trend per unit of time, from a fitted model
- R², showing how well the model fits the data
- a forecast of when the trend reaches a target

```
$ graft reclaimable.log -F '{key}: {value}' --no-chart --stats --target node1=50GB,node2=200GB
• node1: falling by an average of 1.97 GB/day (R² 0.61); reaches 50 GB in about 3.7 days (around 2026-11-06 01:32)
• node2: falling by an average of 7.69 GB/day (R² 0.73); reaches 200 GB in about 27.6 days (around 2026-11-29 23:09)
```

Every part of this is configurable:

| | |
|---|---|
| `--target node1=50GB,node2=200GB` | targets per series; `--target 100GB` sets one for all, `none` turns them off. Without targets, a falling series is forecast to reach 0 |
| `--fit linear\|exp` | constant change per unit of time, or constant percentage change (growth/decay) |
| `--fit-window 12h` / `--fit-window 50` | fit only recent data (a duration or a number of points) |
| `--per 1h` | the unit trends are expressed in (default: chosen from the data's span) |
| `--forecast` / `--forecast=7d` | draw the projected trend (dashed) past the data |
| `--trend=avg,median,p95` | extra lines over the data, any mix: `avg` (moving average, weighted to the centre so it's smooth), `median` (the typical value: ignores spikes), `p95` (95% of values are below it), `line` (the straight fit forecasts use). `--trend` alone is `line`. Each is drawn in its series' colour, with the live data in a darker shade, and the legend shows each line's own value |
| `--trend-window N` | points in the rolling window (default: a tenth of the data, at least 10) |
| `--trend-only` | only the trend lines, not the live data |
| `--side-by-side` | two charts: the live data, and its trends (average + median unless `--trend` says otherwise) |
| `--right-panel live,p95` | choose the right-hand chart's lines (implies `--side-by-side`); the left one shows what `--trend` / `--trend-only` pick |
| `--no-targets` | don't draw target lines (they still drive forecasts and stats) |
| `--last 7d` / `--since …` | fit and forecast from that timeframe only, e.g. just the last week |
| `--stats-template '{name}: {trend} → {target} in {eta}'` | your own output line per series |
| `--stats-format json` | machine-readable output |

Template fields: `name n first last min max mean median stddev p95 change trend
per r2 target eta eta_at direction model unit`. Add `:raw` (e.g. `{last:raw}`)
for unformatted numbers.

`--alert EXPR` evaluates a Rhai condition and exits with status 3 if it holds,
so graft works in cron jobs and scripts:

```sh
graft /var/log/reclaimable.log -F '{key}: {value}' --no-chart \
  --alert 'node1 < 50GB' --alert 'slope("node2", "1d") < -10GB' || notify-send "disk"
```

## Quick start

Run `graft` on its own and pick from ready-made charts. Only the ones that work
on your machine are listed. CPU, memory, load, network and ping have both Linux
(`/proc`) and macOS (`top`, `memory_pressure`, `sysctl`, `netstat`) versions:

| Preset | Shows |
|---|---|
| CPU usage (`-p cpu`) | busy % and I/O wait %, every second |
| Memory (`-p memory`) | used, available and cached memory (macOS: free %) |
| Load average (`-p load`) | 1, 5 and 15 minute load |
| Network traffic (`-p network`) | received / sent bytes per second, per interface |
| Disk usage (`-p disk`) | % used per mounted filesystem |
| CPU temperature (`-p temperature`) | °C from the thermal sensor (e.g. a Raspberry Pi) |
| Ping latency (`-p ping`) | round-trip time to 1.1.1.1 |
| Processes (`-p processes`) | number of running processes |

Tap one to start it, or tap **edit** to open it in the builder and adjust it.
Every preset spells out its format, so it also works as an example to copy.
Your saved profiles appear in the same list, followed by **chart a file** and
**chart a command**.

## Building a chart interactively

Run `graft` with no arguments, `graft FILE` without a format, or add `--build`, to
open the builder:

1. **Source:** a file, a command (run every `N` seconds), or piped stdin. Tap
   *load* to read a sample.
2. **Suggestions:** graft tries a generated template, key/value, CSV, JSON and
   columns on the sample. They're ranked by how many lines parse. Nothing is
   chosen for you: tap one (or use ↑/↓) to use it. Until then you see the raw
   sample, and *start* stays disabled.
3. **Format:** edit the template or regex, group-by or time format. A per-line
   preview shows what each line parses to, or why it didn't.
4. **Series, chart and forecast:** tap series to include or exclude them, toggle
   chart options, and type targets such as `node1=50GB,node2=200GB`.
5. **Start**, or **save profile**. The equivalent command line is shown as you
   go and printed when you quit, so it's easy to reuse.

Press `F` in the viewer to come back here and change the format. For commands
and stdin, the history already received is re-parsed with the new format.

## Interactive viewer

Opens by default when stdout is a terminal. Mouse and touch work everywhere,
including over SSH from a phone.

| Mouse / touch | Action |
|---|---|
| tap | put the cursor there; a box beside it shows every series' value |
| drag the cursor line | scrub through the data, and past it into the forecast |
| **measure**, then tap start and end (or drag across, or click then **shift+click**) | stats for just that stretch, including how much each series *changed*; tap again to clear |
| scroll | zoom around the pointer (sideways or shift + scroll moves); never past your data |
| toolbar / series chips | ◀ ▶ + − reset cursor measure stats **lines** (menu) **side by side** forecast split **all time ▾** (menu); `⋯` shows more: targets, chart type, log, follow, pause, format, console, export, mouse, help, quit |

**Timeframe** (anything on a time axis — log timestamps, or arrival times for
live commands like `ping`): tap **all time ▾** (or press `R`) for a dropdown
of ranges that fit your data (`last 1d`, `last 6h`, … `last 5m`, `last 1m`) and
**custom…**, which opens the console with `range("` ready to type
(`range("2h")`, `range("2026-11-01", "2026-11-02 12:00")`). In the dropdown ↑ ↓
move; outside it `[` / `]` go narrower or wider. Stats, trends and forecasts
all use the chosen timeframe.

**Lines:** tap **lines: live** (or press `T`) for a menu where you tick any mix
of **live**, **average**, **median** and **p95**, and make the rolling window
bigger or smaller (`+` / `−`, shown in points and time). Each line's value is in
the legend and the cursor readout. Ping-style data (mostly low with spikes)
shows why you might want several: the average is pulled up by the spikes, the
median shows the typical value, p95 the bad-but-common case. Forecast (`f`) and
target lines (`G`) are separate toggles. In side by side the menu has a **left** and a **right**
column, so each chart gets its own lines (keys `l a m p` for the left,
`L A M P` for the right); in the console, `lines("live", "avg,median")`.

The **follow** button (key `o`) makes the cursor follow the pointer without
clicking, in terminals that report mouse movement. **mouse** (key `M`) turns
mouse capture off, so your terminal's own text selection works.

| Key | Action |
|---|---|
| `← →` / `h l` | move left / right (when zoomed in) |
| `↑ ↓` / `+ -` | zoom in / out |
| `,` `.` / shift `← →` | step the cursor point by point |
| `H L` | move by half a screen |
| `0` / `r` | reset |
| `c` / `o` | cursor / cursor follows the pointer |
| `Home` / `End` | cursor to first / last point |
| `v` … `v` | measure a range from the keyboard |
| `1`–`9` / `a` | toggle a series / show all |
| `t` / `m` / `y` / `x` | chart type / marker / log scale / split panels |
| `T` | lines menu (inside: `l` `a` `m` `p` toggle, `+` `-` window, Esc closes) |
| `D` | side by side: live data next to its trends |
| `f` / `G` | forecast / target lines on or off |
| `R` / `[` `]` | timeframe dropdown / narrower, wider timeframe |
| `F` | change the format or source (builder) |
| `s` | stats panel |
| `space` | pause the chart: the source keeps running and its data is kept, so resuming jumps to the latest |
| `e` | export the current view to SVG |
| `M` | mouse capture on/off |
| `:` | Rhai console |
| `q` | quit |

### The Rhai console and scripts

Press `:` and type `help` to see every command with a short explanation and
examples. Inside the console, every series is a variable holding its latest value.
Functions work on the visible range, or on the selection if you've made one:

```
:node2 - node1
:slope("node2", "1d")                       // change per day
:fmt("node2", slope("node2", "1d"))         // …formatted: "-7.69 GB"
:fmt_dur(eta("node1", 50GB))                // time until node1 hits 50 GB
:hline(mean("node2"), "mean"); zoom("12h")
:target("node2", 200GB); forecast("14d")    // set a target and show the projection
```

Available functions:
- Statistics: `series()`, `first`, `last`, `min`, `max`, `mean`/`avg`, `median`,
  `stddev`, `p95`, `percentile(name, q)`, `sum`, `count`.
- Trends: `slope(name[, "1d"])`, `r2`, `change`, `pct_change`, `span`,
  `eta(name, target)`, `forecast(name, seconds_ahead)`.
- Data: `values`, `xs`.
- Formatting: `fmt(name, v)`, `fmt_dur`, `fmt_time`, `bytes`.
- Viewer actions — every toolbar button has one, so key bindings and the
  console can do anything a tap can:
  - `timeframe(1h)`, `timeframe("7d")`, `timeframe("500")` (points),
    `timeframe("all")`, `timeframe("2026-11-01", "2026-11-02 12:00")`
    (`""` as the end = until now); `range(…)` is the same
  - `lines("live,avg,median,p95")` (any mix; `"line"` is the straight fit);
    `lines("live", "avg,p95")` sets both charts of side by side,
    `window(50)` / `window("auto")` for the rolling window,
    `trend("avg,p95" | "off")` sets just the trend layers
  - on/off buttons: `live`, `side_by_side`, `stats`, `split`, `log`, `targets`,
    `pause`, `follow`, `cursor`, `measure`, `mouse` — call with no argument
    to toggle (`stats()`), or with a bool to set (`split(true)`)
  - `forecast("7d" | "auto" | "off")`, `target(name, v)`, `clear_target(name)`,
    `chart("bar")`, `show(name)`, `hide(name)`, `hline(v[, label])`,
    `clear_hlines()`
  - `zoom(6h)`, `zoom_in()`, `zoom_out()`, `pan("left" | "right" | -0.5)`,
    `reset()`
  - `svg()` / `svg("out.svg")`, `format()` (open the builder), `quit()`
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
graft /var/log/reclaimable.log -F '{key}: {value}' --stats --split --save-profile reclaim
graft -p reclaim                 # later; extra flags override the profile
graft --list-profiles            # saved profiles + built-in presets
```

Profiles are stored in `~/.config/graft/profiles.toml`, or wherever
`$GRAFT_PROFILES` points.

## Development

```sh
cargo test                                                    # unit, end-to-end and rendering tests
GRAFT_SNAPSHOTS=snaps cargo test snapshots -- --include-ignored  # write every screen as colour ANSI
```

The snapshot scenarios (`src/snapshots.rs`) cover the `--once` output, the
viewer (cursor, split, forecast, timeframe, help, many series), the builder and
the gallery, at desktop and phone sizes. `cat snaps/*.ans` shows them in your
terminal.

## Shell integration

```sh
graft --completions bash > ~/.local/share/bash-completion/completions/graft
graft --man > ~/.local/share/man/man1/graft.1
```
