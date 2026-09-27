//! the enrichment stages, cheapest first, each one resumable.
//!
//! five stages, in the order they run:
//!
//! | stage        | tier | cost              | what it does                  |
//! |--------------|------|-------------------|-------------------------------|
//! | `entities`   | 1    | free              | links, mentions, tags, hashes |
//! | `vision`     | 2    | ~$0.00002/item    | alt text, what needs a look   |
//! | `tags`       | 2    | ~$0.00002/item    | topic from a fixed vocabulary |
//! | `categorize` | 2    | ~$0.00002/item    | category from the taxonomy    |
//! | `describe`   | 3    | one agent call    | title and summary in prose    |
//!
//! every stage stamps its own timestamp column, so an interrupted run resumes
//! from where it stopped with no queue to lose. see [`pipeline`].

pub mod describe;
pub mod entities;
pub mod pipeline;
pub mod tags;
pub mod vision;

pub use describe::{Described, Describe};
pub use entities::Entities;
pub use pipeline::{Plan, Report, StageReport, backlog, backlog_line, column_for, requeue, run};
pub use tags::{CONFIDENCE_FLOOR, Categorizer, Tagger};
pub use vision::{NEEDS_DESCRIPTION, Vision};
