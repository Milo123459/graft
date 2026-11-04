//! The builder: set up a chart from scratch in the terminal. Pick a source, get
//! format suggestions for a sample of it, tweak the format with a live per-line
//! preview, choose series, chart and forecast options, then start the viewer.
//! Everything is tappable for phone use; Tab / Enter / Esc work from a keyboard.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ratatui::Frame;
use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::cli::{Cli, FitModel};
use crate::interactive::{Live, Transition};
use crate::script::ScriptHost;
use crate::stats::Targets;
use crate::suggest::{self, Kind, Preview, Suggestion};
use crate::tui::App;
use crate::ui::{Button, Hits, TextInput, flow_height, render_buttons};

const SAMPLE_LINES: usize = 500;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Source,
    Every,
    Format,
    GroupBy,
    TimeFormat,
    Targets,
    SaveName,
    MergeName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Src {
    File,
    Command,
    /// A WebSocket or server-sent events.
    Stream,
    Stdin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parser {
    /// Nothing chosen yet (a format is required to start).
    Unset,
    Auto,
    Template,
    Regex,
    Kv,
    Columns,
    Csv,
    Json,
    /// Count lines matching patterns (--grep).
    Grep,
}

impl Parser {
    const ALL: [Parser; 8] = [
        Parser::Template,
        Parser::Regex,
        Parser::Kv,
        Parser::Columns,
        Parser::Csv,
        Parser::Json,
        Parser::Grep,
        Parser::Auto,
    ];

    fn name(self) -> &'static str {
        match self {
            Parser::Unset => "?",
            Parser::Auto => "auto-detect",
            Parser::Template => "template",
            Parser::Regex => "regex",
            Parser::Kv => "key: value",
            Parser::Columns => "columns",
            Parser::Csv => "csv",
            Parser::Json => "json",
            Parser::Grep => "count lines",
        }
    }

    fn takes_text(self) -> bool {
        matches!(
            self,
            Parser::Template | Parser::Regex | Parser::Columns | Parser::Grep
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opt {
    Kind,
    Split,
    Stats,
    Forecast,
    Log,
    Binary,
    Follow,
    Fit,
    Trend,
    SideBySide,
    Range,
}

#[derive(Debug, Clone, PartialEq)]
enum Hit {
    /// Gallery: start item `n` / open it in the builder.
    StartItem(usize),
    EditItem(usize),
    /// Remove a chart from the recent list.
    ForgetRecent(usize),
    NewSource(Src),
    Field(Field),
    Src(Src),
    Load,
    Suggestion(usize),
    Parser(Parser),
    Series(String),
    AllSeries,
    Opt(Opt),
    Start,
    Save,
    Back,
    Preview,
}

pub struct Builder {
    cli: Cli,
    src: Src,
    source: TextInput,
    every: TextInput,
    parser: Parser,
    format: TextInput,
    group_by: TextInput,
    time_format: TextInput,
    targets: TextInput,
    save_name: Option<TextInput>,
    /// Editing one of a chart's merged sources (or adding one): its name, and the name
    /// it had (`None`: a new one).
    merge_name: Option<TextInput>,
    merge_old: Option<String>,
    /// …and it's a pane's own source: that pane.
    merge_pane: Option<usize>,
    focus: Option<Field>,
    sample: Vec<String>,
    /// When each sample line arrived, if it came from a running viewer (else empty).
    sample_meta: Vec<crate::data::LineMeta>,
    from_command: bool,
    loading: Option<Receiver<Result<Vec<String>, String>>>,
    /// A stream keeps sending after its sample: the newer messages, in batches.
    more: Option<Receiver<Vec<String>>>,
    loaded_at: Option<Instant>,
    suggestions: Vec<Suggestion>,
    preview: Preview,
    /// `None` shows every series.
    selected: Option<BTreeSet<String>>,
    scroll: usize,
    message: Option<(String, bool)>,
    hits: Hits<Hit>,
    /// The viewer this builder was opened from (returned to on "back").
    previous: Option<Box<App>>,
    /// Piped stdin, when the builder was started with data on stdin.
    live: Option<Live>,
    script: Rc<ScriptHost>,
    /// The quick-start screen (shown when there's no source yet).
    gallery: Option<Gallery>,
    /// Came from the gallery: "back" returns there.
    from_gallery: bool,
}

/// A ready-made chart (built-in preset or saved profile).
#[derive(Debug, Clone)]
struct Item {
    title: String,
    description: String,
    args: Vec<String>,
    kind: ItemKind,
    /// Where a recent command ran.
    cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ItemKind {
    /// Recently viewed: the number in the recent list.
    Recent(usize),
    Preset,
    Saved,
}

/// Recent charts shown on the start screen (all of them: `graft --list-profiles`).
const RECENT_SHOWN: usize = 5;

#[derive(Debug, Clone)]
struct Gallery {
    items: Vec<Item>,
    selected: usize,
}

impl Gallery {
    fn new() -> Self {
        let now = chrono::Local::now().timestamp();
        let mut items: Vec<Item> = crate::recent::list()
            .into_iter()
            .take(RECENT_SHOWN)
            .enumerate()
            .map(|(i, r)| Item {
                title: r.title(),
                description: r.summary(now),
                kind: ItemKind::Recent(i),
                cwd: r.cwd.clone(),
                args: r.args,
            })
            .collect();
        // New to graft? First in the quick start list.
        items.push(Item {
            title: "Tutorial".into(),
            description: "a guided tour of everything graft does, on a live demo chart".into(),
            args: vec!["--tutorial".into()],
            kind: ItemKind::Preset,
            cwd: None,
        });
        items.extend(
            crate::profile::presets()
                .into_iter()
                .filter(|p| p.available())
                .map(|p| Item {
                    title: p.title.into(),
                    description: p.description.into(),
                    args: p.args.iter().map(|s| s.to_string()).collect(),
                    kind: ItemKind::Preset,
                    cwd: None,
                }),
        );
        items.extend(crate::profile::saved().into_iter().map(|(name, p)| Item {
            description: p.description.clone().unwrap_or_else(|| {
                p.args
                    .iter()
                    .map(|a| crate::cli::shell_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
            title: name,
            args: p.args,
            kind: ItemKind::Saved,
            cwd: None,
        }));
        Self { items, selected: 0 }
    }
}

impl Builder {
    pub fn new(cli: Cli, live: Option<Live>, previous: Option<Box<App>>) -> Self {
        let src = if cli.cmd.is_some() {
            Src::Command
        } else if cli.stream.is_some() {
            Src::Stream
        } else if live.is_some() || cli.input.as_ref().is_some_and(|p| p.as_os_str() == "-") {
            Src::Stdin
        } else {
            Src::File
        };
        let source = match src {
            Src::Command => cli.cmd.clone().unwrap_or_default(),
            Src::Stream => cli.stream.clone().unwrap_or_default(),
            Src::File => cli
                .input
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            Src::Stdin => String::new(),
        };
        let (parser, text) = if let Some(t) = &cli.format {
            (Parser::Template, t.clone())
        } else if let Some(r) = &cli.regex {
            (Parser::Regex, r.clone())
        } else if let Some(c) = &cli.columns {
            (Parser::Columns, c.clone())
        } else if cli.kv {
            (Parser::Kv, String::new())
        } else if cli.csv {
            (Parser::Csv, String::new())
        } else if cli.json {
            (Parser::Json, String::new())
        } else if !cli.grep.is_empty() {
            (Parser::Grep, cli.grep.join("; "))
        } else if cli.auto {
            (Parser::Auto, String::new())
        } else {
            (Parser::Unset, String::new())
        };
        let selected = (!cli.select.is_empty()).then(|| cli.select.iter().cloned().collect());
        let script = Rc::new(
            ScriptHost::new(cli.script.as_deref(), cli.binary_units).unwrap_or_else(|_| {
                ScriptHost::new(None, cli.binary_units).expect("plain script host")
            }),
        );
        let mut b = Self {
            src,
            source: TextInput::new(source),
            every: TextInput::new(crate::time::fmt_duration_compact(cli.every.as_secs_f64())),
            parser,
            format: TextInput::new(text),
            group_by: TextInput::new(cli.group_by.clone().unwrap_or_default()),
            time_format: TextInput::new(cli.time_format.clone().unwrap_or_default()),
            targets: TextInput::new(cli.target.join(",")),
            save_name: None,
            merge_name: None,
            merge_old: None,
            merge_pane: None,
            focus: None,
            sample: Vec::new(),
            sample_meta: Vec::new(),
            from_command: false,
            loading: None,
            more: None,
            loaded_at: None,
            suggestions: Vec::new(),
            preview: Preview::default(),
            selected,
            scroll: 0,
            message: None,
            hits: Hits::default(),
            previous,
            live,
            script,
            cli,
            gallery: None,
            from_gallery: false,
        };
        if b.source.value.is_empty()
            && b.src != Src::Stdin
            && b.previous.is_none()
            && !b.cli.has_parser()
        {
            // Nothing to go on yet: start with the quick-start gallery.
            b.gallery = Some(Gallery::new());
        } else if b.source.value.is_empty() && b.src != Src::Stdin {
            b.focus = Some(Field::Source);
            b.message = Some((
                "type a file path, a command or a stream URL, then tap load (or press Enter)"
                    .into(),
                false,
            ));
        } else {
            b.load();
        }
        b
    }

    pub fn error(&mut self, msg: String) {
        self.message = Some((msg, true));
    }

    /// Edit a merged source of `chart` (`name` it's merged as, `cli` its arguments),
    /// or make a new one: start puts it back into the chart.
    /// `pane`: it's that pane's own source (it goes back to the pane, not merged in).
    pub fn for_source(name: String, cli: Cli, chart: Box<App>, pane: Option<usize>) -> Self {
        let new = pane.is_none()
            && !chart
                .current_cli()
                .merge
                .iter()
                .any(|s| crate::sources::Merge::parse(s).is_ok_and(|m| m.name == name));
        // What it has read already: the preview shows those lines (each run at its own
        // time) instead of running the command once.
        let history = chart.source_history(&name);
        let mut b = Builder::new(cli, None, Some(chart));
        if !history.is_empty() {
            let kept = &history[history.len().saturating_sub(SAMPLE_LINES)..];
            b.loading = None;
            b.message = None;
            b.sample = kept.iter().map(|r| r.0.clone()).collect();
            b.sample_meta = kept.iter().map(|r| r.1).collect();
            b.from_command = b.src == Src::Command;
            b.sample_loaded();
        }
        b.merge_pane = pane;
        b.merge_old = (!new).then(|| name.clone());
        b.merge_name = Some(TextInput::new(name));
        if new {
            b.src = Src::Command;
            b.focus = Some(Field::Source);
        }
        b
    }

    pub fn take_previous(&mut self) -> Option<Box<App>> {
        self.previous.take()
    }

    pub fn previous_cli(&self) -> Option<Cli> {
        self.previous.as_ref().map(|a| a.current_cli())
    }

    fn input(&mut self, f: Field) -> &mut TextInput {
        match f {
            Field::Source => &mut self.source,
            Field::Every => &mut self.every,
            Field::Format => &mut self.format,
            Field::GroupBy => &mut self.group_by,
            Field::TimeFormat => &mut self.time_format,
            Field::Targets => &mut self.targets,
            Field::SaveName => self.save_name.get_or_insert_with(TextInput::default),
            Field::MergeName => self.merge_name.get_or_insert_with(TextInput::default),
        }
    }

    fn fields(&self) -> Vec<Field> {
        let mut v = Vec::new();
        if self.merge_name.is_some() {
            v.push(Field::MergeName);
        }
        v.push(Field::Source);
        if self.src == Src::Command {
            v.push(Field::Every);
        }
        if self.parser.takes_text() {
            v.push(Field::Format);
        }
        if matches!(self.parser, Parser::Template | Parser::Regex) {
            v.extend([Field::GroupBy, Field::TimeFormat]);
        }
        v.push(Field::Targets);
        v
    }

    // ───────────── sample & suggestions ─────────────

    fn load(&mut self) {
        self.message = None;
        match self.src {
            Src::File => {
                let path = self.source.value.trim().to_string();
                if path.is_empty() {
                    return self.error("enter a file path".into());
                }
                // A file on another machine: sample it over ssh.
                if self.cli.ssh.is_some() {
                    self.more = None;
                    self.loading = Some(run_sample_command(
                        format!("head -n {SAMPLE_LINES} {}", crate::cli::shell_quote(&path)),
                        self.cli.ssh.clone(),
                    ));
                    self.message = Some(("reading the file over ssh…".into(), false));
                    return;
                }
                match std::fs::File::open(&path) {
                    Ok(f) => {
                        self.sample = BufReader::new(f)
                            .lines()
                            .map_while(std::result::Result::ok)
                            .take(SAMPLE_LINES)
                            .collect();
                        self.sample_meta.clear();
                        self.from_command = false;
                        self.sample_loaded();
                    }
                    Err(e) => self.error(format!("{path}: {e}")),
                }
            }
            Src::Command => {
                let cmd = self.source.value.trim().to_string();
                if cmd.is_empty() {
                    return self.error("enter a command, e.g. cat /proc/meminfo".into());
                }
                // A running viewer already has this command's output.
                if let Some(app) = &self.previous
                    && app.live().keeps_history()
                    && !app.live().raw.is_empty()
                    && matches!(&app.live().spec, crate::source::SourceSpec::Command { cmd: c, .. } if *c == cmd)
                {
                    // With when each line came in: each run has its own time.
                    let raw = &app.live().raw;
                    let kept = &raw[raw.len().saturating_sub(SAMPLE_LINES)..];
                    self.sample = kept.iter().map(|r| r.0.clone()).collect();
                    self.sample_meta = kept.iter().map(|r| r.1).collect();
                    self.from_command = true;
                    return self.sample_loaded();
                }
                self.more = None;
                self.loading = Some(run_sample_command(cmd, self.cli.ssh.clone()));
                self.message = Some(("running the command for a sample…".into(), false));
            }
            Src::Stream => {
                let url = self.source.value.trim().to_string();
                if url.is_empty() {
                    return self.error(
                        "enter a stream's URL: wss://… (a WebSocket) or https://… (server-sent events)"
                            .into(),
                    );
                }
                let params = crate::params::parse_all(&self.cli.param);
                let fill = |t: &str| {
                    crate::params::fill_times(
                        &crate::params::fill(t, &params),
                        crate::time::now_secs(),
                    )
                };
                let spec = crate::source::SourceSpec::Stream {
                    url: fill(&url),
                    send: self.cli.send.iter().map(|m| fill(m)).collect(),
                };
                let (tx, rx) = mpsc::channel();
                let (more_tx, more_rx) = mpsc::channel();
                thread::spawn(move || {
                    let events = crate::source::spawn(spec);
                    let lines = crate::first_messages(&events);
                    let empty = lines.is_empty();
                    let _ = tx.send(if empty {
                        Err("the stream sent nothing in 8 seconds (or couldn't connect)".into())
                    } else {
                        Ok(lines)
                    });
                    if empty {
                        return;
                    }
                    // Then what it sends next, a batch every half second, until the
                    // builder stops listening (another source, or it's closed).
                    loop {
                        let until = Instant::now() + Duration::from_millis(500);
                        let mut batch = Vec::new();
                        while let Some(left) = until.checked_duration_since(Instant::now()) {
                            match events.recv_timeout(left) {
                                Ok(crate::source::Event::Line(l, _)) => batch.push(l),
                                Ok(crate::source::Event::Eof) => return,
                                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                            }
                        }
                        if !batch.is_empty() && more_tx.send(batch).is_err() {
                            return;
                        }
                    }
                });
                self.loading = Some(rx);
                self.more = Some(more_rx);
                self.message = Some(("listening to the stream for a sample…".into(), false));
            }
            Src::Stdin => {
                let raw = match (&self.live, &self.previous) {
                    (Some(l), _) => &l.raw,
                    (None, Some(app)) => &app.live().raw,
                    _ => {
                        return self.error(
                            "no data on stdin: pipe something into graft, or pick file/command"
                                .into(),
                        );
                    }
                };
                self.sample = raw.iter().take(SAMPLE_LINES).map(|r| r.0.clone()).collect();
                self.sample_meta = raw.iter().take(SAMPLE_LINES).map(|r| r.1).collect();
                self.from_command = false;
                self.sample_loaded();
            }
        }
    }

    fn sample_loaded(&mut self) {
        self.loaded_at = Some(Instant::now());
        self.scroll = 0;
        self.suggestions = suggest::suggest(
            &self.sample,
            &self.config_base(),
            self.from_command,
            &self.script,
        );
        // Nothing is chosen for you: the suggestions wait for a tap.
        if self.sample.is_empty() {
            self.error("the source produced no lines".into());
        } else if self.parser == Parser::Unset && !self.suggestions.is_empty() {
            self.message = Some((
                "pick a format: tap a suggestion (or ↑/↓), or choose a parser and write one".into(),
                false,
            ));
        }
        self.refresh();
    }

    /// The configuration without a parser, for generating suggestions.
    fn config_base(&self) -> Cli {
        let mut c = self.cli.clone();
        c.clear_parser();
        c
    }

    fn use_suggestion(&mut self, s: &Suggestion) {
        if self.message.as_ref().is_some_and(|m| !m.1) {
            self.message = None;
        }
        let c = &s.cli;
        let (parser, text) = match s.kind {
            Kind::Template => (Parser::Template, c.format.clone().unwrap_or_default()),
            Kind::Kv => (Parser::Kv, String::new()),
            Kind::Columns => (Parser::Columns, c.columns.clone().unwrap_or_default()),
            Kind::Csv => (Parser::Csv, String::new()),
            Kind::Json => (Parser::Json, String::new()),
        };
        self.parser = parser;
        self.format.set(text);
        self.cli.delimiter = c.delimiter;
        self.group_by.set("");
        self.selected = None;
    }

    /// Fold the form into `self.cli`; errors are shown but don't block previewing.
    fn apply(&mut self) -> Result<()> {
        let c = &mut self.cli;
        let source = self.source.value.trim().to_string();
        match self.src {
            Src::File => {
                c.cmd = None;
                c.stream = None;
                c.input = (!source.is_empty()).then(|| source.into());
            }
            Src::Stream => {
                c.cmd = None;
                c.input = None;
                c.stream = (!source.is_empty()).then_some(source);
            }
            Src::Command => {
                c.input = None;
                c.stream = None;
                c.cmd = (!source.is_empty()).then_some(source);
                c.every = crate::time::parse_duration(self.every.value.trim())
                    .map_err(|e| anyhow::anyhow!("every: {e}"))?;
            }
            Src::Stdin => {
                c.cmd = None;
                c.stream = None;
                c.input = Some("-".into());
            }
        }
        let delimiter = c.delimiter;
        c.clear_parser();
        let text = self.format.value.clone();
        match self.parser {
            Parser::Unset => {}
            Parser::Auto => c.auto = true,
            Parser::Template => c.format = Some(text),
            Parser::Regex => c.regex = Some(text),
            Parser::Kv => c.kv = true,
            Parser::Columns => {
                c.columns = Some(if text.trim().is_empty() {
                    "all".into()
                } else {
                    text
                })
            }
            Parser::Csv => c.csv = true,
            Parser::Json => c.json = true,
            Parser::Grep => {
                c.grep = text
                    .split(';')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(String::from)
                    .collect();
            }
        }
        c.delimiter = if matches!(self.parser, Parser::Csv | Parser::Columns) {
            delimiter
        } else {
            None
        };
        let nonempty =
            |t: &TextInput| (!t.value.trim().is_empty()).then(|| t.value.trim().to_string());
        c.group_by = if matches!(self.parser, Parser::Template | Parser::Regex) {
            nonempty(&self.group_by)
        } else {
            None
        };
        c.time_format = nonempty(&self.time_format);
        c.target = self
            .targets
            .value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        c.select = self
            .selected
            .as_ref()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        Targets::parse(&c.target, c.binary_units).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    fn refresh(&mut self) {
        match self.apply() {
            Ok(()) => {
                if self.message.as_ref().is_some_and(|m| m.1) {
                    self.message = None;
                }
            }
            Err(e) => self.error(format!("{e:#}")),
        }
        self.preview = if self.parser == Parser::Unset {
            // No format, no parsing — just show the raw sample.
            Preview {
                lines: self
                    .sample
                    .iter()
                    .take(300)
                    .map(|l| suggest::LineResult {
                        raw: l.clone(),
                        result: Err("no format yet".into()),
                    })
                    .collect(),
                total: self.sample.len(),
                ..Preview::default()
            }
        } else {
            suggest::preview(
                &self.cli,
                &self.sample,
                &self.sample_meta,
                self.from_command,
                &self.script,
                300,
            )
        };
    }

    /// Validate and build the viewer.
    pub fn launch(&mut self) -> Result<App> {
        if self.cli.tutorial {
            let (setup, live) = crate::tutorial::start()?;
            return Ok(App::new(setup, live));
        }
        if self.merge_name.is_some() {
            return self.launch_merged();
        }
        self.apply()?;
        if !self.cli.has_source() {
            bail!("choose a source first");
        }
        if self.parser == Parser::Unset {
            bail!("pick a format first: tap a suggestion, or choose a parser and write one");
        }
        if let Some(e) = &self.preview.error {
            bail!("{e}");
        }
        let spec = crate::setup::source_spec(&self.cli, true)?;
        let setup = crate::setup::build(self.cli.clone(), spec.clone(), false)?;
        // Keep a running source (and its history) when it's unchanged.
        let reuse = |l: &Live| l.spec == spec && l.keeps_history();
        let live = match (self.previous.take(), self.live.take()) {
            (Some(app), _) if reuse(app.live()) => app.into_live(),
            (_, Some(l)) if reuse(&l) => l,
            _ if matches!(spec, crate::source::SourceSpec::Stdin) => {
                bail!("stdin was already consumed; pick a file or command")
            }
            _ => Live::new(spec),
        };
        Ok(App::new(setup, live))
    }

    /// A merged source: back into the chart it came from.
    fn launch_merged(&mut self) -> Result<App> {
        let name = self
            .merge_name
            .as_ref()
            .map_or(String::new(), |n| n.value.trim().to_string());
        if name.is_empty() || name.contains(|c: char| c.is_whitespace() || c == '=') {
            bail!("give the source a one-word name (its series are named after it)");
        }
        self.apply()?;
        if !self.cli.has_source() {
            bail!("choose a file, command or stream for the source");
        }
        if self.parser == Parser::Unset {
            bail!("pick a format first: tap a suggestion, or choose a parser and write one");
        }
        if let Some(e) = &self.preview.error {
            bail!("{e}");
        }
        let args: Vec<String> = self
            .cli
            .to_args()
            .iter()
            .map(|a| crate::cli::shell_quote(a))
            .collect();
        let spec = format!("{name} = {}", args.join(" "));
        let mut app = self
            .previous
            .take()
            .ok_or_else(|| anyhow::anyhow!("there's no chart to add the source to"))?;
        let done = match self.merge_pane {
            Some(id) => app.set_pane_source_def(id, self.merge_old.as_deref(), spec),
            None => app.set_merged_source(self.merge_old.as_deref(), spec),
        };
        if let Err(e) = done {
            self.previous = Some(app);
            return Err(e);
        }
        Ok(*app)
    }

    pub fn tick(&mut self) {
        // A stream's newer messages: the sample keeps up (the newest SAMPLE_LINES), and
        // so does the preview.
        if self.loading.is_none()
            && let Some(rx) = &self.more
        {
            let mut new = Vec::new();
            loop {
                match rx.try_recv() {
                    Ok(batch) => new.extend(batch),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.more = None;
                        break;
                    }
                }
            }
            if !new.is_empty() && self.src == Src::Stream {
                self.sample.extend(new);
                let over = self.sample.len().saturating_sub(SAMPLE_LINES);
                self.sample.drain(..over);
                self.sample_meta.clear();
                self.refresh();
            }
        }
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(Ok(lines)) => {
                    self.loading = None;
                    self.sample = lines;
                    self.sample_meta.clear();
                    self.from_command = true;
                    self.message = None;
                    self.sample_loaded();
                }
                Ok(Err(e)) => {
                    self.loading = None;
                    self.error(e);
                }
                Err(TryRecvError::Disconnected) => self.loading = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        // Keep streams flowing while the builder is open.
        if let Some(app) = &mut self.previous {
            app.tick();
        }
        if let Some(live) = &mut self.live {
            let before = live.raw.len();
            live.pump(Duration::from_millis(20), &mut |_, _| {});
            // Piped data arriving: refresh the sample while it's still small.
            if self.src == Src::Stdin && live.raw.len() != before && before < SAMPLE_LINES {
                self.load();
            }
        }
    }

    // ───────────── events ─────────────

    pub fn on_event(&mut self, ev: &TermEvent) -> Transition {
        if self.gallery.is_some() {
            return self.on_gallery_event(ev);
        }
        match ev {
            TermEvent::Key(k) if k.kind == KeyEventKind::Press => self.on_key(*k),
            TermEvent::Mouse(m) => match m.kind {
                MouseEventKind::Down(MouseButton::Left) => self.on_click(m.column, m.row),
                MouseEventKind::ScrollDown => {
                    self.scroll = (self.scroll + 2).min(self.preview.lines.len().saturating_sub(1));
                    Transition::None
                }
                MouseEventKind::ScrollUp => {
                    self.scroll = self.scroll.saturating_sub(2);
                    Transition::None
                }
                _ => Transition::None,
            },
            _ => Transition::None,
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Transition {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Transition::Quit;
        }
        if self.save_name.is_some() {
            match key.code {
                KeyCode::Esc => self.save_name = None,
                KeyCode::Enter => self.save(),
                _ => {
                    self.input(Field::SaveName).key(key);
                }
            }
            return Transition::None;
        }
        match key.code {
            KeyCode::Esc if self.focus.is_some() => self.focus = None,
            KeyCode::Esc if self.from_gallery => {
                self.gallery = Some(Gallery::new());
                self.message = None;
            }
            // Back to the chart it came from; Esc never quits (q does, from the gallery).
            KeyCode::Esc if self.previous.is_some() => return Transition::Back,
            KeyCode::Esc => {
                self.gallery = Some(Gallery::new());
                self.message = None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let fields = self.fields();
                let i = self.focus.and_then(|f| fields.iter().position(|x| *x == f));
                let n = fields.len();
                self.focus = Some(match (i, key.code) {
                    (None, KeyCode::Tab) => fields[0],
                    (None, _) => fields[n - 1],
                    (Some(i), KeyCode::Tab) => fields[(i + 1) % n],
                    (Some(i), _) => fields[(i + n - 1) % n],
                });
            }
            KeyCode::Enter if matches!(self.focus, Some(Field::Source | Field::Every)) => {
                self.refresh();
                self.load();
            }
            KeyCode::Enter => return Transition::Start,
            KeyCode::Char('s') if ctrl => self.save_name = Some(TextInput::new(String::new())),
            KeyCode::Char('r') if ctrl => self.load(),
            KeyCode::Up | KeyCode::Down if !self.suggestions.is_empty() => {
                let cur = self.current_suggestion();
                let n = self.suggestions.len();
                let next = match (cur, key.code) {
                    (None, _) => 0,
                    (Some(i), KeyCode::Down) => (i + 1) % n,
                    (Some(i), _) => (i + n - 1) % n,
                };
                let s = self.suggestions[next].clone();
                self.use_suggestion(&s);
                self.refresh();
            }
            KeyCode::PageDown => {
                self.scroll = (self.scroll + 5).min(self.preview.lines.len().saturating_sub(1))
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(5),
            _ => {
                if let Some(f) = self.focus
                    && self.input(f).key(key)
                    && f != Field::Source
                {
                    self.refresh();
                }
            }
        }
        Transition::None
    }

    fn current_suggestion(&self) -> Option<usize> {
        self.suggestions.iter().position(|s| {
            let c = &s.cli;
            c.format == self.cli.format
                && c.kv == self.cli.kv
                && c.csv == self.cli.csv
                && c.json == self.cli.json
                && c.columns == self.cli.columns
        })
    }

    fn on_click(&mut self, col: u16, row: u16) -> Transition {
        let Some((rect, hit)) = self.hits.at(col, row) else {
            self.focus = None;
            return Transition::None;
        };
        match hit {
            Hit::Field(f) => {
                self.focus = Some(f);
                self.input(f).click(rect, col);
            }
            Hit::Src(s) => {
                if s != self.src {
                    self.src = s;
                    self.source.set(match s {
                        Src::Command => self.cli.cmd.clone().unwrap_or_default(),
                        Src::Stream => self.cli.stream.clone().unwrap_or_default(),
                        Src::File => self
                            .cli
                            .input
                            .as_ref()
                            .filter(|p| p.as_os_str() != "-")
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        Src::Stdin => String::new(),
                    });
                    self.focus = (s != Src::Stdin).then_some(Field::Source);
                    if s == Src::Stdin || !self.source.value.is_empty() {
                        self.load();
                    }
                }
            }
            Hit::Load => {
                self.refresh();
                self.load();
            }
            Hit::Suggestion(i) => {
                if let Some(s) = self.suggestions.get(i).cloned() {
                    self.use_suggestion(&s);
                    self.refresh();
                }
            }
            Hit::Parser(p) => {
                self.parser = p;
                if self.message.as_ref().is_some_and(|m| !m.1) {
                    self.message = None;
                }
                if p == Parser::Template && self.format.value.is_empty()
                    || p != Parser::Template && self.cli.format.is_some()
                {
                    let t = if p == Parser::Template {
                        suggest::template_from_lines(&self.sample).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    self.format.set(t);
                }
                if p.takes_text() {
                    self.focus = Some(Field::Format);
                }
                self.refresh();
            }
            Hit::Series(name) => {
                let all: BTreeSet<String> =
                    self.preview.series.iter().map(|s| s.0.clone()).collect();
                let set = self.selected.get_or_insert(all.clone());
                if !set.remove(&name) {
                    set.insert(name);
                }
                if *set == all {
                    self.selected = None;
                }
                self.refresh();
            }
            Hit::AllSeries => {
                self.selected = None;
                self.refresh();
            }
            Hit::Opt(o) => {
                let c = &mut self.cli;
                match o {
                    Opt::Kind => c.kind = c.kind.next(),
                    Opt::Split => c.split = !c.split,
                    Opt::Stats => c.stats = !c.stats,
                    Opt::Forecast => {
                        c.forecast = if c.forecast.is_some() {
                            None
                        } else {
                            Some("auto".into())
                        }
                    }
                    Opt::Log => c.log = !c.log,
                    Opt::Binary => c.binary_units = !c.binary_units,
                    Opt::Follow => c.follow = !c.follow,
                    Opt::Fit => {
                        c.fit = if c.fit == FitModel::Linear {
                            FitModel::Exp
                        } else {
                            FitModel::Linear
                        }
                    }
                    Opt::Trend => {
                        // off → avg → avg + trend line → median + p95 → trend line → off
                        use crate::cli::TrendStyle::{Avg, Median, P95};
                        let cycle: [&[crate::cli::TrendStyle]; 4] =
                            [&[], &[Avg], &[Median], &[Median, P95]];
                        let i = cycle
                            .iter()
                            .position(|set| *set == c.trend.as_slice())
                            .unwrap_or(0);
                        c.trend = cycle[(i + 1) % cycle.len()].to_vec();
                    }
                    Opt::SideBySide => c.side_by_side = !c.side_by_side,
                    Opt::Range => {
                        // all → 1h → 6h → 24h → 7d → 30d → all
                        const STEPS: [f64; 5] = [3600.0, 21600.0, 86400.0, 604_800.0, 2_592_000.0];
                        c.since = None;
                        c.until = None;
                        c.last = match c.last {
                            None => Some(crate::cli::Window::Span(STEPS[0])),
                            Some(crate::cli::Window::Span(d)) => STEPS
                                .iter()
                                .find(|s| **s > d)
                                .map(|s| crate::cli::Window::Span(*s)),
                            Some(crate::cli::Window::Points(_)) => None,
                        };
                    }
                }
                self.refresh();
            }
            Hit::Start => return Transition::Start,
            Hit::Save => {
                if self.save_name.is_some() {
                    self.save();
                } else {
                    self.save_name = Some(TextInput::default());
                }
            }
            Hit::Back if self.from_gallery => {
                self.gallery = Some(Gallery::new());
                self.message = None;
            }
            Hit::Back => return Transition::Back,
            Hit::StartItem(_) | Hit::EditItem(_) | Hit::ForgetRecent(_) | Hit::NewSource(_) => {}
            Hit::Preview => self.focus = None,
        }
        Transition::None
    }

    fn save(&mut self) {
        let name = self
            .save_name
            .as_ref()
            .map(|t| t.value.trim().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            return self.error("type a profile name".into());
        }
        let args: Vec<std::ffi::OsString> = std::iter::once("graft".into())
            .chain(self.cli.to_args().into_iter().map(Into::into))
            .collect();
        match crate::profile::save(&name, &args) {
            Ok(path) => {
                self.save_name = None;
                self.message = Some((
                    format!(
                        "saved profile '{name}' to {} — run it with: graft -p {name}",
                        path.display()
                    ),
                    false,
                ));
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    // ───────────── drawing ─────────────

    fn src_buttons(&self) -> Vec<Button<Hit>> {
        let mut v = vec![
            Button::new("file", Hit::Src(Src::File)).active(self.src == Src::File),
            Button::new("command", Hit::Src(Src::Command)).active(self.src == Src::Command),
            Button::new("stream", Hit::Src(Src::Stream)).active(self.src == Src::Stream),
        ];
        if self.live.is_some()
            || self
                .previous
                .as_ref()
                .is_some_and(|a| matches!(a.live().spec, crate::source::SourceSpec::Stdin))
        {
            v.push(Button::new("stdin", Hit::Src(Src::Stdin)).active(self.src == Src::Stdin));
        }
        v.push(Button::new(
            if self.loading.is_some() {
                "loading…"
            } else {
                "load ↻"
            },
            Hit::Load,
        ));
        v
    }

    fn parser_buttons(&self) -> Vec<Button<Hit>> {
        Parser::ALL
            .iter()
            .map(|&p| Button::new(p.name(), Hit::Parser(p)).active(self.parser == p))
            .collect()
    }

    fn series_buttons(&self) -> Vec<Button<Hit>> {
        let mut v: Vec<Button<Hit>> = self
            .preview
            .series
            .iter()
            .map(|(name, n, _)| {
                let on = self.selected.as_ref().is_none_or(|s| s.contains(name));
                Button::new(
                    format!("{} {name} ({n})", if on { "●" } else { "○" }),
                    Hit::Series(name.clone()),
                )
                .active(on)
            })
            .collect();
        if self.selected.is_some() {
            v.push(Button::new("all", Hit::AllSeries));
        }
        v
    }

    fn option_buttons(&self) -> Vec<Button<Hit>> {
        let c = &self.cli;
        let mut v = vec![
            Button::new(format!("chart: {}", c.kind.name()), Hit::Opt(Opt::Kind)),
            Button::new("split", Hit::Opt(Opt::Split)).active(c.split),
            Button::new("stats", Hit::Opt(Opt::Stats)).active(c.stats),
            Button::new("forecast", Hit::Opt(Opt::Forecast)).active(c.forecast.is_some()),
            Button::new(
                format!("fit: {}", crate::cli::fit_name(c.fit)),
                Hit::Opt(Opt::Fit),
            ),
            Button::new(
                if c.trend.is_empty() {
                    "trends: off".to_string()
                } else {
                    format!(
                        "trends: {}",
                        c.trend
                            .iter()
                            .map(|t| t.label())
                            .collect::<Vec<_>>()
                            .join(" + ")
                    )
                },
                Hit::Opt(Opt::Trend),
            )
            .active(!c.trend.is_empty()),
            Button::new("side by side", Hit::Opt(Opt::SideBySide)).active(c.side_by_side),
            Button::new(
                format!(
                    "range: {}",
                    match c.last {
                        Some(crate::cli::Window::Span(d)) => crate::time::fmt_duration_compact(d),
                        Some(crate::cli::Window::Points(n)) => format!("{n} pts"),
                        None => "all".into(),
                    }
                ),
                Hit::Opt(Opt::Range),
            )
            .active(c.last.is_some()),
            Button::new("log", Hit::Opt(Opt::Log)).active(c.log),
            Button::new("1024 units", Hit::Opt(Opt::Binary)).active(c.binary_units),
        ];
        if self.src == Src::File {
            v.push(Button::new("follow", Hit::Opt(Opt::Follow)).active(c.follow));
        }
        v
    }

    fn action_buttons(&self) -> Vec<Button<Hit>> {
        vec![
            Button::new(
                if self.merge_pane.is_some() {
                    "▶ use in the pane"
                } else if self.merge_name.is_some() {
                    "▶ merge into the chart"
                } else {
                    "▶ start"
                },
                Hit::Start,
            )
            .active(
                self.parser != Parser::Unset
                    && self.preview.error.is_none()
                    && self.preview.matched > 0,
            ),
            Button::new(
                if self.save_name.is_some() {
                    "save ✓"
                } else {
                    "save profile"
                },
                Hit::Save,
            ),
            Button::new(
                if self.previous.is_some() || self.from_gallery {
                    "back"
                } else {
                    "quit"
                },
                Hit::Back,
            ),
        ]
    }

    fn field(&self, f: &mut Frame, area: Rect, label: &str, field: Field, placeholder: &str) {
        let lw = (label.chars().count() as u16 + 1).min(area.width / 3);
        let [l, input] =
            Layout::horizontal([Constraint::Length(lw), Constraint::Min(1)]).areas(area);
        f.render_widget(Span::styled(label, Style::default().fg(Color::Gray)), l);
        let ti = match field {
            Field::Source => &self.source,
            Field::Every => &self.every,
            Field::Format => &self.format,
            Field::GroupBy => &self.group_by,
            Field::TimeFormat => &self.time_format,
            Field::Targets => &self.targets,
            Field::SaveName => self.save_name.as_ref().expect("save name shown"),
            Field::MergeName => self.merge_name.as_ref().expect("merge name shown"),
        };
        ti.render(
            input,
            f.buffer_mut(),
            self.focus == Some(field) || field == Field::SaveName,
            placeholder,
        );
        self.hits.push(input, Hit::Field(field));
    }

    pub fn draw(&self, f: &mut Frame) {
        self.hits.clear();
        if let Some(g) = &self.gallery {
            return self.draw_gallery(f, g);
        }
        let area = f.area();
        let w = area.width;
        let heading = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let (src_b, parser_b, series_b, opt_b, act_b) = (
            self.src_buttons(),
            self.parser_buttons(),
            self.series_buttons(),
            self.option_buttons(),
            self.action_buttons(),
        );
        let n_sugg = self
            .suggestions
            .len()
            .min(if area.height < 30 { 3 } else { 5 }) as u16;
        let wide = w >= 70;
        let pattern_rows = if matches!(self.parser, Parser::Template | Parser::Regex) {
            if wide { 1 } else { 2 }
        } else {
            0
        };
        let command = format!("$ {}", self.cli.command_line());
        let cmd_rows = ((command.chars().count() as u16).div_ceil(w.max(1))).clamp(1, 3);
        let constraints = [
            Constraint::Length(1),                                                  // title
            Constraint::Length(flow_height(&src_b, w)), // source buttons
            Constraint::Length(1 + u16::from(self.src == Src::Command)), // source fields
            Constraint::Length(1 + n_sugg),             // suggestions
            Constraint::Length(flow_height(&parser_b, w)), // parser buttons
            Constraint::Length(u16::from(self.parser.takes_text()) + pattern_rows), // format fields
            Constraint::Length(1 + flow_height(&series_b, w).min(3)), // preview header + series
            Constraint::Min(3),                         // preview lines
            Constraint::Length(flow_height(&opt_b, w) + 1), // options + targets
            Constraint::Length(cmd_rows),               // command line
            Constraint::Length(flow_height(&act_b, w) + u16::from(self.save_name.is_some())),
            Constraint::Length(1), // message
        ];
        let rows = Layout::vertical(constraints).split(area);

        if self.merge_name.is_some() {
            let [t, n] =
                Layout::horizontal([Constraint::Length(30), Constraint::Min(10)]).areas(rows[0]);
            f.render_widget(
                Span::styled(
                    if self.merge_pane.is_some() {
                        " graft · a pane's source "
                    } else {
                        " graft · a source to merge "
                    },
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                t,
            );
            self.field(
                f,
                n,
                "name ",
                Field::MergeName,
                "one word: its series are named after it",
            );
        } else {
            f.render_widget(
                Line::from(vec![
                    Span::styled(
                        " graft · build a chart ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "  tap anything · tab: next field · enter: start · esc: back",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                rows[0],
            );
        }

        // Source.
        render_buttons(&src_b, rows[1], f.buffer_mut(), &self.hits);
        let src_rows =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(rows[2]);
        match self.src {
            Src::File => self.field(f, src_rows[0], "file ", Field::Source, "path/to/file.log"),
            Src::Stream => self.field(
                f,
                src_rows[0],
                "url ",
                Field::Source,
                "wss://… or https://… (server-sent events)",
            ),
            Src::Command => {
                self.field(
                    f,
                    src_rows[0],
                    "command ",
                    Field::Source,
                    "cat /proc/meminfo",
                );
                self.field(f, src_rows[1], "every ", Field::Every, "1s");
            }
            Src::Stdin => {
                let n = self.live.as_ref().map_or(0, |l| l.raw.len());
                f.render_widget(
                    Span::styled(
                        format!("reading piped stdin ({n} lines so far)"),
                        Style::default().fg(Color::Gray),
                    ),
                    src_rows[0],
                );
            }
        }

        // Suggestions.
        let sugg_rows =
            Layout::vertical(vec![Constraint::Length(1); 1 + n_sugg as usize]).split(rows[3]);
        let title = if self.suggestions.is_empty() {
            if self.sample.is_empty() {
                "suggestions appear once a sample is loaded"
            } else {
                "no format found automatically — try a template or regex"
            }
        } else {
            "suggested formats — tap one (↑/↓):"
        };
        f.render_widget(Span::styled(title, heading), sugg_rows[0]);
        let current = self.current_suggestion();
        for (i, s) in self.suggestions.iter().take(n_sugg as usize).enumerate() {
            let r = sugg_rows[i + 1];
            let on = current == Some(i);
            let style = if on {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            f.render_widget(
                Line::from(vec![
                    Span::styled(format!("{} {}", if on { "▶" } else { "▸" }, s.label), style),
                    Span::styled(
                        format!("   {}", s.summary()),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                r,
            );
            self.hits.push(r, Hit::Suggestion(i));
        }

        // Format.
        render_buttons(&parser_b, rows[4], f.buffer_mut(), &self.hits);
        let fmt_rows = Layout::vertical(vec![Constraint::Length(1); 3]).split(rows[5]);
        let mut fr = 0;
        if self.parser.takes_text() {
            let (label, ph) = match self.parser {
                Parser::Template => ("format ", "{time} name: {value}"),
                Parser::Regex => ("regex ", "(?P<time>\\S+) (?P<value>\\d+)"),
                Parser::Grep => ("count ", "errors=ERROR; warnings=WARN  (per minute)"),
                _ => ("columns ", "all  or  1,3"),
            };
            self.field(f, fmt_rows[0], label, Field::Format, ph);
            fr = 1;
        }
        if pattern_rows > 0 {
            if wide {
                let [a, b] =
                    Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                        .areas(fmt_rows[fr]);
                self.field(f, a, "group by ", Field::GroupBy, "(capture)");
                self.field(
                    f,
                    b,
                    " time format ",
                    Field::TimeFormat,
                    "auto, or %d/%m/%Y %H:%M",
                );
            } else {
                self.field(f, fmt_rows[fr], "group by ", Field::GroupBy, "(capture)");
                self.field(
                    f,
                    fmt_rows[fr + 1],
                    "time ",
                    Field::TimeFormat,
                    "auto, or %d/%m/%Y %H:%M",
                );
            }
        }

        // Preview.
        let [ph, chips] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(rows[6]);
        let p = &self.preview;
        let header = match &p.error {
            None if self.parser == Parser::Unset => Line::from(vec![
                Span::styled("sample ", heading),
                Span::styled(
                    format!("{} lines · pick a format to see how they parse", p.total),
                    Style::default().fg(Color::Gray),
                ),
            ]),
            Some(e) => Line::styled(
                format!("format error: {e}"),
                Style::default().fg(Color::LightRed),
            ),
            None => {
                let x = match p.x_kind {
                    Some(crate::data::XKind::Time) => "x: time",
                    Some(crate::data::XKind::Index) => "x: line number",
                    Some(crate::data::XKind::Number) => "x: number",
                    None => "",
                };
                Line::from(vec![
                    Span::styled("preview ", heading),
                    Span::styled(
                        format!(
                            "{}/{} lines parsed · {} series · {x}  (tap series to choose)",
                            p.matched,
                            p.total,
                            p.series.len()
                        ),
                        Style::default().fg(if p.matched == 0 && p.total > 0 {
                            Color::LightRed
                        } else {
                            Color::Gray
                        }),
                    ),
                ])
            }
        };
        f.render_widget(header, ph);
        render_buttons(&series_b, chips, f.buffer_mut(), &self.hits);
        let lines: Vec<Line> = p
            .lines
            .iter()
            .skip(self.scroll)
            .map(|l| match &l.result {
                Ok(summary) => Line::from(vec![
                    Span::styled("✓ ", Style::default().fg(Color::Green)),
                    Span::raw(summary.clone()),
                ]),
                Err(_) if self.parser == Parser::Unset => {
                    Line::styled(l.raw.clone(), Style::default().fg(Color::Gray))
                }
                Err(reason) => Line::from(vec![
                    Span::styled("✗ ", Style::default().fg(Color::Red)),
                    Span::styled(format!("{reason} │ "), Style::default().fg(Color::Yellow)),
                    Span::styled(l.raw.clone(), Style::default().fg(Color::DarkGray)),
                ]),
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Color::Rgb(18, 18, 22))),
            rows[7],
        );
        self.hits.push(rows[7], Hit::Preview);

        // Options and targets.
        let [opts, targets] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(rows[8]);
        render_buttons(&opt_b, opts, f.buffer_mut(), &self.hits);
        self.field(
            f,
            targets,
            "targets ",
            Field::Targets,
            "e.g. node1=50GB,node2=200GB  (default: 0 when falling; none = off)",
        );

        f.render_widget(
            Paragraph::new(command)
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: false }),
            rows[9],
        );

        let [save, acts] = Layout::vertical([
            Constraint::Length(u16::from(self.save_name.is_some())),
            Constraint::Min(1),
        ])
        .areas(rows[10]);
        if self.save_name.is_some() {
            self.field(f, save, "profile name ", Field::SaveName, "e.g. reclaim");
        }
        render_buttons(&act_b, acts, f.buffer_mut(), &self.hits);

        if let Some((m, err)) = &self.message {
            f.render_widget(
                Span::styled(
                    m.clone(),
                    Style::default().fg(if *err { Color::LightRed } else { Color::Cyan }),
                ),
                rows[11],
            );
        }
    }
}

impl Builder {
    /// Load a gallery item; `start` launches it straight away.
    fn open_item(&mut self, idx: usize, start: bool) -> Transition {
        let Some(item) = self
            .gallery
            .as_ref()
            .and_then(|g| g.items.get(idx))
            .cloned()
        else {
            return Transition::None;
        };
        let args: Vec<String> = std::iter::once("graft".to_string())
            .chain(item.args.iter().cloned())
            .collect();
        // A dashboard of other sources: its first source becomes the main one.
        let args = <Cli as clap::Parser>::try_parse_from(&args)
            .ok()
            .and_then(|c| crate::sources::promote(&c, &args).ok().flatten())
            .unwrap_or(args);
        let cli = match <Cli as clap::Parser>::try_parse_from(&args) {
            Ok(cli) => cli,
            Err(e) => {
                self.error(format!(
                    "{}: {}",
                    item.title,
                    e.to_string().lines().next().unwrap_or("")
                ));
                return Transition::None;
            }
        };
        if let Some(d) = &item.cwd
            && std::path::Path::new(d).is_dir()
        {
            let _ = std::env::set_current_dir(d);
        }
        let live = self.live.take();
        *self = Builder::new(cli, live, None);
        self.from_gallery = true;
        if start {
            Transition::Start
        } else {
            Transition::None
        }
    }

    fn forget_recent(&mut self, n: usize) {
        let _ = crate::recent::forget(n);
        let mut g = Gallery::new();
        g.selected = self
            .gallery
            .as_ref()
            .map_or(0, |o| o.selected)
            .min(g.items.len().saturating_sub(1));
        self.gallery = Some(g);
    }

    fn new_source(&mut self, src: Src) {
        self.gallery = None;
        self.from_gallery = true;
        self.src = src;
        self.source.set("");
        self.focus = Some(Field::Source);
        self.message = Some((
            match src {
                Src::Command => "type a command whose output has numbers (e.g. cat /proc/meminfo), then press Enter or tap load",
                Src::Stream => "type a stream's URL: wss://… for a WebSocket (--send in the command line for a subscription), https://… for server-sent events; then press Enter or tap load",
                _ => "type the path of a file (e.g. /var/log/app.log), then press Enter or tap load",
            }
            .into(),
            false,
        ));
    }

    fn on_gallery_event(&mut self, ev: &TermEvent) -> Transition {
        let n = self.gallery.as_ref().map_or(0, |g| g.items.len());
        match ev {
            TermEvent::Key(k) if k.kind == KeyEventKind::Press => {
                let sel = self.gallery.as_ref().map_or(0, |g| g.selected);
                match k.code {
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Transition::Quit;
                    }
                    KeyCode::Char('q') => return Transition::Back,
                    KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                        self.gallery.as_mut().expect("gallery").selected = (sel + n - 1) % n
                    }
                    KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                        self.gallery.as_mut().expect("gallery").selected = (sel + 1) % n
                    }
                    KeyCode::Enter if n > 0 => return self.open_item(sel, true),
                    KeyCode::Char('e') if n > 0 => return self.open_item(sel, false),
                    KeyCode::Char('x') | KeyCode::Delete if n > 0 => {
                        if let Some(ItemKind::Recent(r)) = self
                            .gallery
                            .as_ref()
                            .and_then(|g| g.items.get(sel))
                            .map(|i| i.kind)
                        {
                            self.forget_recent(r);
                        }
                    }
                    KeyCode::Char('f') => self.new_source(Src::File),
                    KeyCode::Char('c') => self.new_source(Src::Command),
                    KeyCode::Char('w') => self.new_source(Src::Stream),
                    _ => {}
                }
            }
            TermEvent::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                match self.hits.at(m.column, m.row).map(|h| h.1) {
                    Some(Hit::StartItem(i)) => return self.open_item(i, true),
                    Some(Hit::EditItem(i)) => return self.open_item(i, false),
                    Some(Hit::ForgetRecent(n)) => self.forget_recent(n),
                    Some(Hit::NewSource(src)) => self.new_source(src),
                    Some(Hit::Back) => return Transition::Back,
                    _ => {}
                }
            }
            _ => {}
        }
        Transition::None
    }

    fn draw_gallery(&self, f: &mut Frame, g: &Gallery) {
        let area = f.area();
        let heading = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        let mut y = area.y;
        let line = |f: &mut Frame, l: Line, y: &mut u16| {
            if *y < area.bottom() {
                f.render_widget(
                    l,
                    Rect {
                        x: area.x,
                        y: *y,
                        width: area.width,
                        height: 1,
                    },
                );
            }
            *y += 1;
        };
        line(
            f,
            Line::from(vec![
                Span::styled(
                    " graft ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "  what would you like to chart?",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            &mut y,
        );
        y += 1;
        let title_w = g
            .items
            .iter()
            .map(|i| i.title.chars().count())
            .max()
            .unwrap_or(0) as u16
            + 2;
        let mut section = None;
        for (idx, item) in g.items.iter().enumerate() {
            let this = match item.kind {
                ItemKind::Recent(_) => "recent — pick up where you left off",
                ItemKind::Saved => "your saved profiles",
                ItemKind::Preset => "quick start — tap to start (edit to adjust first)",
            };
            let group = std::mem::discriminant(&item.kind);
            if section != Some(group) {
                if section.is_some() {
                    y += 1;
                }
                line(f, Line::styled(this, heading), &mut y);
                section = Some(group);
            }
            if y >= area.bottom() {
                break;
            }
            let selected = idx == g.selected;
            // Keep "edit" near the text on wide screens.
            // Recent rows also need room for ✕ after "edit".
            let reserve = if matches!(item.kind, ItemKind::Recent(_)) {
                4
            } else {
                0
            };
            let right = area
                .right()
                .saturating_sub(reserve)
                .min(area.x + title_w + 70);
            let row = Rect {
                x: area.x,
                y,
                width: right.saturating_sub(area.x + 7),
                height: 1,
            };
            let name_style = if selected {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            let pad = " ".repeat((title_w as usize).saturating_sub(item.title.chars().count()));
            f.render_widget(
                Line::from(vec![
                    Span::styled(format!(" ▸ {}{pad}", item.title), name_style),
                    Span::styled(format!(" {}", item.description), dim),
                ]),
                row,
            );
            self.hits.push(row, Hit::StartItem(idx));
            let edit = Rect {
                x: right.saturating_sub(6),
                y,
                width: 6,
                height: 1,
            };
            f.render_widget(
                Span::styled(
                    " edit ",
                    Style::default().fg(Color::White).bg(Color::DarkGray),
                ),
                edit,
            );
            self.hits.push(edit, Hit::EditItem(idx));
            if let ItemKind::Recent(n) = item.kind
                && right < area.right().saturating_sub(3)
            {
                let x = Rect {
                    x: right + 1,
                    y,
                    width: 3,
                    height: 1,
                };
                f.render_widget(
                    Span::styled(" ✕ ", Style::default().fg(Color::White).bg(Color::DarkGray)),
                    x,
                );
                self.hits.push(x, Hit::ForgetRecent(n));
            }
            y += 1;
        }
        y += 1;
        line(f, Line::styled("or make your own", heading), &mut y);
        let buttons = vec![
            Button::new("chart a file", Hit::NewSource(Src::File)),
            Button::new("chart a command", Hit::NewSource(Src::Command)),
            Button::new("chart a stream", Hit::NewSource(Src::Stream)),
            Button::new("quit", Hit::Back),
        ];
        let h = flow_height(&buttons, area.width);
        if y + h <= area.bottom() {
            render_buttons(
                &buttons,
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: h,
                },
                f.buffer_mut(),
                &self.hits,
            );
        }
        y += h + 1;
        line(
            f,
            Line::styled(
                "keys: ↑/↓ choose · enter start · e edit · x forget · f file · c command · w stream · q quit",
                dim,
            ),
            &mut y,
        );
        line(
            f,
            Line::styled(
                "next time: graft -p NAME runs a preset or saved profile; graft -p @ the latest chart",
                dim,
            ),
            &mut y,
        );
        if let Some((m, err)) = &self.message {
            f.render_widget(
                Span::styled(
                    m.clone(),
                    Style::default().fg(if *err { Color::LightRed } else { Color::Cyan }),
                ),
                Rect {
                    y: area.bottom().saturating_sub(1),
                    height: 1,
                    ..area
                },
            );
        }
    }
}

/// Run a command once in the background and collect its first lines.
fn run_sample_command(cmd: String, ssh: Option<String>) -> Receiver<Result<Vec<String>, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| -> Result<Vec<String>, String> {
            let mut child = crate::remote::command(ssh.as_deref(), &cmd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("failed to run `{cmd}`: {e}"))?;
            let stdout = child.stdout.take().expect("piped stdout");
            let reader = thread::spawn(move || {
                BufReader::new(stdout)
                    .lines()
                    .map_while(std::result::Result::ok)
                    .take(SAMPLE_LINES)
                    .collect::<Vec<_>>()
            });
            let start = Instant::now();
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) if start.elapsed() > COMMAND_TIMEOUT => {
                        let _ = child.kill();
                        break None;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(30)),
                    Err(e) => return Err(e.to_string()),
                }
            };
            let lines = reader.join().unwrap_or_default();
            match status {
                Some(s) if !s.success() && lines.is_empty() => {
                    let mut err = String::new();
                    if let Some(mut e) = child.stderr.take() {
                        let _ = std::io::Read::read_to_string(&mut e, &mut err);
                    }
                    Err(format!(
                        "`{cmd}` {s}: {}",
                        err.lines().next().unwrap_or("").trim()
                    ))
                }
                None if lines.is_empty() => Err(format!(
                    "`{cmd}` produced nothing in {}s",
                    COMMAND_TIMEOUT.as_secs()
                )),
                _ => Ok(lines),
            }
        })();
        let _ = tx.send(result);
    });
    rx
}

/// Run a command once and return its first lines (for suggestions outside the TUI).
pub fn sample_command_blocking(cmd: &str, ssh: Option<&str>) -> Option<Vec<String>> {
    run_sample_command(cmd.to_string(), ssh.map(String::from))
        .recv()
        .ok()?
        .ok()
}

#[cfg(test)]
impl Builder {
    /// Type keys (`↓` is the Down arrow, `\n` Enter).
    pub(crate) fn test_keys(&mut self, keys: &str) {
        for c in keys.chars() {
            let code = match c {
                '\u{2193}' => KeyCode::Down,
                '\n' => KeyCode::Enter,
                c => KeyCode::Char(c),
            };
            self.on_event(&TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::MouseEvent;

    fn builder(args: &[&str]) -> Builder {
        let mut b = Builder::new(Cli::parse_from(args), None, None);
        let start = Instant::now();
        while b.loading.is_some() && start.elapsed() < Duration::from_secs(5) {
            b.tick();
            thread::sleep(Duration::from_millis(10));
        }
        b
    }

    fn screen(b: &Builder, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| b.draw(f)).unwrap();
        crate::render::ansi::to_ansi(term.backend().buffer(), false)
    }

    fn find(b: &Builder, text: &str) -> (u16, u16) {
        let s = screen(b, 100, 40);
        for (row, line) in s.lines().enumerate() {
            if let Some(byte) = line.find(text) {
                return (line[..byte].chars().count() as u16, row as u16);
            }
        }
        panic!("'{text}' not on screen:\n{s}");
    }

    fn tap(b: &mut Builder, text: &str) -> Transition {
        let (x, y) = find(b, text);
        b.on_event(&TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x + 1,
            row: y,
            modifiers: KeyModifiers::NONE,
        }))
    }

    fn mouse_tap(b: &mut Builder, column: u16, row: u16) -> Transition {
        b.on_event(&TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
    }

    fn key(b: &mut Builder, code: KeyCode) -> Transition {
        b.on_event(&TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    #[test]
    fn gallery_shows_recent_charts_first() {
        let file =
            std::env::temp_dir().join(format!("graft-gallery-recent-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&file);
        crate::recent::TEST_PATH.with(|t| *t.borrow_mut() = Some(file.clone()));
        let parse = |a: &[&str]| <Cli as clap::Parser>::parse_from(a);
        crate::recent::remember(&parse(&[
            "graft",
            "examples/reclaimable.log",
            "-F",
            "{time} node1: {node1} node2: {node2}",
            "--pane",
            "node1",
            "--pane",
            "node2",
        ]));
        crate::recent::remember(&parse(&["graft", "-c", "uptime", "--kv"]));
        let mut b = builder(&["graft"]);
        let s = screen(&b, 100, 40);
        let at = |t: &str| s.find(t).unwrap_or_else(|| panic!("{t} missing: {s}"));
        assert!(
            at("recent — pick up where you left off") < at("quick start"),
            "{s}"
        );
        assert!(
            at("▸ uptime") < at("▸ reclaimable.log"),
            "newest first: {s}"
        );
        assert!(s.contains("2 panes"), "{s}");
        // ✕ forgets one.
        let row = s.lines().position(|l| l.contains("▸ uptime")).unwrap() as u16;
        let col = s
            .lines()
            .nth(row as usize)
            .unwrap()
            .chars()
            .position(|c| c == '✕')
            .unwrap() as u16;
        mouse_tap(&mut b, col, row);
        assert!(!screen(&b, 100, 40).contains("▸ uptime"));
        // Tapping a recent chart starts it as it was left.
        assert_eq!(tap(&mut b, "reclaimable.log"), Transition::Start);
        let app = b.launch().expect("recent chart launches");
        assert_eq!(app.current_cli().pane, ["node1", "node2"]);
        crate::recent::TEST_PATH.with(|t| *t.borrow_mut() = None);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn gallery_starts_and_edits_presets() {
        let mut b = builder(&["graft"]);
        let s = screen(&b, 100, 40);
        assert!(s.contains("what would you like to chart?"), "{s}");
        assert!(
            s.contains("CPU usage") && s.contains("Memory") && s.contains("chart a file"),
            "{s}"
        );
        // Tapping a row starts it.
        assert_eq!(tap(&mut b, "Memory"), Transition::Start);
        let app = b.launch().expect("preset launches");
        assert_eq!(app.current_cli().title.as_deref(), Some("Memory"));
        // The tutorial is there too, and starts the tour.
        let mut b = builder(&["graft"]);
        assert_eq!(tap(&mut b, "Tutorial"), Transition::Start);
        let app = b.launch().expect("tutorial launches");
        assert!(app.current_cli().tutorial);
        // "edit" opens it in the builder with its explicit format, and back returns.
        let mut b = builder(&["graft"]);
        // (Down past the tutorial and CPU usage.)
        key(&mut b, KeyCode::Down);
        key(&mut b, KeyCode::Down);
        assert_eq!(key(&mut b, KeyCode::Char('e')), Transition::None);
        assert!(b.gallery.is_none());
        assert_eq!(b.format.value, "{key}: {value}");
        key(&mut b, KeyCode::Esc);
        assert!(b.gallery.is_some());
        // Phone-sized.
        assert!(screen(&b, 46, 30).contains("CPU usage"));
    }

    #[test]
    fn sources_menu_edits_adds_and_removes_merged_sources() {
        use crate::interactive::Transition as T;
        let log = "examples/reclaimable.log";
        let mut app =
            crate::tui::tests::app(&["graft", log, "--kv", "--merge", &format!("b = {log} --kv")]);
        // The menu: the main source and the merged one.
        let ev = |app: &mut App, c: char| {
            app.on_event(&ratatui::crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )))
        };
        ev(&mut app, 'S');
        let s = crate::tui::tests::screen_of(&app, 100, 40);
        assert!(
            s.contains(" sources ") && s.contains("main") && s.contains("1 · b"),
            "{s}"
        );
        // 1: edit merged source b in the builder; rename it c, and it goes back in.
        assert_eq!(ev(&mut app, '1'), T::EditSource);
        let (name, cli) = app.take_source_edit();
        assert_eq!(name, "b");
        let mut b = Builder::for_source(name, cli, Box::new(app), None);
        let start = Instant::now();
        while b.loading.is_some() && start.elapsed() < Duration::from_secs(5) {
            b.tick();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(screen(&b, 100, 40).contains("a source to merge"));
        assert!(screen(&b, 100, 40).contains("merge into the chart"));
        b.merge_name.as_mut().unwrap().set("c");
        let mut app = b.launch().expect("merged back");
        let merged = app.current_cli().merge;
        assert_eq!(merged.len(), 1);
        assert!(merged[0].starts_with("c = "), "{merged:?}");
        // (Merged sources are read in the background.)
        let start = Instant::now();
        while !app.ds_names().iter().any(|n| n.starts_with("c."))
            && start.elapsed() < Duration::from_secs(5)
        {
            app.tick();
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            app.ds_names().iter().any(|n| n.starts_with("c.")),
            "{:?}",
            app.ds_names()
        );
        // a: a new source, named for you.
        ev(&mut app, 'S');
        assert_eq!(ev(&mut app, 'a'), T::EditSource);
        let (name, _) = app.take_source_edit();
        assert_eq!(name, "source2");
        let cli = Cli::parse_from(["graft", log, "--kv"]);
        let mut b = Builder::for_source(name, cli, Box::new(app), None);
        let start = Instant::now();
        while b.loading.is_some() && start.elapsed() < Duration::from_secs(5) {
            b.tick();
            thread::sleep(Duration::from_millis(10));
        }
        let mut app = b.launch().expect("added");
        assert_eq!(app.current_cli().merge.len(), 2);
        // ✕ removes one.
        ev(&mut app, 'S');
        let s = crate::tui::tests::screen_of(&app, 100, 40);
        let (row, line) = s
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains('✕'))
            .unwrap();
        let col = line.chars().position(|c| c == '✕').unwrap() as u16;
        app.on_event(&ratatui::crossterm::event::Event::Mouse(
            ratatui::crossterm::event::MouseEvent {
                kind: ratatui::crossterm::event::MouseEventKind::Down(
                    ratatui::crossterm::event::MouseButton::Left,
                ),
                column: col,
                row: row as u16,
                modifiers: KeyModifiers::NONE,
            },
        ));
        assert_eq!(app.current_cli().merge.len(), 1);
    }

    #[test]
    fn a_panes_source_can_be_edited() {
        use crate::interactive::Transition as T;
        let log = "examples/reclaimable.log";
        let mut app =
            crate::tui::tests::app(&["graft", log, "--kv", "--pane", "*", "--pane", "source=load"]);
        app.on_event(&ratatui::crossterm::event::Event::Key(KeyEvent::new(
            KeyCode::Char('S'),
            KeyModifiers::NONE,
        )));
        let s = crate::tui::tests::screen_of(&app, 100, 40);
        assert!(
            s.contains("pane 2 · load") && s.contains(" edit ") && s.contains(" other source… "),
            "{s}"
        );
        // edit: the builder gets the pane's source, and puts it back in the pane.
        assert_eq!(app.edit_pane_source_for_test(1), T::EditSource);
        let (name, cli) = app.take_source_edit();
        let pane = app.take_pane_to_edit();
        assert_eq!((name.as_str(), pane), ("load", Some(1)));
        assert_eq!(cli.cmd.as_deref(), Some("cat /proc/loadavg"));
        let mut b = Builder::for_source(name, cli, Box::new(app), pane);
        let start = Instant::now();
        while b.loading.is_some() && start.elapsed() < Duration::from_secs(5) {
            b.tick();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(screen(&b, 100, 40).contains("use in the pane"));
        b.merge_name.as_mut().unwrap().set("myload");
        let app = b.launch().expect("back in the pane");
        let cli = app.current_cli();
        assert_eq!(cli.source.len(), 1);
        assert!(cli.source[0].starts_with("myload = "), "{:?}", cli.source);
        assert_eq!(cli.pane[1], "* source=myload");
        // A named source takes the place of a preset with the same name.
        let c = crate::sources::cli_for("load", None, &[format!("load = {log} --kv")]).unwrap();
        assert_eq!(
            c.input.as_ref().map(|p| p.display().to_string()),
            Some(log.to_string())
        );
    }

    #[test]
    fn a_panes_source_previews_what_it_has_read() {
        let mut app = crate::tui::tests::app(&[
            "graft",
            "examples/reclaimable.log",
            "--kv",
            "--source",
            "fast = -c 'echo n=1' -n 0.1s --kv",
            "--pane",
            "*",
            "--pane",
            "source=fast",
        ]);
        let start = Instant::now();
        while app.source_history("fast").len() < 5 && start.elapsed() < Duration::from_secs(5) {
            app.tick();
            thread::sleep(Duration::from_millis(20));
        }
        app.edit_pane_source_for_test(1);
        let (name, cli) = app.take_source_edit();
        let b = Builder::for_source(name, cli, Box::new(app), Some(1));
        assert!(b.loading.is_none(), "no need to run the command again");
        assert!(b.preview.lines.len() >= 5, "{:?}", b.preview.lines);
        let times: std::collections::BTreeSet<String> = b
            .preview
            .lines
            .iter()
            .filter_map(|l| l.result.as_ref().ok())
            .map(|r| r.split("  ").next().unwrap_or("").to_string())
            .collect();
        assert!(times.len() >= 5, "{times:?}");
    }

    #[test]
    fn preview_keeps_each_runs_time() {
        // A command chart that ran three times: the builder's preview shows three times,
        // not the moment it was opened.
        let app = crate::tui::tests::app(&[
            "graft",
            "-c",
            "echo load=1",
            "-n",
            "0.3s",
            "--count",
            "3",
            "--kv",
        ]);
        let cli = app.current_cli();
        let mut b = Builder::new(cli, None, Some(Box::new(app)));
        let start = Instant::now();
        while b.loading.is_some() && start.elapsed() < Duration::from_secs(5) {
            b.tick();
            thread::sleep(Duration::from_millis(10));
        }
        let times: std::collections::BTreeSet<String> = b
            .preview
            .lines
            .iter()
            .filter_map(|l| l.result.as_ref().ok())
            .map(|r| r.split("  ").next().unwrap_or("").to_string())
            .collect();
        assert_eq!(b.preview.lines.len(), 3, "{:?}", b.preview.lines);
        assert_eq!(times.len(), 3, "{times:?}");
    }

    #[test]
    fn suggests_and_previews_a_file() {
        let mut b = builder(&["graft", "examples/reclaimable.log", "--build"]);
        // Nothing is chosen until you pick: the sample is shown raw and start is refused.
        assert_eq!(b.parser, Parser::Unset);
        assert!(screen(&b, 100, 40).contains("29 lines · pick a format"));
        assert!(b.launch().is_err());
        assert_eq!(tap(&mut b, "▸ template"), Transition::None);
        assert_eq!(b.parser, Parser::Template);
        assert_eq!(b.format.value, "{time} node1: {node1} node2: {node2}");
        assert_eq!(b.preview.matched, 29);
        let s = screen(&b, 100, 40);
        assert!(s.contains("29/29 lines parsed · 2 series · x: time"), "{s}");
        assert!(
            s.contains(
                "$ graft --format '{time} node1: {node1} node2: {node2}' examples/reclaimable.log"
            ),
            "{s}"
        );
        // Phone-sized.
        let small = screen(&b, 46, 30);
        assert!(small.contains("start"), "{small}");
    }

    #[test]
    fn editing_the_format_updates_the_preview() {
        let mut b = builder(&["graft", "examples/reclaimable.log"]);
        key(&mut b, KeyCode::Down); // the first suggestion
        let (x, y) = find(&b, "format  {time}");
        b.on_event(&TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x + 10,
            row: y,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(b.focus, Some(Field::Format));
        key(&mut b, KeyCode::End);
        for _ in 0.." node2: {node2}".len() {
            key(&mut b, KeyCode::Backspace);
        }
        assert_eq!(b.preview.series.len(), 1);
        // Pick the key/value suggestion by tapping it, then choose series and options.
        tap(&mut b, "key: value pairs");
        assert_eq!(b.parser, Parser::Kv);
        assert_eq!(b.preview.series.len(), 2);
        tap(&mut b, "● node2");
        assert_eq!(b.cli.select, ["node1"]);
        tap(&mut b, " forecast ");
        tap(&mut b, " stats ");
        tap(&mut b, "e.g. node1=50GB");
        "node1=50GB".chars().for_each(|c| {
            key(&mut b, KeyCode::Char(c));
        });
        assert_eq!(b.cli.target, ["node1=50GB"]);
        assert!(b.cli.stats && b.cli.forecast.is_some());
        // Start → a viewer with this configuration.
        assert_eq!(tap(&mut b, "▶ start"), Transition::Start);
        let app = b.launch().unwrap();
        assert_eq!(app.current_cli().select, ["node1"]);
    }

    #[test]
    fn bad_inputs_are_reported_not_fatal() {
        let mut b = builder(&["graft", "examples/reclaimable.log"]);
        tap(&mut b, " regex ");
        "(unclosed".chars().for_each(|c| {
            b.input(Field::Format)
                .key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        });
        b.refresh();
        assert!(b.preview.error.is_some());
        assert!(b.launch().is_err());
        let mut b = builder(&["graft", "--build"]);
        assert!(b.gallery.is_some(), "no source: the gallery comes first");
        key(&mut b, KeyCode::Char('f'));
        assert_eq!(b.focus, Some(Field::Source));
        assert!(b.launch().is_err(), "no source yet");
        "/nope".chars().for_each(|c| {
            key(&mut b, KeyCode::Char(c));
        });
        key(&mut b, KeyCode::Enter);
        assert!(
            b.message.as_ref().unwrap().1,
            "missing file is an error message"
        );
    }

    #[test]
    fn a_busy_stream_is_sampled_in_about_a_second() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/feed", listener.local_addr().unwrap());
        // A message every 200 ms, forever: it never fills 50 or closes.
        thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n"
                );
                for i in 0.. {
                    if write!(s, "data: {{\"p\":{i}}}\n\n").is_err() {
                        break;
                    }
                    let _ = s.flush();
                    thread::sleep(Duration::from_millis(200));
                }
            }
        });
        let start = Instant::now();
        let lines = crate::sample_stream(crate::source::SourceSpec::Stream {
            url,
            send: Vec::new(),
        });
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(lines.len() >= 3, "{lines:?}");
    }

    #[test]
    fn stream_source_samples_server_sent_events() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/feed", listener.local_addr().unwrap());
        thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nevent: tick\ndata: {{\"p\":1}}\n\ndata: {{\"p\":2}}\n\n"
                );
            }
        });
        let mut b = builder(&["graft", "--build"]);
        tap(&mut b, " chart a stream ");
        assert_eq!(b.src, Src::Stream);
        url.chars().for_each(|c| {
            key(&mut b, KeyCode::Char(c));
        });
        key(&mut b, KeyCode::Enter);
        let start = Instant::now();
        while b.loading.is_some() && start.elapsed() < Duration::from_secs(15) {
            b.tick();
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(b.sample, ["{\"p\":1}", "{\"p\":2}"]);
        b.apply().unwrap();
        assert_eq!(b.cli.stream.as_deref(), Some(url.as_str()));
        assert!(b.cli.cmd.is_none() && b.cli.input.is_none());
    }

    #[test]
    fn command_source_samples_in_the_background() {
        let mut b = builder(&["graft", "--build"]);
        tap(&mut b, " chart a command ");
        "printf 'MemTotal: 100 kB\\nMemFree: 40 kB\\n'"
            .chars()
            .for_each(|c| {
                key(&mut b, KeyCode::Char(c));
            });
        key(&mut b, KeyCode::Enter);
        let start = Instant::now();
        while b.loading.is_some() && start.elapsed() < Duration::from_secs(5) {
            b.tick();
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(b.sample.len(), 2);
        key(&mut b, KeyCode::Down); // pick the first suggestion
        assert_eq!(
            b.preview
                .series
                .iter()
                .map(|s| s.0.as_str())
                .collect::<Vec<_>>(),
            ["MemTotal", "MemFree"]
        );
        assert!(b.cli.command_line().contains("--cmd 'printf"));
    }
}
