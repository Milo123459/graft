//! The interactive session: owns the terminal (and mouse capture) and switches
//! between the builder and the viewer.

use std::io::stdout;
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;

use crate::builder::Builder;
use crate::cli::Cli;
use crate::data::LineMeta;
use crate::setup::Setup;
use crate::source::{self, Event, SourceSpec};
use crate::tui::App;

/// Raw lines kept for re-parsing when the format changes (sources that can't be re-read).
const RAW_CAP: usize = 200_000;

/// A running input source, plus the raw lines it produced (for sources that can't be re-read).
pub struct Live {
    pub spec: SourceSpec,
    rx: Receiver<Event>,
    pub raw: Vec<(String, LineMeta)>,
    pub eof: bool,
}

impl Live {
    pub fn new(spec: SourceSpec) -> Self {
        let rx = source::spawn(spec.clone());
        Self::from_rx(spec, rx)
    }

    pub fn from_rx(spec: SourceSpec, rx: Receiver<Event>) -> Self {
        Self {
            spec,
            rx,
            raw: Vec::new(),
            eof: false,
        }
    }

    /// Files can simply be read again; other sources keep their history in memory.
    pub fn keeps_history(&self) -> bool {
        !matches!(self.spec, SourceSpec::File { .. })
    }

    /// Process pending events for up to `budget`; returns source errors.
    pub fn pump(
        &mut self,
        budget: Duration,
        on_line: &mut dyn FnMut(&str, &LineMeta),
    ) -> Vec<String> {
        let deadline = Instant::now() + budget;
        let mut errors = Vec::new();
        while Instant::now() < deadline {
            match self.rx.try_recv() {
                Ok(Event::Line(line, meta)) => {
                    on_line(&line, &meta);
                    if self.keeps_history() {
                        if self.raw.len() >= RAW_CAP {
                            self.raw.drain(..RAW_CAP / 10);
                        }
                        self.raw.push((line, meta));
                    }
                }
                Ok(Event::Error(e)) => errors.push(e),
                Ok(Event::SampleDone) => {}
                Ok(Event::Eof) | Err(TryRecvError::Disconnected) => {
                    self.eof = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        errors
    }
}

/// What a screen wants to happen after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    None,
    Quit,
    /// Viewer → builder.
    OpenBuilder,
    /// Builder → back to the viewer it came from (or quit).
    Back,
    /// Builder → start a viewer with its configuration.
    Start,
    /// Turn mouse capture on/off.
    Mouse(bool),
}

pub enum Start {
    Builder(Box<Cli>, Option<Live>),
    Viewer(Box<Setup>, Live),
}

enum Screen {
    Builder(Box<Builder>),
    Viewer(Box<App>),
}

pub fn run(start: Start) -> Result<ExitCode> {
    let mut mouse = match &start {
        Start::Builder(cli, _) => !cli.no_mouse,
        Start::Viewer(setup, _) => !setup.cli.no_mouse,
    };
    let mut from_builder = matches!(start, Start::Builder(..));
    let mut screen = match start {
        Start::Builder(cli, live) => Screen::Builder(Box::new(Builder::new(*cli, live, None))),
        Start::Viewer(setup, live) => Screen::Viewer(Box::new(App::new(*setup, live))),
    };
    // Remembered when it opens (in case it's killed) and again, as left, when it closes.
    if let Screen::Viewer(a) = &screen {
        crate::recent::remember(&a.current_cli());
    }

    let mut terminal = ratatui::init();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        previous_hook(info);
    }));
    if mouse {
        execute!(stdout(), EnableMouseCapture)?;
    }

    let result = (|| -> Result<Option<Cli>> {
        loop {
            match &mut screen {
                Screen::Builder(b) => b.tick(),
                Screen::Viewer(a) => a.tick(),
            }
            terminal.draw(|f| match &screen {
                Screen::Builder(b) => b.draw(f),
                Screen::Viewer(a) => a.draw(f),
            })?;
            if !event::poll(Duration::from_millis(80))? {
                continue;
            }
            let ev = event::read()?;
            let t = match &mut screen {
                Screen::Builder(b) => b.on_event(&ev),
                Screen::Viewer(a) => a.on_event(&ev),
            };
            screen = match (screen, t) {
                (s, Transition::None) => s,
                (Screen::Viewer(a), Transition::Quit) => {
                    let cli = a.current_cli();
                    crate::recent::remember(&cli);
                    return Ok(from_builder.then_some(cli));
                }
                (Screen::Builder(b), Transition::Quit) => return Ok(b.previous_cli()),
                (s, Transition::Mouse(on)) => {
                    mouse = on;
                    if on {
                        execute!(stdout(), EnableMouseCapture)?;
                    } else {
                        execute!(stdout(), DisableMouseCapture)?;
                    }
                    s
                }
                (Screen::Viewer(a), Transition::OpenBuilder) => {
                    let cli = a.current_cli();
                    crate::recent::remember(&cli);
                    Screen::Builder(Box::new(Builder::new(cli, None, Some(a))))
                }
                (Screen::Builder(mut b), Transition::Back) => match b.take_previous() {
                    Some(app) => Screen::Viewer(app),
                    None => return Ok(None),
                },
                (Screen::Builder(mut b), Transition::Start) => match b.launch() {
                    Ok(app) => {
                        crate::recent::remember(&app.current_cli());
                        from_builder = true;
                        Screen::Viewer(Box::new(app))
                    }
                    Err(e) => {
                        b.error(format!("{e:#}"));
                        Screen::Builder(b)
                    }
                },
                (s, _) => s,
            };
            let _ = mouse;
        }
    })();

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Some(cli) = result? {
        eprintln!("graft: to run this again:\n  {}", cli.command_line());
    }
    Ok(ExitCode::SUCCESS)
}
