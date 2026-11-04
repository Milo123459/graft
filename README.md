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

New to graft? `graft --tutorial` walks you through every feature on a live demo chart
(a made-up service log): reading values, zooming, measuring, trend lines, timeframes,
stats, forecasts, chart types, panes, the console, markers, alerts, custom Rhai lines,
undo and saving. Each step explains how the feature works, lists its keys, gives
examples (flags, console commands, real uses), then asks you to try it and ticks
itself off; it finishes with recipes for real use, each with a copy button. Enter (or
next ▶) moves on, Backspace goes back, PgUp/PgDn or the mouse wheel scroll a long
step, *hide* folds the panel to one line. It's also first in the quick
start list when you run `graft` on its own.

```sh
graft --tutorial                       # the guided tour
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
| A stream a server pushes | `--stream URL` (or `--ws`, `--sse`): a WebSocket (`wss://…`; `--send MSG` goes out once connected, e.g. a subscription) or a streaming HTTP response (`https://…`: server-sent events, each event's `data:` is a record, or plain lines; with `--send` it's a POST). Reconnects when dropped. A URL whose reply ends (a REST snapshot) is asked again every `--every`: `--stream https://api.hyperliquid.xyz/info --send '{"type":"l2Book","coin":"BTC"}' --every 1s` polls Hyperliquid's order book each second. With `--once`, `--count N` records |
| Record a source | `--record ~/graft/btc.jsonl`: saves every line it reads (appended), and the next run reads the file back first in the source's own format, so a stream with no history to download builds its own, run after run. In a `--merge`'s arguments it records that source. In the viewer `record("…")`, `record(false)`, or **record…** in **⋯**; **● rec** in the legend shows it's on (tap to stop) |
| Older data for a stream | `--history 'ARGS'`: another source with its own format (like `--source`), whose series go in before the source's own under the same names; with `{from_ms:…}` / `{now_ms}` in its command it pages back to `--history-depth 6h` (default 1h), four pages at once, saying how far it's got, and scrolling to the oldest data fetches more. In the viewer: `history_depth("24h")` (or **history depth** in **⋯**, which cycles) goes further back at once, `history("…")` / **history…** changes the source, `history(false)` drops it. Give `--history` again for more sources (`history(["…", "…"])`), each filling in series the others don't have. History is resampled like the chart |
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

### Charting another machine (SSH)

`--ssh HOST` runs the command — or reads the file — on another machine and
charts it here. HOST is anything `ssh` accepts (`pi@raspberrypi`, or a name from
`~/.ssh/config`).

```sh
graft --ssh pi -p cpu                                   # presets pick the remote OS's variant
graft --ssh pi ~/reclaimable.log -F '{time} node1: {node1} node2: {node2}'
graft --ssh pi /var/log/syslog --grep 'errors=(?i)error' -f   # follow a remote log
graft --ssh nas -c 'df -kP /volume1' -F '{_} {_} {used:int} {free:int}' -n 30s
```

One connection is opened and reused (OpenSSH connection sharing, kept for two
minutes), so sampling every second doesn't log in every second. graft can't
type passwords for you: use key login (`ssh-copy-id HOST`) or `ssh-add`; if it
can't log in, it says how to fix it. Remote charts show up in your recent list
as `HOST: …`.

### Custom lines (Rhai)

Draw your own line over the data, worked out in Rhai over the whole series —
for example an EMA30 over a live ETH price:

```sh
graft -c 'curl -s https://api.coinbase.com/v2/prices/ETH-USD/spot' -n 1s \
  --regex '"amount":"(?P<ETH>[0-9.]+)"' -u USD \
  --line 'ema30 = ema(values("ETH"), 30)'
```

Using series by name works point by point, like `--series`:
`--line 'total = user + sys'` (or `:add_line("total", "user + sys")` in the
viewer) adds the two at every moment; `--line 'above = api - mean("api")'` mixes in
whole-series functions.

`--line 'NAME[:UNIT] = EXPR'` (repeatable). Otherwise the expression returns an array
(one value per point of the series it names; a shorter one lines up with the
newest points), `[x, y]` pairs, or a number for a flat line. Built in:
`ema(a, n)`, `sma(a, n)`, `rsi(a, n)`, `macd(a)`, `bollinger(a, n, k)` (k < 0 the
lower band), `vwap(price, volume)`, `atr(high, low, close, n)`, `stoch(high, low, close, n)`,
`highest(a, n)`, `lowest(a, n)`, `values(name)`, `xs(name)`, `values_at(name, xs)`
(another series at those times), and every console function (`mean`, `p95`,
`slope`…). Functions from `--script` work too: `examples/indicators.rhai` has an
EMA, Bollinger bands and a spread written in Rhai:

```sh
graft … --script examples/indicators.rhai \
  --line 'upper = band(values("ETH"), 20, 2.0)' --line 'lower = band(values("ETH"), 20, -2.0)'
```

Or load a script while the chart is running and use it straight away:

```
:load("examples/indicators.rhai")
:add_line("upper", "band(values(\"eth\"), 20, 2.0)"); add_line("lower", "band(values(\"eth\"), 20, -2.0)")
```

A custom line is a series like any other (legend, readout, stats, markers,
alerts). In the viewer, the lines menu (`T`) lists them: tap one to edit it,
**✕** to delete, **+ custom line** to add; or `add_line("name", "expr")` and
`remove_line("name")` in the console. Money: `-u USD` (or `$`, `EUR`, `GBP`)
shows `$3,456.78`; `:unit("USD")` and `:title("ETH")` change it while running.

### A trading dashboard (candles, EMAs, RSI, MACD)

Hyperliquid's candles, polled every 5 s: **coin ▾** and **interval ▾** switch the asset and candle size
(`--param`), and the window is 500 candles of it (`{from_ms:500*{interval}}` …
`{now_ms}`). Press ← (or scroll back to the oldest candle) and graft asks for the 500 before it
(see below). The API returns a JSON array
(one record per candle, `--json --time-field t`) and the same candles each time,
so `--replace` updates the newest one instead of adding copies. The top pane is
`kind=candles`: candlesticks from the open/high/low/close series (green closed up, red
down; candles that share a column are merged), with the EMAs drawn over them. A
candles pane brings those four series along by itself (so the pane lists only the
EMAs), and they're one **candles** chip in the legend and series lists. RSI and MACD are
built in (`rsi(values("close"), 14)`, `macd(values("close"))`); `hist = macd - signal`
would add the histogram (series by name are worked out at each point).

```sh
graft -c 'curl -s --json "{\"type\":\"candleSnapshot\",\"req\":{\"coin\":\"{coin}\",\"interval\":\"{interval}\",\"startTime\":{from_ms:500*{interval}},\"endTime\":{now_ms}}}" https://api.hyperliquid.xyz/info' \
  --param 'coin = ETH | BTC | SOL | HYPE' --param 'interval = 1m | 5m | 15m | 1h | 4h | 1d' \
  -n 5s --json --time-field t --replace \
  --series 'open:USD = o' --series 'high:USD = h' --series 'low:USD = l' --series 'close:USD = c' \
  --series 'volume:{coin} = v' --select open,high,low,close,volume \
  --line 'ema30 = ema(values("close"), 30)' --line 'ema50 = ema(values("close"), 50)' \
  --line 'rsi14:% = rsi(values("close"), 14)' --line 'overbought:% = 70' --line 'oversold:% = 30' \
  --line 'macd = macd(values("close"))' --line 'signal = ema(values("macd"), 9)' \
  --mark 'cross_above(ema30, ema50) => golden cross | ▲ cyan badge' \
  --mark 'cross_below(ema30, ema50) => death cross | ▼ magenta badge' \
  --pane 'ema30,ema50 kind=candles title={coin} {interval}' --pane 'rsi14,overbought,oversold front=rsi14' --pane 'macd,signal front=macd' --pane volume \
  --layout '1 /60% ((2 | 3) / 4)' --title '{coin} {interval}'
```

### Older data as you scroll back

A command can say which time window it wants with placeholders, filled in each time
it runs: `{now}` / `{now_ms}` (the end: now) and `{from:6h}` / `{from_ms:6h}` (that
long before; `{from_ms:500*1m}` is 500 one-minute steps). Press ← while everything is in
view, or zoom in and scroll back to the oldest data, and graft runs it once more for the window before that, adding what comes
back (`--replace` takes care of the overlap), until the source has nothing older.

### Source settings (`--param`)

A source can have settings with a few choices, filled in wherever its command, file
name, titles, series and units (`--series 'volume:{coin} = v'`), lines, markers or
alerts say `{name}` (saved charts keep the `{name}`):

```sh
graft -c 'curl -s … "interval":"{interval}" …' --param 'interval = 1m | 5m | 15m | 1h | 4h | 1d' …
```

The viewer shows a dropdown for each (`interval: 1m ▾`); picking another value runs
the source again with fresh data, staying on the stretch of time you were looking at (`param("interval", "1h")` in the console does the
same, so it can go on a key). `*` marks the current choice: `1m | *1h`. Merged and
pane sources can have their own (`--merge 'b = -c "…{coin}…" --param "coin = ETH | BTC"'`).

### Combining sources (`--merge`)

Several sources can share one chart: each `--merge` adds another source's
series, named after it — a preset or saved profile (`--merge load`), or inline,
with that source's own arguments after `NAME =`. With only `--merge`s, the
first becomes the main source. In the viewer, the **sources** button (`S`)
lists them: **edit** opens one in the builder (source, format, live preview) and
puts it back with **merge into the chart**; **+ add source** makes a new one;
**✕** removes one. In a dashboard it also lists each pane's own source
(`--pane source=memory`), with **other source…** to show different data there and **edit** to change
what it runs in the builder. An edited pane source is kept with the chart as a
named source, `--source 'memory = -c "…" --kv'`, which a pane shows with
`source=memory` and which takes the place of a preset or profile of that name
for this chart only. Custom lines can then combine them, e.g. ETH
from two exchanges, their average, and an EMA30 of it:

```sh
graft \
  --merge 'coinbase = -c "curl -s https://api.coinbase.com/v2/prices/ETH-USD/spot" --regex "amount\W+(?P<p>[0-9.]+)" -n 1s -u USD' \
  --merge "hyperliquid = -c 'curl -s --json {\\\"type\\\":\\\"allMids\\\"} https://api.hyperliquid.xyz/info' --regex '\\bETH\\W+(?P<p>[0-9.]+)' -n 1s -u USD" \
  --line 'eth = mean_of(["coinbase", "hyperliquid"])' \
  --line 'ema30 = ema(values("eth"), 30)' \
  --mark 'name == "eth" && prev < ema30_prev && value >= ema30 => crossed above EMA30' \
  --mark 'name == "eth" && prev > ema30_prev && value <= ema30 => crossed below EMA30' \
  --title ETH --last 15m
```

With several live sources the chart redraws in rounds — once every one of them
has sent something new — so a combined line moves once per round instead of
each time one source happens to answer first (a stalled source can't freeze it:
after about two of its intervals it redraws anyway). `--no-sync` redraws on
every update instead.

Sources sample at slightly different moments, so `mean_of([...])` and
`values_at(name, xs)` line other series up on the first one's times. In the
viewer: `:merge("binance = -c '…' --regex '…'")` adds one, `:unmerge("binance")`
removes it.

In marker rules every series is also a variable — its value at that point
(`ema30`) and at the point before (`ema30_prev`) — which is how the crossings
above work.

### Showing and hiding lines, and your own keys

Tap a series chip under the chart (● coinbase ● hyperliquid ● eth…) to hide or
show that line; keys `1`–`9` do the same, `a` shows them all. To start with
some hidden — e.g. only the combined price, not the raw feeds behind it:
`--hide coinbase,hyperliquid`.

Bind keys of your own to any console code:

```sh
--bind 'r = only("eth", "ema30")'      # just the combined price and its EMA
--bind 'R = show_all()'
--bind 'c = toggle("coinbase")'
```

or while it runs: `:bind("r", only("eth"))` — the second argument is code, not a
string (a `{ … }` block runs several things: `:bind("x", { hide("coinbase");
title("ETH only") })`). It works straight away. Bound
keys win over built-in ones, are listed in `?`, and are kept when you save or
copy the view.

### Markers

Put a ▼ on the chart wherever something happens — a condition checked at every
point of every series:

```sh
# reclaimable space dropped by more than 1 GB between two samples
graft ~/reclaimable.log -F '{time} node1: {node1} node2: {node2}' \
  --mark 'delta < -1GB => dropped {delta}'
# only node2, and big percentage jumps
--mark 'name == "node2" && pct < -0.5 => {name} {pct}'
--mark 'value > 90% => hot'                     # e.g. on the cpu preset
```

Variables: `value`, `prev` (the point before), `delta` (value − prev), `pct`
(that as a % of prev), `rate` (delta per second), `time`, `i`, `name`. Unit
literals work. The label after `=>` can use `{name} {value} {prev} {delta}
{pct} {time}` (default `{name} {delta}`). Markers show in the cursor readout,
and `--stats` lists them. In a `--script` file or the console:
`mark_when("delta < -1GB", "dropped {delta}")`; `mark("deploy")` drops one by
hand at the cursor (or the newest data); `clear_marks()` removes them all.
Crossings have a shorthand: `cross_above(a, b)` holds where `a` went from below `b`
to at or above it (`b` a series or a number, e.g. `cross_above(rsi14, 70)`),
`cross_below(a, b)` the other way and `cross(a, b)` either way. A rule like that is
checked once per point of the first series it names (which the marker goes on), not
once for every series. Give a rule its own look after `|` so different events read differently: a symbol
(upward ones sit below the point, the rest above), a colour (the terminal's names,
plus orange, purple, pink, gold, teal, brown, lime, sky, violet; `#ff8800`; `208`),
effects (`bold`, `italic`, `underline`, `dim`, `reverse`) and a background
(`on:yellow`). `badge` puts the label on the marker's colour, like a tag — easy to
tell apart from candles or lines in the same colours:

```sh
--mark 'cross_above(eth, ema30) => above EMA30 | ▲ green'
--mark 'cross_below(eth, ema30) => below EMA30 | ▼ red'
--mark 'notify value > 4000 => over 4k | ★ yellow'     # notify: tell me about new ones
--mark 'value > 4200 => ATH | ★ orange badge'            # a tag in orange
```

**Backtests.** Once a rule has placed markers, ask what followed them:
`:backtest("golden cross", 10)` says what the series did 10 points later (or
`backtest(1, "close", "1h")`: an hour later) — on average, the median, how often it
went up, the best and worst. `:strategy("golden cross", "death cross")` treats one
rule's markers as entries and another's as exits: the trades' total, wins, average,
best and worst, the largest drawdown and how much of the time it was in a trade. A
rule is named by its number in the markers menu or its label; the series is the one
given, else a `close` series, else the one the markers are on. Nothing here is
trading-specific: `backtest("deploy", "latency", "1h")` works the same way.

**Many strategies, many settings.** Label rule pairs `NAME in` and `NAME out` (or
`buy`/`sell`, `entry`/`exit`) and `:strategies()` trades them all: **long** (in at the
in markers, out at the out markers), **short** (the other way round), and the best
side, stop-loss and take-profit (SL none/0.5–5%, TP none/1–8%; stops and take-profits
are checked against each candle's high and low, the stop first if a candle touches
both). That best way is picked on the first half of the data and then checked
**walking forward** over the second: in stretches (as many as leave about 5 signals
each, up to 5), each traded the way that was best on everything before it — as you'd
have traded it then. If it only did well where it was picked, it fitted the past. It
also says how many stretches were traded the way it picked, and the way that's best on
all of it (how to trade it from now on) when that's another. Each result is next to what holding did (overall and in each part);
`:strategies(0.045)` takes a 0.045% fee off every buy and sell. Put settings in the
lines with `--param` (`--param 'fast = 9 | 12 | 20'` and `ema(values("close"),
{fast})`) and `:sweep()` does all of that for every combination of them — without
fetching anything (settings the source's command uses are left as they are) — each
strategy's setting picked on the first half, ranked by walking forward, with the median of every way it tried. These tables stay above the
legend until you close them (Esc, a tap, or the next command).

While it runs, press `K` (or tap **markers**) for the list of rules and
markers: tap a rule to edit it (it opens in the console; Enter saves, Esc
cancels), tap the buttons under it to cycle its symbol, colour or effect (plain, badge, bold,
underline, italic, reverse, dim — for several at once, edit the rule), **notify** to be
told about new ones, **✕** deletes one, **+ add rule** and **mark now** add more.
`B` (or **marks below** in **⋯**, `marks_below()`, `--marks-below`) moves every
marker off the lines into a strip below the chart: a row per rule, named by its label
without the `{…}` parts (or its condition), each marker at its moment and lined up
with the chart above; discovery's trades get a row of their own, markers placed by
hand one called *marked*. Past six rows the rest share the last.
`N` (or **alerts**) lists alerts, which markers notify, and where notifications
go (bell, desktop, ntfy on your phone, a webhook or a command), with a test
button.

### Discovering strategies (`discover()`)

`:discover(0.045)` (or `--discover 0.045`, or `E`, or the **discover** button) hunts
for edges in a price as data streams in, without being told which indicators to use.
It searches nonstop in the background (3 s each time, on every core but one) — on as many of the newest
points as leave time to try about 2,000 formulas, measured on each search (on a fast
machine, everything loaded; paper trading always uses everything) — on what's on the
chart, after the timeframe (so `last 1h` searches that hour): the
price (`close`, `price` or `mid`, else the first series, or `discover(fees, "name")`),
bid and ask if there are series named …bid… and …ask…, and every other series as a
possible signal.

**Costs** are either a flat fee, `discover(0.045)` (0.045% per buy and per sell), or
**profit sharing** as on Veranta's Upside Perps, `discover("upside:250")` at 250x: a
losing trade pays nothing (you lose the move, at most your margin); a winning one gives
up part of its profit, less the more it made — you keep 75% up to 250% ROI, 80% up to
500%, 90% up to 1,500%, 95% beyond, where ROI is the move × the leverage (+2% at 250x
is 500% ROI: keep 80%). The take-profit can't go past 2,500% ROI, and a trade is
liquidated at −85% ROI (a 34bp move against you at 250x, 17bp at 500x).
`upside:250:staked` takes a quarter less (staking AVNT). The spread is paid either way:
in at the ask, out at the bid (where there's no bid and ask, e.g. in history from
elsewhere, the average spread around the price).

**0. The layout comes from the data.** Nothing about how the data is used is fixed;
each search works it out from what it has:

- **learning and checking**: it learns on the first half and **walks forward** over
  the second, in as many stretches (up to 5) as leave each one 10 spans of the longest
  horizon. Each stretch is traded with the thresholds re-fitted on everything before
  it — as trading it from then on would — so every point of the second half is
  out-of-sample. Why half: for an edge that's really there, t grows with √trades, so
  the weaker of the two t's (what strategies are ranked by) is largest when both
  halves are the same size;
- **horizons**: how long trades are held, spread over everything the data allows —
  from 2 points up to 1/30 of the learning half, each about 3× the last (up to 6), so
  quick trades and long holds are both tried. Costs don't choose them: every trade
  pays the spread and fees, and walking forward says what's left. For longer holds give
  it more data, or fewer points (24 hours of 1-second points reach about 24 minutes;
  `--resample 5s --history-depth 3d`, about 2 hours);
- **tails**: for each horizon, enough of a formula's range that events h apart come
  up about 20 times per tail (2% to 10%), and 3× that;
- **thresholds are ranks**, not numbers: "in the lowest 5% so far". Re-fitting keeps
  the rank, so a rule still means the same thing when the market drifts.

The panel's second line shows the layout, e.g. *learned on the first half (12000
points), checked walking forward over 2 stretches · horizons 18 s 27 s 62 s 2.3 min ·
tails 3–30%*.

**1. Formulas.** Signals are formulas in a small language over the series:
`pct(price, 5)` (the change over 5 points, in bp), `z(…, 50)` (how many standard
deviations from the 50-point average), `pos` (place in the range, 0–100), `lag diff
mean ema std max min rsi slope since_high since_low`, `abs neg sign log run`
(`run`: the streak of moves one way), `+ - * /`, `gap(a, b)` (a relative to b, in bp),
`corr(a, b, n)` and `motif(price, "++-")` (1 where the last moves had that shape). Every
value uses only points up to its own.

Every formula has a **kind** — a level (the price), a change in its units, bp,
unitless around zero (z, corr), 0–100 (rsi, pos), 0–1 (a motif), a count of points —
and whether it's always positive. New formulas whose parts don't go together are
never tried, however well they'd happen to fit: a price plus an RSI, a level times
anything, dividing by something that crosses zero, a % change or log of something
that isn't a level, the sign of something always positive.

The search starts from the usual indicators, 150 random formulas and **templates** —
shapes traders use, with random parts: momentum in units of volatility
(`pct(price, n) / std(pct(price, 1), m)`), a divergence between the price's trend and a
signal's, a signal taken only with the trend (`z(…) * sign(slope(price, m))`), mean
reversion weighted by how wild it is, acceleration, a short range against a long one,
another series' surprise in the price's direction, a pattern in context. Then it
evolves them: mutations (another window, another operation, a part replaced),
crossovers (a part of one swapped into another), and **compositions** of two that
already work — `z(A) + z(B)`, `z(A) * z(B)` (each matters only when the other is
high), A only when B's sign agrees, `corr(A, B)`, how fast A changes — so deep
formulas grow out of ones with a record rather than out of chance. A formula's
**fitness** is how clearly the price moved one way after its extremes — its lowest and
highest 10% — on the learning half only (the best |t| over the horizons, less 0.04 a
part: deep formulas win when they really are better, and the walk-forward catches the
ones that only fit the past). Formulas that drift out of the range they had (like the
price itself: "price above 78,000" means something else tomorrow), that move with the
price's level, or that add things of wildly different sizes, are thrown out. The best
150 carry over to the next search, so it keeps improving as data arrives.

**2. Theories** are its findings, each a statement you can check: *when this formula
is in its lowest (or highest) few %, the price over the next h points moves this much
on average*. For each of the fittest 40 formulas (plus the usual indicators), each
horizon and its tails:

- the moments it holds on the learning half are taken **at least h points apart**, so
  no two overlap (overlapping windows would count the same move many times and
  inflate the evidence);
- the price's change h points after each gives an average and a **t-statistic**: the
  average divided by its standard error (t = mean ÷ (sd ÷ √n)). Around 2 is "probably
  not luck" for one test — but it tests thousands, and the strongest of thousands of
  pure-noise tests has a |t| near √(2 ln N) (about 4.5 for 20,000). That's the **bar**:
  a theory is listed only above it;
- then it's checked **walking forward** over the second half, which it never saw:
  **✓ held** when the move there is the same way with t ≥ 2, else **✗ faded**.

A theory is about the move, before costs — e.g. on 6 hours of BTC 1-second prices:
*z(price, 15) > 1.53 (top 15%) → 5 s later +0.51bp (t 7.4, 761 times) · walking
forward: +0.43bp (t 6.9) ✓ held* — real short-term momentum, but half a basis point.

**3. Strategies** trade the theories that cleared t 2 on the learning half: in their
direction — long, short, or both ways when the formula's other extreme moves the price
the other way — out after h points or at a take-profit / stop-loss sized from the
usual move (several ways, including small stops with big targets, and with profit
sharing targets just past the ROI tiers); the best 15 learn one condition they lose in
(**except when …**: what tells their losing trades from their winners). Every trade
pays the costs above. A strategy must make money on the learning half (t ≥ 1.5, at
least 10 trades) **and beat holding**: each trade's move is compared with what holding
the same way for as long made in that stretch (the price's drift), so a strategy that
only rides the trend — long for ten minutes while the price rises, winning 100% of the
time — has no edge and isn't kept. It's checked walking forward and ranked by the
weakest of the three (where it was picked, walking forward, and against holding), less
when it made money in fewer than half the stretches (one lucky stretch). The panel
shows **beats holding by …bp** for each; the review says when one only rode the trend. What's
then **paper-traded live**, from the moment it's found, has its thresholds re-fitted
on all the data — the live record is the only one the search never saw, so trust it
most. The panel shows each one's record where it was picked, walking forward (up in
how many stretches) and live (and, with leverage, the ROI a trade), then what it
risked walking forward and live: its **max drawdown** (the most its running total fell
from a high), its **worst** and **best** trade — with leverage, also as a share of the
margin a trade puts up (a 34bp drawdown at 250x is 85% of it).

**4. Learning from what it trades.** Each search reviews the strategies being
paper-traded: whether costs eat an edge that's there, whether it faded (it fitted the
past) or never predicted, and what it loses in; a strategy with a fix is tried again
with that condition left out (✎ in the panel). Ones that keep losing live are retired
with what they learned, and never picked again.

Tap a strategy (or `discover_pick(2)`) to draw its trades (▲/▼ in, • out with its
result); **theories** switches the panel to them. **keep** (`discover_keep()`) turns a
strategy into ordinary chart parts: its formulas become hidden lines (`--line 'disc1_1
= formula("…")'`) and its entries a marker rule (`disc1 long`) you can backtest,
notify on or alert. `undiscover()` stops. The pieces work on their own: `formula("z(pct(
price, 5), 50)")` in a line, and `zscore`, `change_bp`, `range_pos`, `updown`,
`volatility`, `gap` for arrays.

Be sceptical: most of what looks good fades, and few edges survive costs — on BTC the
moves it finds are fractions of a basis point, so give it more than the price.

**Order flow.** Every series on the chart is a possible signal, so merge in another
source's volume. For two series where one is always a part of the other (taker buys of
all the volume, …) it adds their **share** and how unusual it is now (buying against
selling), and for each how unusual its last 10 or 60 points are (bursts of volume, of
trades); a price-like one (another exchange's price) is compared to the price as a gap
(its premium, which often moves first). The evolution combines them with everything
else. A live BTC price with bid and ask from an oracle feed, resampled to a point a
second, with Binance's live 1-second candles merged in (price, volume, taker-buy
volume, trades), 24 hours of Binance's 1-second history with the same columns (named
`binance.…` so they join the live ones), and Upside's profit sharing at 250x:

```sh
graft --stream 'https://feed-v3.veranta.xyz/v1/stream?price_feed_ids=1' --json --time-field timestampUs \
  --series 'price:USD = priceFeeds_0_price * 1e-8' --series 'bid:USD = priceFeeds_0_bestBidPrice * 1e-8' \
  --series 'ask:USD = priceFeeds_0_bestAskPrice * 1e-8' --select price,bid,ask --resample 1s:last \
  --merge 'binance = --stream wss://data-stream.binance.vision/ws/btcusdt@kline_1s --json --time-field k.t --replace --series "price = k_c" --series "vol = k_v" --series "buys = k_V" --series "trades = k_n" --select price' \
  --history '-c "curl -s \"https://data-api.binance.vision/api/v3/klines?symbol=BTCUSDT&interval=1s&limit=1000&startTime={from_ms:1000s}&endTime={now_ms}\"" --json --time-field 0 --series "price:USD = c4" --series "binance.price = c4" --series "binance.vol = c5" --series "binance.buys = c9" --series "binance.trades = c8" --select price' \
  --history-depth 24h --hide bid,ask,binance.price,binance.vol,binance.buys,binance.trades \
  --discover upside:250 --marks-below
```

**The order book.** Hyperliquid's book snapshot (the 20 best levels a side, as they
are right now) is a REST call, so `--stream` polls it every `--every 1s`: each reply is
the whole book, with no changes to apply to an earlier copy. From it: the mid price,
the size on the 5 best levels each side, their **imbalance** (buyers' size against
sellers', −1 to 1) and the **microprice** (the mid tilted toward the thinner side).
A second snapshot with `nSigFigs: 3` groups the book into $100 levels, so 4 of them
reach about half a percent each way: the **deep imbalance**, for moves over minutes.
Binance adds the order flow, with 24 hours of history. There's no book history to
download, so `--record` saves every snapshot (about 140 MB a day per file; a day reads
back in a few seconds) and the next run reads them back first:
the longer it runs, the more book there is to learn from (until a recording reaches
back far enough, the price before it is Binance's). Searches take turns on all the data
and on the newest stretch where the book series exist, once there are 10 minutes of
them, so book signals get tried as soon as there's enough. Hyperliquid's taker fee is
0.045% a side:

```sh
graft --stream https://api.hyperliquid.xyz/info --send '{"type":"l2Book","coin":"BTC"}' --every 1s --json --time-field time \
  --series 'price:USD = (levels_0_0_px + levels_1_0_px) / 2' \
  --series 'bids = levels_0_0_sz + levels_0_1_sz + levels_0_2_sz + levels_0_3_sz + levels_0_4_sz' --series 'asks = levels_1_0_sz + levels_1_1_sz + levels_1_2_sz + levels_1_3_sz + levels_1_4_sz' --series 'imbalance = (bids - asks) / (bids + asks)' \
  --series 'micro:USD = (levels_1_0_px * levels_0_0_sz + levels_0_0_px * levels_1_0_sz) / (levels_0_0_sz + levels_1_0_sz)' \
  --select price,imbalance,micro,bids,asks --resample 1s:last --record ~/graft/hl-btc-book.jsonl \
  --merge 'deep = --stream https://api.hyperliquid.xyz/info --send "{\"type\":\"l2Book\",\"coin\":\"BTC\",\"nSigFigs\":3}" --every 1s --json --time-field time --series "bids = levels_0_0_sz + levels_0_1_sz + levels_0_2_sz + levels_0_3_sz" --series "asks = levels_1_0_sz + levels_1_1_sz + levels_1_2_sz + levels_1_3_sz" --series "imbalance = (bids - asks) / (bids + asks)" --select imbalance,bids,asks --record ~/graft/hl-btc-deep.jsonl' \
  --merge 'binance = --stream wss://data-stream.binance.vision/ws/btcusdt@kline_1s --json --time-field k.t --replace --series "price = k_c" --series "vol = k_v" --series "buys = k_V" --series "trades = k_n" --select price' \
  --history '-c "curl -s \"https://data-api.binance.vision/api/v3/klines?symbol=BTCUSDT&interval=1s&limit=1000&startTime={from_ms:1000s}&endTime={now_ms}\"" --json --time-field 0 --series "price:USD = c4" --series "binance.price = c4" --series "binance.vol = c5" --series "binance.buys = c9" --series "binance.trades = c8" --select price' \
  --history-depth 24h --hide imbalance,micro,bids,asks,deep.imbalance,deep.bids,deep.asks,binance.price,binance.vol,binance.buys,binance.trades --discover 0.045 --marks-below --title 'BTC order-book lab'
```

**It never stops inventing.** Searches run one after another (3 s each) on every core
but one, scoring new formulas in parallel — hundreds of thousands a minute on a laptop;
the panel counts them (formulas a second, and in all). How many it keeps for the next
search isn't fixed: as many as it has time to score again in a third of a search, so it
grows with the machine. **Retired strategies are still watched**: paper-traded from
when they were retired, and one that pays again (8 trades, t ≥ 1) comes back — a
strategy that only works in some markets isn't lost after one bad patch.

### Counting log lines

Some logs don't have numbers worth charting, but *how often* something happens
is: `--grep` counts the lines matching a regex in each time bucket.

```sh
graft /var/log/app.log --grep 'errors=ERROR|FATAL' --grep 'warnings=WARN'   # per minute
graft /var/log/nginx/access.log --grep '5xx=" 5\d\d ' --grep-every 5m
journalctl -f -o short-iso | graft --grep 'failed=(?i)fail'                  # live
```

`NAME=` names the series (else the regex is the name); `--grep lines=.` counts
every line. Buckets with nothing in them count as 0, lines without a timestamp
(stack traces) count with the entry above them, and live sources use arrival
time. `{sum}` in `--stats-template` gives the total.

### Choosing and transforming series

| | |
|---|---|
| `-s, --select 'Mem*,Cached'` / `-e, --exclude lo` | choose series (globs allowed) |
| `--series 'used = MemTotal - MemAvailable'` | derived series (Rhai expression, unit literals allowed); `--series 'busy:% = …'` sets its unit. Computed after `--rate`/`--delta`, so ratios of rates work, and in order, so each can use the ones before it (`--series 'bids = b0 + b1' --series 'asks = a0 + a1' --series 'imbalance = (bids - asks) / (bids + asks)'`) |
| `--rate` | counter → per-second rate |
| `--delta` | change between points |
| `--resample 1h:max` | bucket (mean, min, max, sum, last, count); in the viewer `resample("5s")`, `resample(false)`, or **resample** in **⋯** (cycles 1s, 5s, 15s, 1m, 5m, off) — applied at once, since the raw points are kept |
| `--smooth 5` | moving average |

### Charts

| | |
|---|---|
| `-k line\|scatter\|bar\|histogram\|sparkline\|stat\|candles` | chart type (bar = latest values; stat = a tile per series with the value in big digits, its change, trend, target ETA and a sparkline — good as a pane above a chart; candles = candlesticks from series named open/high/low/close or o/h/l/c, else the first four, with any other series as lines over them) |
| `--split` | one panel per series, each with its own y axis |
| `--pane 'node1 kind=bar lines=live,avg'` | panes: several charts in one view (repeat `--pane`); each picks series (`*` = all), `kind=`, `lines=`, `front=` (series drawn on top of the others, e.g. an RSI over its 70/30 lines; `--front` without panes, `front("rsi14")` in the console), `source=` (another preset, saved profile or recent chart, with its own command and format) and `title=` (last: the rest of the spec; `{name}` fills in a source setting) |
| `--layout '(1 \| 2) / 3'` | how panes are arranged: `\|` side by side, `/` stacked, `\|60%` sizes (default: two side by side, more in a grid) |
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

Template fields: `name n first last min max mean median stddev p95 sum change trend
per r2 target eta eta_at direction model unit`. Add `:raw` (e.g. `{last:raw}`)
for unformatted numbers.

`--alert EXPR` evaluates a Rhai condition and exits with status 3 if it holds,
so graft works in cron jobs and scripts:

`--notify` tells you when an alert starts firing — in the viewer (once per
time it starts) and with `--once`/`--no-chart`, which makes graft a cron job:

```sh
# every hour: push to your phone (install the ntfy app, subscribe to the topic)
graft ~/reclaimable.log -F '{time} node1: {node1} node2: {node2}' --no-chart \
  --alert 'node1 < 55GB' --alert 'eta("node2", 200GB) < 3d' --notify ntfy:my-disks
```

Targets: `bell`, `desktop` (notify-send / macOS notifications),
`ntfy:TOPIC` (ntfy.sh, or `ntfy:https://your-server/TOPIC`), `webhook:URL`
(Slack/Discord-style JSON), `cmd:COMMAND` (with `$GRAFT_ALERT` and
`$GRAFT_TITLE`). Repeat `--notify` for several.

**Copy the command:** `C`, the **copy cmd** button under `⋯`, or `:copy()` puts
the command that reproduces the current view (panes, lines, timeframe…) on your
clipboard — via the terminal (OSC 52), so it works over SSH from a phone too
(in tmux: `set -g set-clipboard on`). `copy(anything)` copies a value.

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
Your saved profiles appear in the same list, followed by **chart a file**,
**chart a command** and **chart a stream** (a WebSocket or server-sent events URL).

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
| toolbar | **lines ▾** **all time ▾** **stats** **panes** **sources**, plus ↶ undo, **markers** and **alerts** once there are some, and ◀ ▶ **show all** while zoomed in; `⋯` shows the rest: + − cursor measure side by side forecast split targets, chart type, log, follow, pause, format, save, copy, console, export, mouse, help, quit |
| legend row | under the chart (with several panes, each pane also keeps its own legend box): live or paused, the time of the newest data (or **cursor** and its time), each series with its value (at the cursor, if there is one), target lines and a firing alert. Tap a series to hide or show it |

**Timeframe** (anything on a time axis — log timestamps, or arrival times for
live commands like `ping`): tap **all time ▾** (or press `r`) for a dropdown
of ranges that fit your data (`last 1d`, `last 6h`, … `last 5m`, `last 1m`) and
**custom…**, which opens the console with `range("` ready to type
(`range("2h")`, `range("2026-11-01", "2026-11-02 12:00")`). In the dropdown ↑ ↓
move; outside it `[` / `]` go narrower or wider. Stats, trends and forecasts
all use the chosen timeframe.

**Lines:** tap **lines: live** (or press `T`) for a menu where you tick any mix
of **live**, **average**, **median** and **p95**, and make the rolling window
bigger or smaller (`+` / `−`, shown in points and time). Each line's value is in
the cursor readout. Ping-style data (mostly low with spikes)
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
| `← →` / `h l` | move left / right when zoomed in (at the newest it stays zoomed and follows new data; `0` shows everything); on a timeframe like `1h`, `←` pages that hour back (still 1h wide, loading older data if the source can) and `→` at the newest is the live last hour again |
| `↑ ↓` / `+ -` | zoom in / out |
| `,` `.` / shift `← →` | step the cursor point by point |
| `H L` | move by half a screen |
| `0` | show all (reset the zoom) |
| `c` / `o` | cursor / cursor follows the pointer |
| `Home` / `End` | cursor to first / last point |
| `v` … `v` | measure a range from the keyboard |
| `1`–`9` / `a` | toggle a series / show all |
| `t` / `b` / `y` / `x` | chart type / marker style / log scale / split panels |
| `T` | lines menu (inside: `l` `a` `m` `p` toggle, `+` `-` window, Esc closes) |
| `D` | side by side: live data next to its trends |
| `w` | pane panel (inside: `v` / `s` split right / down, `x` close, arrows pick, shift+arrows move, ctrl+arrows or `< > + -` resize, `=` even, `t` chart type, `T` lines, `1`–`9` series, `o` one chart) |
| `Tab` / `Alt+arrows` | pick the next pane / the pane that way |
| `K` | markers: edit, delete or add rules and markers |
| `S` / **sources** | where the data comes from: edit the main source, add, edit or remove merged ones (in the builder, with a preview) |
| `Ctrl+Z` / `u` | undo the last change (chart type, lines, panes, timeframe, markers…); `Ctrl+Y` / `U` redo. Also the **↶ undo** button once there's something to undo |
| `f` / `G` | forecast / target lines on or off |
| `r` / `[` `]` | timeframe dropdown / narrower, wider timeframe |
| `F` | change the format or source (builder) |
| `s` | stats panel |
| `space` | pause the chart: the source keeps running and its data is kept, so resuming jumps to the latest |
| `e` | export the current view to SVG |
| `m` | mouse capture on/off (off: your terminal can select text; the legend row says how to turn it back on) |
| `:` | Rhai console |
| right-click / **⚙** | a pane's (or the chart's) settings in one place: its source (**edit** what it runs, **other source…** for different data) and that source's settings (`--param`), chart type, lines, series, split or close |
| `i` | legends on/off (also `legend()`, and under ⋯) |
| `↑ ↓ ← →` / `Tab`, `Enter` | in a menu (timeframe, lines, sources, alerts): move the highlight between its buttons; Enter presses it, like a tap |
| `q` | quit |

### Panes

Like a terminal multiplexer, but for charts. Tap **panes** (or press `w`) and
the panel opens under the chart:

- **split right** / **split down** splits the picked pane (outlined in yellow);
  the new pane starts as a copy, so change what it shows: its **chart** type,
  its **lines** and its **series** (the legend row lists the picked pane's)
- tap a pane to pick it, or use the **focus** arrows; **move** swaps it with
  its neighbour; **size** moves its edge, or just drag the border between two
  panes (with a finger too)
- **even** evens the sizes out, **one chart** goes back to a single chart

Zoom, the cursor and the timeframe are shared, so the panes stay lined up in
time. Side by side is the two-pane case: splitting from it turns it into
panes. Every layout can be written down and saved in a profile:

```sh
graft ~/reclaimable.log -F '{time} node1: {node1} node2: {node2}' \
  --pane 'node1 lines=live,median' --pane 'node2' --pane '* kind=bar' \
  --layout '(1 / 2) |65% 3'
```

**Different data in each pane.** A pane can show another source entirely:
`source=` takes any preset, saved profile or recent chart (`@2`), which runs
with its own command and format. In the panel, tap **source** to pick one.
With no main file or command, the panes alone make a dashboard:

```sh
graft -p dashboard          # built in: CPU, memory, load and network in a grid
graft --pane source=cpu --pane 'source=load kind=stat' --pane source=@1 --layout '(1 | 2) / 3'
graft ~/reclaimable.log -F '…' --pane '*' --pane 'source=disk kind=stat'
```

Time-based panes share zoom and the cursor when their times overlap; the
timeframe applies to every pane.

### The Rhai console and scripts

Press `:` to open the console. As you type, the row above it suggests commands
(and, inside quotes, series names or a function's choices such as chart types, or
for `param(…)` the source's settings and then that setting's values; for
`backtest(…)` and `strategy(…)` the marker rules by label),
and shows how the call you're in is used: **Tab** (or a tap) puts the highlighted
one in, again for the next. `help` lists every command at once, grouped; tap one to
use it.

Inside the console, every series is a variable holding its latest value.
Functions work on the visible range, or on the selection if you've made one.
Answers come in the series' unit and say which point they're from:
`last("api")` → `api: 44.0 ms — the newest value (09:27:10)`, and with a cursor
placed, the value there too (`at("api")` is the value at the cursor). Trends read
as words: `slope("api", "1h")` → `api is rising 2.1 ms per hour`.

Where a function wants a series' name, a series written plainly is its name:
`last(busy)`, `slope(user, 1h)`, `only(web, api)`, `mean_of([web, api])` (a
script's own variables are left alone). Examples in hints and errors use the
chart's own series.

Expressions that are kept to run later can be written as code, no quotes needed:
`add_line("total", user + sys)`, `alert(api > 45ms)`,
`mark_when(value > 60ms, "spike")`, `bind("r", only("api"))`; Tab completes series,
functions and (in a marker rule) `value`, `prev`, `delta`… inside them, quoted or not.

A mistake is explained in plain words with a fix, and the console stays open on it
with the cursor at the problem: `last(api)` → *last() needs the series' name in
quotes: last("api")*; `lst("api")` → *did you mean last("api")?*.

```
:node2 - node1
:slope("node2", "1d")                       // change per day
:fmt("node2", slope("node2", "1d"))         // …formatted: "-7.69 GB"
:fmt_dur(eta("node1", 50GB))                // time until node1 hits 50 GB
:hline(mean("node2"), "mean"); zoom("12h")
:target("node2", 200GB); forecast("14d")    // set a target and show the projection
```

Available functions:
- Statistics: `series()`, `first`, `last`, `at` (at the cursor), `min`, `max`, `mean`/`avg`, `median`,
  `stddev`, `p95`, `percentile(name, q)`, `sum`, `count`.
- Trends: `slope(name[, "1d" or 1d])`, `r2`, `change`, `pct_change`, `span`,
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
  - `svg()` / `svg("out.svg")`, `format()` (open the builder), `quit()`,
    `save("disks")` — saves the whole view (source, format, panes, lines,
    timeframe…) as a profile; `graft -p disks` opens it again (also the
    **save** button under `⋯`)
  - panes: `split_right()`, `split_down()`, `close_pane()`,
    `focus_pane(2 | "left")`, `move_pane("left")`, `resize_pane("right"[, 10])`,
    `even_panes()`, `layout("(1 | 2) / 3")`, `pane("node1 kind=bar lines=live,avg")`,
    `pane_source("cpu" | "@2" | "main")`,
    `single()`. With panes, `chart(…)` and `lines(…)` change the picked pane
- Output: `print(...)`.

Unit literals such as `50GB`, `200ms`, `5m` and `1d` work in all expressions.

`--script file.rhai` loads functions and key bindings
([examples/keys.rhai](examples/keys.rhai)):

```rhai
fn daily(name) { fmt(name, slope(name, "1d")) + "/day" }
bind("D", daily("node2"));
bind("W", hline(50GB, "warn"));
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

### Recent charts

You don't have to name anything: every chart you view is remembered as you
left it (format, panes, lines, timeframe…), one entry per file or command, and
`graft` on its own lists the latest at the top of the start screen — tap one to
pick up where you left off, **edit** to change it first, **✕** (or `x`) to
forget it.

```sh
graft -p @          # reopen the most recent chart
graft -p @2         # the one before
graft --list-profiles   # also lists recent charts as @1, @2, …
```

They're kept in `~/.local/state/graft/recent.toml` (`$XDG_STATE_HOME`, or
`$GRAFT_RECENT`). Charts of piped input aren't kept, since they can't be reopened.

## Development

```sh
cargo test                                                    # unit, end-to-end and rendering tests
GRAFT_SNAPSHOTS=snaps cargo test snapshots -- --include-ignored  # write every screen as colour ANSI
```

The snapshot scenarios (`src/snapshots.rs`) cover the `--once` output, the
viewer (cursor, split, forecast, timeframe, help, many series), the builder and
the gallery, at desktop and phone sizes. `cat snaps/*.ans` shows them in your
terminal.

**Adding a feature? Add it to the tutorial too** (`src/tutorial.rs`): a key or
button goes in a step's keys table, a flag or console command in its examples, a
bigger feature gets a step of its own. The tests hold this up:

- `the_tutorial_covers_every_feature` fails for any flag, toolbar button or console
  function the tour doesn't mention, and says which;
- `every_tutorial_example_works` runs every console example on the demo chart and
  parses every flag example with the real argument parser;
- `the_tutorial_walks_through_everything` does each step's task the way a user
  would and checks it ticks off, at desktop and phone sizes.

## Shell integration

```sh
graft --completions bash > ~/.local/share/bash-completion/completions/graft
graft --man > ~/.local/share/man/man1/graft.1
```
