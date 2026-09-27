//! tagging and categorising: the two stages that cost a fraction of a cent.
//!
//! both are a single typed question against the gateway, which is the shape the
//! whole design is built around. the model is never asked to write anything; it
//! picks a name from a list that was decided beforehand, and a probability comes
//! back with it. that is what makes the cost knowable in advance and the output
//! usable as a query.
//!
//! the rules in the taxonomy are tried first, because a rule costs nothing and
//! a person who wrote `github.com → engineering` means it exactly. the model
//! only sees items the rules did not claim.

use std::collections::BTreeSet;
use std::sync::Arc;

use mbm_core::bookmark::{Assigner, Bookmark, CategoryAssignment};
use mbm_core::error::{Error, Result};
use mbm_core::matcher::CompiledTaxonomy;
use mbm_core::port::{EnrichStage, Enricher};
use mbm_jev::{Jev, Question};
use serde_json::json;

/// the confidence below which a model's answer is thrown away.
///
/// below this the model is saying it does not know, and storing a
/// low-confidence category makes every later query worse rather than better.
pub const CONFIDENCE_FLOOR: f32 = 0.35;

/// the tagging stage.
#[derive(Debug, Clone)]
pub struct Tagger {
    jev: Option<Jev>,
    /// tags offered to the model, in the order it should prefer them.
    vocabulary: Arc<Vec<String>>,
    /// how many tags one bookmark may end up with.
    max_tags: usize,
}

impl Tagger {
    /// build the stage with a gateway key.
    pub fn new(api_key: Option<String>, vocabulary: Vec<String>) -> Result<Self> {
        let jev = api_key.map(Jev::new).transpose()?;
        Ok(Self { jev, vocabulary: Arc::new(vocabulary), max_tags: 6 })
    }

    /// build the stage from the environment.
    pub fn from_env(vocabulary: Vec<String>) -> Result<Self> {
        let jev = Jev::from_env().ok();
        Ok(Self { jev, vocabulary: Arc::new(vocabulary), max_tags: 6 })
    }

    /// change how many tags one bookmark may end up with.
    #[must_use]
    pub fn with_max_tags(mut self, max: usize) -> Self {
        self.max_tags = max.max(1);
        self
    }

    /// the tags the model would add, before the store's tags are merged in.
    #[must_use]
    pub fn candidate_tags(bookmark: &Bookmark) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(handle) = &bookmark.author {
            out.push(format!("@{}", handle.handle));
        }
        for tag in &bookmark.tags {
            if !tag.starts_with('@') && !out.contains(tag) {
                out.push(tag.clone());
            }
        }
        for link in &bookmark.links {
            if let Some(host) = link.resolved.host_str()
                && !out.iter().any(|t| t == host)
            {
                out.push(host.to_owned());
            }
        }
        out
    }
}

#[async_trait::async_trait]
impl Enricher for Tagger {
    fn stage(&self) -> EnrichStage {
        EnrichStage::Tags
    }

    async fn enrich(&self, bookmark: &mut Bookmark) -> Result<()> {
        let mut extra: BTreeSet<String> = BTreeSet::new();

        // the author and the hosts are facts about the bookmark, so they are
        // tags whatever a model says
        if let Some(handle) = &bookmark.author {
            extra.insert(format!("@{}", handle.handle));
        }
        for host in hosts(bookmark) {
            extra.insert(host);
        }
        if let Some(url) = &bookmark.url
            && let Some(host) = url.host_str()
        {
            extra.insert(host.to_owned());
        }

        if let Some(jev) = &self.jev
            && !self.vocabulary.is_empty()
        {
            let state = json!({
                "text": bookmark.text.chars().take(800).collect::<String>(),
                "title": bookmark.title.clone().unwrap_or_default(),
                "tags": Self::candidate_tags(bookmark),
            });
            let options = self
                .vocabulary
                .iter()
                .take(24)
                .map(|t| (t.clone(), t.clone()))
                .collect::<BTreeMapAlias>();

            let answer = jev
                .ask(
                    &state,
                    "topics",
                    Question::choice(
                        "Which of these topics best describe this bookmark? Choose the one \
                         that a person would most likely file it under. Choose `none` if none of \
                         them fit.",
                        options,
                    ),
                )
                .await;

            if let Ok(answer) = answer
                && let Some(chosen) = answer.choice()
                && chosen != "none"
                && answer.probability_of(chosen).is_none_or(|p| p >= f64::from(CONFIDENCE_FLOOR))
            {
                extra.insert(chosen.to_owned());
            }
        }

        for tag in extra {
            bookmark.push_tag(&tag);
        }
        if bookmark.tags.len() > self.max_tags * 4 {
            // a bookmark with hundreds of tags is a page of links, not a subject
            let trimmed: Vec<String> =
                bookmark.tags.iter().take(self.max_tags * 4).cloned().collect();
            bookmark.tags = trimmed.into_iter().collect();
        }
        Ok(())
    }
}

/// the hosts a bookmark's links point at, in the order they appear.
#[must_use]
pub fn hosts(bookmark: &Bookmark) -> Vec<String> {
    let mut out = Vec::new();
    for link in &bookmark.links {
        if let Some(host) = link.resolved.host_str()
            && !out.iter().any(|h| h == host)
        {
            out.push(host.to_owned());
        }
    }
    out
}

// the choice question takes a sorted map, and a `BTreeMap` reads better here
// than the alias that keeps the signature off this file
type BTreeMapAlias = std::collections::BTreeMap<String, String>;

/// the categorising stage.
#[derive(Debug, Clone)]
pub struct Categorizer {
    jev: Option<Jev>,
    taxonomy: Arc<CompiledTaxonomy>,
    categories: Arc<Vec<(String, String)>>,
}

impl Categorizer {
    /// build the stage with a gateway key.
    pub fn new(api_key: Option<String>, taxonomy: &mbm_core::category::Taxonomy) -> Result<Self> {
        let jev = api_key.map(Jev::new).transpose()?;
        let categories = taxonomy
            .categories
            .values()
            .map(|c| (c.slug.clone(), format!("{}: {}", c.name, c.description)))
            .collect();
        Ok(Self {
            jev,
            taxonomy: Arc::new(CompiledTaxonomy::new(taxonomy)),
            categories: Arc::new(categories),
        })
    }

    /// build the stage from the environment.
    pub fn from_env(taxonomy: &mbm_core::category::Taxonomy) -> Result<Self> {
        let jev = Jev::from_env().ok();
        let categories = taxonomy
            .categories
            .values()
            .map(|c| (c.slug.clone(), format!("{}: {}", c.name, c.description)))
            .collect();
        Ok(Self {
            jev,
            taxonomy: Arc::new(CompiledTaxonomy::new(taxonomy)),
            categories: Arc::new(categories),
        })
    }

    /// the categories the rules claim for this bookmark, with the rule's
    /// confidence standing in for a model's.
    #[must_use]
    pub fn by_rule(&self, bookmark: &Bookmark) -> Vec<CategoryAssignment> {
        let mut out: Vec<CategoryAssignment> = Vec::new();
        for link in &bookmark.links {
            let (slug, _) = self.taxonomy.route(link.resolved.as_str());
            if *slug == *self.taxonomy.fallback() && out.is_empty() {
                continue;
            }
            if out.iter().any(|a| a.slug == *slug) {
                continue;
            }
            out.push(CategoryAssignment {
                slug: slug.to_string(),
                confidence: 1.0,
                assigned_by: Assigner::Rule,
            });
        }
        out
    }

    /// the slug the fallback names.
    #[must_use]
    pub fn fallback(&self) -> &str {
        self.taxonomy.fallback()
    }
}

#[async_trait::async_trait]
impl Enricher for Categorizer {
    fn stage(&self) -> EnrichStage {
        EnrichStage::Categorize
    }

    async fn enrich(&self, bookmark: &mut Bookmark) -> Result<()> {
        let by_rule = self.by_rule(bookmark);
        if !by_rule.is_empty() {
            bookmark.categories = by_rule;
            return Ok(());
        }

        let Some(jev) = &self.jev else { return Ok(()) };
        if self.categories.is_empty() {
            return Ok(());
        }

        let mut options: std::collections::BTreeMap<String, String> =
            self.categories.iter().cloned().collect();
        options.insert("none".to_owned(), "none of the categories fit this bookmark".to_owned());

        let state = json!({
            "text": bookmark.text.chars().take(800).collect::<String>(),
            "title": bookmark.title.clone().unwrap_or_default(),
            "url": bookmark.url.as_ref().map(url::Url::as_str).unwrap_or_default(),
        });

        let answer = jev
            .ask(
                &state,
                "category",
                Question::choice("Which single category does this bookmark belong in?", options),
            )
            .await
            .map_err(|e| Error::Jev(e.to_string()))?;

        let chosen = answer.choice().unwrap_or("none").to_owned();
        if chosen == "none" {
            bookmark.categories = vec![CategoryAssignment {
                slug: self.fallback().to_owned(),
                confidence: 0.5,
                assigned_by: Assigner::Jev,
            }];
            return Ok(());
        }

        let confidence =
            answer.probability_of(&chosen).unwrap_or(f64::from(CONFIDENCE_FLOOR)) as f32;
        // a model that says it is guessing gets the fallback and a confidence
        // that says so
        if confidence < CONFIDENCE_FLOOR {
            bookmark.categories = vec![CategoryAssignment {
                slug: self.fallback().to_owned(),
                confidence,
                assigned_by: Assigner::Jev,
            }];
            return Ok(());
        }
        bookmark.categories =
            vec![CategoryAssignment { slug: chosen, confidence, assigned_by: Assigner::Jev }];
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::bookmark::{Link, SourceRef};
    use mbm_core::category::{Category, CategoryRule, Taxonomy};
    use mbm_core::medium::SourceMedium;
    use url::Url;

    fn one(text: &str) -> Bookmark {
        Bookmark::new(SourceRef::new(SourceMedium::X, "1", None), text, 0)
    }

    fn with_link(bookmark: &mut Bookmark, url: &str) {
        let parsed = Url::parse(url).unwrap();
        bookmark.links.push(Link {
            original: parsed.clone(),
            resolved: parsed,
            kind: mbm_core::medium::LinkKind::Article,
            title: None,
            body: None,
            summary: None,
            blocked: None,
        });
    }

    fn taxonomy() -> Taxonomy {
        let mut t = Taxonomy::empty();
        t.insert(Category::new("engineering", "Engineering", "#ff0000", "code and tools"));
        t.insert(Category::new("reading", "Reading", "#00ff00", "things to read"));
        t.insert(Category::new("general", "General", "#0000ff", "everything else"));
        t.fallback = "general".to_owned();
        t.rules.push(CategoryRule {
            slug: "engineering".to_owned(),
            match_all: Vec::new(),
            match_any: vec!["github.com".to_owned(), "arxiv.org".to_owned()],
            action: None,
        });
        t
    }

    #[tokio::test]
    async fn without_a_gateway_the_tags_come_from_the_bookmark_alone() {
        let mut b = one("a post");
        with_link(&mut b, "https://github.com/simonw/llm");
        Tagger::new(None, Vec::new()).unwrap().enrich(&mut b).await.unwrap();
        assert!(b.tags.contains("github.com"));
        assert!(b.tags.contains("github.com"));
    }

    #[tokio::test]
    async fn an_author_handle_becomes_a_tag() {
        let mut b = one("a post");
        b.author = Some(mbm_core::bookmark::Author::new("simonw"));
        Tagger::new(None, Vec::new()).unwrap().enrich(&mut b).await.unwrap();
        assert!(b.tags.contains("@simonw"));
    }

    #[tokio::test]
    async fn a_rule_beats_the_model() {
        let mut b = one("a repo");
        with_link(&mut b, "https://github.com/simonw/llm");
        Categorizer::new(None, &taxonomy()).unwrap().enrich(&mut b).await.unwrap();
        assert_eq!(b.categories.len(), 1);
        assert_eq!(b.categories[0].slug, "engineering");
        assert_eq!(b.categories[0].assigned_by, Assigner::Rule);
        // a rule claims outright, so its confidence is exactly one
        assert!((b.categories[0].confidence - 1.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn an_unmatched_bookmark_gets_no_category_without_a_gateway() {
        let mut b = one("a thought");
        with_link(&mut b, "https://example.com/thing");
        Categorizer::new(None, &taxonomy()).unwrap().enrich(&mut b).await.unwrap();
        assert!(b.categories.is_empty(), "a guess would be worse than nothing");
    }

    #[test]
    fn two_links_to_the_same_host_categorise_once() {
        let mut b = one("two links");
        with_link(&mut b, "https://github.com/a/one");
        with_link(&mut b, "https://github.com/b/two");
        let found = Categorizer::new(None, &taxonomy()).unwrap().by_rule(&b);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_link_to_an_unrouted_host_claims_nothing() {
        let mut b = one("one link");
        with_link(&mut b, "https://example.com/thing");
        assert!(Categorizer::new(None, &taxonomy()).unwrap().by_rule(&b).is_empty());
    }

    #[test]
    fn the_fallback_is_whatever_the_taxonomy_names() {
        let mut t = taxonomy();
        t.fallback = "reading".to_owned();
        assert_eq!(Categorizer::new(None, &t).unwrap().fallback(), "reading");
    }

    #[tokio::test]
    async fn a_post_with_a_hundred_tags_is_trimmed() {
        let mut b = one("a post");
        for i in 0..100 {
            b.push_tag(format!("tag{i}"));
        }
        Tagger::new(None, Vec::new()).unwrap().with_max_tags(2).enrich(&mut b).await.unwrap();
        assert_eq!(b.tags.len(), 8, "four times the cap, and no more");
    }

    #[test]
    fn the_stages_declare_themselves_remote() {
        assert!(Tagger::new(None, Vec::new()).unwrap().stage().is_remote());
        assert!(Categorizer::new(None, &taxonomy()).unwrap().stage().is_remote());
    }

    #[test]
    fn candidate_tags_offer_the_model_its_grounding() {
        let mut b = one("a post");
        with_link(&mut b, "https://arxiv.org/abs/1234");
        b.push_tag("rust");
        let candidates = Tagger::candidate_tags(&b);
        assert!(candidates.iter().any(|c| c == "rust"));
        assert!(candidates.iter().any(|c| c == "arxiv.org"));
    }
}
