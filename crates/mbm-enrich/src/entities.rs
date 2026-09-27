//! entity extraction: the free stage, run on every item.
//!
//! this is the only stage that is purely deterministic. it reads the text a
//! source already gave us, pulls out the things that are unambiguously there —
//! urls, `@` mentions, `#` hashtags, `$` cashtags, bare domains — and writes
//! the dedup fingerprint. no network, no model, no cost.
//!
//! running it first matters for the rest of the pipeline: the later stages all
//! read the links this one finds, and a duplicate is cheaper to spot here than
//! to describe twice.

use mbm_core::bookmark::{BlockedReason, Bookmark, Link};
use mbm_core::error::Result;
use mbm_core::medium::LinkKind;
use mbm_core::port::{EnrichStage, Enricher};
use mbm_extract::links;
use mbm_store::fingerprint;
use url::Url;

/// a prefix that marks a cashtag rather than a hashtag.
const CASHTAGS: bool = true;

/// the entity stage.
#[derive(Debug, Clone)]
pub struct Entities {
    /// refuse to record more than this many links on one bookmark.
    ///
    /// a post that quotes a thread of thirty links is one bookmark, and the
    /// links past the first dozen are almost never the point.
    max_links: usize,
}

impl Default for Entities {
    fn default() -> Self {
        Self { max_links: 24 }
    }
}

impl Entities {
    /// build the stage.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// change the per-bookmark link ceiling.
    #[must_use]
    pub fn with_max_links(mut self, max: usize) -> Self {
        self.max_links = max.max(1);
        self
    }

    /// the tokens a fingerprint is taken over.
    ///
    /// urls are dropped, because two copies of the same link in different
    /// threads should read as the same bookmark, and the url itself is not
    /// what the bookmark is about.
    #[must_use]
    pub fn fingerprint_tokens(bookmark: &Bookmark) -> Vec<String> {
        let mut text = String::with_capacity(bookmark.text.len() + 64);
        if let Some(title) = &bookmark.title {
            text.push_str(title);
            text.push(' ');
        }
        text.push_str(&bookmark.text);
        text.split_whitespace()
            .filter(|word| {
                let lower = word.to_ascii_lowercase();
                !lower.starts_with("http")
                    && !lower.contains(".com/")
                    && !lower.contains(".org/")
                    && word.chars().all(|c| c.is_alphanumeric() || c == '\'' || c == '-')
            })
            .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase())
            .filter(|word| word.len() > 2)
            .collect()
    }
}

#[async_trait::async_trait]
impl Enricher for Entities {
    fn stage(&self) -> EnrichStage {
        EnrichStage::Entities
    }

    fn max_batch(&self) -> usize {
        512
    }

    async fn enrich(&self, bookmark: &mut Bookmark) -> Result<()> {
        let mut found: Vec<Link> = Vec::new();

        for url in links::links_in(&bookmark.text) {
            if found.len() >= self.max_links {
                break;
            }
            if found.iter().any(|l| l.resolved == url) {
                continue;
            }
            found.push(Link {
                original: url.clone(),
                resolved: url.clone(),
                kind: links::classify(&url),
                title: None,
                body: None,
                summary: None,
                blocked: links::is_paywalled(&url).then_some(BlockedReason::Paywall),
            });
        }

        // a bare domain in the text, with no scheme, is still a link. the
        // runs already claimed as urls are masked out first, or `example.com/a`
        // would be found a second time as the bare host `example.com`
        let bare_source = mask(&bookmark.text, &found);
        for host in bare_domains(&bare_source) {
            if found.len() >= self.max_links {
                break;
            }
            let Ok(url) = Url::parse(&format!("https://{host}")) else { continue };
            if found.iter().any(|l| l.resolved == url) {
                continue;
            }
            found.push(Link {
                kind: links::classify(&url),
                original: url.clone(),
                resolved: url.clone(),
                title: None,
                body: None,
                summary: None,
                blocked: links::is_paywalled(&url).then_some(BlockedReason::Paywall),
            });
        }

        // keep the order the reader would meet them in, and the display order
        // matches, so a sink can write them out as ordinals without sorting
        found.sort_by(|a, b| {
            let pa = bookmark.text.find(a.original.as_str()).unwrap_or(usize::MAX);
            let pb = bookmark.text.find(b.original.as_str()).unwrap_or(usize::MAX);
            pa.cmp(&pb).then_with(|| a.resolved.as_str().cmp(b.resolved.as_str()))
        });
        bookmark.links = found;

        for handle in mentions(&bookmark.text) {
            bookmark.push_tag(format!("@{handle}"));
        }
        for tag in hashtags(&bookmark.text) {
            bookmark.push_tag(&tag);
        }
        if CASHTAGS {
            for tag in cashtags(&bookmark.text) {
                bookmark.push_tag(&tag);
            }
        }

        if bookmark.tags.contains("needs-transcript") {
            // a video's own text is not its text, so anything downstream that
            // reads the body needs the transcript stage to have run first
            bookmark.push_tag("transcript-pending");
        }

        let tokens = Self::fingerprint_tokens(bookmark);
        bookmark.fingerprint = Some(fingerprint::fingerprint(&tokens));

        Ok(())
    }
}

/// blank out every already-claimed url, leaving the rest of the text in place.
///
/// the offsets stay the same length, so a position found later still points at
/// the right place in the original string.
fn mask(text: &str, found: &[Link]) -> String {
    let mut masked: Vec<u8> = text.as_bytes().to_vec();
    for link in found {
        let raw = link.original.as_str();
        let mut at = 0usize;
        while let Some(offset) = text[at..].find(raw) {
            let start = at + offset;
            masked[start..start + raw.len()].fill(b' ');
            at = start + raw.len();
        }
    }
    String::from_utf8_lossy(&masked).into_owned()
}

/// the bare domains in a text, without their scheme or path.
#[must_use]
pub fn bare_domains(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0usize;

    while at < bytes.len() {
        let rest = &text[at..];
        let Some(offset) = rest.find('.') else { break };
        let start = at + offset;

        // walk back over the label, which must start with a letter or digit
        let mut begin = start;
        while begin > 0 {
            let c = bytes[begin - 1];
            if c.is_ascii_alphanumeric() || c == b'-' || c == b'.' {
                begin -= 1;
            } else {
                break;
            }
        }
        if begin == start {
            at = start + 1;
            continue;
        }

        // walk forward over the domain and any single label of path
        let mut end = start;
        while end < bytes.len() {
            let c = bytes[end];
            if c.is_ascii_alphanumeric() || c == b'-' || c == b'.' || c == b'_' {
                end += 1;
            } else {
                break;
            }
        }

        let candidate = &text[begin..end];
        let tld = candidate.rsplit('.').next().unwrap_or_default();
        if is_domain(candidate, tld) {
            let host = candidate.trim_matches('.').to_ascii_lowercase();
            if !out.contains(&host) {
                out.push(host);
            }
        }
        at = end.max(start + 1);
    }
    out
}

/// a tld worth treating as a domain ending.
///
/// a curated list rather than a rule. the rule that comes to mind is "any two
/// letters is a country code", and it is wrong often enough to matter:
/// `main.rs` and `report.zip` are in a sentence far more often than
/// `example.rs` is a site.
#[must_use]
pub fn is_tld(raw: &str) -> bool {
    const KNOWN: &[&str] = &[
        "com", "org", "net", "edu", "gov", "mil", "int", "io", "dev", "app", "ai", "me", "tv",
        "sh", "fm", "ly", "is", "it", "at", "so", "to", "cc", "gg", "st", "im", "one", "blog",
        "wiki", "news", "info", "biz", "cloud", "tech", "xyz", "online", "site", "page", "link",
        "click", "rss", "moe", "best", "pro", "eu", "us", "uk", "de", "fr", "jp", "ru", "br",
        "in", "cn", "au", "ca", "nl", "se", "no", "fi", "dk", "es", "pl", "ch", "be", "cz",
        "gr", "hu", "ie", "nz", "pt", "ro", "sg", "tr", "ua", "za", "kr", "co", "tv", "fm",
    ];
    KNOWN.contains(&raw)
}

/// whether a dotted run reads as a domain.
///
/// a two-letter ending is a country code, and country-code domains always carry
/// a second-level label: `bbc.co.uk`, `globo.com.br`. that one rule is what
/// separates a real domain from a filename.
#[must_use]
pub fn is_domain(candidate: &str, tld: &str) -> bool {
    if !is_tld(tld) {
        return false;
    }
    let labels = candidate.split('.').filter(|l| !l.is_empty()).count();
    if labels < 2 {
        return false;
    }
    if tld.len() == 2 {
        return labels >= 3;
    }
    true
}

/// the `@` mentions in a text, lowercased and without the `@`.
///
/// a trailing full stop or bracket is part of the sentence, not the handle.
#[must_use]
pub fn mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0usize;

    while let Some(offset) = text[at..].find('@') {
        let mark = at + offset;
        let start = mark + 1;
        let mut end = start;
        while end < bytes.len() {
            let c = bytes[end];
            if c.is_ascii_alphanumeric() || c == b'_' {
                end += 1;
            } else {
                break;
            }
        }
        // an `@` inside a word belongs to an address, as in `a@b.example`
        let inside_word = mark > 0 && bytes[mark - 1].is_ascii_alphanumeric();
        if !inside_word && end > start {
            let handle = text[start..end].to_ascii_lowercase();
            if handle.chars().next().is_some_and(char::is_alphabetic) && !out.contains(&handle)
            {
                out.push(handle);
            }
        }
        at = end.max(start);
    }
    out
}

/// the `#` hashtags in a text, without the `#`.
#[must_use]
pub fn hashtags(text: &str) -> Vec<String> {
    collect_marked(text, '#')
}

/// the `$` cashtags in a text, without the `$`.
#[must_use]
pub fn cashtags(text: &str) -> Vec<String> {
    collect_marked(text, '$')
}

fn collect_marked(text: &str, mark: char) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0usize;

    while let Some(offset) = text[at..].find(mark) {
        let start = at + offset + mark.len_utf8();
        // a mark inside a word is part of it, as in `c#` or a price
        let inside_word = start > 0 && bytes[start - 1].is_ascii_alphanumeric();
        let mut end = start;
        while end < bytes.len() {
            let c = bytes[end];
            if c.is_ascii_alphanumeric() || c == b'_' {
                end += 1;
            } else {
                break;
            }
        }
        // a tag has to carry a letter: `$100` is a price, `#2026` is a year
        let tag = text[start..end].to_ascii_lowercase();
        if !inside_word && tag.chars().any(char::is_alphabetic) && !out.contains(&tag) {
            out.push(tag);
        }
        at = end.max(start);
    }
    out
}

/// the link kinds a bookmark ended up with, for reporting.
#[must_use]
pub fn kinds(bookmark: &Bookmark) -> Vec<LinkKind> {
    let mut out: Vec<LinkKind> = bookmark.links.iter().map(|l| l.kind).collect();
    out.sort_by_key(|k| k.worth_extracting());
    out.dedup_by_key(|k| *k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::bookmark::SourceRef;
    use mbm_core::medium::SourceMedium;

    fn one(text: &str) -> Bookmark {
        Bookmark::new(SourceRef::new(SourceMedium::X, "1", None), text, 0)
    }

    #[tokio::test]
    async fn urls_in_the_text_become_links() {
        let mut b = one("look at https://example.com/a and http://other.example/b");
        Entities::new().enrich(&mut b).await.unwrap();
        assert_eq!(b.links.len(), 2);
        assert!(b.links.iter().all(|l| l.kind != LinkKind::Unknown));
    }

    #[tokio::test]
    async fn a_trailing_punctuation_is_not_part_of_a_url() {
        let mut b = one("read https://example.com/a.");
        Entities::new().enrich(&mut b).await.unwrap();
        assert_eq!(b.links[0].resolved.as_str(), "https://example.com/a");
    }

    #[tokio::test]
    async fn a_url_repeated_twice_is_one_link() {
        let mut b = one("https://example.com/a and again https://example.com/a");
        Entities::new().enrich(&mut b).await.unwrap();
        assert_eq!(b.links.len(), 1);
    }

    #[tokio::test]
    async fn links_keep_the_order_they_were_written_in() {
        let mut b = one("first https://b.example/1 then https://a.example/2");
        Entities::new().enrich(&mut b).await.unwrap();
        assert_eq!(b.links[0].resolved.host_str(), Some("b.example"));
        assert_eq!(b.links[1].resolved.host_str(), Some("a.example"));
    }

    #[tokio::test]
    async fn a_paywalled_link_is_marked() {
        let mut b = one("https://www.nytimes.com/2026/01/02/thing.html");
        Entities::new().enrich(&mut b).await.unwrap();
        assert!(b.links[0].blocked.is_some(), "{:?}", b.links[0].blocked);
    }

    #[tokio::test]
    async fn a_bare_domain_becomes_a_link() {
        let mut b = one("saw it on news.ycombinator.com today");
        Entities::new().enrich(&mut b).await.unwrap();
        assert_eq!(b.links.len(), 1);
        assert_eq!(b.links[0].resolved.host_str(), Some("news.ycombinator.com"));
    }

    #[tokio::test]
    async fn a_sentence_ending_in_a_period_is_not_a_domain() {
        assert!(bare_domains("the meeting went fine.").is_empty());
        assert!(bare_domains("version 1.2.3 shipped").is_empty());
    }

    #[tokio::test]
    async fn a_filename_is_not_a_domain() {
        assert!(bare_domains("edit report.zip first").is_empty());
        assert!(bare_domains("open main.rs").is_empty());
    }

    #[tokio::test]
    async fn mentions_become_tags() {
        let mut b = one("thanks @simonw and @swyx, also @SimonW again");
        Entities::new().enrich(&mut b).await.unwrap();
        assert!(b.tags.contains("@simonw"));
        assert!(b.tags.contains("@swyx"));
        assert_eq!(b.tags.iter().filter(|t| *t == "@simonw").count(), 1, "a repeat is one tag");
    }

    #[tokio::test]
    async fn a_trailing_full_stop_is_not_part_of_a_handle() {
        assert_eq!(mentions("hi @simonw."), vec!["simonw".to_owned()]);
        assert_eq!(mentions("hi @simonw,"), vec!["simonw".to_owned()]);
        assert_eq!(mentions("(@simonw)"), vec!["simonw".to_owned()]);
    }

    #[tokio::test]
    async fn an_at_sign_inside_a_word_is_left_alone() {
        assert!(mentions("a@b.example is an address").is_empty());
    }

    #[tokio::test]
    async fn hashtags_and_cashtags_become_tags() {
        let mut b = one("#rustlang and $RUST on the list, #Rust twice");
        Entities::new().enrich(&mut b).await.unwrap();
        assert!(b.tags.contains("rustlang"));
        assert!(b.tags.contains("rust"));
    }

    #[tokio::test]
    async fn a_tag_with_no_letter_is_skipped() {
        // `#2026` is a year and `$100` is a price
        assert!(hashtags("released in #2026").is_empty());
        assert!(cashtags("it costs $100").is_empty());
        assert_eq!(hashtags("#rust2026"), vec!["rust2026".to_owned()]);
    }

    #[tokio::test]
    async fn a_price_is_not_a_cashtag() {
        assert!(cashtags("it costs $100 or so").iter().all(|t| t != "100"));
    }

    #[tokio::test]
    async fn every_item_gets_a_fingerprint() {
        let mut a = one("the same text about a thing");
        let mut b = one("the same text about a thing");
        Entities::new().enrich(&mut a).await.unwrap();
        Entities::new().enrich(&mut b).await.unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert!(a.fingerprint.is_some());
    }

    #[tokio::test]
    async fn a_url_rewrite_of_the_same_text_keeps_the_fingerprint() {
        let mut a = one("read https://example.com/a about caching");
        let mut b = one("read https://example.com/b?utm=x about caching");
        Entities::new().enrich(&mut a).await.unwrap();
        Entities::new().enrich(&mut b).await.unwrap();
        assert_eq!(a.fingerprint, b.fingerprint, "the url is not the subject");
    }

    #[tokio::test]
    async fn a_video_waiting_for_a_transcript_is_flagged() {
        let mut b = one("a video");
        b.tags.insert("needs-transcript".to_owned());
        Entities::new().enrich(&mut b).await.unwrap();
        assert!(b.tags.contains("transcript-pending"));
    }

    #[tokio::test]
    async fn a_post_with_fifty_links_is_capped() {
        let text = (0..50)
            .map(|i| format!("https://example.com/{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut b = one(&text);
        Entities::new().with_max_links(10).enrich(&mut b).await.unwrap();
        assert_eq!(b.links.len(), 10);
    }

    #[test]
    fn the_stage_declares_itself_free() {
        let stage = Entities::new().stage();
        assert_eq!(stage, EnrichStage::Entities);
        assert!(!stage.is_remote());
    }

    #[test]
    fn tld_matching_covers_the_common_cases() {
        assert!(is_tld("com"));
        assert!(is_tld("co"));
        assert!(is_tld("xyz"));
        assert!(!is_tld("rs"));
        assert!(!is_tld("zip"));
        assert!(!is_tld("c"));
    }

    #[test]
    fn a_country_code_ending_needs_a_second_level_label() {
        assert!(is_domain("bbc.co.uk", "uk"));
        assert!(is_domain("globo.com.br", "br"));
        assert!(!is_domain("main.rs", "rs"), "a source file is not a domain");
        assert!(!is_domain("example", "com"), "one label is not a domain");
    }

    #[tokio::test]
    async fn a_url_is_not_also_read_as_a_bare_domain() {
        let mut b = one("see https://news.ycombinator.com/item?id=1 for the thread");
        Entities::new().enrich(&mut b).await.unwrap();
        assert_eq!(b.links.len(), 1, "{:?}", b.links.iter().map(|l| l.resolved.as_str()).collect::<Vec<_>>());
    }
}

