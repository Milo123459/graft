// Indicators written in Rhai, for custom lines:
//   graft … --script examples/indicators.rhai --line 'ema30 = my_ema(values("ETH"), 30)'
// (graft also has ema(), sma(), rsi() and macd() built in; these show how to write
// your own.)

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
