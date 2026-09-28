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
