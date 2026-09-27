//! orchestration, configuration, and the two front ends.
//!
//! three layers, each testable on its own:
//!
//! - [`config`] — one toml file with a default for every key, so a missing file
//!   is a working install
//! - [`pipeline`] — sources to store to stages to sinks, in the order that
//!   makes the run cheap
//! - [`cli`] and [`tui`] — one front end for a person at a shell and one for a
//!   person in a terminal window

pub mod cli;
pub mod config;
pub mod pipeline;
pub mod tui;

pub use cli::{Cli, Command, Output};
pub use config::{Agent, Config, Enrich, Sink, Source};
pub use pipeline::{Job, RunReport, open, run};
