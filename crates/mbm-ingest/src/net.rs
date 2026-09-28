//! the network source adapters.
//!
//! one file per family, one shape of response per family. they all share the
//! [`mbm_core::port::Source`] port, so adding a new one here changes
//! nothing above.
//!
//! every adapter is written to be testable without a network: the parsing half
//! of each lives in a pure function that takes a byte slice, and only the thin
//! request half is `async`.

use async_trait::async_trait;
use mbm_core::bookmark::{Author, Bookmark, SourceRef};
use mbm_core::error::{Error, Result};
use mbm_core::medium::SourceMedium;
use mbm_core::port::{FetchPage, FetchRequest, Source};
use mbm_extract::{Http, Request};
use serde::Deserialize;
use url::Url;

/// now, in unix milliseconds.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// a bookmark built from a record that already has most of the fields.
pub(crate) fn build(
    medium: SourceMedium,
    id: impl Into<String>,
    text: impl Into<String>,
    author: Option<&str>,
    author_name: Option<&str>,
    created: Option<i64>,
    url: Option<&str>,
) -> Bookmark {
    let id = id.into();
    let text = text.into();
    let created = created.unwrap_or_else(now);
    let parsed = url.and_then(|u| Url::parse(u).ok());

    // the collection is which stream within the medium the item came from: a
    // subreddit, a hacker news collection, a playlist. it is not the author,
    // which is what this used to set it to, and an opml export that files every
    // hacker news item under a folder named after its submitter is a folder per
    // bookmark and no way to browse anything.
    let source = SourceRef::new(medium, id.clone(), parsed.clone());

    let mut bookmark = Bookmark::new(source, text, created);
    bookmark.created_at = Some(created);
    bookmark.url = parsed;
    if let Some(handle) = author {
        let mut a = Author::new(handle);
        if let Some(name) = author_name.filter(|n| !n.trim().is_empty()) {
            a = a.with_name(name);
        }
        bookmark.author = Some(a);
        bookmark.push_tag(handle);
    }
    bookmark
}

/// push a link onto a bookmark, classifying it first.
///
/// classifying here rather than leaving it unknown matters: the entity stage
/// reads these kinds to decide what is worth extracting, and `unknown` tells it
/// nothing.
pub(crate) fn link(bookmark: &mut Bookmark, url: &str) {
    let Ok(parsed) = Url::parse(url) else { return };
    if bookmark.links.iter().any(|l| l.resolved == parsed) {
        return;
    }
    bookmark.links.push(mbm_core::bookmark::Link {
        kind: mbm_extract::links::classify(&parsed),
        blocked: mbm_extract::links::is_paywalled(&parsed)
            .then_some(mbm_core::bookmark::BlockedReason::Paywall),
        original: parsed.clone(),
        resolved: parsed,
        title: None,
        body: None,
        summary: None,
    });
}

// hacker news

/// a story or comment from the algolia api.
///
/// the fields this reader uses. the raw hit is kept alongside it, untouched, so
/// a field nobody here knows about still reaches the archive.
#[derive(Debug, Clone, Deserialize)]
pub struct HnItem {
    /// the item id, or `None` for a deleted ancestor.
    #[serde(default, alias = "objectID", alias = "objectId", alias = "id")]
    pub object_id: Option<String>,
    /// the story or comment text.
    #[serde(default)]
    pub comment_text: Option<String>,
    /// the title, on a story.
    #[serde(default)]
    pub title: Option<String>,
    /// the url, on a story.
    #[serde(default)]
    pub url: Option<String>,
    /// who wrote it.
    #[serde(default)]
    pub author: Option<String>,
    /// the ask hn or show hn body.
    #[serde(default)]
    pub story_text: Option<String>,
    /// when it was created.
    #[serde(default)]
    pub created_at: Option<String>,
    /// the parent item, for a comment.
    #[serde(default)]
    pub parent_id: Option<i64>,
}

/// read one algolia response into bookmarks.
///
/// the api returns a wrapper with a `hits` array, and every field is optional
/// because a deleted item comes back as a null.
pub fn parse_hackernews(body: &[u8], collection: Option<&str>) -> Result<Vec<Bookmark>> {
    // read as raw json first and parse each hit out of that, so the stored raw
    // payload is exactly what the api returned. a struct would drop every field
    // it does not name, and the point of the archive format is that a later
    // version can re-parse it
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        hits: Vec<serde_json::Value>,
    }

    let response: Response =
        serde_json::from_slice(body).map_err(|e| Error::Ingest(format!("hacker news: {e}")))?;

    let mut out = Vec::with_capacity(response.hits.len());
    for raw in response.hits {
        let Ok(hit) = serde_json::from_value::<HnItem>(raw.clone()) else { continue };
        let Some(id) = hit.object_id.clone() else { continue };

        let mut text = String::new();
        if let Some(title) = &hit.title {
            text.push_str(title);
        }
        if let Some(story) = &hit.story_text {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&strip_html(story));
        }
        if let Some(comment) = &hit.comment_text {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&strip_html(comment));
        }
        if text.trim().is_empty() {
            continue;
        }

        let created = hit.created_at.as_deref().and_then(crate::json::parse_date);
        let fallback =
            hit.object_id.as_ref().map(|id| format!("https://news.ycombinator.com/item?id={id}"));
        let url = hit.url.as_deref().or(fallback.as_deref());

        let mut bookmark =
            build(SourceMedium::HackerNews, id, text, hit.author.as_deref(), None, created, url);
        if hit.parent_id.is_some() {
            bookmark.role = Some(mbm_core::bookmark::ThreadRole::Reply);
        }
        bookmark.push_tag("hackernews");
        if let Some(collection) = collection {
            bookmark.push_tag(collection);
            bookmark.source.collection = Some(collection.to_owned());
        }
        if let Some(url) = hit.url.as_deref() {
            link(&mut bookmark, url);
        }
        bookmark.raw = Some(raw);
        out.push(bookmark);
    }
    Ok(out)
}

/// the hacker news source.
#[derive(Debug)]
pub struct HackerNews {
    http: Http,
    collection: Option<String>,
    base: &'static str,
}

impl HackerNews {
    /// the algolia endpoint, and the only host this adapter ever talks to.
    const API: &'static str = "https://hn.algolia.com/api/v1";

    /// build the adapter.
    pub fn new(http: Http) -> Self {
        Self { http, collection: None, base: Self::API }
    }

    /// point the adapter somewhere else.
    ///
    /// this is how a mirror, a proxy, or a test server is used, and it is the
    /// only reason the endpoint is a field rather than a literal in the middle
    /// of a function.
    #[must_use]
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = Box::leak(base.into().into_boxed_str());
        self
    }

    /// the endpoint this adapter is pointed at.
    #[must_use]
    pub fn base(&self) -> &str {
        self.base
    }

    /// restrict to one tag, such as `show_hn` or `ask_hn`.
    #[must_use]
    pub fn in_collection(mut self, collection: impl Into<String>) -> Self {
        self.collection = Some(collection.into());
        self
    }
}

#[async_trait]
impl Source for HackerNews {
    fn medium(&self) -> SourceMedium {
        SourceMedium::HackerNews
    }

    async fn fetch(&self, request: &FetchRequest) -> Result<FetchPage> {
        use std::fmt::Write as _;

        let limit = request.limit.unwrap_or(50).min(1000);
        // the algolia api's `story_` prefix names a story *type*, and the
        // curated tags are their own namespace: `show_hn` is 542,680 hits and
        // `story_show_hn` is zero. the prefix is only right for `story` itself.
        let tag = self.collection.as_deref().unwrap_or("story");
        let mut url = format!("{}/search_by_date?hitsPerPage={limit}&tags={tag}", self.base);
        if let Some(cursor) = request.collection.as_deref() {
            // the endpoint sorts newest first, so the next page is everything
            // *older* than the last item on this one. filtering the other way
            // returns nothing at all, which looks like a source that has run
            // out rather than one that is being paged.
            let _ = write!(url, "&numericFilters=created_at_i<{cursor}");
        }

        let response = self.http.send(&Request::get(url)).await?;
        let items = parse_hackernews(&response.body, self.collection.as_deref())?;
        let count = items.len();
        let next = items.last().and_then(|b| b.created_at).map(|ms| (ms / 1000).to_string());
        Ok(FetchPage { items, has_more: count >= limit, next_cursor: next, skipped: 0 })
    }
}

// reddit

/// read a reddit listing into bookmarks.
///
/// the public `.json` endpoint returns a listing whose posts carry a `data`
/// object; a comment is a nested reply with a `body`.
pub fn parse_reddit(body: &[u8], subreddit: Option<&str>) -> Result<Vec<Bookmark>> {
    #[derive(Deserialize)]
    struct Listing {
        #[serde(default)]
        data: ListingData,
    }
    #[derive(Deserialize, Default)]
    struct ListingData {
        #[serde(default)]
        children: Vec<Child>,
    }
    #[derive(Deserialize)]
    struct Child {
        #[serde(default)]
        data: Option<Post>,
    }
    #[derive(Deserialize)]
    struct Post {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        selftext: Option<String>,
        #[serde(default)]
        author: Option<String>,
        #[serde(default)]
        permalink: Option<String>,
        #[serde(default)]
        created_utc: Option<f64>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        stickied: bool,
    }

    let listing: Listing =
        serde_json::from_slice(body).map_err(|e| Error::Ingest(format!("reddit: {e}")))?;

    let mut out = Vec::with_capacity(listing.data.children.len());
    for child in listing.data.children {
        let Some(post) = child.data else { continue };
        let Some(id) = post.id.clone() else { continue };
        // the pinned announcements are not bookmarks
        if post.stickied {
            continue;
        }

        let mut text = post.title.clone().unwrap_or_default();
        if let Some(body) = post.selftext.as_deref().filter(|b| !b.trim().is_empty()) {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(body);
        }
        if text.trim().is_empty() {
            continue;
        }

        let fallback = post.permalink.as_deref().map(|p| format!("https://www.reddit.com{p}"));
        let url = post.url.as_deref().filter(|u| !u.contains("reddit.com")).or(fallback.as_deref());

        let mut bookmark = build(
            SourceMedium::Reddit,
            id,
            text,
            post.author.as_deref(),
            None,
            post.created_utc.map(|s| (s * 1000.0) as i64),
            url,
        );
        bookmark.push_tag("reddit");
        if let Some(subreddit) = subreddit {
            bookmark.push_tag(subreddit);
            bookmark.source.collection = Some(subreddit.to_owned());
        }
        if let Some(url) = url {
            link(&mut bookmark, url);
        }
        out.push(bookmark);
    }
    Ok(out)
}

/// the reddit source.
#[derive(Debug)]
pub struct Reddit {
    http: Http,
    subreddit: Option<String>,
    sort: String,
    time: String,
    base: String,
}

impl Reddit {
    /// reddit's public listing endpoint, which needs no account.
    const API: &'static str = "https://www.reddit.com";

    /// build the adapter.
    pub fn new(http: Http) -> Self {
        Self {
            http,
            subreddit: None,
            sort: "new".to_owned(),
            time: "all".to_owned(),
            base: Self::API.to_owned(),
        }
    }

    /// point the adapter somewhere else: a mirror, a proxy, or a test server.
    #[must_use]
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// the endpoint this adapter is pointed at.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// watch one subreddit.
    #[must_use]
    pub fn subreddit(mut self, name: impl Into<String>) -> Self {
        self.subreddit = Some(name.into());
        self
    }

    /// set the sort order: `new`, `hot`, `top`, or `rising`.
    #[must_use]
    pub fn sorted_by(mut self, sort: impl Into<String>) -> Self {
        self.sort = sort.into();
        self
    }

    /// set the time window: `hour`, `day`, `week`, `month`, `year`, or `all`.
    #[must_use]
    pub fn over_time(mut self, time: impl Into<String>) -> Self {
        self.time = time.into();
        self
    }
}

#[async_trait]
impl Source for Reddit {
    fn medium(&self) -> SourceMedium {
        SourceMedium::Reddit
    }

    async fn preflight(&self) -> Result<()> {
        // reddit blocks the tool's own default, and it blocks it with a 429
        // rather than an error, so the run is stopped before it starts. a user
        // agent the person chose is theirs: this only refuses the exact default
        // and leaves anything else alone.
        let default = format!("mebookmarker/{}", env!("CARGO_PKG_VERSION"));
        if self.http.user_agent().trim() == default {
            return Err(Error::Config(
                "reddit blocks the default user agent. set `user_agent` in the config to \
                 something that names you."
                    .to_owned(),
            ));
        }
        Ok(())
    }

    async fn fetch(&self, request: &FetchRequest) -> Result<FetchPage> {
        let limit = request.limit.unwrap_or(50).min(100);
        let path = self.subreddit.as_deref().unwrap_or("all");
        // `t` is how far back the listing reaches, and the value is the only one
        // reddit documents. an option that is accepted and then ignored is worse
        // than one that is not offered, because a person who sets it is told
        // they have a week of posts and gets all of them.
        let window = match self.time.as_str() {
            "hour" | "day" | "week" | "month" | "year" => format!("&t={}", self.time),
            "all" => String::new(),
            other => {
                return Err(Error::Config(format!(
                    "reddit time must be hour, day, week, month, year, or all; got {other}"
                )));
            }
        };
        let url =
            format!("{}/r/{path}/{}/.json?limit={limit}{window}&raw_json=1", self.base, self.sort);

        let response = self.http.send(&Request::get(url)).await?;
        let items = parse_reddit(&response.body, self.subreddit.as_deref())?;
        let count = items.len();
        Ok(FetchPage { items, has_more: count >= limit, next_cursor: None, skipped: 0 })
    }
}

// github stars

/// a starred repository.
#[derive(Debug, Clone, Deserialize)]
pub struct StarredRepo {
    /// `owner/name`.
    pub full_name: String,
    /// the description.
    #[serde(default)]
    pub description: Option<String>,
    /// the homepage.
    #[serde(default)]
    pub homepage: Option<String>,
    /// the language.
    #[serde(default)]
    pub language: Option<String>,
    /// star count.
    #[serde(default)]
    pub stargazers_count: u64,
    /// topics.
    #[serde(default)]
    pub topics: Vec<String>,
}

/// read a github stars page into bookmarks.
pub fn parse_github_stars(body: &[u8]) -> Result<Vec<Bookmark>> {
    // raw json first, for the same reason the hacker news reader does
    let raw_repos: Vec<serde_json::Value> =
        serde_json::from_slice(body).map_err(|e| Error::Ingest(format!("github: {e}")))?;

    let mut out = Vec::with_capacity(raw_repos.len());
    for raw in raw_repos {
        let Ok(repo) = serde_json::from_value::<StarredRepo>(raw.clone()) else { continue };
        let mut text = repo.full_name.clone();
        if let Some(description) = &repo.description {
            text.push_str("\n\n");
            text.push_str(description);
        }
        if let Some(language) = &repo.language {
            use std::fmt::Write as _;
            let _ = write!(text, "\n\nLanguage: {language}");
        }

        let repo_url = format!("https://github.com/{}", repo.full_name);
        let mut bookmark = build(
            SourceMedium::Github,
            repo.full_name.clone(),
            text,
            None,
            None,
            None,
            Some(&repo_url),
        );
        bookmark.push_tag("github");
        bookmark.push_tag("starred");
        for topic in repo.topics {
            bookmark.push_tag(topic);
        }
        if let Some(homepage) = &repo.homepage {
            link(&mut bookmark, homepage);
        }
        link(&mut bookmark, &repo_url);
        bookmark.raw = Some(raw);
        let _ = repo.stargazers_count;
        out.push(bookmark);
    }
    Ok(out)
}

/// the github stars source.
#[derive(Debug)]
pub struct GithubStars {
    http: Http,
    token: Option<String>,
    base: String,
}

impl GithubStars {
    /// github's rest api, which needs a token for anything private.
    const API: &str = "https://api.github.com";

    /// build the adapter.
    pub fn new(http: Http, token: Option<String>) -> Self {
        Self { http, token, base: Self::API.to_owned() }
    }

    /// point the adapter somewhere else: an enterprise instance, a proxy, or a
    /// test server.
    #[must_use]
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// the endpoint this adapter is pointed at.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }
}

#[async_trait]
impl Source for GithubStars {
    fn medium(&self) -> SourceMedium {
        SourceMedium::Github
    }

    async fn preflight(&self) -> Result<()> {
        if self.token.is_none() {
            return Err(Error::Auth(
                "github needs a token in the `GITHUB_TOKEN` environment variable.".to_owned(),
            ));
        }
        Ok(())
    }

    async fn fetch(&self, request: &FetchRequest) -> Result<FetchPage> {
        let limit = request.limit.unwrap_or(100).min(100);
        let page = request.max_pages.unwrap_or(1);
        let mut items = Vec::new();

        for page in 1..=page {
            let mut req =
                Request::get(format!("{}/user/starred?per_page={limit}&page={page}", self.base))
                    .header("accept", "application/vnd.github+json");
            if let Some(token) = &self.token {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            let response = self.http.send(&req).await?;
            if !response.is_success() {
                break;
            }
            let page_items = parse_github_stars(&response.body)?;
            let count = page_items.len();
            items.extend(page_items);
            if count < limit {
                break;
            }
        }

        let count = items.len();
        Ok(FetchPage { has_more: count >= limit, items, next_cursor: None, skipped: 0 })
    }
}

// rss and atom

/// read a feed into bookmarks.
///
/// rss 2.0, rdf, and atom are all handled, because every generator produces a
/// slightly different one and a feed reader that only handles the most common
/// shape silently drops the rest.
pub fn parse_feed(body: &[u8], feed_url: &str, medium: SourceMedium) -> Result<Vec<Bookmark>> {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_ascii_lowercase();
    if !lower.contains("<rss") && !lower.contains("<feed") && !lower.contains("<rdf") {
        return Err(Error::Ingest("not an rss, rdf, or atom feed".to_owned()));
    }

    let mut out = Vec::new();
    let mut cursor = 0usize;
    let is_atom = lower.contains("<feed");

    while let Some(offset) =
        lower[cursor..].find("<item").or_else(|| lower[cursor..].find("<entry"))
    {
        let at = cursor + offset;
        let close = if is_atom { "</entry>" } else { "</item>" };
        let Some(end) = lower[at..].find(close).map(|e| at + e) else {
            break;
        };
        let block = &text[at..end];
        cursor = end + close.len();

        if let Some(bookmark) = one_feed_entry(block, feed_url, medium, is_atom) {
            out.push(bookmark);
        }
    }
    Ok(out)
}

fn one_feed_entry(
    block: &str,
    feed_url: &str,
    medium: SourceMedium,
    is_atom: bool,
) -> Option<Bookmark> {
    let lower = block.to_ascii_lowercase();
    let title = tag(block, &lower, "title")?;
    let body = if is_atom {
        tag(block, &lower, "content").or_else(|| tag(block, &lower, "summary"))
    } else {
        tag(block, &lower, "description").or_else(|| tag(block, &lower, "encoded"))
    };

    let entry_link = if is_atom {
        // an atom link is an attribute, and the html variant is the useful one
        attribute(block, "href")
            .filter(|h| h.contains("html") || !h.contains("/self"))
            .or_else(|| tag(block, &lower, "id"))
    } else {
        tag(block, &lower, "link")
    };

    let created = tag(block, &lower, "pubdate")
        .or_else(|| tag(block, &lower, "updated"))
        .or_else(|| tag(block, &lower, "published"))
        .and_then(|d| crate::json::parse_date(&d));

    let author = tag(block, &lower, "creator")
        .or_else(|| tag(block, &lower, "name"))
        .or_else(|| tag(block, &lower, "author"))
        .map(|a| a.trim().to_owned());
    let (author, author_name) = match &author {
        Some(raw) => split_feed_author(raw),
        None => (None, None),
    };

    let mut text = strip_html(&title);
    if let Some(body) = body.as_deref().filter(|b| !b.trim().is_empty()) {
        text.push_str("\n\n");
        text.push_str(&strip_html(body));
    }

    let id =
        tag(block, &lower, "guid").or_else(|| entry_link.clone()).unwrap_or_else(|| title.clone());
    let id = id.trim().to_owned();
    if id.is_empty() {
        return None;
    }

    let target = entry_link.filter(|l| l.starts_with("http")).or_else(|| Some(feed_url.to_owned()));
    let mut bookmark = build(
        medium,
        id,
        text,
        author.as_deref(),
        author_name.as_deref(),
        created,
        target.as_deref(),
    );
    bookmark.push_tag(feed_host(feed_url));

    if let Some(target) = target.as_deref() {
        link(&mut bookmark, target);
    }
    Some(bookmark)
}

/// split an `<author>` into the account and the name.
///
/// feeds spell this `email (Name)` or `Name (email)`, and both are out there, so
/// the bracketed half is the name whichever side it is on. the account is what
/// a reader would type to find the author again, so the address wins when the
/// two are both present.
fn split_feed_author(raw: &str) -> (Option<String>, Option<String>) {
    let raw = raw.trim();
    if raw.is_empty() {
        return (None, None);
    }
    if let Some(open) = raw.find('(')
        && let Some(close) = raw.rfind(')')
        && close > open + 1
    {
        let inner = raw[open + 1..close].trim();
        let outer = format!("{}{}", &raw[..open], &raw[close + 1..]).trim().to_owned();
        let email = [outer.as_str(), inner].into_iter().find(|p| p.contains('@')).unwrap_or("");
        let name = [inner, &outer].into_iter().find(|p| !p.contains('@') && !p.is_empty());
        return ((!email.is_empty()).then(|| email.to_ascii_lowercase()), name.map(str::to_owned));
    }
    (Some(raw.to_ascii_lowercase()), None)
}

fn feed_host(url: &str) -> &str {
    url.split("//").nth(1).and_then(|r| r.split('/').next()).unwrap_or("feed")
}

/// the text inside `<name>...</name>`, unescaped.
fn tag(block: &str, lower: &str, name: &str) -> Option<String> {
    let open = format!("<{name}");
    let close = format!("</{name}");
    let at = lower.find(&open)?;
    // a tag may carry attributes, so advance past its own `>`
    let start = lower[at..].find('>')? + at + 1;
    // `encoded` appears as `<content:encoded>`, whose tag name is longer
    let end = lower[start..].find(&close)? + start;
    let raw = &block[start..end];
    if raw.trim().is_empty() { None } else { Some(unescape(raw.trim())) }
}

/// an attribute value from the first tag of a block.
fn attribute(block: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let at = block.find(&needle)?;
    let rest = &block[at + needle.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

fn unescape(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
}

/// turn a fragment of html into plain text.
pub(crate) fn strip_html(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut inside = false;
    for c in raw.chars() {
        match c {
            '<' => inside = true,
            // a tag boundary is also a word boundary, so `</p><p>` has to leave
            // a space behind rather than gluing two words together
            '>' => {
                inside = false;
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            '\n' | '\r' => {
                if !inside && !out.ends_with('\n') && !out.is_empty() {
                    out.push('\n');
                }
            }
            _ if !inside => out.push(c),
            _ => {}
        }
    }
    unescape(out.split_whitespace().collect::<Vec<_>>().join(" ").trim())
}

/// the rss source.
#[derive(Debug)]
pub struct Feed {
    http: Http,
    urls: Vec<String>,
}

impl Feed {
    /// build the adapter for one or more feed urls.
    pub fn new(http: Http, urls: Vec<String>) -> Self {
        Self { http, urls }
    }
}

#[async_trait]
impl Source for Feed {
    fn medium(&self) -> SourceMedium {
        SourceMedium::Rss
    }

    async fn fetch(&self, _request: &FetchRequest) -> Result<FetchPage> {
        let mut items = Vec::new();
        let mut skipped = 0usize;
        for url in &self.urls {
            let response = self.http.send(&Request::get(url)).await?;
            match parse_feed(&response.body, url, SourceMedium::Rss) {
                Ok(mut found) => items.append(&mut found),
                Err(_) => skipped += 1,
            }
        }
        Ok(FetchPage { items, has_more: false, next_cursor: None, skipped })
    }
}

/// a youtube video, from the oembed or the innertube response.
#[derive(Debug, Clone, Deserialize)]
pub struct Video {
    /// the video id.
    pub id: String,
    /// the title.
    pub title: String,
    /// the channel.
    #[serde(default)]
    pub channel: Option<String>,
    /// the description.
    #[serde(default)]
    pub description: Option<String>,
}

/// the video id in a youtube url, if it is one.
#[must_use]
pub fn youtube_id(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?.trim_start_matches("www.");
    match host {
        "youtu.be" => {
            let id = parsed.path_segments()?.next()?.to_owned();
            (!id.is_empty()).then_some(id)
        }
        "youtube.com" | "m.youtube.com" | "music.youtube.com" => parsed
            .query_pairs()
            .find(|(k, _)| k == "v")
            .map(|(_, v)| v.into_owned())
            .or_else(|| {
                parsed
                    .path()
                    .strip_prefix("/embed/")
                    .map(str::to_owned)
                    .or_else(|| parsed.path().strip_prefix("/shorts/").map(str::to_owned))
            })
            .filter(|id: &String| !id.is_empty()),
        _ => None,
    }
}

/// read a playlist page into bookmarks.
///
/// the key a playlist page holds each entry's id under.
///
/// the key is followed by a colon, and youtube has shipped the page both with and
/// without a space after it, so a value is found by its closing quote rather than
/// by a fixed offset. a fixed offset is the kind of thing that silently drops the
/// first character of every id and produces a url that 404s.
const VIDEO_ID_KEY: &str = r#""videoId""#;

/// the page embeds its entries as json inside a script tag, so the ids are
/// pulled from that rather than by parsing a rendered list.
pub fn parse_youtube_playlist(body: &[u8], playlist: &str) -> Result<Vec<Bookmark>> {
    let text = String::from_utf8_lossy(body);
    let entries = youtube_entries(&text);

    if entries.is_empty() {
        return Err(Error::Ingest(format!("{playlist} has no readable entries")));
    }

    let mut out = Vec::with_capacity(entries.len());
    for (id, title, channel) in entries {
        let url = format!("https://www.youtube.com/watch?v={id}");
        let text = title.clone().unwrap_or_else(|| format!("YouTube video {id}"));
        let mut bookmark = build(
            SourceMedium::YouTube,
            id.clone(),
            text,
            channel.as_deref(),
            None,
            None,
            Some(&url),
        );
        bookmark.title = title;
        bookmark.push_tag("youtube");
        // a video has no text until the transcript is fetched, and a stage that
        // reads the body of a bookmark with no body finds nothing and says
        // nothing, so the queue says so out loud
        bookmark.tags.insert("needs-transcript".to_owned());
        link(&mut bookmark, &url);
        out.push(bookmark);
    }
    Ok(out)
}

/// one entry of a playlist page: an id, and whatever the page said about it.
type Entry = (String, Option<String>, Option<String>);

/// read every entry out of a playlist page.
///
/// the page holds its entries as json inside a script tag, and youtube has
/// shipped two shapes for that: `playlistVideoRenderer`, where each entry is an
/// object with a `videoId` and a `title`, and `lockupViewModel`, which puts the
/// id on `contentId` and the title under `metadata`. reading the json and
/// handling both is what a rewrite of the site costs otherwise, and reading only
/// the ids gives a bookmark whose title is its own url.
fn youtube_entries(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Some(data) = embedded_json(text, "ytInitialData") {
        collect_youtube_entries(&data, &mut out);
    }
    // the older pages and the client variants that some requests get back hold
    // the entries as bare `videoId` pairs, which is all there is to read
    collect_youtube_ids(text, &mut out);

    out.retain(|(id, _, _)| !id.is_empty() && seen.insert(id.clone()));
    out
}

/// the balanced json object that follows an assignment to `name`.
fn embedded_json(text: &str, name: &str) -> Option<serde_json::Value> {
    let marker = format!("{name} = ");
    let at = text.find(&marker)? + marker.len();
    let start = text[at..].find('{')? + at;
    let end = balanced_end(text, start)?;
    serde_json::from_str(&text[start..=end]).ok()
}

/// the index of the `}` that closes the object opening at `start`.
///
/// braces inside strings do not count, and a `"` inside a string does not end
/// the string. counting them without that check finds the end of the wrong
/// object about half the time on a page this size.
fn balanced_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        let at = start + offset;
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// walk the page's json and collect both entry shapes, in the order they appear.
fn collect_youtube_entries(value: &serde_json::Value, out: &mut Vec<Entry>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "playlistVideoRenderer" || key == "lockupViewModel" {
                    if let Some(entry) = youtube_entry(child) {
                        out.push(entry);
                        continue;
                    }
                }
                collect_youtube_entries(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_youtube_entries(item, out);
            }
        }
        _ => {}
    }
}

/// read one entry, whichever of the two shapes it is.
fn youtube_entry(value: &serde_json::Value) -> Option<Entry> {
    let pointer = |path: &str| value.pointer(path).and_then(serde_json::Value::as_str);

    let id = pointer("/videoId").or_else(|| pointer("/contentId")).map(str::to_owned)?;

    // the title is a `runs` array in the older shape and a `content` string in
    // the newer one, and both are inside `title` rather than beside it
    let title = value
        .pointer("/title/simpleText")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value.pointer("/title/runs").and_then(serde_json::Value::as_array).map(|runs| {
                runs.iter()
                    .filter_map(|run| run.get("text").and_then(serde_json::Value::as_str))
                    .collect::<String>()
            })
        })
        .or_else(|| pointer("/metadata/lockupMetadataViewModel/title/content").map(str::to_owned))
        .filter(|title| !title.trim().is_empty());

    // the channel is the first metadata row in the newer shape and a byline in
    // the older one. a row can hold several parts and the channel is the first,
    // with the view count and the duration after it.
    let channel = pointer("/shortBylineText/runs/0/text")
        .or_else(|| pointer("/shortBylineText/simpleText"))
        .map(str::to_owned)
        .or_else(|| {
            value
                .pointer("/metadata/lockupMetadataViewModel/metadata/contentMetadataViewModel/metadataRows")
                .and_then(serde_json::Value::as_array)
                .and_then(|rows| rows.first())
                .and_then(|row| row.pointer("/metadataParts/0/text/content"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .filter(|channel| !channel.trim().is_empty());

    Some((id, title, channel))
}

/// the ids on a page that has no parsable entry objects.
fn collect_youtube_ids(text: &str, out: &mut Vec<Entry>) {
    let mut cursor = 0usize;
    while let Some(at) = text[cursor..].find(VIDEO_ID_KEY) {
        let after = cursor + at + VIDEO_ID_KEY.len();
        let Some(open) = text[after..].find('"').map(|o| after + o + 1) else { break };
        let Some(close) = text[open..].find('"').map(|c| open + c) else { break };
        out.push((text[open..close].to_owned(), None, None));
        cursor = close;
    }
}

/// the youtube source.
#[derive(Debug)]
pub struct YouTube {
    http: Http,
    playlists: Vec<String>,
}

impl YouTube {
    /// build the adapter.
    pub fn new(http: Http, playlists: Vec<String>) -> Self {
        Self { http, playlists }
    }
}

#[async_trait]
impl Source for YouTube {
    fn medium(&self) -> SourceMedium {
        SourceMedium::YouTube
    }

    async fn fetch(&self, _request: &FetchRequest) -> Result<FetchPage> {
        let mut items = Vec::new();
        let mut skipped = 0usize;
        for playlist in &self.playlists {
            let response = self.http.send(&Request::get(playlist)).await?;
            match parse_youtube_playlist(&response.body, playlist) {
                Ok(mut found) => items.append(&mut found),
                Err(_) => skipped += 1,
            }
        }
        Ok(FetchPage { items, has_more: false, next_cursor: None, skipped })
    }
}
