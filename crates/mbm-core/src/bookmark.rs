//! The [`Bookmark`] aggregate and the values that make one up.
//!
//! A bookmark is deliberately *medium-agnostic*. Nothing in this module
//! mentions X, Reddit, or Markdown: an adapter's job is to turn whatever its
//! source produces into these types, and a sink's job is to render them out.
//! That boundary is what makes "all mediums to all mediums" cheap — an
//! X-shaped value becomes a Reddit-shaped one by changing only the adapter.

use crate::id::Id;
use crate::medium::{LinkKind, SourceMedium};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use url::Url;

/// Who said it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Author {
    /// The handle, without a leading `@`. Lowercased, because `SimonW` and
    /// `simonw` are the same person and must not split a tag space.
    pub handle: String,
    /// The display name, if the source supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A link to the author's profile page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Url>,
}

impl Author {
    /// Build an author from a handle, normalising the leading `@`.
    #[must_use]
    pub fn new(handle: impl AsRef<str>) -> Self {
        Self {
            handle: normalize_handle(handle.as_ref()),
            name: None,
            profile: None,
        }
    }

    /// Add a display name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        if !name.trim().is_empty() {
            self.name = Some(name);
        }
        self
    }
}

/// Strip a leading `@` and lowercase, so handles are comparable and hashable.
fn normalize_handle(raw: &str) -> String {
    raw.trim().trim_start_matches('@').to_ascii_lowercase()
}

/// Where a bookmark came from, and what it is called there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceRef {
    /// Which adapter produced it.
    pub medium: SourceMedium,
    /// The source's own identifier for the item.
    pub external_id: String,
    /// A direct link to the item on the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Url>,
    /// A sub-stream within the source: an X bookmark folder, a subreddit, an
    /// RSS feed's URL, a Notion database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
}

impl SourceRef {
    /// A reference with no sub-stream.
    #[must_use]
    pub fn new(medium: SourceMedium, external_id: impl Into<String>, url: Option<Url>) -> Self {
        Self { medium, external_id: external_id.into(), url, collection: None }
    }

    /// Attach a sub-stream, which becomes a tag during enrichment.
    #[must_use]
    pub fn in_collection(mut self, collection: impl Into<String>) -> Self {
        let c = collection.into();
        if !c.trim().is_empty() {
            self.collection = Some(c);
        }
        self
    }
}

/// A file attached to the bookmark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Media {
    /// What sort of attachment this is.
    pub kind: MediaKind,
    /// A link to the full asset.
    pub url: Url,
    /// A smaller preview, when the source offers one. Previews are what the
    /// vision stage and the archive exporter use; full assets are only
    /// downloaded on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<Url>,
    /// Pixel dimensions, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Pixel dimensions, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Duration in milliseconds, for video and audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Vision-model output, once the enrichment stage has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_text: Option<String>,
}

/// The kind of attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// A still image.
    Photo,
    /// A video.
    Video,
    /// An animated image.
    Gif,
    /// An audio file.
    Audio,
}

impl MediaKind {
    /// Guess the kind from a URL's file extension.
    ///
    /// Uses a suffix match rather than a full parse because CDN URLs bury the
    /// extension behind query strings (`?format=jpg&token=…`), and the
    /// extension is only ever a hint anyway.
    #[must_use]
    pub fn from_url(url: &str) -> Self {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "mp4" | "m4v" | "mov" | "webm" | "mkv" => Self::Video,
            "mp3" | "m4a" | "wav" | "ogg" | "opus" | "flac" | "aac" => Self::Audio,
            "gif" | "gifv" => Self::Gif,
            _ => Self::Photo,
        }
    }
}

/// A link found in the bookmark, with whatever was extracted from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// The link as it appeared in the source, before expansion.
    pub original: Url,
    /// Where the link actually points. Equal to `original` when no expansion
    /// was needed.
    pub resolved: Url,
    /// What kind of thing it is.
    pub kind: LinkKind,
    /// A human-readable title, if one was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The extracted body, truncated to a configured budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// A one-line description from the page's metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Whether a fetch was tried and refused, so the pipeline does not retry
    /// a paywall on every run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedReason>,
}

/// Why a link's content could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockedReason {
    /// The publisher requires a subscription.
    Paywall,
    /// The page needs JavaScript and no headless browser was available.
    NeedsRendering,
    /// The server refused the request.
    Refused,
    /// The resource is gone.
    Gone,
    /// The content was fetched but contained nothing usable.
    Empty,
}

/// A category assignment and how sure the assigner was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryAssignment {
    /// The category's stable slug.
    pub slug: String,
    /// Confidence in `[0, 1]`.
    pub confidence: f32,
    /// Which stage made the call, so a user can re-run just the cheap tier.
    pub assigned_by: Assigner,
}

/// Where a category assignment came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Assigner {
    /// A URL or domain rule. Instant, free, and always available.
    Rule,
    /// The JEV evaluation model.
    Jev,
    /// A local coding agent.
    Agent,
    /// A human.
    Human,
}

/// Progress through the enrichment pipeline.
///
/// Each stage is `None` until it has run. That is the entire resume mechanism:
/// "what is left" is a query for rows with a `NULL` column, so an interrupted
/// run continues exactly where it stopped with no cursor bookkeeping and no
/// possibility of a stage silently skipping a record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enrichment {
    /// Hashtag, mention, domain, and tool extraction finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entities_at: Option<i64>,
    /// Vision analysis of attached media has finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_at: Option<i64>,
    /// Semantic search tags have been generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagged_at: Option<i64>,
    /// Categories have been assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categorized_at: Option<i64>,
    /// A title and summary have been written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub described_at: Option<i64>,
}

/// How the record relates to the conversation it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadRole {
    /// Stands alone.
    Original,
    /// Quotes another post.
    Quote,
    /// Replies to another post.
    Reply,
    /// One part of a self-thread.
    Thread,
}

/// The aggregate root: one bookmarked thing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    /// Store-assigned, chronologically sortable.
    pub id: Id,
    /// Where it came from.
    pub source: SourceRef,
    /// Who said it, when the source has an author concept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
    /// A generated title. `None` until the describe stage runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The body text as the source gave it.
    pub text: String,
    /// A canonical link to the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Url>,
    /// When the item was created upstream, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    /// When we first saw it, in Unix milliseconds.
    pub ingested_at: i64,
    /// User and system tags, deduplicated and sorted.
    pub tags: BTreeSet<String>,
    /// Links found in the text, expanded and classified.
    #[serde(default, skip_serializing_if = "is_empty", skip)]
    pub links: Vec<Link>,
    /// Attached media.
    #[serde(default, skip_serializing_if = "is_empty", skip)]
    pub media: Vec<Media>,
    /// Category assignments.
    #[serde(default, skip_serializing_if = "is_empty", skip)]
    pub categories: Vec<CategoryAssignment>,
    /// How this record relates to its thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ThreadRole>,
    /// Per-stage completion markers.
    #[serde(default, skip_serializing_if = "Enrichment::is_empty")]
    pub enrichment: Enrichment,
    /// 64-bit `SimHash` fingerprint, for near-duplicate detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<u64>,
    /// The untouched source payload, kept for re-parsing when an adapter
    /// learns a new field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl Enrichment {
    /// Whether no stage has run yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entities_at.is_none()
            && self.vision_at.is_none()
            && self.tagged_at.is_none()
            && self.categorized_at.is_none()
            && self.described_at.is_none()
    }

    /// Whether every stage has run.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        !self.is_empty()
            && self.entities_at.is_some()
            && self.vision_at.is_some()
            && self.tagged_at.is_some()
            && self.categorized_at.is_some()
            && self.described_at.is_some()
    }
}

impl Bookmark {
    /// Create a bookmark that has been ingested but not yet enriched.
    ///
    /// `links`, `media`, and `categories` start empty on purpose. The
    /// [`Links`](https://docs.rs) and media tables are what make a bookmark
    /// row narrow, and the store reassembles the aggregate on load.
    #[must_use]
    pub fn new(source: SourceRef, text: impl Into<String>, ingested_at: i64) -> Self {
        Self {
            id: Id::now(),
            source,
            author: None,
            title: None,
            text: text.into(),
            url: None,
            created_at: None,
            ingested_at,
            tags: BTreeSet::new(),
            links: Vec::new(),
            media: Vec::new(),
            categories: Vec::new(),
            role: None,
            enrichment: Enrichment::default(),
            fingerprint: None,
            raw: None,
        }
    }

    /// Attach an author.
    #[must_use]
    pub fn by(mut self, author: Author) -> Self {
        self.author = Some(author);
        self
    }

    /// Set the upstream creation time.
    #[must_use]
    pub fn created_at(mut self, ms: i64) -> Self {
        self.created_at = Some(ms);
        self
    }

    /// Add a tag. Empty and duplicate tags are dropped.
    #[must_use]
    pub fn tag(mut self, tag: impl AsRef<str>) -> Self {
        let tag = tag.as_ref().trim();
        if !tag.is_empty() {
            self.tags.insert(tag.to_ascii_lowercase());
        }
        self
    }

    /// The best time to sort this bookmark by: when it was made, falling back
    /// to when we saw it.
    #[must_use]
    pub const fn sort_timestamp(&self) -> i64 {
        match self.created_at {
            Some(ms) => ms,
            None => self.ingested_at,
        }
    }

    /// The best available display title, never empty.
    ///
    /// Sinks call this instead of reaching for `title` directly, so a record
    /// that never went through the describe stage still renders as something
    /// other than a blank line.
    #[must_use]
    pub fn display_title(&self) -> String {
        if let Some(t) = self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            return t.to_owned();
        }
        if let Some(t) =
            self.links.iter().filter_map(|l| l.title.as_deref()).find(|t| !t.trim().is_empty())
        {
            return t.to_owned();
        }
        let first_line = self.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("Untitled");
        truncate_chars(first_line.trim(), 90)
    }

    /// The handle to show next to the title, if the source has one.
    #[must_use]
    pub fn display_author(&self) -> Option<&str> {
        self.author.as_ref().map(|a| a.handle.as_str())
    }

    /// The most informative link, preferring links that carry extracted text.
    #[must_use]
    pub fn primary_link(&self) -> Option<&Link> {
        self.links
            .iter()
            .filter(|l| l.kind != LinkKind::Unknown || l.body.is_some())
            .max_by_key(|l| (l.body.is_some(), l.title.is_some(), l.kind.is_prose()))
            .or_else(|| self.links.first())
    }
}

/// Truncate on a character boundary, appending an ellipsis when cut.
///
/// Counts characters, not bytes, so a multi-byte string is never sliced
/// through the middle of a code point.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bookmark {
        let mut b = Bookmark::new(
            SourceRef::new(SourceMedium::X, "123", Url::parse("https://x.com/i/status/123").ok()),
            "This is a long opening line that runs well past any sensible title budget and therefore must be cut short",
            1_700_000_000_000,
        );
        b.author = Some(Author::new("@SimonW").with_name("Simon Willison"));
        b.links.push(Link {
            original: Url::parse("https://t.co/abc").unwrap(),
            resolved: Url::parse("https://github.com/simonw/llm").unwrap(),
            kind: LinkKind::Repository,
            title: Some("simonw/llm".into()),
            body: None,
            summary: None,
            blocked: None,
        });
        b
    }

    #[test]
    fn handles_are_normalised() {
        let a = Author::new("@SimonW");
        assert_eq!(a.handle, "simonw");
        assert_eq!(a.handle, Author::new("simonw").handle);
    }

    #[test]
    fn empty_tags_are_dropped_and_the_rest_are_lowercased() {
        let b = sample().tag("Rust").tag("  ").tag("rust");
        assert_eq!(b.tags.len(), 1);
        assert!(b.tags.contains("rust"));
    }

    #[test]
    fn display_title_prefers_the_generated_title() {
        let mut b = sample();
        b.title = Some("A generated title".into());
        assert_eq!(b.display_title(), "A generated title");
    }

    #[test]
    fn display_title_falls_back_to_a_link_title() {
        assert_eq!(sample().display_title(), "simonw/llm");
    }

    #[test]
    fn display_title_falls_back_to_the_first_line_of_text() {
        let mut b = sample();
        b.links.clear();
        let t = b.display_title();
        assert!(t.starts_with("This is a long opening line"), "got {t:?}");
        assert!(t.ends_with('…'), "should truncate, got {t:?}");
    }

    #[test]
    fn display_title_is_never_empty() {
        let b = Bookmark::new(SourceRef::new(SourceMedium::Manual, "1", None), "", 0);
        assert_eq!(b.display_title(), "Untitled");
    }

    #[test]
    fn sort_timestamp_prefers_creation_over_ingestion() {
        let mut b = sample();
        assert_eq!(b.sort_timestamp(), b.ingested_at);
        b.created_at = Some(42);
        assert_eq!(b.sort_timestamp(), 42);
    }

    #[test]
    fn enrichment_starts_empty_and_completes_in_one_step() {
        let b = sample();
        assert!(b.enrichment.is_empty());
        assert!(!b.enrichment.is_complete());
        let done = Enrichment {
            entities_at: Some(1),
            vision_at: Some(1),
            tagged_at: Some(1),
            categorized_at: Some(1),
            described_at: Some(1),
        };
        assert!(done.is_complete());
    }

    #[test]
    fn partial_enrichment_is_neither_empty_nor_complete() {
        let partial = Enrichment { entities_at: Some(1), ..Enrichment::default() };
        assert!(!partial.is_empty());
        assert!(!partial.is_complete());
    }

    #[test]
    fn media_kind_is_guessed_from_the_url_ignoring_the_query() {
        assert_eq!(MediaKind::from_url("https://pbs.twimg.com/a.jpg?format=jpg&token=x"), MediaKind::Photo);
        assert_eq!(MediaKind::from_url("https://video.twimg.com/a.mp4"), MediaKind::Video);
        assert_eq!(MediaKind::from_url("https://x.com/a.GIF"), MediaKind::Gif);
        assert_eq!(MediaKind::from_url("https://x.com/audio.m4a"), MediaKind::Audio);
        assert_eq!(MediaKind::from_url("https://x.com/no-extension"), MediaKind::Photo);
    }

    #[test]
    fn truncation_respects_code_points() {
        // A string of multi-byte characters must not panic or split a char.
        let s = "→".repeat(200);
        let t = truncate_chars(&s, 10);
        assert_eq!(t.chars().count(), 10);
        assert_eq!(t, format!("{}…", "→".repeat(9)));
    }

    #[test]
    fn bookmarks_round_trip_through_json() {
        let b = sample();
        let json = serde_json::to_string(&b).unwrap();
        let back: Bookmark = serde_json::from_str(&json).unwrap();
        // `links` is skip-serialised, so it does not survive the round trip;
        // the store reassembles it. Everything else must.
        assert_eq!(back.id, b.id);
        assert_eq!(back.text, b.text);
        assert_eq!(back.author, b.author);
        assert!(back.links.is_empty());
    }
}
