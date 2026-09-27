//! categories and the url rules that route bookmarks to them.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    File,

    #[default]
    Capture,

    Defer,
}

impl FromStr for Action {
    type Err = crate::Error;

    fn from_str(s: &str) -> crate::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "file" | "note" => Ok(Self::File),
            "capture" | "timeline" => Ok(Self::Capture),
            "defer" | "transcribe" => Ok(Self::Defer),
            other => Err(crate::Error::Config(format!(
                "unknown action `{other}`: expected file, capture, or defer"
            ))),
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::File => "file",
            Self::Capture => "capture",
            Self::Defer => "defer",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Category {
    pub slug: String,

    pub name: String,

    pub color: String,

    pub description: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,

    #[serde(default)]
    pub action: Action,
}

impl Category {
    #[must_use]
    pub fn new(slug: &str, name: &str, color: &str, description: &str) -> Self {
        Self {
            slug: slug.to_owned(),
            name: name.to_owned(),
            color: normalize_color(color),
            description: description.to_owned(),
            folder: None,
            action: Action::Capture,
        }
    }

    #[must_use]
    pub fn filed_to(mut self, folder: impl Into<PathBuf>) -> Self {
        self.folder = Some(folder.into());
        self
    }

    #[must_use]
    pub fn with_action(mut self, action: Action) -> Self {
        self.action = action;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryRule {
    pub slug: String,

    #[serde(default)]
    pub match_any: Vec<String>,

    #[serde(default)]
    pub match_all: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

impl CategoryRule {
    #[must_use]
    pub fn any(slug: &str, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            slug: slug.to_owned(),
            match_any: patterns.into_iter().map(Into::into).map(|p| p.to_ascii_lowercase()).collect(),
            match_all: Vec::new(),
            action: None,
        }
    }

    #[must_use]
    pub fn requiring(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.match_all = patterns.into_iter().map(Into::into).map(|p| p.to_ascii_lowercase()).collect();
        self
    }

    #[must_use]
    pub fn with_action(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    #[must_use]
    pub fn matches(&self, target: &str) -> bool {    // strips scheme, query, and fragment first, so a pattern matches the host
    // and path only

        if self.match_any.is_empty() {
            return false;
        }
        let trimmed = target
            .trim()
            .split_once("://")
            .map_or(target.trim(), |(_, rest)| rest);
        let host_and_path = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
        let folded = host_and_path.to_ascii_lowercase();
        if !self.match_any.iter().any(|p| folded.contains(p.as_str())) {
            return false;
        }
        self.match_all.iter().all(|p| folded.contains(p.as_str()))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Taxonomy {
    #[serde(default)]
    pub categories: BTreeMap<String, Category>,

    #[serde(default)]
    pub rules: Vec<CategoryRule>,

    #[serde(default = "default_fallback")]
    pub fallback: String,
}

fn default_fallback() -> String {
    String::from("general")
}

impl Taxonomy {
    /// the taxonomy a fresh install starts with.
    ///
    /// a starter set rather than an empty one, for two reasons. a person opening
    /// the config sees what a category actually is, which is the fastest way to
    /// make them write their own. and the rules work on the first run, so
    /// `github.com` is filed under engineering before anyone has edited
    /// anything.
    #[must_use]
    pub fn default_taxonomy() -> Self {
        let mut taxonomy = Self::empty();
        for (slug, name, color, description) in [
            ("engineering", "Engineering", "#4f46e5", "code, tools, and systems"),
            ("reading", "Reading", "#0891b2", "things to read later"),
            ("writing", "Writing", "#7c3aed", "things being written"),
            ("design", "Design", "#db2777", "interfaces, type, and craft"),
            ("business", "Business", "#ca8a04", "markets, companies, and money"),
            ("science", "Science", "#16a34a", "research and findings"),
            ("news", "News", "#dc2626", "what happened"),
            ("media", "Media", "#ea580c", "video, audio, and pictures"),
        ] {
            taxonomy.insert(Category::new(slug, name, color, description));
        }
        taxonomy.insert(Category::new("general", "General", "#6b7280", "everything else"));

        for (slug, needles) in [
            ("engineering", vec!["github.com", "gitlab.com", "arxiv.org", "docs.rs", "crates.io", "developer.mozilla.org", "stackoverflow.com"]),
            ("design", vec!["figma.com", "dribbble.com", "awwwards.com", "fonts.google.com", "typewolf.com"]),
            ("business", vec!["substack.com", "stripe.com", "a16z.com", "bloomberg.com"]),
            ("science", vec!["nature.com", "science.org", "arxiv.org", "quantamagazine.org", "nasa.gov"]),
            ("news", vec!["reuters.com", "apnews.com", "bbc.com", "theverge.com", "arstechnica.com"]),
            ("media", vec!["youtube.com", "youtu.be", "vimeo.com", "spotify.com", "twitch.tv"]),
            ("reading", vec!["readwise.io", "pocket.com", "instapaper.com"]),
        ] {
            taxonomy.rules.push(CategoryRule {
                slug: slug.to_owned(),
                match_all: Vec::new(),
                match_any: needles.into_iter().map(str::to_owned).collect(),
                action: None,
            });
        }
        taxonomy
    }

    #[must_use]
    pub fn empty() -> Self {
        Self {
            categories: BTreeMap::new(),
            rules: Vec::new(),
            fallback: default_fallback(),
        }
    }

    pub fn insert(&mut self, category: Category) -> Option<Category> {
        self.categories.insert(category.slug.clone(), category)
    }

    #[must_use]
    pub fn get(&self, slug: &str) -> Option<&Category> {
        self.categories.get(slug)
    }

    #[must_use]
    pub fn route(&self, host_and_path: &str) -> (String, Action) {
        for rule in &self.rules {
            if !rule.matches(host_and_path) {
                continue;
            }
            let action = rule
                .action
                .or_else(|| self.categories.get(&rule.slug).map(|c| c.action))
                .unwrap_or(Action::Capture);
            let slug =
                if self.categories.contains_key(&rule.slug) { rule.slug.clone() } else { self.fallback.clone() };
            return (slug, action);
        }
        let action =
            self.categories.get(&self.fallback).map_or(Action::Capture, |c| c.action);
        (self.fallback.clone(), action)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Category> {
        self.categories.values()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.categories.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.categories.is_empty()
    }

    #[must_use]
    pub fn as_rubric(&self) -> String {
        let mut out = String::with_capacity(self.categories.len() * 96);
        for c in self.categories.values() {
            out.push_str("- ");
            out.push_str(&c.slug);
            out.push_str(": ");
            out.push_str(c.description.trim());
            out.push('\n');
        }
        out
    }
}

fn normalize_color(input: &str) -> String {
    let hex = input.trim().trim_start_matches('#');
    match hex.len() {
        3 => {
            let mut expanded = String::with_capacity(6);
            for c in hex.chars() {
                expanded.push(c);
                expanded.push(c);
            }
            format!("#{expanded}")
        }
        6 => format!("#{hex}"),
        _ => "#64748b".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taxonomy() -> Taxonomy {
        let mut t = Taxonomy::empty();
        t.insert(Category::new("repository", "Repository", "#06b6d4", "Source code.").filed_to("tools").with_action(Action::File));
        t.insert(Category::new("article", "Article", "#ec4899", "Written prose.").filed_to("articles").with_action(Action::File));
        t.insert(Category::new("general", "General", "64748b", "Anything else."));
        t.rules.push(CategoryRule::any("repository", ["github.com", "gitlab.com"]));
        t.rules.push(CategoryRule::any("article", ["medium.com", "substack.com", "/blog", "arxiv.org"]));
        t
    }

    #[test]
    fn rules_route_by_host_and_path() {
        let t = taxonomy();
        assert_eq!(t.route("github.com/a/b").0, "repository");
        assert_eq!(t.route("medium.com/some-post").0, "article");
    }

    #[test]
    fn routing_falls_back_when_nothing_matches() {
        let t = taxonomy();
        let (slug, action) = t.route("example.com/whatever");
        assert_eq!(slug, "general");
        assert_eq!(action, Action::Capture);
    }

    #[test]
    fn a_pattern_does_not_match_the_query_string() {
        let t = taxonomy();

        assert_eq!(t.route("example.com/x?ref=github.com").0, "general");
        assert_eq!(t.route("https://example.com/x#github.com").0, "general");

        assert_eq!(t.route("https://github.com/a/b?tab=readme").0, "repository");
    }

    #[test]
    fn match_all_narrows_a_rule() {
        let mut t = Taxonomy::empty();
        t.insert(Category::new("research", "Research", "#3b82f6", "Papers.").filed_to("papers"));
        t.rules.push(
            CategoryRule::any("research", ["arxiv.org"]).requiring(["/abs/"]),
        );
        assert_eq!(t.route("arxiv.org/abs/1234").0, "research");
        assert_eq!(t.route("arxiv.org/list/cs.AI").0, "general");
    }

    #[test]
    fn an_empty_rule_never_matches() {
        let rule = CategoryRule::any("x", Vec::<String>::new());
        assert!(!rule.matches("anything.at.all"));
    }

    #[test]
    fn a_rule_naming_an_unknown_category_falls_back_rather_than_dropping() {
        let mut t = taxonomy();
        t.rules.push(CategoryRule::any("typo-that-does-not-exist", ["example.com"]));
        assert_eq!(t.route("example.com").0, "general");
    }

    #[test]
    fn a_rule_can_override_the_category_action() {
        let mut t = Taxonomy::empty();
        t.insert(Category::new("video", "Video", "#ef4444", "Video."));
        t.rules.push(CategoryRule::any("video", ["youtube.com"]).with_action(Action::Defer));
        assert_eq!(t.route("youtube.com/watch").1, Action::Defer);
    }

    #[test]
    fn colours_are_normalised_to_six_digits() {
        assert_eq!(normalize_color("#abc"), "#aabbcc");
        assert_eq!(normalize_color("06b6d4"), "#06b6d4");
        assert_eq!(normalize_color("#06b6d4"), "#06b6d4");
        assert_eq!(normalize_color("garbage"), "#64748b");
    }

    #[test]
    fn the_rubric_lists_every_category_for_the_model() {
        let rubric = taxonomy().as_rubric();
        assert!(rubric.contains("- repository: Source code."));
        assert!(rubric.contains("- general: Anything else."));
        assert_eq!(rubric.lines().count(), 3);
    }

    #[test]
    fn actions_parse_from_their_config_spelling() {
        assert_eq!("file".parse::<Action>().unwrap(), Action::File);
        assert_eq!("transcribe".parse::<Action>().unwrap(), Action::Defer);
        assert_eq!("timeline".parse::<Action>().unwrap(), Action::Capture);
        assert!("nonsense".parse::<Action>().is_err());
    }
}
