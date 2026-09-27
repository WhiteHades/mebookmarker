//! Entities: the facts a bookmark states, extracted without spending a token.
//!
//! Everything here comes from re-reading the text and the source payload.
//! Hashtags, mentions, and domains are already present in the data; a tool
//! name is a table lookup. That is the point: this stage costs microseconds
//! and no money, so it runs on every record, including the ones a user will
//! never look at again.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// What a bookmark is, structurally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
    /// Stands alone.
    #[default]
    Original,
    /// Quotes another post.
    Quote,
    /// Replies to another post.
    Reply,
    /// Part of a self-thread.
    Thread,
    /// A link with almost no commentary — the common case for a bookmark.
    Link,
}

impl Shape {
    /// Whether this shape needs the quoted or parent post fetched to make
    /// sense on its own.
    ///
    /// `Link` deliberately does not, even when it is a quote: a bare shared
    /// link is already a complete bookmark, and fetching the quoted post
    /// doubles the network cost of the most common item in the corpus.
    #[must_use]
    pub const fn needs_context(self) -> bool {
        matches!(self, Self::Quote | Self::Reply | Self::Thread)
    }
}

/// The overall tone, as a five-way label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Sentiment {
    /// Positive.
    Positive,
    /// Negative.
    Negative,
    /// Neither.
    #[default]
    Neutral,
    /// Intended to be funny.
    Humorous,
    /// Intended to provoke disagreement.
    Controversial,
}

impl Sentiment {
    /// Parse from the label a model produced.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "positive" | "pos" => Self::Positive,
            "negative" | "neg" => Self::Negative,
            "humorous" | "humour" | "funny" => Self::Humorous,
            "controversial" => Self::Controversial,
            _ => Self::Neutral,
        }
    }

    /// The label as it appears in output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Positive => "positive",
            Self::Negative => "negative",
            Self::Neutral => "neutral",
            Self::Humorous => "humorous",
            Self::Controversial => "controversial",
        }
    }
}

/// The zero-cost facts about a bookmark.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Entities {
    /// Hashtags, without the `#`, lowercased.
    pub hashtags: BTreeSet<String>,
    /// @-mentions, without the `@`, lowercased.
    pub mentions: BTreeSet<String>,
    /// Hostnames of the links, lowercased, de-duplicated. Suffix-matched
    /// against the tool table, so `xyz.github.io` resolves to GitHub.
    pub domains: BTreeSet<String>,
    /// Display names of recognised products and services, from the tool table.
    pub tools: BTreeSet<String>,
    /// Non-tool domains worth a category of their own: a paper host, a news
    /// outlet, a documentation site. Kept separate from `tools` so the
    /// evaluation model can be told "this is a product" versus "this is a
    /// publication".
    pub publications: BTreeSet<String>,
    /// The structural shape of the post.
    pub shape: Shape,
    /// Predicted tone, if a model has run.
    #[serde(default, skip_serializing_if = "is_default_sentiment")]
    pub sentiment: Sentiment,
    /// People named in the text, if a model has run.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub people: BTreeSet<String>,
    /// Companies or products named in the text, if a model has run.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub companies: BTreeSet<String>,
}

// `serde` hands `skip_serializing_if` a reference, so this cannot take the
// value by value the way clippy would otherwise prefer.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_sentiment(s: &Sentiment) -> bool {
    matches!(s, Sentiment::Neutral)
}

impl Entities {
    /// Whether anything at all was found.
    ///
    /// An empty result means the extractor ran and found nothing, which is
    /// different from never having run. That distinction is what makes
    /// `entities_at` a usable resume cursor.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hashtags.is_empty()
            && self.mentions.is_empty()
            && self.domains.is_empty()
            && self.tools.is_empty()
            && self.publications.is_empty()
            && self.people.is_empty()
            && self.companies.is_empty()
            && self.shape == Shape::Original
    }

    /// Every recognised name, tools and publications together, for building a
    /// single prompt field.
    pub fn all_names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().chain(self.publications.iter()).map(String::as_str)
    }

    /// A compact, deterministic rendering for prompts and index entries.
    ///
    /// Sets serialise in sorted order, so the same bookmark always produces
    /// byte-identical output. That matters because this string is hashed into
    /// the fingerprint: a non-deterministic ordering would make near-duplicate
    /// detection produce different results for identical content.
    #[must_use]
    pub fn to_context_line(&self) -> String {
        let mut parts = Vec::new();
        if !self.hashtags.is_empty() {
            parts.push(format!("#{}", self.hashtags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" #")));
        }
        if !self.mentions.is_empty() {
            parts.push(
                self.mentions.iter().map(|m| format!("@{m}")).collect::<Vec<_>>().join(" "),
            );
        }
        if !self.tools.is_empty() {
            parts.push(self.tools.iter().cloned().collect::<Vec<_>>().join(", "));
        }
        if !self.publications.is_empty() {
            parts.push(self.publications.iter().cloned().collect::<Vec<_>>().join(", "));
        }
        if self.sentiment != Sentiment::Neutral {
            parts.push(self.sentiment.label().to_owned());
        }
        if parts.is_empty() { String::new() } else { parts.join(" | ") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated() -> Entities {
        let mut e = Entities::default();
        e.hashtags.insert("rust".into());
        e.hashtags.insert("simd".into());
        e.mentions.insert("simonw".into());
        e.domains.insert("github.com".into());
        e.tools.insert("GitHub".into());
        e.publications.insert("Hacker News".into());
        e.sentiment = Sentiment::Humorous;
        e
    }

    #[test]
    fn a_fresh_extraction_is_empty() {
        assert!(Entities::default().is_empty());
    }

    #[test]
    fn a_shape_alone_counts_as_extracted() {
        let e = Entities { shape: Shape::Quote, ..Default::default() };
        assert!(!e.is_empty(), "a quote is a finding even with no names");
    }

    #[test]
    fn the_context_line_is_deterministic() {
        let a = populated();
        let mut b = populated();
        // Rebuild with the opposite insertion order.
        b.hashtags.clear();
        b.hashtags.insert("simd".into());
        b.hashtags.insert("rust".into());
        assert_eq!(a.to_context_line(), b.to_context_line());
    }

    #[test]
    fn the_context_line_includes_every_populated_field() {
        let line = populated().to_context_line();
        assert!(line.contains("#rust"), "{line}");
        assert!(line.contains("#simd"), "{line}");
        assert!(line.contains("@simonw"), "{line}");
        assert!(line.contains("GitHub"), "{line}");
        assert!(line.contains("Hacker News"), "{line}");
        assert!(line.contains("humorous"), "{line}");
    }

    #[test]
    fn the_context_line_is_empty_when_nothing_was_found() {
        assert_eq!(Entities::default().to_context_line(), "");
    }

    #[test]
    fn neutral_sentiment_is_omitted_from_serialised_form() {
        let json = serde_json::to_string(&Entities::default()).unwrap();
        assert!(!json.contains("sentiment"), "{json}");
    }

    #[test]
    fn sentiment_parsing_is_forgiving() {
        assert_eq!(Sentiment::parse("Funny"), Sentiment::Humorous);
        assert_eq!(Sentiment::parse(" POS "), Sentiment::Positive);
        assert_eq!(Sentiment::parse("gibberish"), Sentiment::Neutral);
    }

    #[test]
    fn only_context_needing_shapes_request_the_parent_post() {
        assert!(Shape::Quote.needs_context());
        assert!(Shape::Reply.needs_context());
        assert!(!Shape::Original.needs_context());
        assert!(!Shape::Link.needs_context());
    }
}
