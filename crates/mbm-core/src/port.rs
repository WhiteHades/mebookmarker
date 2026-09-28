//! the three port traits: source, enricher, sink.
use crate::bookmark::Bookmark;
use crate::error::Result;
use crate::medium::{SinkMedium, SourceMedium};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FetchRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,

    #[serde(default)]
    pub paginate: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,

    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub only_ids: BTreeSet<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FetchPage {
    pub items: Vec<Bookmark>,

    pub has_more: bool,

    pub next_cursor: Option<String>,

    pub skipped: usize,
}

impl FetchPage {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn of(items: Vec<Bookmark>) -> Self {
        Self { items, ..Self::default() }
    }
}

#[async_trait]
pub trait Source: Send + Sync {
    fn medium(&self) -> SourceMedium;

    fn name(&self) -> &str {
        self.medium().name()
    }

    async fn preflight(&self) -> Result<()> {
        Ok(())
    }

    async fn fetch(&self, request: &FetchRequest) -> Result<FetchPage>;
}

#[async_trait]
pub trait Enricher: Send + Sync {
    fn stage(&self) -> EnrichStage;

    fn name(&self) -> &str {
        self.stage().label()
    }

    fn is_enabled(&self) -> bool {
        true
    }

    fn max_batch(&self) -> usize {
        64
    }

    async fn enrich(&self, bookmark: &mut Bookmark) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnrichStage {
    Entities,

    Vision,

    Tags,

    Categorize,

    Describe,
}

impl EnrichStage {
    pub const ALL: &'static [Self] =
        &[Self::Entities, Self::Vision, Self::Tags, Self::Categorize, Self::Describe];

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
        Self::ALL.iter().copied().find(|stage| stage.label() == normalised).ok_or_else(|| {
            crate::Error::Config(format!(
                "unknown enrichment stage `{s}`: expected one of {}",
                Self::ALL.iter().map(|s| s.label()).collect::<Vec<_>>().join(", ")
            ))
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkReport {
    pub written: usize,

    pub skipped: usize,

    pub failed: usize,

    pub files: usize,
}

impl SinkReport {
    pub fn absorb(&mut self, other: Self) {
        self.written += other.written;
        self.skipped += other.skipped;
        self.failed += other.failed;
        self.files += other.files;
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.written == 0 && self.skipped == 0 && self.failed == 0 && self.files == 0
    }
}

#[async_trait]
pub trait Sink: Send + Sync {
    fn kind(&self) -> SinkMedium;

    fn name(&self) -> &str {
        self.kind().name()
    }

    async fn prepare(&self) -> Result<()> {
        Ok(())
    }

    async fn write(&self, items: &[&Bookmark]) -> Result<SinkReport>;

    async fn finish(&self) -> Result<SinkReport> {
        Ok(SinkReport::default())
    }
}

#[async_trait]
pub trait DirectorySink: Sink {
    fn root(&self) -> &PathBuf;
}
