# graft — handoff

Context for picking this project up in a new session (written when moving from a cloud
session to a local one, 2026-11-03). Read `CLAUDE.md` first. Its rules are mandatory.
This file explains what's built, how, and what's next.

## What graft is

A Rust CLI/TUI that graphs any data in the terminal: files, pipes, commands polled on
an interval, presets (cpu, memory…), remote commands over SSH. It parses text, JSON,
CSV and key=value, auto-detects formats, and draws line, scatter, bar, histogram,
sparkline, stat and candles charts. Panes, a Rhai console, custom lines, markers,
alerts, backtests. Stack: Rust 2024, ratatui 0.30, crossterm, clap (derive), Rhai
(`only_i64`), regex, serde_json.

The user (milo123459, repo `milo123459/graft`, branch `master`):
- builds locally on a Mac and a Raspberry Pi, and uses graft from a phone over SSH;
- mostly charts Hyperliquid crypto candles, and is building toward a trading-alert /
  strategy tool;
- wants direct action, short answers and to see results; likes ideas and asks for many
  features at once;
- uses release builds (`cargo r -r --`). Don't touch Cargo profiles: they rejected
  `[profile.dev.package."*"] opt-level` and asked for it removed.

## Rules (from CLAUDE.md, enforced by tests)

- Before every commit: `cargo fmt`, `cargo clippy --all-targets` (no warnings),
  `cargo test`. Commit and push after each batch.
- Commit messages end with
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` (plus a `Claude-Session:` line
  when there is a session URL). No model IDs in commits or code.
- Everything must work at **46 columns** and by **tapping** (mouse) as well as by keys.
- Every toolbar button has a console command (`every_button_has_a_console_command`).
- Every console function needs a USAGE line and exactly one GROUP in `src/script.rs`
  (`every_function_has_usage_and_a_group`). That test finds functions by the literal
  text `register_fn("name"`, so register each name literally (no loops over names).
- **Keep the tutorial up to date** (`src/tutorial.rs`) for every user-visible change,
  and README.md alongside it. These enforce it: `the_tutorial_covers_every_feature`
  (every flag, key and console function is mentioned), `every_tutorial_example_works`
  (examples run with the `graft app.log` demo prefix) and
  `the_tutorial_walks_through_everything`. Don't silence them with exemptions.

## Layout of the code (~34k lines)

| file | what |
|---|---|
| `src/main.rs` | entry; `run_once` (`--once`, non-interactive output) |
| `src/cli.rs` | clap `Cli`, `ChartKind` (Line, Scatter, Bar, Histogram, Sparkline, Stat, Candles; `next()` cycles), `to_args` (writes a Cli back as flags), `with_params()` (fills `{name}` source settings into series, units, lines, markers, alerts…) |
| `src/setup.rs` | `source_spec` (fills params and time placeholders into the command and path), `build` (makes the Session and ChartOpts from `cli.with_params()`; `Setup.cli` keeps the raw `{name}` form) |
| `src/session.rs`, `data.rs`, `parse.rs`, `transform.rs` | Store, parsers (a JSON array is one record per object), pipeline, `--series` derived series (`derive`: a plain name is copied directly; otherwise only the series it mentions are lined up) |
| `src/source.rs`, `interactive.rs` | running commands (stdin null, stderr piped), `Live` (thread + channel, `pump` doesn't block) |
| `src/sources.rs` | sub-sources for panes/merges, named `--source` definitions |
| `src/params.rs` | `--param 'interval = 1m \| *5m'`, `fill`, `fill_times` (`{now}`, `{now_ms}`, `{from:6h}`, `{from_ms:500*{interval}}`), `has_window` |
| `src/script.rs` | Rhai host: console API (USAGE, GROUPS, `register_api`), completion (`complete_with(text, cursor, series, params, rules)`), `explain_error`, built-in indicators |
| `src/lines.rs` | `--line` custom lines; indicator maths: `ema sma rsi macd bollinger vwap atr stoch highest lowest` |
| `src/marks.rs` | `--mark` rules: `expand_cross` (cross_above/below/cross → plain conditions), evaluation (one scope per series; rules not using per-series variables run once per point of the first series named), styles (symbol, colour names incl. orange/purple/…, effects bold/italic/underline/dim/reverse/badge, `on:COLOUR`) |
| `src/backtest.rs` | `followed` (what came after markers), `simulate` (long/short, SL/TP against high/low, time window), `best` (grid of side×SL×TP picked on the first 70%, run on the last 30%), `pairs` (NAME in/out), `series_for` (prefers `close`) |
| `src/render/mod.rs` | `ChartWidget`, `render_xy` (lines, `front=` order, candles via `draw_candles`, readout box), `candle_roles_named/of`, `pane_view` (candles panes bring their OHLC along) |
| `src/render/panes.rs` | `Pane` spec: series, `kind=`, `lines=`, `front=`, `source=`, `title=` (takes the rest) |
| `src/tui.rs` (10k lines) | `App`, the viewer: toolbar, legend row, popups, console, settings menu, markers menu, backfill, params, backtest/strategies/sweep commands. Tests are at the bottom (`pub(crate) mod tests`: `app()`, `screen`, `screen_sized`, `run`, `key`, `mouse`, `find`, `type_str`) |
| `src/builder.rs` | the format builder (F); sources: file, command, stream, stdin |
| `src/stream.rs` | `--stream URL` (`--ws`/`--sse`): WebSockets (tungstenite + rustls/ring, webpki roots) and streaming HTTP (ureq): server-sent events (`data:` = one record) or lines; `--send` messages; reconnect with backoff |
| `src/formula.rs` | the formula language (`z(pct(price, 5), 50)`): parse/print, eval over lined-up columns (NaN-aware, causal), random/mutate/cross for evolution, `classics()` seeds |
| `src/discover.rs` | the discovery engine: `Input` (price, bid/ask by name, other series), `Layout` (from the data: learn on the first half, walk forward over up to 5 stretches with thresholds re-fitted by rank (`Ranks`), horizons from where σ_h ≥ 4× round-trip cost, tails per horizon), evolve formulas (typed by `Expr::kind`, templates + compositions of fit ones, fitness = best |t| after 10% tails on the learning half − 0.04/part), theories (bar = √(2 ln tests)), strategies (exits sized from σ, costs: fee + ask/bid), dedupe by overlapping entries |
| `src/lab.rs` | the viewer's lab: background searches every 20 s (3 s budget, pool carried over), paper-trading tracked strategies from when found, markers |
| `src/tutorial.rs` | tutorial steps (keys, examples, checks) |

## Features added in this session (newest last)

- Source settings `--param` with toolbar dropdowns, console `param()`, filled
  everywhere (`with_params`); saved charts keep the `{name}` placeholders (`current_cli`
  maps lines/markers/alerts back using `built_params`).
- Changing a setting (coin, interval) keeps your window, cursor and selection.
- Time placeholders, plus **backfill**: scroll or zoom to the oldest data (or press ←
  with everything in view) and the command reruns for the window before it.
- `--replace` (same-x points are updated, not appended).
- Pane settings menu (right-click or ⚙): source (edit / "other source…"), params,
  chart kind, lines, series. `i` hides legends.
- **Candles** chart: half-block bodies, wicks, merged per column. Lines and the cursor
  show through. One "candles" chip in the legend and series lists (close value). The
  readout shows O/H/L/C on one line. A candles pane brings its OHLC series along.
  Picking candles adds them.
- `front=` (pane), `--front`, `front()`: draw a series on top of the others.
- Marker styles: effects, badges, backgrounds, more colours, an effect button in the
  markers menu (each rule's buttons sit on their own row).
- `cross_above / cross_below / cross`, `is_num`.
- Built-in indicators (above); `rsi`/`macd` were moved out of `examples/indicators.rhai`
  because they were slow in Rhai.
- **Backtests**: `backtest(rule, after)` (after = points or `"1h"`), `strategy(entry,
  exit)`, `strategies(fee)`, `sweep(fee)`. Strategies are rule pairs labelled
  `NAME in` / `NAME out`. Both report long, short, and the best side/SL/TP (picked on the
  first 70%, checked on the last 30%). `sweep` covers every combination of the `--param`
  settings used by lines and rules (not the source's) and is ranked by the last 30%.
  Results tables stay until Esc, a tap or the next command.
- Performance: a full rebuild went from 67 ms to 6 ms at 3000 candles.
- Messages draw on the chart's bottom edge (they never move the layout). The legend
  height only grows.
- ← on a "last 1h" timeframe pages that hour back and keeps the timeframe (`paged`);
  → at the newest goes back to live.
- Console completion: `param(` names then values, `backtest(`/`strategy(` rule labels,
  prefix matches first.

## Pending (agreed with the user, roughly in order)

1. Done (2026-11-04): **streams** (`--stream`, `--ws`, `--sse`, `--send`) and
   **discovery** (`discover()`, `--discover FEE`, `E`, the discover panel,
   `discover_keep/pick`, `undiscover`, `formula()`, `zscore change_bp range_pos updown
   volatility gap`). Ideas next: let users seed the search with their own formulas,
   multi-asset features (several --merge'd prices), a maker-fee/limit-order exit model,
   walk-forward (rolling) re-validation of tracked strategies, alerts on live entries.
   Six tests fail on macOS only (presets use sysctl, tests expect /proc): pre-existing.
2. **Volume bars under the candles** (own small scale in the candles pane) and a
   **Heikin-Ashi** candle style.
3. **Draw tool**: pick a shape (horizontal level, vertical line, box, trend line), tap
   to place it, saved with the chart (a flag plus a console command plus a toolbar
   button).
4. **Rules as Rhai functions** (marker rules calling script functions with a map of
   the point's variables) and an **in-app script editor**.
5. Later or maybe: alert actions (`cmd:` on alert, cooldowns; the user said "not
   yet"), multi-timeframe ("not sure"), kitty graphics, recording/replay, a headless
   daemon, a watchlist pane, a command palette.

## The current test command (Hyperliquid strategies)

```sh
cargo r -r -- -c 'curl -s --json "{\"type\":\"candleSnapshot\",\"req\":{\"coin\":\"{coin}\",\"interval\":\"{interval}\",\"startTime\":{from_ms:500*{interval}},\"endTime\":{now_ms}}}" https://api.hyperliquid.xyz/info' \
  --param 'coin = ETH | BTC | SOL | HYPE' --param 'interval = 1m | 5m | *15m | 1h | 4h | 1d' \
  --param 'fast = 9 | 12 | 20' --param 'slow = 21 | 26 | 50' --param 'trend = 100 | 200' --param 'band = 20 | 30' --param 'width = 2.0 | 2.5' \
  -n 5s --json --time-field t --replace \
  --series 'open:USD = o' --series 'high:USD = h' --series 'low:USD = l' --series 'close:USD = c' \
  --series 'volume:{coin} = v' --select open,high,low,close,volume \
  --line 'ema_fast:USD = ema(values("close"), {fast})' \
  --line 'ema_slow:USD = ema(values("close"), {slow})' \
  --line 'ema_trend:USD = ema(values("close"), {trend})' \
  --line 'upper:USD = bollinger(values("close"), {band}, {width})' \
  --line 'mid:USD = bollinger(values("close"), {band}, 0.0)' \
  --line 'lower:USD = bollinger(values("close"), {band}, -{width})' \
  --line 'hi:USD = highest(values("high"), {band})' \
  --line 'rsi14:% = rsi(values("close"), 14)' \
  --line 'k:% = stoch(values("high"), values("low"), values("close"), 14)' \
  --line 'macd = macd(values("close"))' \
  --line 'signal = ema(values("macd"), 9)' \
  --line 'overbought:% = 70' --line 'oversold:% = 30' \
  --mark 'cross_above(ema_fast, ema_slow) && close > ema_trend => cross in | ▲ lime' \
  --mark 'cross_below(ema_fast, ema_slow) => cross out | ▼ red' \
  --mark 'close > ema_trend && cross_above(rsi14, 40) => pullback in | ▲ cyan' \
  --mark 'cross_above(rsi14, 70) || cross_below(close, ema_slow) => pullback out | ▼ magenta' \
  --mark 'cross_below(close, lower) && rsi14 < 35 => revert in | ◆ sky' \
  --mark 'cross_above(close, mid) => revert out | ◆ pink' \
  --mark 'name == "close" && prev <= hi_prev && value > hi_prev && close > ema_trend => breakout in | ★ gold' \
  --mark 'cross_below(close, ema_fast) => breakout out | ✕ orange' \
  --mark 'close > ema_slow && cross_above(k, 20) => stoch in | ↑ green' \
  --mark 'cross_below(k, 80) || cross_below(close, ema_slow) => stoch out | ↓ red' \
  --mark 'cross_above(macd, signal) && macd < 0 && close > ema_trend => macd in | ● teal' \
  --mark 'cross_below(macd, signal) => macd out | ● purple' \
  --pane 'ema_fast,ema_slow,ema_trend,upper,lower kind=candles title={coin} {interval}' \
  --pane 'rsi14,overbought,oversold front=rsi14' --pane 'macd,signal front=macd' --pane 'k front=k' \
  --layout '1 /60% ((2 | 3) / 4)' --title '{coin} {interval}'
```

Then in the console (`:`): `strategies(0.045)`, `sweep(0.045)`,
`backtest("revert in", 4)`, `strategy("revert in", "revert out")`. The simpler TA
dashboard (EMA30/50, RSI, MACD, volume) is the tutorial's "recipe: a trading dashboard".

Findings so far: no strategy beats fees and holding robustly. The "best" SL/TP picks
mostly fade walking forward, which is the honest answer the user has accepted.

## Gotchas

- `tui.rs` is huge. Edit by exact text, not line numbers; `cargo fmt` reflows code, so
  re-grep after formatting.
- Tests that need a source use `-c 'echo …' --count 1`, then `tick()` until
  `app.live.eof`, then `rebuild(true)`.
- `ScriptHost::compile_expr` runs `quote_series_names` (bare series names become
  strings for functions in `SERIES_FIRST`/`SERIES_ANY`). Marker rules run
  `expand_cross` before compiling.
- The console's code-argument quoting (`CODE_ARGS`: `bind` 1, `add_line` 1, `alert` 0,
  `unalert` 0, `mark_when` 0) lets you write `mark_when(value > 1, "x")` without quotes.
- An action that writes `console_note` itself (backtests) stops `eval` from overwriting
  it with "ok".
- `backtest::series_for` prefers a series called `close` over a raw `c` field.
- Marker rules are refused only if they match more than 300 points *and* over half of
  the points they check (a hard cap of 50k).
- Hyperliquid: `POST https://api.hyperliquid.xyz/info`,
  `{"type":"candleSnapshot","req":{coin, interval, startTime, endTime}}` returns at
  most 5000 candles, with fields `t o h l c v` (prices are strings).
