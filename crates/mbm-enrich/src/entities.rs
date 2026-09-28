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

        // the hosts are facts about the item, and a person searching their
        // archive wants `mbm list -t arxiv.org` to work the moment the item
        // lands, rather than only after the tag stage has paid for a question
        let hosts: Vec<String> =
            found.iter().filter_map(|link| link.resolved.host_str().map(str::to_owned)).collect();

        // the links already on the bookmark came from whoever read it: a
        // browser folder, a feed's own target, a `- **Filed:**` line in an
        // archive file. replacing them would throw away context this stage never
        // had, so the two sets are merged, in the order a reader would meet them.
        for link in std::mem::take(&mut bookmark.links) {
            if !found.iter().any(|f| f.resolved == link.resolved) {
                found.push(link);
            }
        }
        found.sort_by(|a, b| {
            let position =
                |link: &Link| bookmark.text.find(link.original.as_str()).unwrap_or(usize::MAX);
            position(a).cmp(&position(b)).then_with(|| a.resolved.as_str().cmp(b.resolved.as_str()))
        });
        bookmark.links = found;
        for host in hosts {
            bookmark.push_tag(host);
        }

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
        "click", "rss", "moe", "best", "pro", "eu", "us", "uk", "de", "fr", "jp", "ru", "br", "in",
        "cn", "au", "ca", "nl", "se", "no", "fi", "dk", "es", "pl", "ch", "be", "cz", "gr", "hu",
        "ie", "nz", "pt", "ro", "sg", "tr", "ua", "za", "kr", "co", "tv", "fm",
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
            if handle.chars().next().is_some_and(char::is_alphabetic) && !out.contains(&handle) {
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

/// whether a tag is an html entity rather than a tag.
///
/// escaped html is everywhere in a feed, and `&#x27;` is a quote mark whose body
/// reads as a hashtag named `x27`. the token scan stops at the `;`, so the
/// caller passes the character that ended it and the check needs that one too.
fn is_html_entity(tag: &str, terminator: char) -> bool {
    if terminator != ';' {
        return false;
    }
    match tag.strip_prefix('x').or_else(|| tag.strip_prefix('X')) {
        Some(hex) => !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()),
        None => !tag.is_empty() && tag.chars().all(|c| c.is_ascii_digit()),
    }
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
        let terminator = text[end..].chars().next().unwrap_or('\0');
        if !inside_word
            && tag.chars().any(char::is_alphabetic)
            && !is_html_entity(&tag, terminator)
            && !out.contains(&tag)
        {
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
