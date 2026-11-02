// A custom parser for formats a regex can't express comfortably.
//
//   graft examples/reclaimable.log --script examples/parse.rhai --parse-fn
//
// `parse(line)` returns a map of series name -> value (numbers, or strings with
// units such as "59.9 GB"), plus an optional `time` (or numeric `x`). Return ()
// to skip the line.
fn parse(line) {
    let m = #{};
    for field in line.split("  ") {
        let kv = field.split(": ");
        if kv.len() == 2 {
            m[kv[0]] = kv[1];
        } else if field.contains("-") {
            m.time = field;
        }
    }
    if m.len() < 2 { () } else { m }
}
