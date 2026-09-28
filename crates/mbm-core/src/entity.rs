//! facts pulled out of a bookmark without spending a token.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
    #[default]
    Original,

    Quote,

    Reply,

    Thread,

    Link,
}

impl Shape {
    #[must_use]
    pub const fn needs_context(self) -> bool {
        matches!(self, Self::Quote | Self::Reply | Self::Thread)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Sentiment {
    Positive,

    Negative,

    #[default]
    Neutral,

    Humorous,

    Controversial,
}

impl Sentiment {
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Entities {
    pub hashtags: BTreeSet<String>,

    pub mentions: BTreeSet<String>,

    pub domains: BTreeSet<String>,

    pub tools: BTreeSet<String>,

    pub publications: BTreeSet<String>,

    pub shape: Shape,

    #[serde(default, skip_serializing_if = "is_default_sentiment")]
    pub sentiment: Sentiment,

    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub people: BTreeSet<String>,

    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub companies: BTreeSet<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_sentiment(s: &Sentiment) -> bool {
    matches!(s, Sentiment::Neutral)
}

impl Entities {
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

    pub fn all_names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().chain(self.publications.iter()).map(String::as_str)
    }

    #[must_use]
    pub fn to_context_line(&self) -> String {
        let mut parts = Vec::new();
        if !self.hashtags.is_empty() {
            parts.push(format!(
                "#{}",
                self.hashtags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" #")
            ));
        }
        if !self.mentions.is_empty() {
            parts.push(self.mentions.iter().map(|m| format!("@{m}")).collect::<Vec<_>>().join(" "));
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
