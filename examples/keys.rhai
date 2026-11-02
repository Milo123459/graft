// Key bindings and helpers for the interactive viewer.
//
//   graft examples/reclaimable.log --script examples/keys.rhai
//
// Functions defined here can also be called from the `:` console.

// Average change per day of a series, formatted in its unit.
fn daily(name) {
    fmt(name, slope(name, "1d")) + "/day"
}

// One-line trend summary of every series (for the visible range / selection).
fn summary() {
    let parts = [];
    for s in series() {
        let eta = eta(s, 0);
        let when = if eta == () { "" } else { ", empty in " + fmt_dur(eta) };
        parts.push(s + " " + daily(s) + when);
    }
    let out = "";
    for p in parts { if out != "" { out += " · "; } out += p; }
    out
}

bind("D", summary());                       // trend summary
bind("W", hline(50GB, "50 GB"));           // add a warning line
bind("X", clear_hlines());
bind("Z", zoom("12h"));                    // last 12 hours
