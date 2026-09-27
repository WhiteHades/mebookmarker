//! source adapters: everything mebookmarker can read from.

pub mod json;

pub use json::parse as parse_json;
