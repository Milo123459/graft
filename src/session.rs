//! Ties a parser, the raw store and the series pipeline together.

use std::rc::Rc;

use anyhow::Result;

use crate::cli::Cli;
use crate::data::{Dataset, LineMeta, Store};
use crate::parse::{self, LineParser, Skip};
use crate::script::ScriptHost;
use crate::transform::Pipeline;
use crate::units::{Dim, lookup_unit};

pub struct Session {
    pub store: Store,
    parser: Box<dyn LineParser>,
    pipeline: Pipeline,
    pub script: Rc<ScriptHost>,
    pub lines: u64,
    pub matched: u64,
    /// --grep: lines with no timestamp at all are counted when they arrive.
    arrival_time: bool,
    /// Custom lines (`--line`), worked out after everything else.
    pub custom: Vec<crate::lines::CustomLine>,
    /// Why custom lines failed, last time they were worked out.
    pub line_errors: std::cell::RefCell<Vec<String>>,
    binary: bool,
}

impl Session {
    pub fn new(cli: &Cli, script: Rc<ScriptHost>) -> Result<Self> {
        let default_unit = cli.unit.as_deref().map(|u| {
            lookup_unit(u, cli.binary_units).unwrap_or_else(|| (Dim::Other(u.to_string()), 1.0))
        });
        Ok(Self {
            store: {
                let mut s = Store::new(cli.x, default_unit, cli.window);
                s.replace = cli.replace;
                s
            },
            parser: parse::build(cli, script.clone())?,
            pipeline: Pipeline::from_cli(cli, &script)?,
            script,
            lines: 0,
            matched: 0,
            // Only for live sources: a file's untimed lines aren't "now".
            arrival_time: !cli.grep.is_empty()
                && cli.x == crate::cli::XMode::Auto
                && (cli.cmd.is_some()
                    || cli.stream.is_some()
                    || cli.follow
                    || cli.input.as_ref().is_none_or(|p| p.as_os_str() == "-")),
            custom: cli
                .line
                .iter()
                .map(|l| crate::lines::CustomLine::parse(l).map_err(anyhow::Error::msg))
                .collect::<Result<_>>()?,
            line_errors: Default::default(),
            binary: cli.binary_units,
        })
    }

    /// Parse and store one line, returning the x it was placed at.
    pub fn feed(&mut self, line: &str, meta: &LineMeta) -> Result<f64, Skip> {
        self.lines += 1;
        let mut parsed = self.parser.parse(line)?;
        if self.arrival_time && parsed.x.is_none() {
            parsed.x = Some(parse::XVal::Time(meta.arrival));
        }
        let x = self.store.ingest(parsed, meta)?;
        self.matched += 1;
        Ok(x)
    }

    /// Every series after derived series and transforms (what scripts and alerts see).
    pub fn full_dataset(&self) -> Dataset {
        let mut ds = self.base_dataset();
        self.add_custom_lines(&mut ds);
        ds
    }

    /// The parsed data after transforms, before custom lines (so other sources can be
    /// merged in first).
    /// Change --resample: the raw points are kept, so it applies at once.
    pub fn set_resample(&mut self, r: Option<crate::cli::Resample>) {
        self.pipeline.set_resample(r);
    }

    pub fn base_dataset(&self) -> Dataset {
        self.pipeline.apply(self.store.dataset(), &self.script)
    }

    /// Work out the custom lines (`--line`) on `ds`.
    pub fn add_custom_lines(&self, ds: &mut Dataset) {
        if !self.custom.is_empty() {
            *self.line_errors.borrow_mut() =
                crate::lines::apply(&self.script, &self.custom, ds, self.binary);
        }
    }

    /// The series selected for display (custom lines always are).
    pub fn select(&self, full: &Dataset) -> Dataset {
        let mut out = self.pipeline.select(full);
        for l in &self.custom {
            if !out.series.iter().any(|s| s.name == l.name)
                && let Some(s) = full.series.iter().find(|s| s.name == l.name)
            {
                out.series.push(s.clone());
            }
        }
        out
    }
}
