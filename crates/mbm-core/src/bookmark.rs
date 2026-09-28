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
        Self { handle: normalize_handle(handle.as_ref()), name: None, profile: None }
    }

    /// add a display name.
    #[must_use]
    pub fn with_name(self, name: impl Into<String>) -> Self {
        self.with_name_opt(Some(name.into()))
    }

    /// whether this account is an email address rather than a platform handle.
    ///
    /// the two are worth telling apart because a handle is something a person
    /// types to find the account again, and an address is not: tagging a post
    /// `@writer@example` puts a tag in the archive that no one will ever search
    /// for and that looks like a mention that was never there.
    #[must_use]
    pub fn is_address(&self) -> bool {
        // `@someone` is a handle on every platform that uses the mark, and an
        // address is the one with a local part in front of the `@`
        if self.handle.starts_with('@') {
            return false;
        }
        let Some((local, domain)) = self.handle.split_once('@') else {
            return false;
        };
        !local.is_empty() && domain.contains('.')
    }

    /// how a person writes this account.
    ///
    /// one form for every sink and every importer: the handle as the platform
    /// wrote it, and the display name in brackets when there is one. eight
    /// renderings of an author across one archive is how an export and a
    /// listing end up disagreeing about the same post.
    #[must_use]
    pub fn display(&self) -> String {
        match &self.name {
            Some(name) => format!("{} ({name})", self.handle),
            None => self.handle.clone(),
        }
    }

    /// add a display name, when there is one.
    #[must_use]
    pub fn with_name_opt(mut self, name: Option<String>) -> Self {
        if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
            self.name = Some(name);
        }
        self
    }
}

/// a handle as the archive stores it.
///
/// lowercased, because x, reddit and github all treat handles case
/// insensitively and `@Trq212` and `@trq212` are one account. the leading `@` is
/// kept where the platform wrote one, because the mention in a post is written
/// with it and a tag of `@trq212` that never matches the mention `@trq212` is a
/// tag nobody can search for.
fn normalize_handle(raw: &str) -> String {
    let trimmed = raw.trim();
    match trimmed.strip_prefix('@') {
        Some(rest) => format!("@{}", rest.to_ascii_lowercase()),
        None => trimmed.to_ascii_lowercase(),
    }
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
    /// parse a source's spelling, returning `None` for anything unrecognised.
    ///
    /// a source that says "image" means a photo and one that says
    /// `animated_gif` means a gif. a caller that wants a guess either way uses
    /// [`MediaKind::from_url`] instead.
    #[must_use]
    pub fn try_parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "photo" | "image" => Some(Self::Photo),
            "video" => Some(Self::Video),
            "gif" | "animated_gif" => Some(Self::Gif),
            "audio" => Some(Self::Audio),
            _ => None,
        }
    }

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

    /// one line on what the item is for.
    ///
    /// generated by the describe stage, or read off a page's metadata. it is
    /// the difference between a list a person can skim and a list they have to
    /// open every entry to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,

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
            summary: None,
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
