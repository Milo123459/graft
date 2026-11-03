// Indicators written in Rhai, for custom lines:
//   graft … --script examples/indicators.rhai --line 'ema30 = my_ema(values("ETH"), 30)'
// (graft also has ema() and sma() built in; these show how to write your own.)

// Exponential moving average over n samples, seeded with the first value.
fn my_ema(xs, n) {
    let k = 2.0 / (n + 1);
    let out = [];
    let prev = ();
    for v in xs {
        prev = if prev == () { v } else { v * k + prev * (1.0 - k) };
        out.push(prev);
    }
    out
}

// Bollinger band: the moving average plus `width` standard deviations over n samples
// (use a negative width for the lower band).
fn band(xs, n, width) {
    let out = [];
    for i in 0..xs.len() {
        let from = if i >= n { i - n + 1 } else { 0 };
        let w = xs.extract(from, i - from + 1);
        let mean = w.reduce(|sum, v| sum + v, 0.0) / w.len();
        let variance = w.reduce(|sum, v| sum + (v - mean) * (v - mean), 0.0) / w.len();
        out.push(mean + width * variance.sqrt());
    }
    out
}

// The gap between two series, lined up on the first one's times.
fn spread(a, b) {
    let other = values_at(b, xs(a));
    let mine = values(a);
    let out = [];
    for i in 0..mine.len() {
        out.push(if other[i] == () { () } else { mine[i] - other[i] });
    }
    out
}

// ── Technical analysis, e.g. on a price series "eth" ──
//   --line 'rsi14:% = rsi(values("eth"), 14)'
//   --line 'macd = macd(values("eth"))'  --line 'signal = ema(values("macd"), 9)'
//   --line 'hist = macd - signal'        (series by name: worked out at each point)
//   --line 'upper = band(values("eth"), 20, 2.0)'

// Relative strength index (Wilder's smoothing): 0–100; above 70 is often read as
// overbought, below 30 as oversold. The first n points warm up.
fn rsi(v, n) {
    let out = [];
    let gain = 0.0;
    let loss = 0.0;
    let n = n.to_float();
    for i in 0..v.len() {
        if i == 0 {
            out.push(50.0);
            continue;
        }
        let d = v[i] - v[i - 1];
        let g = if d > 0.0 { d } else { 0.0 };
        let l = if d < 0.0 { -d } else { 0.0 };
        if i.to_float() <= n {
            // Warm-up: a plain average of the first n changes.
            gain += g / n;
            loss += l / n;
        } else {
            gain = (gain * (n - 1.0) + g) / n;
            loss = (loss * (n - 1.0) + l) / n;
        }
        out.push(if loss == 0.0 { 100.0 } else { 100.0 - 100.0 / (1.0 + gain / loss) });
    }
    out
}

// MACD line: the 12-point EMA minus the 26-point EMA (its signal line is
// ema(values("macd"), 9), and macd - signal the histogram).
fn macd(v) {
    let fast = ema(v, 12);
    let slow = ema(v, 26);
    let out = [];
    for i in 0..v.len() {
        out.push(fast[i] - slow[i]);
    }
    out
}
