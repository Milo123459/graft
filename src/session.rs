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
}

impl Session {
    pub fn new(cli: &Cli, script: Rc<ScriptHost>) -> Result<Self> {
        let default_unit = cli.unit.as_deref().map(|u| {
            lookup_unit(u, cli.binary_units).unwrap_or_else(|| (Dim::Other(u.to_string()), 1.0))
        });
        Ok(Self {
            store: Store::new(cli.x, default_unit, cli.window),
            parser: parse::build(cli, script.clone())?,
            pipeline: Pipeline::from_cli(cli, &script)?,
            script,
            lines: 0,
            matched: 0,
            arrival_time: !cli.grep.is_empty() && cli.x == crate::cli::XMode::Auto,
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
        self.pipeline.apply(self.store.dataset(), &self.script)
    }

    /// The series selected for display.
    pub fn select(&self, full: &Dataset) -> Dataset {
        self.pipeline.select(full)
    }
}
