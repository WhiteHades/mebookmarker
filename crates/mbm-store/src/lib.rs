//! storage engine: schema, full-text index, fingerprints.

pub mod db;
pub mod fingerprint;
pub mod prefilter;
pub mod repo;
pub mod schema;
pub mod search;

pub use repo::{Filter, Repo};
pub use schema::{SCHEMA_VERSION, is_current, open, open_memory, reindex};
pub use search::{Hit, Mode, Searcher, build_prefilter, indexed_text};
