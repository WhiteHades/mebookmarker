//! media understanding: alt text, dimensions, and what needs a description.
//!
//! the cheap half is a source's own metadata. x has carried `ext_alt_text` on
//! every photo since 2023, reddit gives a title and a thumbnail size, and a
//! podcast enclosure declares its duration and mime type. reading that costs a
//! request and no model, and it covers the large majority of media a bookmark
//! archive actually holds.
//!
//! the expensive half is a description of the image itself. that is prose, so
//! it belongs to the local agent rather than to a typed decision, and the gate
//! in front of it is a typed one: a `boolean` question that costs a fraction of
//! a cent and keeps the agent off the images whose alt text is already enough.

use mbm_core::bookmark::{Bookmark, MediaKind};
use mbm_core::error::Result;
use mbm_core::port::{EnrichStage, Enricher};
use mbm_jev::{Jev, Question};
use serde_json::json;

/// the tag a bookmark carries when its media has no usable description.
pub const NEEDS_DESCRIPTION: &str = "needs-description";

/// the media stage.
#[derive(Debug, Clone)]
pub struct Vision {
    jev: Option<Jev>,
    /// the probability above which an image is worth describing.
    threshold: f64,
    /// the largest number of media per bookmark that may be described.
    budget: usize,
}

impl Vision {
    /// build the stage with a gateway key.
    pub fn new(api_key: Option<String>) -> Result<Self> {
        let jev = api_key.map(Jev::new).transpose()?;
        Ok(Self { jev, threshold: 0.5, budget: 4 })
    }

    /// build the stage from the environment.
    ///
    /// an absent key leaves the gate off, which means the source's own alt text
    /// is all that gets recorded. that is the right behaviour for an install
    /// with no gateway, and it is why this is a `Result` rather than a panic.
    pub fn from_env() -> Result<Self> {
        let jev = Jev::from_env().ok();
        Ok(Self { jev, threshold: 0.5, budget: 4 })
    }

    /// change the probability above which an image is described.
    #[must_use]
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    /// change how many media per bookmark may be described.
    #[must_use]
    pub fn with_budget(mut self, budget: usize) -> Self {
        self.budget = budget.max(1);
        self
    }

    /// whether the gate is on.
    #[must_use]
    pub fn has_gate(&self) -> bool {
        self.jev.is_some()
    }

    /// the alt text a source recorded for a piece of media, if any.
    #[must_use]
    pub fn source_alt_text(&self, raw: Option<&serde_json::Value>) -> Option<String> {
        let raw = raw?;
        for path in [
            "/ext_alt_text",
            "/ext_alt_text/0",
            "/mediaDetails/altText",
            "/alt_text",
            "/altText",
            "/description",
        ] {
            if let Some(text) = raw.pointer(path).and_then(|v| v.as_str())
                && !text.trim().is_empty()
            {
                return Some(text.trim().to_owned());
            }
        }
        None
    }
}

#[async_trait::async_trait]
impl Enricher for Vision {
    fn stage(&self) -> EnrichStage {
        EnrichStage::Vision
    }

    async fn enrich(&self, bookmark: &mut Bookmark) -> Result<()> {
        if bookmark.media.is_empty() {
            return Ok(());
        }

        let mut described = 0usize;
        let mut unknown = Vec::new();

        for media in &bookmark.media {
            // a description longer than a phrase is a description
            if media.alt_text.as_deref().is_some_and(|a| a.len() > 24) {
                described += 1;
                continue;
            }
            if described + unknown.len() >= self.budget {
                continue;
            }
            unknown.push(media.url.clone());
        }
        if described + unknown.len() > described {
            // something in this bookmark's media is undescribed
            bookmark.push_tag(NEEDS_DESCRIPTION);
        }

        // the gate is a single `boolean` question about the whole bookmark, so
        // an item with four images costs one decision rather than four
        let Some(jev) = &self.jev else { return Ok(()) };
        if unknown.is_empty() {
            return Ok(());
        }

        let state = json!({
            "text": bookmark.text.chars().take(600).collect::<String>(),
            "media": unknown.iter().map(url::Url::as_str).collect::<Vec<_>>(),
            "alt_text": bookmark
                .media
                .iter()
                .filter_map(|m| m.alt_text.clone())
                .collect::<Vec<_>>(),
        });

        let answer = jev
            .ask(
                &state,
                "worth_describing",
                Question::boolean_with(
                    "The user bookmarked this post for its images, and the images have no \
                     usable description. Answer true only if a person reading the archive \
                     later would want a written description of what the images show.",
                    "the images have no alt text, or it says less than a short phrase",
                    "the alt text already describes the images, or the text explains them",
                ),
            )
            .await;

        if let Ok(answer) = answer
            && answer.probability_of("true").is_some_and(|p| p >= self.threshold)
        {
            bookmark.push_tag("describe-media");
        }
        Ok(())
    }
}

/// a media attachment's shape, for reporting and for the sinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// what kind it is.
    pub kind: MediaKind,
    /// whether it carries a description.
    pub described: bool,
}

/// the media shapes on a bookmark.
#[must_use]
pub fn shapes(bookmark: &Bookmark) -> Vec<Shape> {
    bookmark
        .media
        .iter()
        .map(|m| Shape {
            kind: m.kind,
            described: m.alt_text.as_deref().is_some_and(|a| a.len() > 24),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::bookmark::{Media, SourceRef};
    use mbm_core::medium::SourceMedium;
    use url::Url;

    fn photo(alt: Option<&str>) -> Media {
        Media {
            kind: MediaKind::Photo,
            url: Url::parse("https://pbs.twimg.com/media/a.jpg").unwrap(),
            preview_url: None,
            width: Some(1200),
            height: Some(800),
            duration_ms: None,
            alt_text: alt.map(str::to_owned),
        }
    }

    fn with_media(media: Vec<Media>) -> Bookmark {
        let mut b = Bookmark::new(SourceRef::new(SourceMedium::X, "1", None), "a post", 0);
        b.media = media;
        b
    }

    #[tokio::test]
    async fn a_bookmark_with_no_media_is_left_alone() {
        let mut b = with_media(Vec::new());
        Vision::new(None).unwrap().enrich(&mut b).await.unwrap();
        assert!(!b.tags.contains(NEEDS_DESCRIPTION));
    }

    #[tokio::test]
    async fn a_described_image_needs_nothing_further() {
        let mut b = with_media(vec![photo(Some("a chart of the migration over time"))]);
        Vision::new(None).unwrap().enrich(&mut b).await.unwrap();
        assert!(!b.tags.contains(NEEDS_DESCRIPTION), "{:?}", b.tags);
    }

    #[tokio::test]
    async fn an_undescribed_image_is_flagged() {
        let mut b = with_media(vec![photo(None)]);
        Vision::new(None).unwrap().enrich(&mut b).await.unwrap();
        assert!(b.tags.contains(NEEDS_DESCRIPTION));
    }

    #[tokio::test]
    async fn a_two_character_alt_text_is_too_short_to_count() {
        let mut b = with_media(vec![photo(Some("a cat"))]);
        Vision::new(None).unwrap().enrich(&mut b).await.unwrap();
        assert!(b.tags.contains(NEEDS_DESCRIPTION), "\"a cat\" says almost nothing");
    }

    #[tokio::test]
    async fn the_per_bookmark_budget_is_respected() {
        let media: Vec<Media> = (0..10)
            .map(|i| {
                let mut m = photo(None);
                m.url = Url::parse(&format!("https://pbs.twimg.com/media/{i}.jpg")).unwrap();
                m
            })
            .collect();
        let mut b = with_media(media);
        Vision::new(None).unwrap().with_budget(3).enrich(&mut b).await.unwrap();
        assert!(b.tags.contains(NEEDS_DESCRIPTION));
    }

    #[tokio::test]
    async fn without_a_gateway_the_gate_stays_off() {
        let stage = Vision::new(None).unwrap();
        assert!(!stage.has_gate());
        let mut b = with_media(vec![photo(None)]);
        stage.enrich(&mut b).await.unwrap();
        assert!(b.tags.contains(NEEDS_DESCRIPTION));
        assert!(!b.tags.contains("describe-media"), "the gate was never asked");
    }

    #[test]
    fn alt_text_is_read_from_wherever_the_source_put_it() {
        let stage = Vision::new(None).unwrap();
        let x = json!({"ext_alt_text": "a photo of a cat"});
        assert_eq!(stage.source_alt_text(Some(&x)).as_deref(), Some("a photo of a cat"));
        let reddit = json!({"alt_text": "another cat"});
        assert_eq!(stage.source_alt_text(Some(&reddit)).as_deref(), Some("another cat"));
        assert_eq!(stage.source_alt_text(Some(&json!({}))), None);
        assert_eq!(stage.source_alt_text(None), None);
    }

    #[test]
    fn an_empty_alt_text_is_no_alt_text() {
        let stage = Vision::new(None).unwrap();
        assert_eq!(stage.source_alt_text(Some(&json!({"ext_alt_text": "   "}))), None);
    }

    #[test]
    fn the_stage_declares_itself_remote() {
        assert!(Vision::new(None).unwrap().stage().is_remote());
    }

    #[test]
    fn shapes_report_what_a_sink_needs() {
        let mut b = with_media(vec![photo(Some("a long enough description here"))]);
        b.media.push(photo(None));
        let shapes = shapes(&b);
        assert!(shapes[0].described);
        assert!(!shapes[1].described);
    }
}
