# graft

A Rust CLI/TUI that graphs any data in the terminal (ratatui, clap, Rhai).

## Checks

Run before every commit: `cargo fmt`, `cargo clippy --all-targets` (no warnings),
`cargo test`.

## Conventions

- The viewer is used with a mouse, a keyboard and from a phone over ssh: everything
  must work at 46 columns, by tapping as well as by keys.
- Every toolbar button has a console command (test: `every_button_has_a_console_command`).
- **Flags, the console and the UI can all do the same things.** Anything you can do
  with a command-line flag must also be possible from the console and in the UI (a
  button, menu item or key, so it works by tapping on a phone too); anything you can
  do in the UI or the console must also be a flag (when it's a setting) and the other
  one. Changing a setting in the viewer applies it then and there (re-reading or
  re-fetching what it needs), and "copy cmd" gives the flags for it. Add all three in
  the same change.
- **Keep the tutorial up to date.** Whenever you add or change something a user can
  see or do (a flag, key, button, menu, console function, chart type, source or
  parser), update `src/tutorial.rs` in the same change: keys and buttons go in the
  keys table of the step where the feature fits, flags and console commands in its
  examples, and a bigger feature gets a step of its own (with a `Check` if the user
  can try it on the demo chart). Explain how it works, not just that it exists.
  `the_tutorial_covers_every_feature`, `every_tutorial_example_works` and
  `the_tutorial_walks_through_everything` enforce this — don't silence them by
  adding to their exemption lists unless the feature really doesn't belong in a
  tour (e.g. `--help`).
- Update README.md alongside the tutorial.
