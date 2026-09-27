//! mebookmarker core: the domain model and the traits every adapter implements.
//!
//! # Why this crate exists
//!
//! The whole project is a pipeline with pluggable ends. Keeping the vocabulary
//! in one leaf crate with no I/O dependencies means the adapters
//! (`mbm-ingest`), the enrichers (`mbm-enrich`), and the writers (`mbm-sink`)
//! can all depend on the *types* without depending on each other. Adding a new
//! source medium or a new output format therefore never requires touching the
//! pipeline, and the compiler enforces that rather than a review comment.
//!
//! # The three port traits
//!
//! - [`Source`] pulls bookmarks in.
//! - [`Enricher`] adds derived data to a bookmark in place.
//! - [`Sink`] writes bookmarks out.
//!
//! Everything else in the workspace is an implementation of one of those three
//! plus the storage engine that sits between them.

#![doc(html_no_source)]

pub mod bookmark;
pub mod category;
pub mod entity;
pub mod error;
pub mod id;
pub mod medium;
pub mod port;

pub use bookmark::{
    Assigner, Author, BlockedReason, Bookmark, CategoryAssignment, Enrichment, Link, Media,
    MediaKind, SourceRef, ThreadRole,
};
pub use category::{Action, Category, CategoryRule, Taxonomy};
pub use entity::{Entities, Sentiment};
pub use error::{Class, Error, Layer, Result};
pub use id::Id;
pub use medium::{LinkKind, SinkMedium, SourceMedium, UnknownMedium};
pub use port::{Enricher, EnrichStage, FetchPage, FetchRequest, Sink, SinkReport, Source};
