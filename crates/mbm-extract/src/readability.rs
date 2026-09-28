//! pulling the readable part out of a web page.
//!
//! a heuristic reader, not a dom library. it scores block-level elements by
//! how much text they hold against how much punctuation surrounds them, keeps
//! the best run of sibling blocks, and drops the rest. that handles the common
//! case — an article wrapped in navigation, a sidebar, and a comment thread —
//! without a full parse.
//!
//! the alternative was a headless browser, which is what
//! [`crate::api::Browser`] is for when this returns nothing.

use mbm_core::bookmark::BlockedReason;

/// what came out of a page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Article {
    /// the page title.
    pub title: Option<String>,
    /// a one-line description, from the metadata tags.
    pub summary: Option<String>,
    /// the author, when the page names one.
    pub author: Option<String>,
    /// the site name.
    pub site: Option<String>,
    /// the readable body, as plain text with blank lines between paragraphs.
    pub body: String,
    /// why the body is empty, when it is.
    pub blocked: Option<BlockedReason>,
    /// the canonical url the page pointed at, if it declared one.
    pub canonical: Option<String>,
}

impl Article {
    /// an article that could not be read, and why.
    pub fn blocked(reason: BlockedReason) -> Self {
        Self { blocked: Some(reason), ..Self::default() }
    }

    /// whether anything usable came out.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.body.chars().count() > MIN_BODY_CHARS
    }
}

/// the shortest body worth keeping. a page whose readable text is under this
/// is a stub, a consent wall, or a page that never finished rendering.
const MIN_BODY_CHARS: usize = 280;

/// tags whose text is never article content.
const JUNK_TAGS: &[&str] = &[
    "script", "style", "nav", "header", "footer", "aside", "form", "noscript", "iframe", "svg",
    "button", "select", "template",
];

/// meta names that carry a description, in the order they are trusted.
const DESCRIPTION_KEYS: &[&str] = &["description", "og:description", "twitter:description"];

/// meta names that carry an author.
const AUTHOR_KEYS: &[&str] = &["author", "article:author", "og:article:author"];

/// meta names that carry a site name.
const SITE_KEYS: &[&str] = &["og:site_name", "application-name"];

/// extract the readable part of a page.
#[must_use]
pub fn read(html: &str, base_url: &str) -> Article {
    // tag names and attribute keys are matched against a lowercased copy, but
    // every value is read out of the original, because a title read from the
    // lowercase copy comes back lowercase
    // ascii lowering preserves byte length, so every offset found in `lower`
    // is also valid in `html`. that is what lets the scan match tags
    // case-insensitively while keeping the original's capitalisation.
    let lower = html.to_ascii_lowercase();
    let stripped = strip_junk(html, &lower);
    let stripped_lower = stripped.to_ascii_lowercase();

    let title = meta_content(html, &lower, &["og:title", "twitter:title"])
        .or_else(|| element_text(html, &lower, "title"))
        .map(|v| clean(&v));
    let summary =
        DESCRIPTION_KEYS.iter().find_map(|k| meta_content(html, &lower, &[k])).map(|v| clean(&v));
    let author =
        AUTHOR_KEYS.iter().find_map(|k| meta_content(html, &lower, &[k])).map(|v| clean(&v));
    let site = SITE_KEYS.iter().find_map(|k| meta_content(html, &lower, &[k])).map(|v| clean(&v));
    // a canonical link declares `rel`, a page url declares `og:url`
    let canonical = link_href(html, &lower, "canonical")
        .or_else(|| meta_content(html, &lower, &["og:url"]))
        .map(|v| clean(&v));

    let body = if is_paywall_wall(&lower) {
        String::new()
    } else {
        best_paragraphs(&stripped, &stripped_lower)
    };

    let mut article = Article {
        title,
        summary,
        author,
        site,
        body,
        blocked: None,
        canonical: canonical.or_else(|| Some(base_url.to_owned())),
    };

    if article.body.chars().count() <= MIN_BODY_CHARS {
        article.blocked = Some(if is_paywall_wall(&lower) {
            BlockedReason::Paywall
        } else {
            BlockedReason::Empty
        });
    }
    article
}

/// remove the tags whose contents are never prose.
fn strip_junk(html: &str, lower: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;

    while let Some(offset) = lower[cursor..].find('<') {
        let at = cursor + offset;
        out.push_str(&html[cursor..at]);

        // the extent of this tag, opening or closing
        let Some(tag_end) = lower[at..].find('>').map(|e| at + e + 1) else {
            // a truncated document: the rest is a broken tag, not content
            break;
        };

        let name = tag_name(&lower[at..]);
        if !name.is_empty() && JUNK_TAGS.contains(&name.as_str()) && !lower[at..].starts_with("</")
        {
            // skip the whole element, contents included
            let needle = format!("</{name}");
            match lower[tag_end..].find(&needle) {
                Some(close) => {
                    cursor = tag_end + close;
                    continue;
                }
                None => break,
            }
        }

        // keep this tag, since the block scan below looks for `<p>` and `</p>`
        // pairs and needs them to still be there
        out.push_str(&html[at..tag_end]);
        cursor = tag_end;
    }

    if cursor < html.len() {
        out.push_str(&html[cursor..]);
    }
    out
}

/// roughly how much text a document shows, in characters.
///
/// an approximation on purpose: the question is whether a page is a shell
/// waiting for javascript, and counting characters after the markup is removed
/// answers it without building a tree.
#[must_use]
pub fn visible_text(html: &str) -> usize {
    let lower = html.to_ascii_lowercase();
    let without_junk = strip_junk(html, &lower);

    // count what a reader would see, which means the tags have to go. counting
    // whitespace-separated tokens instead would keep the text glued to its
    // opening tag and drop every word that started one.
    let mut chars = 0usize;
    let mut inside_tag = false;
    for c in without_junk.chars() {
        match c {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => chars += 1,
            _ => {}
        }
    }
    chars
}

/// the tag name at the start of a tag, lowercased.
fn tag_name(tag: &str) -> String {
    tag.trim_start_matches('<').chars().take_while(char::is_ascii_alphanumeric).collect::<String>()
}

/// the text of the paragraphs that look like article body.
///
/// each block element's text is scored by length against a penalty for the
/// link density, and the highest-scoring ones are kept in document order. a
/// paragraph that is mostly links is a navigation list, not prose.
fn best_paragraphs(html: &str, lower: &str) -> String {
    const BLOCKS: &[&str] =
        &["<p", "<li", "<blockquote", "<h1", "<h2", "<h3", "<h4", "<pre", "<td"];
    const CLOSERS: &[(&str, &str)] = &[
        ("<p", "</p>"),
        ("<li", "</li>"),
        ("<blockquote", "</blockquote>"),
        ("<h1", "</h1>"),
        ("<h2", "</h2>"),
        ("<h3", "</h3>"),
        ("<h4", "</h4>"),
        ("<pre", "</pre>"),
        ("<td", "</td>"),
    ];

    let mut blocks: Vec<(usize, String, f64)> = Vec::new();
    let mut cursor = 0usize;

    while cursor < lower.len() {
        let Some((open_len, closer)) = next_block(lower, cursor, BLOCKS, CLOSERS) else {
            break;
        };
        let start = cursor + open_len;
        let Some(end_rel) = lower[start..].find(closer) else {
            break;
        };
        let text = clean(&html[start..start + end_rel]);
        let score = score(&text);
        if score > 0.0 {
            blocks.push((start, text, score));
        }
        cursor = start + end_rel + closer.len();
    }

    if blocks.is_empty() {
        return String::new();
    }

    // the top of the distribution is the article. dropping the tail removes
    // footers and comment threads, which score similarly but sit far below.
    let mut scores: Vec<f64> = blocks.iter().map(|(_, _, s)| *s).collect();
    scores.sort_unstable_by(|a, b| b.total_cmp(a));
    let cutoff = scores[(scores.len() * 3 / 4).min(scores.len() - 1)].max(1.0);

    let mut kept: Vec<&str> =
        blocks.iter().filter(|(_, _, s)| *s >= cutoff).map(|(_, t, _)| t.as_str()).collect();
    kept.dedup();
    kept.join("\n\n")
}

fn next_block(
    html: &str,
    from: usize,
    opens: &[&str],
    closers: &[(&str, &'static str)],
) -> Option<(usize, &'static str)> {
    let tail = &html[from..];
    let mut best: Option<(usize, usize, &'static str)> = None;
    for open in opens {
        let open: &str = open;
        if let Some(at) = tail.find(open) {
            // skip a match that is a prefix of a longer tag name, so `<p` does
            // not match `<pre`
            let after = tail.as_bytes().get(at + open.len()).copied();
            if matches!(after, Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')) {
                continue;
            }
            if best.is_none_or(|(b, _, _)| at < b) {
                let closer = closers.iter().find(|(o, _)| *o == open).map_or("", |(_, c)| *c);
                if closer.is_empty() {
                    continue;
                }
                // the text starts after the opening tag's own `>`, not after
                // its name, or every block begins with a stray angle bracket
                let Some(gt) = tail[at..].find('>') else {
                    continue;
                };
                best = Some((at, gt + 1, closer));
            }
        }
    }
    best.map(|(at, len, closer)| (at + len, closer))
}

/// a block's worth, or zero when it reads as chrome rather than prose.
fn score(text: &str) -> f64 {
    let len = text.chars().count() as f64;
    if len < 40.0 {
        return 0.0;
    }
    // link density is the standard signal. a block that is mostly anchors is a
    // list of links, however long it is.
    let links = count_tag(text, "<a");
    let link_chars: usize = between_all(text, "<a", "</a>").iter().map(String::len).sum();
    let density = if len > 0.0 { link_chars as f64 / len } else { 0.0 };
    if density > 0.5 {
        return 0.0;
    }
    if looks_like_chrome(text) {
        return 0.0;
    }
    // commas and periods carry the signal that this is a sentence rather than
    // a list item or a heading
    let punctuation = text.matches(['.', ',', ';', ':', '!', '?']).count() as f64;
    (len + punctuation * 12.0) / (1.0 + links as f64 + density * 8.0)
}

/// whether a block reads like a menu, a share bar, or a cookie notice.
fn looks_like_chrome(text: &str) -> bool {
    const CHROME: &[&str] = &[
        "cookie",
        "accept all",
        "sign in",
        "log in",
        "subscribe",
        "newsletter",
        "share this",
        "read more",
        "related posts",
        "follow us",
        "advertisement",
        "privacy policy",
        "terms of service",
        "all rights reserved",
        "skip to content",
    ];
    let lower = text.to_ascii_lowercase();
    CHROME.iter().filter(|c| lower.contains(*c)).count() >= 2
}

/// whether the page is a paywall or consent wall rather than an article.
fn is_paywall_wall(html: &str) -> bool {
    const WALLS: &[&str] = &[
        "subscribe to continue",
        "this article is for subscribers",
        "subscribers only",
        "create an account to read",
        "sign in to continue reading",
        "enable javascript to continue",
        "accept cookies to continue",
    ];
    WALLS.iter().any(|w| html.contains(w))
}

/// read a `content="..."` value out of a `<meta>` tag.
///
/// `lower` is the lowercased copy used to find the tag; `html` is the original
/// the value is read from.
fn meta_content(html: &str, lower: &str, names: &[&str]) -> Option<String> {
    for name in names {
        for (key_attr, value) in [("name", name), ("property", name)] {
            let needle = format!("{key_attr}=\"{value}\"");
            let Some(at) = lower.find(&needle) else {
                continue;
            };
            let Some(tag) = enclosing_tag(html, at) else {
                continue;
            };
            if let Some(content) = attribute(tag, "content") {
                return Some(content);
            }
        }
    }
    None
}

/// the text inside `<name ...>...</name>`, read from the original document.
fn element_text(html: &str, lower: &str, name: &str) -> Option<String> {
    let open = format!("<{name}");
    let close = format!("</{name}");
    let at = lower.find(&open)?;
    // an opening tag may carry attributes, so advance past its own `>`
    let tag_end = lower[at..].find('>')? + at + 1;
    let end = lower[tag_end..].find(&close)? + tag_end;
    Some(html[tag_end..end].to_owned())
}

/// the tag surrounding a byte offset, from the open angle bracket to the
/// closing `>`.
fn enclosing_tag(html: &str, at: usize) -> Option<&str> {
    let start = html[..at].rfind('<')?;
    let end = html[at..].find('>')? + at + 1;
    Some(&html[start..end])
}

/// the `href` of a `<link rel="...">`, for a given rel value.
fn link_href(html: &str, lower: &str, rel: &str) -> Option<String> {
    let needle = format!("rel=\"{rel}\"");
    let at = lower.find(&needle)?;
    let tag = enclosing_tag(html, at)?;
    attribute(tag, "href")
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let at = tag.find(&needle)?;
    let rest = &tag[at + needle.len()..];
    let end = rest.find('"')?;
    Some(unescape(&rest[..end]))
}

fn between_all(html: &str, open: &str, close: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find(open) {
        let tail = &rest[at + open.len()..];
        match tail.find(close) {
            Some(end) => {
                out.push(tail[..end].to_owned());
                rest = &tail[end..];
            }
            None => break,
        }
    }
    out
}

fn count_tag(html: &str, tag: &str) -> usize {
    let mut n = 0;
    let mut rest = html;
    while let Some(at) = rest.find(tag) {
        let after = html.as_bytes().get(at + tag.len()).copied();
        if !matches!(after, Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')) {
            n += 1;
        }
        rest = &rest[at + tag.len()..];
    }
    n
}

fn unescape(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…")
}

/// turn a fragment into a single line of clean text.
fn clean(raw: &str) -> String {
    let text = unescape(raw);
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    for c in text.chars() {
        if c.is_whitespace() || c == '\u{a0}' {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    out.trim().to_owned()
}
