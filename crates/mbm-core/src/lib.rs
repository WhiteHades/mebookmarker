//! domain model and port traits. no io in here on purpose.
#![doc(html_no_source)]

pub mod bookmark;
pub mod category;
pub mod entity;
pub mod error;
pub mod id;
pub mod matcher;
pub mod medium;
pub mod port;

pub use bookmark::{
    Assigner, Author, BlockedReason, Bookmark, CategoryAssignment, Enrichment, Link, Media,
    MediaKind, SourceRef, ThreadRole,
};
pub use category::{Action, Category, CategoryRule, Taxonomy};
pub use entity::{Entities, Sentiment};
pub use error::{Class, Error, Layer, Result};
pub use matcher::{CompiledTaxonomy, MatchedRule};
pub use id::Id;
pub use medium::{LinkKind, SinkMedium, SourceMedium, UnknownMedium};
pub use port::{Enricher, EnrichStage, FetchPage, FetchRequest, Sink, SinkReport, Source};
