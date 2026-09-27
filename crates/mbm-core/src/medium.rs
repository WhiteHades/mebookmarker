//! the taxonomy of what can be read from and written to.
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceMedium {
    X,

    XBird,

    Json,

    Reddit,

    HackerNews,

    Github,

    YouTube,

    Rss,

    ReadLater,

    Readwise,

    BrowserBookmarks,

    LocalFile,

    Manual,

    /// a personal archive file: the markdown shape `bookmarks.md` is written in.
    MarkdownFile,
}

impl SourceMedium {
    pub const ALL: &'static [Self] = &[
        Self::X,
        Self::XBird,
        Self::Json,
        Self::Reddit,
        Self::HackerNews,
        Self::Github,
        Self::YouTube,
        Self::Rss,
        Self::ReadLater,
        Self::Readwise,
        Self::BrowserBookmarks,
        Self::LocalFile,
        Self::Manual,
        Self::MarkdownFile,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::X => "x",
            Self::XBird => "x-bird",
            Self::Json => "json",
            Self::Reddit => "reddit",
            Self::HackerNews => "hackernews",
            Self::Github => "github",
            Self::YouTube => "youtube",
            Self::Rss => "rss",
            Self::ReadLater => "read-later",
            Self::Readwise => "readwise",
            Self::BrowserBookmarks => "browser-bookmarks",
            Self::LocalFile => "local-file",
            Self::Manual => "manual",
            Self::MarkdownFile => "markdown-file",
        }
    }

    #[must_use]
    pub const fn requires_auth(self) -> bool {
        match self {
            Self::X | Self::XBird | Self::Reddit | Self::HackerNews | Self::Github
            | Self::YouTube | Self::ReadLater | Self::Readwise => true,
            Self::Json | Self::Rss | Self::BrowserBookmarks | Self::LocalFile | Self::Manual
            | Self::MarkdownFile => false,
        }
    }

    #[must_use]
    pub const fn supports_paging(self) -> bool {
        match self {
            Self::X | Self::XBird | Self::Reddit | Self::HackerNews | Self::Github
            | Self::ReadLater | Self::Readwise => true,
            Self::Json | Self::YouTube | Self::Rss | Self::BrowserBookmarks | Self::LocalFile
            | Self::Manual | Self::MarkdownFile => false,
        }
    }
}

impl fmt::Display for SourceMedium {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for SourceMedium {
    type Err = UnknownMedium;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const ALIASES: &[(&str, SourceMedium)] = &[
            ("hackernews", SourceMedium::HackerNews),
            ("hacker-news", SourceMedium::HackerNews),
            ("hn", SourceMedium::HackerNews),
            ("twitter", SourceMedium::X),
            ("yt", SourceMedium::YouTube),
            ("browser", SourceMedium::BrowserBookmarks),
            ("bookmarks", SourceMedium::BrowserBookmarks),
            ("file", SourceMedium::LocalFile),
            ("files", SourceMedium::LocalFile),
        ];

        let normalised = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        if let Some((_, medium)) = ALIASES.iter().find(|(alias, _)| *alias == normalised) {
            return Ok(*medium);
        }
        Self::ALL
            .iter()
            .copied()
            .find(|m| m.name() == normalised)
            .ok_or_else(|| UnknownMedium { kind: "source", value: s.to_owned() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SinkMedium {
    Markdown,

    Obsidian,

    Html,

    Csv,

    Jsonl,

    Json,

    Opml,

    Archive,

    Store,
}

impl SinkMedium {
    pub const ALL: &'static [Self] = &[
        Self::Markdown,
        Self::Obsidian,
        Self::Html,
        Self::Csv,
        Self::Jsonl,
        Self::Json,
        Self::Opml,
        Self::Archive,
        Self::Store,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Obsidian => "obsidian",
            Self::Html => "html",
            Self::Csv => "csv",
            Self::Jsonl => "jsonl",
            Self::Json => "json",
            Self::Opml => "opml",
            Self::Archive => "archive",
            Self::Store => "store",
        }
    }

    #[must_use]
    pub const fn requires_enrichment(self) -> bool {
        matches!(self, Self::Markdown | Self::Obsidian | Self::Html)
    }
}

impl fmt::Display for SinkMedium {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for SinkMedium {
    type Err = UnknownMedium;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalised = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Self::ALL
            .iter()
            .copied()
            .find(|m| m.name() == normalised)
            .ok_or_else(|| UnknownMedium { kind: "sink", value: s.to_owned() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkKind {
    Repository,

    Article,

    LongForm,

    Video,

    Podcast,

    Post,

    Image,

    Thread,

    Paper,

    Product,

    Release,

    Unknown,
}

impl LinkKind {
    /// the stored name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Article => "article",
            Self::LongForm => "long-form",
            Self::Video => "video",
            Self::Podcast => "podcast",
            Self::Post => "post",
            Self::Image => "image",
            Self::Thread => "thread",
            Self::Paper => "paper",
            Self::Product => "product",
            Self::Release => "release",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub const fn worth_extracting(self) -> bool {
        match self {
            Self::Repository
            | Self::Article
            | Self::LongForm
            | Self::Paper
            | Self::Product
            | Self::Release => true,
            Self::Video | Self::Podcast | Self::Post | Self::Image | Self::Thread
            | Self::Unknown => false,
        }
    }

    #[must_use]
    pub const fn is_prose(self) -> bool {
        matches!(self, Self::Article | Self::LongForm | Self::Paper | Self::Release)
    }
}

impl LinkKind {
    /// a noun phrase for this kind, used in indexed text and prompts.
    #[must_use]
    pub const fn title(self) -> Option<&'static str> {
        Some(match self {
            Self::Repository => "repository",
            Self::Article => "article",
            Self::LongForm => "long-form article",
            Self::Video => "video",
            Self::Podcast => "podcast",
            Self::Post => "post",
            Self::Image => "image",
            Self::Thread => "thread",
            Self::Paper => "paper",
            Self::Product => "product",
            Self::Release => "release",
            Self::Unknown => return None,
        })
    }
}

impl fmt::Display for LinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Repository => "repository",
            Self::Article => "article",
            Self::LongForm => "long-form",
            Self::Video => "video",
            Self::Podcast => "podcast",
            Self::Post => "post",
            Self::Image => "image",
            Self::Thread => "thread",
            Self::Paper => "paper",
            Self::Product => "product",
            Self::Release => "release",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

impl FromStr for LinkKind {
    type Err = UnknownMedium;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const ALL: &[(LinkKind, &str)] = &[
            (LinkKind::Repository, "repository"),
            (LinkKind::Repository, "repo"),
            (LinkKind::Repository, "github"),
            (LinkKind::Article, "article"),
            (LinkKind::LongForm, "long-form"),
            (LinkKind::LongForm, "x-article"),
            (LinkKind::Video, "video"),
            (LinkKind::Podcast, "podcast"),
            (LinkKind::Post, "post"),
            (LinkKind::Post, "tweet"),
            (LinkKind::Image, "image"),
            (LinkKind::Thread, "thread"),
            (LinkKind::Paper, "paper"),
            (LinkKind::Product, "product"),
            (LinkKind::Release, "release"),
            (LinkKind::Unknown, "unknown"),
        ];
        let normalised = s.trim().to_ascii_lowercase();
        ALL.iter()
            .find(|(_, name)| *name == normalised)
            .map(|(k, _)| *k)
            .ok_or_else(|| UnknownMedium { kind: "link kind", value: s.to_owned() })
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown {kind} medium `{value}`")]
pub struct UnknownMedium {
    pub kind: &'static str,

    pub value: String,
}

impl UnknownMedium {
    #[must_use]
    pub fn suggestions(&self, valid: impl IntoIterator<Item = &'static str>) -> Vec<&'static str> {
        let needle = self.value.to_ascii_lowercase();
        let mut scored: Vec<(f64, &'static str)> = valid
            .into_iter()
            .map(|name| (jaro_winkler(&needle, name), name))
            .filter(|(score, _)| *score >= 0.7)
            .collect();
        scored.sort_unstable_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.dedup_by(|a, b| a.1 == b.1);
        scored.into_iter().map(|(_, name)| name).take(3).collect()
    }
}

fn jaro_winkler(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return f64::from(u8::from(a == b));
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);

    let mut a_hit = vec![false; a.len()];
    let mut b_hit = vec![false; b.len()];
    let mut matches = 0_usize;

    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_hit[j] && b[j] == *ca {
                a_hit[i] = true;
                b_hit[j] = true;
                matches += 1;
                break;
            }
        }
    }

    if matches == 0 {
        return 0.0;
    }

    let mut transpositions = 0_usize;
    let mut k = 0_usize;
    for i in 0..a.len() {
        if !a_hit[i] {
            continue;
        }
        while !b_hit[k] {
            k += 1;
        }
        if a[i] != b[k] {
            transpositions += 1;
        }
        k += 1;
    }

    let m = matches as f64;
    let jaro = (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64 / 2.0) / m) / 3.0;

    let prefix = a.iter().zip(&b).take(4).take_while(|(x, y)| x == y).count().min(4);
    jaro + 0.1 * prefix as f64 * (1.0 - jaro)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_parses_from_its_own_name() {
        for m in SourceMedium::ALL {
            assert_eq!(m.name().parse::<SourceMedium>().unwrap(), *m);
        }
    }

    #[test]
    fn every_sink_parses_from_its_own_name() {
        for m in SinkMedium::ALL {
            assert_eq!(m.name().parse::<SinkMedium>().unwrap(), *m);
        }
    }

    #[test]
    fn source_parsing_is_forgiving_about_separators_case_and_aliases() {
        assert_eq!("Hacker News".parse::<SourceMedium>().unwrap(), SourceMedium::HackerNews);
        assert_eq!("read_later".parse::<SourceMedium>().unwrap(), SourceMedium::ReadLater);
        assert_eq!("  X  ".parse::<SourceMedium>().unwrap(), SourceMedium::X);
        assert_eq!("twitter".parse::<SourceMedium>().unwrap(), SourceMedium::X);
        assert_eq!("HN".parse::<SourceMedium>().unwrap(), SourceMedium::HackerNews);
    }

    #[test]
    fn unknown_media_report_the_name_they_rejected() {
        let err = "myspace".parse::<SourceMedium>().unwrap_err();
        assert!(err.to_string().contains("myspace"));
    }

    #[test]
    fn near_misses_get_a_did_you_mean_list() {
        let err = "reddi".parse::<SourceMedium>().unwrap_err();
        let suggestions = err.suggestions(SourceMedium::ALL.iter().map(|m| m.name()));
        assert!(suggestions.contains(&"reddit"), "got {suggestions:?}");

        let err = "zzzzzzzz".parse::<SourceMedium>().unwrap_err();
        assert!(err.suggestions(SourceMedium::ALL.iter().map(|m| m.name())).is_empty());
    }

    #[test]
    fn only_text_bearing_kinds_are_worth_extracting() {
        assert!(LinkKind::Repository.worth_extracting());
        assert!(LinkKind::Article.worth_extracting());

        assert!(!LinkKind::Image.worth_extracting());
        assert!(!LinkKind::Video.worth_extracting());
        assert!(!LinkKind::Post.worth_extracting());
    }

    #[test]
    fn auth_and_paging_capabilities_are_consistent() {
        for m in SourceMedium::ALL {
            if m.supports_paging() {
                assert!(m.requires_auth(), "{m} pages but claims no auth");
            }
        }
    }

    #[test]
    fn link_kind_accepts_the_legacy_smaug_vocabulary() {
        assert_eq!("tweet".parse::<LinkKind>().unwrap(), LinkKind::Post);
        assert_eq!("x-article".parse::<LinkKind>().unwrap(), LinkKind::LongForm);
        assert_eq!("github".parse::<LinkKind>().unwrap(), LinkKind::Repository);
    }
}
