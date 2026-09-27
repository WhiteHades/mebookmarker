//! The three port traits: [`Source`], [`Enricher`], and [`Sink`].
//!
//! These are the seams of the system. Everything the project does that is not
//! "store a row" or "run the pipeline" happens behind one of them, which is
//! what keeps the pipeline itself free of `match` arms on medium names.
//!
//! # Object safety
//!
//! All three traits are object safe, so the pipeline holds `Vec<Box<dyn Source>>`
//! and friends and picks adapters at runtime from configuration. That is why
//! the trait methods avoid `impl Trait` in argument position and generics on
//! the type: an adapter is chosen by name, not by the type system.

use crate::bookmark::Bookmark;
use crate::error::Result;
use crate::medium::{SinkMedium, SourceMedium};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// What to ask a source for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FetchRequest {
    /// How many items to return. `None` means "as many as you have".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Page through everything rather than stopping at the first page.
    #[serde(default)]
    pub paginate: bool,
    /// Cap on pages, to bound a runaway backfill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<usize>,
    /// Restrict to a sub-stream: a bookmark folder, a subreddit, a feed URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    /// Only these identifiers, when the user named them explicitly.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub only_ids: BTreeSet<String>,
    /// Resume after this identifier, so an interrupted backfill continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

/// One page of results from a source.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FetchPage {
    /// The bookmarks found.
    pub items: Vec<Bookmark>,
    /// How many more pages exist, if the source knows.
    pub has_more: bool,
    /// The source's own cursor, to be passed back as `since`.
    pub next_cursor: Option<String>,
    /// Items dropped because they were malformed. Surfaced so a silent data
    /// loss bug is visible in the run summary rather than invisible.
    pub skipped: usize,
}

impl FetchPage {
    /// An empty page.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// A page containing exactly these bookmarks.
    #[must_use]
    pub fn of(items: Vec<Bookmark>) -> Self {
        Self { items, ..Self::default() }
    }
}

/// A place bookmarks come from.
///
/// Implementations are stateless with respect to the store: they fetch and
/// parse, and the pipeline decides what to keep. That separation is what lets
/// a source be re-run for a backfill without any risk of duplicating or
/// clobbering enrichment that has already happened.
#[async_trait]
pub trait Source: Send + Sync {
    /// Which medium this source reads.
    fn medium(&self) -> SourceMedium;

    /// A short name for logs and error messages.
    fn name(&self) -> &str {
        self.medium().name()
    }

    /// Check that the source is usable, without fetching anything.
    ///
    /// Called once before a run so a missing credential fails in one clear
    /// message instead of on item 400.
    async fn preflight(&self) -> Result<()> {
        Ok(())
    }

    /// Fetch one page.
    async fn fetch(&self, request: &FetchRequest) -> Result<FetchPage>;
}

/// One stage of the enrichment pipeline.
///
/// Stages are ordered by [`EnrichStage::order`] and the pipeline runs them in
/// that order. A stage reports completion by writing to
/// [`Enrichment`](crate::bookmark::Enrichment), which is what makes an
/// interrupted run resume without a cursor.
#[async_trait]
pub trait Enricher: Send + Sync {
    /// Which stage this is.
    fn stage(&self) -> EnrichStage;

    /// A short name for logs.
    fn name(&self) -> &str {
        self.stage().label()
    }

    /// Whether this enricher is switched on.
    fn is_enabled(&self) -> bool {
        true
    }

    /// Work out how much of a batch this enricher can handle at once.
    ///
    /// Stages that spend money cap this well below the batch size, because a
    /// rate-limited API will start rejecting calls rather than queueing them.
    fn max_batch(&self) -> usize {
        64
    }

    /// Enrich one bookmark in place.
    async fn enrich(&self, bookmark: &mut Bookmark) -> Result<()>;
}

/// The enrichment stages, in the order the pipeline runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnrichStage {
    /// Zero-cost fact extraction: hashtags, mentions, domains, tools, shape.
    Entities,
    /// Vision analysis of attached media.
    Vision,
    /// Search-tag generation.
    Tags,
    /// Category assignment.
    Categorize,
    /// Title and summary writing.
    Describe,
}

impl EnrichStage {
    /// Every stage, in order.
    pub const ALL: &'static [Self] = &[
        Self::Entities,
        Self::Vision,
        Self::Tags,
        Self::Categorize,
        Self::Describe,
    ];

    /// Stable label used in config, logs, and resume cursors.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Entities => "entities",
            Self::Vision => "vision",
            Self::Tags => "tags",
            Self::Categorize => "categorize",
            Self::Describe => "describe",
        }
    }

    /// Position in the pipeline, starting at zero.
    #[must_use]
    pub const fn order(self) -> usize {
        match self {
            Self::Entities => 0,
            Self::Vision => 1,
            Self::Tags => 2,
            Self::Categorize => 3,
            Self::Describe => 4,
        }
    }

    /// Whether this stage calls a paid, rate-limited API.
    ///
    /// The pipeline uses this to decide whether it may run stages
    /// concurrently with each other, and to warn when a run is about to cost
    /// money.
    #[must_use]
    pub const fn is_remote(self) -> bool {
        !matches!(self, Self::Entities)
    }
}

impl std::fmt::Display for EnrichStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::str::FromStr for EnrichStage {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self> {
        let normalised = s.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|stage| stage.label() == normalised)
            .ok_or_else(|| {
                crate::Error::Config(format!(
                    "unknown enrichment stage `{s}`: expected one of {}",
                    Self::ALL.iter().map(|s| s.label()).collect::<Vec<_>>().join(", ")
                ))
            })
    }
}

/// What a sink did with the items it was given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkReport {
    /// Items the sink wrote.
    pub written: usize,
    /// Items the sink skipped, usually because they already existed.
    pub skipped: usize,
    /// Items the sink could not handle.
    pub failed: usize,
    /// Files the sink created or modified.
    pub files: usize,
}

impl SinkReport {
    /// Add another report into this one.
    pub fn absorb(&mut self, other: Self) {
        self.written += other.written;
        self.skipped += other.skipped;
        self.failed += other.failed;
        self.files += other.files;
    }

    /// Whether anything at all happened.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.written == 0 && self.skipped == 0 && self.failed == 0 && self.files == 0
    }
}

/// A place bookmarks go.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Which medium this sink writes.
    fn kind(&self) -> SinkMedium;

    /// A short name for logs and error messages.
    fn name(&self) -> &str {
        self.kind().name()
    }

    /// Prepare the destination. Called once before any writes.
    async fn prepare(&self) -> Result<()> {
        Ok(())
    }

    /// Write a batch. Sinks are expected to be idempotent: writing the same
    /// bookmark twice must not duplicate it.
    async fn write(&self, items: &[&Bookmark]) -> Result<SinkReport>;

    /// Finish up: close files, write an index, flush a cache.
    async fn finish(&self) -> Result<SinkReport> {
        Ok(SinkReport::default())
    }
}

/// A sink that renders to a directory tree.
#[async_trait]
pub trait DirectorySink: Sink {
    /// The root directory this sink writes into.
    fn root(&self) -> &PathBuf;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_are_ordered_cheapest_first() {
        assert!(EnrichStage::Entities.order() < EnrichStage::Vision.order());
        assert!(EnrichStage::Vision.order() < EnrichStage::Tags.order());
        assert!(EnrichStage::Tags.order() < EnrichStage::Categorize.order());
        assert!(EnrichStage::Categorize.order() < EnrichStage::Describe.order());
    }

    #[test]
    fn only_entity_extraction_is_free() {
        assert!(!EnrichStage::Entities.is_remote());
        for stage in [EnrichStage::Vision, EnrichStage::Tags, EnrichStage::Categorize, EnrichStage::Describe] {
            assert!(stage.is_remote(), "{stage} costs money and must be reported as remote");
        }
    }

    #[test]
    fn stage_labels_round_trip() {
        for stage in EnrichStage::ALL {
            assert_eq!(stage.label().parse::<EnrichStage>().unwrap(), *stage);
        }
    }

    #[test]
    fn an_unknown_stage_lists_the_valid_ones() {
        let err = "nonsense".parse::<EnrichStage>().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nonsense"));
        assert!(msg.contains("categorize"), "should suggest the real stages: {msg}");
    }

    #[test]
    fn sink_reports_accumulate() {
        let mut total = SinkReport::default();
        assert!(total.is_empty());
        total.absorb(SinkReport { written: 3, skipped: 1, failed: 0, files: 2 });
        total.absorb(SinkReport { written: 4, skipped: 0, failed: 1, files: 1 });
        assert_eq!(total.written, 7);
        assert_eq!(total.skipped, 1);
        assert_eq!(total.failed, 1);
        assert_eq!(total.files, 3);
        assert!(!total.is_empty());
    }

    #[test]
    fn fetch_requests_default_to_a_single_page() {
        let r = FetchRequest::default();
        assert_eq!(r.limit, None);
        assert!(!r.paginate);
        assert!(r.only_ids.is_empty());
    }
}
