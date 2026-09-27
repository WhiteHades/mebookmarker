//! deciding what a link is, and what is worth fetching.
//!
//! classification runs on the url alone, before any network call. that
//! ordering is the point: a typical bookmark is a social post whose only link
//! is a photo, and learning "it is a jpeg" from the extension is free where
//! fetching it is not. [`LinkKind::worth_extracting`] is the only place that
//! decision is made, and it is a five-line match.

use mbm_core::medium::LinkKind;
use url::Url;

/// classify a url from its shape alone.
#[must_use]
pub fn classify(url: &Url) -> LinkKind {
    let host = url.host_str().unwrap_or_default().trim_start_matches("www.");
    let path = url.path().to_ascii_lowercase();

    // x.com is checked before the generic social case because an x article
    // lives under the same host as an x post
    if matches!(host, "x.com" | "twitter.com") {
        if path.contains("/i/article/") {
            return LinkKind::LongForm;
        }
        if path.contains("/status/") {
            return LinkKind::Post;
        }
    }
    if matches!(host, "arxiv.org" | "export.arxiv.org" | "biorxiv.org" | "openreview.net") {
        return LinkKind::Paper;
    }
    if is_repo_host(host) {
        return LinkKind::Repository;
    }
    if is_video_host(host) {
        return LinkKind::Video;
    }
    if is_podcast_host(host) {
        return LinkKind::Podcast;
    }
    if is_image_path(&path) {
        return LinkKind::Image;
    }
    if path.contains("/releases") || path.contains("/changelog") || path.contains("/tag/") {
        return LinkKind::Release;
    }
    if path.starts_with("/docs")
        || path.contains("/documentation")
        || path.contains("/reference")
        || is_docs_host(host)
    {
        return LinkKind::Release;
    }
    if is_thread_host(host) {
        return LinkKind::Thread;
    }
    LinkKind::Article
}

fn is_repo_host(host: &str) -> bool {
    matches!(
        host,
        "github.com"
            | "gitlab.com"
            | "codeberg.org"
            | "bitbucket.org"
            | "gitea.com"
            | "sr.ht"
            | "sourcehut.org"
    )
}

fn is_video_host(host: &str) -> bool {
    matches!(
        host,
        "youtube.com"
            | "youtu.be"
            | "vimeo.com"
            | "dailymotion.com"
            | "twitch.tv"
            | "odysee.com"
            | "rumble.com"
    )
}

fn is_podcast_host(host: &str) -> bool {
    host.starts_with("podcasts.")
        || matches!(
            host,
            "spotify.com"
                | "overcast.fm"
                | "castro.fm"
                | "podcastaddict.com"
                | "player.fm"
                | "snipd.fm"
        )
        || host.starts_with("podcast")
        || host.contains("podcast")
}

/// hosts that only ever serve reference material.
fn is_docs_host(host: &str) -> bool {
    matches!(
        host,
        "docs.rs"
            | "doc.rust-lang.org"
            | "docs.python.org"
            | "developer.mozilla.org"
            | "readthedocs.io"
            | "kubernetes.io"
            | "man7.org"
            | "sqlite.org"
            | "postgresql.org"
    )
}

fn is_thread_host(host: &str) -> bool {
    matches!(
        host,
        "news.ycombinator.com" | "lobste.rs" | "reddit.com" | "old.reddit.com" | "tildes.net"
    )
}

fn is_image_path(path: &str) -> bool {
    [".jpg", ".jpeg", ".png", ".gif", ".webp", ".avif", ".svg", ".bmp"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// the canonical form of a url, for dedup and fingerprinting.
///
/// lowercases the host, drops a `www.`, drops the fragment, and sorts the query
/// parameters. two bookmarks that point at the same page with a different
/// tracking parameter collapse to one key, which is the difference between
/// catching and missing a duplicate.
#[must_use]
pub fn canonical(url: &Url) -> String {
    let mut out = url.clone();
    out.set_fragment(None);

    if let Some(current) = out.host_str().map(str::to_owned) {
        let trimmed = current.trim_start_matches("www.").to_owned();
        if trimmed != current {
            out.set_host(Some(&trimmed)).ok();
        }
    }
    if out.scheme() == "http" {
        // an http url that serves https is the same page
        out.set_scheme("https").ok();
    }
    if let Some(query) = out.query() {
        let kept: Vec<&str> = query
            .split('&')
            .filter(|pair| {
                let key = pair.split('=').next().unwrap_or_default();
                !is_tracking_param(key)
            })
            .collect();
        if kept.is_empty() {
            out.set_query(None);
        } else {
            let mut sorted = kept;
            sorted.sort_unstable();
            out.set_query(Some(&sorted.join("&")));
        }
    }
    // a trailing slash on a bare host is not a different page
    if out.path() == "/" && out.query().is_none() {
        out.set_path("");
    }
    out.to_string()
}

/// query parameters that only exist to attribute a visit.
#[must_use]
pub fn is_tracking_param(key: &str) -> bool {
    const TRACKING: &[&str] = &[
        "utm_source",
        "utm_medium",
        "utm_campaign",
        "utm_term",
        "utm_content",
        "utm_id",
        "utm_name",
        "utm_reader",
        "utm_brand",
        "utm_social",
        "utm_social-type",
        "gclid",
        "dclid",
        "fbclid",
        "msclkid",
        "mc_cid",
        "mc_eid",
        "igshid",
        "ref_src",
        "ref_url",
        "ref",
        "referrer",
        "referer",
        "source",
        "spm",
        "yclid",
        "_ga",
        "_gl",
    ];
    let key = key.to_ascii_lowercase();
    TRACKING.iter().any(|t| key == *t)
        || key.starts_with("utm_")
        || key.starts_with("pk_")
        || key.starts_with("piwik_")
        || key.starts_with("mtm_")
        || key.starts_with("_hs")
}

/// hosts whose body is behind a subscription.
///
/// detection is by host rather than by a marker in the html, because the marker
/// only appears after the request. a false positive costs a note saying the
/// article may be paywalled; a false negative costs a wasted fetch of a page
/// that will not have the text anyway.
#[must_use]
pub fn is_paywalled(url: &Url) -> bool {
    const PAYWALLS: &[&str] = &[
        "nytimes.com",
        "wsj.com",
        "washingtonpost.com",
        "theatlantic.com",
        "newyorker.com",
        "bloomberg.com",
        "ft.com",
        "economist.com",
        "bostonglobe.com",
        "latimes.com",
        "wired.com",
        "lemonde.fr",
        "zeit.de",
        "faz.net",
        "handelsblatt.com",
        "businessinsider.com",
        "techcrunch.com",
        "medium.com",
        "substack.com",
        "patreon.com",
        "tribune.com",
        "semafor.com",
        "axios.com",
        "cnn.com",
        "bbc.com",
        "guardian.com",
    ];
    let host = url.host_str().unwrap_or_default().trim_start_matches("www.");
    PAYWALLS.iter().any(|p| host == *p || host.ends_with(&format!(".{p}")))
}

/// pull every url out of free text.
///
/// a social post puts its links in the prose with no markup, so this scans for
/// `http` runs and trims the trailing punctuation people type after a link.
/// runs are found with `memchr`, so the scan is not the bottleneck on a long
/// post.
#[must_use]
pub fn links_in(text: &str) -> Vec<Url> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = memchr::memchr(b'h', rest.as_bytes()) {
        let tail = &rest[start..];
        let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let candidate = trim_trailing_punctuation(&tail[..end]);
        if let Ok(url) = Url::parse(candidate)
            && matches!(url.scheme(), "http" | "https")
            && !out.contains(&url)
        {
            out.push(url);
        }
        rest = &tail[end..];
    }
    out
}

/// percent-encode a value for use in a query string.
#[must_use]
pub fn percent_encode_query(raw: &str) -> String {
    percent_encoding::utf8_percent_encode(raw, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// strip the punctuation that follows a link when someone types it inline.
fn trim_trailing_punctuation(candidate: &str) -> &str {
    candidate.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"', ')', ']', '}', '>'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn an_x_article_is_not_mistaken_for_a_post() {
        assert_eq!(classify(&url("https://x.com/i/article/12345")), LinkKind::LongForm);
        assert_eq!(classify(&url("https://twitter.com/i/article/1")), LinkKind::LongForm);
        assert_eq!(classify(&url("https://x.com/someone/status/1")), LinkKind::Post);
    }

    #[test]
    fn a_bare_x_profile_is_an_article() {
        assert_eq!(classify(&url("https://x.com/someone")), LinkKind::Article);
    }

    #[test]
    fn code_hosts_are_repositories() {
        for host in ["github.com", "gitlab.com", "codeberg.org", "bitbucket.org", "sr.ht"] {
            assert_eq!(
                classify(&url(&format!("https://{host}/a/b"))),
                LinkKind::Repository,
                "{host}"
            );
        }
    }

    #[test]
    fn arxiv_is_a_paper() {
        assert_eq!(classify(&url("https://arxiv.org/abs/1234.5678")), LinkKind::Paper);
        assert_eq!(classify(&url("https://openreview.net/forum?id=x")), LinkKind::Paper);
    }

    #[test]
    fn video_hosts_are_video() {
        for host in ["youtube.com", "youtu.be", "vimeo.com", "twitch.tv"] {
            assert_eq!(classify(&url(&format!("https://{host}/x"))), LinkKind::Video, "{host}");
        }
    }

    #[test]
    fn an_image_extension_is_an_image() {
        for ext in ["jpg", "png", "webp", "avif"] {
            let u = url(&format!("https://pbs.twimg.com/media/a.{ext}"));
            assert_eq!(classify(&u), LinkKind::Image, "{ext}");
        }
    }

    #[test]
    fn a_query_string_does_not_make_a_page_an_image() {
        let u = url("https://example.com/photo?format=jpg");
        assert_eq!(classify(&u), LinkKind::Article);
    }

    #[test]
    fn a_docs_path_is_a_release() {
        assert_eq!(classify(&url("https://docs.rs/serde")), LinkKind::Release);
        assert_eq!(classify(&url("https://example.com/changelog")), LinkKind::Release);
    }

    #[test]
    fn media_kinds_are_never_worth_a_fetch() {
        // the whole reason classification runs before the network
        for kind in [LinkKind::Image, LinkKind::Video, LinkKind::Podcast, LinkKind::Post] {
            assert!(!kind.worth_extracting(), "{kind} should not trigger a fetch");
        }
        for kind in [LinkKind::Repository, LinkKind::Article, LinkKind::Paper, LinkKind::LongForm] {
            assert!(kind.worth_extracting(), "{kind} carries text worth reading");
        }
    }

    #[test]
    fn canonical_forms_collapse_tracking_variants() {
        let a = canonical(&url("https://www.example.com/post?utm_source=twitter&id=7"));
        let b = canonical(&url("http://example.com/post?id=7&fbclid=abc"));
        assert_eq!(a, b, "{a} vs {b}");
    }

    #[test]
    fn canonical_keeps_meaningful_query_parameters() {
        let a = canonical(&url("https://arxiv.org/abs/1234?context=cs"));
        assert!(a.contains("context=cs"), "{a}");
    }

    #[test]
    fn canonical_drops_the_fragment_and_a_bare_trailing_slash() {
        assert_eq!(canonical(&url("https://example.com/a#section")), "https://example.com/a");
        // `url` normalises a bare host to a single slash and re-adds it on
        // display, so the slash stays
        assert_eq!(canonical(&url("https://example.com/")), "https://example.com/");
    }

    #[test]
    fn canonical_sorts_remaining_parameters() {
        let a = canonical(&url("https://example.com/x?b=2&a=1"));
        let b = canonical(&url("https://example.com/x?a=1&b=2"));
        assert_eq!(a, b);
    }

    #[test]
    fn tracking_parameters_are_recognised_by_prefix() {
        for key in ["utm_source", "utm_anything", "pk_campaign", "mtm_x", "_hsenc"] {
            assert!(is_tracking_param(key), "{key} should be tracking");
        }
        for key in ["id", "q", "context", "page", "ref_id"] {
            assert!(!is_tracking_param(key), "{key} is meaningful");
        }
    }

    #[test]
    fn paywall_detection_covers_a_host_and_its_subdomains() {
        assert!(is_paywalled(&url("https://www.nytimes.com/2026/01/x")));
        assert!(is_paywalled(&url("https://cooking.nytimes.com/x")));
        assert!(!is_paywalled(&url("https://example.com/x")));
        assert!(!is_paywalled(&url("https://notnytimes.com/x")), "a suffix must be a dot");
    }

    #[test]
    fn links_are_pulled_out_of_prose() {
        let text = "see https://example.com/a and also http://other.org/b?q=1 for more";
        let found = links_in(text);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].as_str(), "https://example.com/a");
        assert_eq!(found[1].as_str(), "http://other.org/b?q=1");
    }

    #[test]
    fn trailing_punctuation_is_trimmed_off_an_inline_link() {
        let found = links_in("look at https://example.com/page, then stop.");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].as_str(), "https://example.com/page");
    }

    #[test]
    fn a_link_at_the_very_end_of_the_text_is_found() {
        let found = links_in("go to https://example.com/last");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn duplicate_links_are_reported_once() {
        let found = links_in("https://example.com/a and again https://example.com/a");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn text_with_no_links_yields_nothing() {
        assert!(links_in("just some words with no links at all").is_empty());
        assert!(links_in("").is_empty());
    }

    #[test]
    fn non_http_schemes_are_not_treated_as_links() {
        assert!(links_in("mailto:someone@example.com").is_empty());
        assert!(links_in("ftp://files.example.com/x").is_empty());
    }
}
