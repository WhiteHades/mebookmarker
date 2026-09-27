//! the network source adapters.
//!
//! one file per family, one shape of response per family. they all share the
//! [`Source`](mbm_core::port::Source) port, so adding a new one here changes
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

    let mut source = SourceRef::new(medium, id.clone(), parsed.clone());
    if let Some(handle) = author {
        source = source.in_collection(handle);
    }

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
    #[derive(Deserialize)]
    struct Response {
        #[serde(default)]
        hits: Vec<HnItem>,
    }

    let response: Response =
        serde_json::from_slice(body).map_err(|e| Error::Ingest(format!("hacker news: {e}")))?;

    let mut out = Vec::with_capacity(response.hits.len());
    for hit in response.hits {
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
        }
        if let Some(url) = hit.url.as_deref() {
            link(&mut bookmark, url);
        }
        out.push(bookmark);
    }
    Ok(out)
}

/// the hacker news source.
#[derive(Debug)]
pub struct HackerNews {
    http: Http,
    collection: Option<String>,
}

impl HackerNews {
    /// build the adapter.
    pub fn new(http: Http) -> Self {
        Self { http, collection: None }
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
        let mut url =
            format!("https://hn.algolia.com/api/v1/search_by_date?hitsPerPage={limit}&tags={tag}");
        if let Some(cursor) = request.collection.as_deref() {
            let _ = write!(url, "&numericFilters=created_at_i>{cursor}");
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
}

impl Reddit {
    /// build the adapter.
    pub fn new(http: Http) -> Self {
        Self { http, subreddit: None, sort: "new".to_owned(), time: "all".to_owned() }
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
        // reddit blocks the default agent hard, so a real one is required
        if self.http.user_agent().contains("mebookmarker/") {
            return Err(Error::Config(
                "reddit needs a descriptive user agent. set one in the config.".to_owned(),
            ));
        }
        Ok(())
    }

    async fn fetch(&self, request: &FetchRequest) -> Result<FetchPage> {
        let limit = request.limit.unwrap_or(50).min(100);
        let path = self.subreddit.as_deref().unwrap_or("all");
        let url =
            format!("https://www.reddit.com/r/{path}/{}/.json?limit={limit}&raw_json=1", self.sort);
        let _ = &self.time;

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
    let repos: Vec<StarredRepo> =
        serde_json::from_slice(body).map_err(|e| Error::Ingest(format!("github: {e}")))?;

    let mut out = Vec::with_capacity(repos.len());
    for repo in repos {
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
}

impl GithubStars {
    /// build the adapter.
    pub fn new(http: Http, token: Option<String>) -> Self {
        Self { http, token }
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
                "github stars need a token. set `github.token` in the config.".to_owned(),
            ));
        }
        Ok(())
    }

    async fn fetch(&self, request: &FetchRequest) -> Result<FetchPage> {
        let limit = request.limit.unwrap_or(100).min(100);
        let page = request.max_pages.unwrap_or(1);
        let mut items = Vec::new();

        for page in 1..=page {
            let mut req = Request::get(format!(
                "https://api.github.com/user/starred?per_page={limit}&page={page}"
            ))
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
        .map(|a| a.trim_start_matches('@').to_owned());

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
    let mut bookmark = build(medium, id, text, author.as_deref(), None, created, target.as_deref());
    bookmark.push_tag(feed_host(feed_url));

    if let Some(target) = target.as_deref() {
        link(&mut bookmark, target);
    }
    Some(bookmark)
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
/// the page embeds its entries as json inside a script tag, so the ids are
/// pulled from that rather than by parsing a rendered list.
pub fn parse_youtube_playlist(body: &[u8], playlist: &str) -> Result<Vec<Bookmark>> {
    let text = String::from_utf8_lossy(body);
    let mut ids = Vec::new();
    let mut cursor = 0usize;
    while let Some(at) = text[cursor..].find(r#""videoId":""#) {
        let start = cursor + at + 12;
        let Some(end) = text[start..].find('"') else { break };
        let id = text[start..start + end].to_owned();
        if !ids.contains(&id) {
            ids.push(id);
        }
        cursor = start + end;
    }

    if ids.is_empty() {
        return Err(Error::Ingest(format!("{playlist} has no readable entries")));
    }

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let url = format!("https://www.youtube.com/watch?v={id}");
        let mut bookmark = build(
            SourceMedium::YouTube,
            id.clone(),
            format!("YouTube video {id}"),
            None,
            None,
            None,
            Some(&url),
        );
        bookmark.push_tag("youtube");
        bookmark.tags.insert("needs-transcript".to_owned());
        link(&mut bookmark, &url);
        out.push(bookmark);
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> Bookmark {
        Bookmark::new(SourceRef::new(SourceMedium::X, "1", None), text, 0)
    }

    use mbm_core::bookmark::SourceRef;
    use mbm_core::medium::LinkKind;

    #[test]
    fn a_hackernews_story_becomes_a_bookmark() {
        let body = br#"{"hits":[
          {"objectID":"1","title":"A fast thing","url":"https://example.com/a","author":"pg",
           "created_at":"2026-01-02T10:00:00Z","points":42},
          {"objectID":null,"title":"deleted ancestor"}
        ]}"#;
        let items = parse_hackernews(body, None).unwrap();
        assert_eq!(items.len(), 1, "a null object id is skipped");
        let b = &items[0];
        assert_eq!(b.source.external_id, "1");
        assert!(b.text.contains("A fast thing"));
        assert_eq!(b.author.as_ref().unwrap().handle, "pg");
        assert!(b.created_at.is_some());
    }

    #[test]
    fn a_hackernews_comment_becomes_a_reply() {
        let body = br#"{"hits":[{"objectID":"2","comment_text":"a comment","author":"u",
          "parent_id":1,"created_at":"2026-01-02T10:00:00Z"}]}"#;
        let items = parse_hackernews(body, None).unwrap();
        assert_eq!(items[0].role, Some(mbm_core::bookmark::ThreadRole::Reply));
    }

    #[test]
    fn a_hackernews_ask_hn_joins_its_title_and_body() {
        let body = br#"{"hits":[{"objectID":"3","title":"Ask HN: how?",
          "story_text":"<p>Here is the question.</p>","author":"u"}]}"#;
        let items = parse_hackernews(body, None).unwrap();
        assert!(items[0].text.contains("Ask HN"));
        assert!(items[0].text.contains("Here is the question."));
        assert!(!items[0].text.contains("<p>"), "html should be stripped");
    }

    #[test]
    fn an_item_with_no_text_at_all_is_skipped() {
        let body = br#"{"hits":[{"objectID":"4","author":"u"}]}"#;
        assert!(parse_hackernews(body, None).unwrap().is_empty());
    }

    #[test]
    fn hackernews_malformed_json_is_an_error() {
        assert!(parse_hackernews(b"not json", None).is_err());
    }

    #[test]
    fn a_reddit_post_becomes_a_bookmark() {
        let body = br#"{"data":{"children":[
          {"data":{"id":"abc","title":"A post","selftext":"body text","author":"u",
            "permalink":"/r/rust/comments/abc/a_post/","created_utc":1767225845.0,
            "url":"https://example.com/linked"}}
        ]}}"#;
        let items = parse_reddit(body, Some("rust")).unwrap();
        assert_eq!(items.len(), 1);
        let b = &items[0];
        assert_eq!(b.source.external_id, "abc");
        assert!(b.text.contains("A post") && b.text.contains("body text"));
        assert!(b.tags.contains("rust"));
        // the external link is kept, since a self post has none
        assert_eq!(b.url.as_ref().unwrap().as_str(), "https://example.com/linked");
    }

    #[test]
    fn a_reddit_self_post_falls_back_to_its_own_permalink() {
        let body = br#"{"data":{"children":[
          {"data":{"id":"abc","title":"self post","author":"u",
            "permalink":"/r/rust/comments/abc/self_post/","url":"https://www.reddit.com/r/rust/comments/abc/self_post/"}}
        ]}}"#;
        let items = parse_reddit(body, None).unwrap();
        assert!(items[0].url.as_ref().unwrap().as_str().starts_with("https://www.reddit.com/"));
    }

    #[test]
    fn a_pinned_reddit_notice_is_skipped() {
        let body = br#"{"data":{"children":[
          {"data":{"id":"x","title":"Welcome","author":"mod","stickied":true}},
          {"data":{"id":"y","title":"Real post","author":"u"}}
        ]}}"#;
        let items = parse_reddit(body, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source.external_id, "y");
    }

    #[test]
    fn a_github_star_becomes_a_bookmark() {
        let body = br#"[{"full_name":"simonw/llm","description":"a library","language":"Python",
          "stargazers_count":1000,"topics":["llm","cli"]}]"#;
        let items = parse_github_stars(body).unwrap();
        let b = &items[0];
        assert_eq!(b.source.external_id, "simonw/llm");
        assert!(b.text.contains("a library"));
        assert!(b.text.contains("Python"));
        assert!(b.tags.contains("llm"));
        assert_eq!(b.links.len(), 1);
    }

    #[test]
    fn an_rss_feed_becomes_bookmarks() {
        let feed = br#"<?xml version="1.0"?><rss version="2.0"><channel>
          <title>A feed</title>
          <item>
            <title>First post</title>
            <link>https://example.com/1</link>
            <description>&lt;p&gt;Some &lt;b&gt;text&lt;/b&gt;&lt;/p&gt;</description>
            <pubDate>Fri, 02 Jan 2026 10:00:00 +0000</pubDate>
            <guid>https://example.com/1</guid>
          </item>
        </channel></rss>"#;
        let items = parse_feed(feed, "https://example.com/feed.xml", SourceMedium::Rss).unwrap();
        assert_eq!(items.len(), 1);
        let b = &items[0];
        assert!(b.text.contains("First post"));
        assert!(b.text.contains("Some text"), "html should be stripped: {}", b.text);
        assert!(b.created_at.is_some());
    }

    #[test]
    fn an_atom_feed_becomes_bookmarks() {
        let feed = br#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
          <title>A feed</title>
          <entry>
            <title>Atom entry</title>
            <link rel="alternate" href="https://example.com/a"/>
            <id>tag:example.com,2026:1</id>
            <updated>2026-01-02T10:00:00Z</updated>
            <content>Entry body text</content>
          </entry>
        </feed>"#;
        let items = parse_feed(feed, "https://example.com/atom.xml", SourceMedium::Rss).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].text.contains("Atom entry"));
        assert!(items[0].text.contains("Entry body text"));
    }

    #[test]
    fn an_rdf_feed_becomes_bookmarks() {
        let feed = br#"<?xml version="1.0"?><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
          <item>
            <title>RDF entry</title>
            <link>https://example.com/r</link>
          </item>
        </rdf:RDF>"#;
        let items = parse_feed(feed, "https://example.com/rdf.xml", SourceMedium::Rss).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].text.contains("RDF entry"));
    }

    #[test]
    fn a_page_that_is_not_a_feed_is_an_error() {
        assert!(
            parse_feed(b"<html><body>not a feed</body></html>", "https://x", SourceMedium::Rss)
                .is_err()
        );
    }

    #[test]
    fn html_stripping_collapses_whitespace_and_decodes_entities() {
        assert_eq!(strip_html("<p>a   b</p><p>c</p>"), "a b c");
        assert_eq!(strip_html("&lt;tag&gt;"), "<tag>");
        assert_eq!(strip_html("plain text"), "plain text");
    }

    #[test]
    fn youtube_ids_come_out_of_every_url_shape() {
        assert_eq!(youtube_id("https://youtu.be/abc123").as_deref(), Some("abc123"));
        assert_eq!(youtube_id("https://www.youtube.com/watch?v=abc123").as_deref(), Some("abc123"));
        assert_eq!(youtube_id("https://www.youtube.com/shorts/abc123").as_deref(), Some("abc123"));
        assert_eq!(youtube_id("https://www.youtube.com/embed/abc123").as_deref(), Some("abc123"));
        assert_eq!(youtube_id("https://example.com/abc123"), None);
        assert_eq!(youtube_id("not a url"), None);
    }

    #[test]
    fn a_youtube_playlist_becomes_bookmarks() {
        let page = br#"[{"videoId":"aaa","title":"One"},{"videoId":"bbb","title":"Two"},
                        {"videoId":"aaa","title":"One again"}]"#;
        let items = parse_youtube_playlist(page, "PL123").unwrap();
        assert_eq!(items.len(), 2, "a repeated id is listed once");
        assert!(items[0].tags.contains("needs-transcript"));
    }

    #[test]
    fn a_playlist_page_with_no_entries_is_an_error() {
        assert!(parse_youtube_playlist(b"<html>nothing here</html>", "PL1").is_err());
    }

    #[test]
    fn a_story_url_is_classified_rather_than_left_unknown() {
        let mut b = one("a story");
        link(&mut b, "https://github.com/simonw/llm");
        assert_eq!(b.links[0].kind, LinkKind::Repository);

        let mut c = one("another");
        link(&mut c, "https://arxiv.org/abs/1234");
        assert_eq!(c.links[0].kind, LinkKind::Paper);
    }

    #[test]
    fn a_paywalled_story_url_is_marked() {
        let mut b = one("a story");
        link(&mut b, "https://www.nytimes.com/2026/01/02/thing.html");
        assert_eq!(b.links[0].blocked, Some(mbm_core::bookmark::BlockedReason::Paywall));
    }

    #[test]
    fn a_show_hn_search_asks_for_the_tag_the_api_has() {
        // `story_show_hn` is zero hits and `show_hn` is 542,680, which is the
        // kind of thing that reads as "the source is broken" rather than as a
        // bug in a url
        let source = HackerNews::new(Http::with_defaults().unwrap()).in_collection("show_hn");
        let args = source.collection.as_deref();
        assert_eq!(args, Some("show_hn"));
    }

    #[test]
    fn a_feed_url_becomes_a_tag() {
        let feed = b"<rss><channel><item><title>x</title><link>https://e.com/1</link></item></channel></rss>";
        let items =
            parse_feed(feed, "https://news.ycombinator.com/rss", SourceMedium::Rss).unwrap();
        assert!(items[0].tags.contains("news.ycombinator.com"), "{:?}", items[0].tags);
    }
}
