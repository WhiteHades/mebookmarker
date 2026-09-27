//! the bookmark aggregate: one saved thing, whatever medium it came from.
use crate::id::Id;
use crate::medium::{LinkKind, SourceMedium};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Author {
    pub handle: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Url>,
}

impl Author {
    #[must_use]
    pub fn new(handle: impl AsRef<str>) -> Self {
        Self {
            handle: normalize_handle(handle.as_ref()),
            name: None,
            profile: None,
        }
    }

    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        if !name.trim().is_empty() {
            self.name = Some(name);
        }
        self
    }
}

fn normalize_handle(raw: &str) -> String {
    raw.trim().trim_start_matches('@').to_ascii_lowercase()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceRef {
    pub medium: SourceMedium,

    pub external_id: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Url>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
}

impl SourceRef {
    #[must_use]
    pub fn new(medium: SourceMedium, external_id: impl Into<String>, url: Option<Url>) -> Self {
        Self { medium, external_id: external_id.into(), url, collection: None }
    }

    #[must_use]
    pub fn in_collection(mut self, collection: impl Into<String>) -> Self {
        let c = collection.into();
        if !c.trim().is_empty() {
            self.collection = Some(c);
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Media {
    pub kind: MediaKind,

    pub url: Url,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<Url>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_text: Option<String>,
}

/// generate a `name`/`parse` pair for an enum whose storage form is kebab-case.
///
/// storage goes through these rather than through serde: a plain column holds
/// `jev`, while a serde string would hold `"jev"`, and a column that quotes its
/// own values does not read back cleanly with ordinary SQL.
macro_rules! stored_name {
    ($ty:ty, { $($variant:ident => $name:literal),+ $(,)? }, default = $fallback:expr) => {
        impl $ty {
            /// the stored name.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }

            /// parse a stored name.
            #[must_use]
            pub fn parse(raw: &str) -> Self {
                match raw { $($name => Self::$variant,)+ _ => $fallback }
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Photo,

    Video,

    Gif,

    Audio,
}

impl MediaKind {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub original: Url,

    pub resolved: Url,

    pub kind: LinkKind,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockedReason {
    Paywall,

    NeedsRendering,

    Refused,

    Gone,

    Empty,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryAssignment {
    pub slug: String,

    pub confidence: f32,

    pub assigned_by: Assigner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Assigner {
    Rule,

    Jev,

    Agent,

    Human,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enrichment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entities_at: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_at: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagged_at: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categorized_at: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub described_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadRole {
    Original,

    Quote,

    Reply,

    Thread,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    pub id: Id,

    pub source: SourceRef,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    pub text: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Url>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,

    pub ingested_at: i64,

    pub tags: BTreeSet<String>,

    #[serde(default, skip_serializing_if = "is_empty", skip)]
    pub links: Vec<Link>,

    #[serde(default, skip_serializing_if = "is_empty", skip)]
    pub media: Vec<Media>,

    #[serde(default, skip_serializing_if = "is_empty", skip)]
    pub categories: Vec<CategoryAssignment>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ThreadRole>,

    #[serde(default, skip_serializing_if = "Enrichment::is_empty")]
    pub enrichment: Enrichment,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl Enrichment {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entities_at.is_none()
            && self.vision_at.is_none()
            && self.tagged_at.is_none()
            && self.categorized_at.is_none()
            && self.described_at.is_none()
    }

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

    #[must_use]
    pub fn by(mut self, author: Author) -> Self {
        self.author = Some(author);
        self
    }

    #[must_use]
    pub fn created_at(mut self, ms: i64) -> Self {
        self.created_at = Some(ms);
        self
    }

    #[must_use]
    pub fn tag(mut self, tag: impl AsRef<str>) -> Self {
        self.push_tag(tag);
        self
    }

    /// add a tag in place, for callers that already own the bookmark.
    pub fn push_tag(&mut self, tag: impl AsRef<str>) -> &mut Self {
        let tag = tag.as_ref().trim();
        if !tag.is_empty() {
            self.tags.insert(tag.to_ascii_lowercase());
        }
        self
    }

    #[must_use]
    pub const fn sort_timestamp(&self) -> i64 {
        match self.created_at {
            Some(ms) => ms,
            None => self.ingested_at,
        }
    }

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

    #[must_use]
    pub fn display_author(&self) -> Option<&str> {
        self.author.as_ref().map(|a| a.handle.as_str())
    }

    #[must_use]
    pub fn primary_link(&self) -> Option<&Link> {
        self.links
            .iter()
            .filter(|l| l.kind != LinkKind::Unknown || l.body.is_some())
            .max_by_key(|l| (l.body.is_some(), l.title.is_some(), l.kind.is_prose()))
            .or_else(|| self.links.first())
    }
}

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

        assert_eq!(back.id, b.id);
        assert_eq!(back.text, b.text);
        assert_eq!(back.author, b.author);
        assert!(back.links.is_empty());
    }
}

stored_name!(
    MediaKind,
    { Photo => "photo", Video => "video", Gif => "gif", Audio => "audio" },
    default = MediaKind::Photo
);
stored_name!(
    Assigner,
    { Rule => "rule", Jev => "jev", Agent => "agent", Human => "human" },
    default = Assigner::Rule
);
impl BlockedReason {
    /// the stored name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Paywall => "paywall",
            Self::NeedsRendering => "needs-rendering",
            Self::Refused => "refused",
            Self::Gone => "gone",
            Self::Empty => "empty",
        }
    }

    /// parse a stored name. an unrecognised value means the column is stale or
    /// hand-edited, and the right answer there is to treat the link as still
    /// fetchable.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "paywall" => Self::Paywall,
            "needs-rendering" => Self::NeedsRendering,
            "refused" => Self::Refused,
            "gone" => Self::Gone,
            "empty" => Self::Empty,
            _ => return None,
        })
    }
}

stored_name!(
    ThreadRole,
    { Original => "original", Quote => "quote", Reply => "reply", Thread => "thread" },
    default = ThreadRole::Original
);
