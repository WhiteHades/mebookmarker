//! orchestration, configuration, and the command-line front end.
//!
//! three layers, each testable on its own:
//!
//! - [`config`] — one toml file with a default for every key, so a missing file
//!   is a working install
//! - [`pipeline`] — sources to store to stages to sinks, in the order that
//!   makes the run cheap
//! - [`cli`] and [`tui`] — the two front ends, one for a person at a terminal
//!   and one for a person in one

pub mod config;
pub mod pipeline;

pub use config::{Agent, Config, Enrich, Sink, Source};
pub use pipeline::{Job, RunReport, open, run};
