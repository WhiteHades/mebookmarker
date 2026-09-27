//! the describe stage: the one that writes prose.
//!
//! this is tier three, and it is the only stage in the pipeline that calls a
//! model to produce text rather than to pick a name. the cost is not a per-item
//! fraction of a cent; it is one process launch and however long the agent
//! takes. that is why it runs last, why it is off unless an agent is
//! configured, and why it is the only stage with a batch size of one.

use std::sync::Arc;

use mbm_agent::{Driver, describe_prompt};
use mbm_core::bookmark::Bookmark;
use mbm_core::error::{Error, Result};
use mbm_core::port::{EnrichStage, Enricher};
use serde::{Deserialize, Serialize};

/// what the agent is asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Described {
    /// a short name for the item.
    #[serde(default)]
    pub title: String,
    /// one sentence on what it is for.
    #[serde(default)]
    pub summary: String,
}

/// the describe stage.
#[derive(Debug, Clone)]
pub struct Describe {
    driver: Arc<Driver>,
    /// only describe items this long or shorter, in characters.
    ///
    /// a post that is already a paragraph needs nothing added to it, and the
    /// posts worth describing are the ones with a link and a sentence.
    max_chars: usize,
}

impl Describe {
    /// build the stage.
    #[must_use]
    pub fn new(driver: Driver) -> Self {
        Self { driver: Arc::new(driver), max_chars: 2000 }
    }

    /// change the length above which an item is left alone.
    #[must_use]
    pub fn with_max_chars(mut self, max: usize) -> Self {
        self.max_chars = max.max(1);
        self
    }

    /// whether a bookmark is worth asking about.
    ///
    /// three cases are skipped: something that already has a title, something
    /// long enough that its own text is the description, and something with no
    /// text at all to describe.
    #[must_use]
    pub fn is_worth_describing(&self, bookmark: &Bookmark) -> bool {
        if bookmark.title.as_deref().is_some_and(|t| !t.trim().is_empty()) {
            return false;
        }
        let text = bookmark.text.trim();
        if text.is_empty() || text.chars().count() > self.max_chars {
            return false;
        }
        true
    }

    /// ask the agent about one bookmark.
    pub async fn describe(&self, bookmark: &Bookmark) -> Result<Described> {
        let prompt = describe_prompt(bookmark);
        let answer: Described = self.driver.ask_json(&prompt).await.map_err(|e| {
            Error::Agent(format!("describing {}: {e}", bookmark.source.external_id))
        })?;
        Ok(answer)
    }
}

#[async_trait::async_trait]
impl Enricher for Describe {
    fn stage(&self) -> EnrichStage {
        EnrichStage::Describe
    }

    fn max_batch(&self) -> usize {
        1
    }

    async fn enrich(&self, bookmark: &mut Bookmark) -> Result<()> {
        if !self.is_worth_describing(bookmark) {
            return Ok(());
        }
        let described = self.describe(bookmark).await?;
        if let Some(title) = clean(&described.title, 120) {
            bookmark.title = Some(title);
        }
        if let Some(summary) = clean(&described.summary, 400) {
            // the store reads a bookmark's summary from its first link, which
            // is also the one the description is about. a bookmark with no link
            // at all gets an empty one to hold the summary, rather than the
            // summary going somewhere the store never looks.
            if let Some(link) = bookmark.links.first_mut() {
                link.summary = Some(summary);
            } else {
                let url = bookmark
                    .url
                    .clone()
                    .unwrap_or_else(|| url::Url::parse("about:blank").expect("valid"));
                bookmark.links.push(mbm_core::bookmark::Link {
                    original: url.clone(),
                    resolved: url,
                    kind: mbm_core::medium::LinkKind::Unknown,
                    title: None,
                    body: None,
                    summary: Some(summary),
                    blocked: None,
                });
            }
        }
        Ok(())
    }
}

/// tidy a generated string: trim, drop a wrapping quote, cap the length.
fn clean(raw: &str, max: usize) -> Option<String> {
    let trimmed = raw.trim().trim_matches('"').trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(if trimmed.chars().count() > max {
        let cut: String = trimmed.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    } else {
        trimmed.to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::bookmark::SourceRef;
    use mbm_core::medium::SourceMedium;

    fn one(text: &str) -> Bookmark {
        Bookmark::new(SourceRef::new(SourceMedium::X, "1", None), text, 0)
    }

    fn stage() -> Describe {
        Describe::new(Driver::new(mbm_agent::Agent::Opencode))
    }

    #[test]
    fn a_short_post_with_no_title_is_worth_describing() {
        assert!(stage().is_worth_describing(&one("a short post")));
    }

    #[test]
    fn something_that_already_has_a_title_is_left_alone() {
        let mut b = one("a short post");
        b.title = Some("already named".to_owned());
        assert!(!stage().is_worth_describing(&b));
    }

    #[test]
    fn a_post_long_enough_to_be_its_own_description_is_left_alone() {
        let long = "x".repeat(3000);
        assert!(!stage().is_worth_describing(&one(&long)));
    }

    #[test]
    fn an_empty_post_is_left_alone() {
        assert!(!stage().is_worth_describing(&one("   \n  ")));
    }

    #[test]
    fn the_threshold_is_adjustable() {
        let b = one(&"y".repeat(100));
        assert!(stage().is_worth_describing(&b));
        assert!(!stage().with_max_chars(50).is_worth_describing(&b));
    }

    #[test]
    fn a_generated_string_is_trimmed_and_unquoted() {
        assert_eq!(clean("  \"a title\"  ", 70).as_deref(), Some("a title"));
        assert_eq!(clean("", 70), None);
        assert_eq!(clean("   ", 70), None);
    }

    #[test]
    fn a_generated_string_is_capped() {
        let long = "z".repeat(200);
        let capped = clean(&long, 70).unwrap();
        assert!(capped.chars().count() <= 70);
        assert!(capped.ends_with('…'));
    }

    #[test]
    fn the_stage_declares_itself_a_batch_of_one() {
        // a process launch per item is the cost, so there is no batching
        assert_eq!(stage().max_batch(), 1);
        assert!(stage().stage().is_remote());
    }

    #[tokio::test]
    async fn an_item_that_needs_nothing_is_never_asked_about() {
        let mut b = one("   ");
        // an agent is not installed in the test environment, and this must not
        // reach one: the stage returns before it would
        stage().enrich(&mut b).await.unwrap();
        assert!(b.title.is_none());
    }
}
