//! the x source.
//!
//! two ways in, one bookmark shape out.
//!
//! - **native**, against x's own graphql endpoint using the session cookies a
//!   browser already has. no extra binary, and the whole point of the
//!   rewrite.
//! - **bird**, shelling out to the `bird` cli, for an install that already
//!   has it and prefers its auth handling.
//!
//! # what is verified and what is not
//!
//! the parsing half of both paths is covered by tests against recorded
//! responses, because that is where the bugs live and it is the part that can
//! be tested without an account. the request half is not covered by an
//! automated test, because that would need a live session. the first run
//! against the real endpoint is where that gets checked, and the adapter fails
//! with a clear message rather than a silent empty result if the shape has
//! moved.

use async_trait::async_trait;
use mbm_core::bookmark::{Bookmark, Media, MediaKind, SourceRef, ThreadRole};
use mbm_core::error::{Error, Result};
use mbm_core::medium::SourceMedium;
use mbm_core::port::{FetchPage, FetchRequest, Source};
use mbm_extract::{Http, Request};
use serde_json::{Value, json};
use url::Url;

/// x's graphql endpoint. the bearer token here is the public one the web app
/// ships with; it is not a secret and it does not authenticate on its own.
const GRAPHQL: &str = "https://x.com/i/api/graphql";

/// the two paths every adapter in this file builds.
///
/// the whole reason these are fields rather than literals in the middle of a
/// function is that a server that answers the same protocol somewhere else is
/// then a configuration change: a mirror, a proxy, a replay, or a test.
const DEFAULT_GRAPHQL: &str = GRAPHQL;
const DEFAULT_POST_URL: &str = "https://x.com";

/// the public bearer the web client sends.
const BEARER: &str =
    "AAAAAAAAAAAAAAAAAAAAANRILgAAAAAA1qcIY9SjgRzByYKt2%2FwuM2Pm6lNqCi4%2FyGeN5c4vcm9JKI4hlxj";

/// the query id for a bookmarks page. these change when x ships a change, and
/// a stale one comes back as a 404 with an empty body, which
/// [`XClient::fetch_bookmarks`] reports as a shape error rather than as "no
/// bookmarks".
const BOOKMARKS_QUERY_ID: &str = "VpbH7gkKQ4G5j1oGCk0r0C0Bc";

/// session cookies, read out of a browser export.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cookies {
    /// the `auth_token` cookie.
    pub auth_token: Option<String>,
    /// the `ct0` cookie, which every graphql call must echo back.
    pub ct0: Option<String>,
}

impl Cookies {
    /// whether both required cookies are present.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.auth_token.as_deref().is_some_and(|t| !t.is_empty())
            && self.ct0.as_deref().is_some_and(|c| !c.is_empty())
    }

    /// the `cookie` header value.
    #[must_use]
    pub fn header_value(&self) -> String {
        let mut parts = Vec::new();
        if let Some(token) = &self.auth_token {
            parts.push(format!("auth_token={token}"));
        }
        if let Some(ct0) = &self.ct0 {
            parts.push(format!("ct0={ct0}"));
        }
        parts.join("; ")
    }

    /// read the two cookies out of a netscape cookie jar.
    #[must_use]
    pub fn from_cookie_jar(body: &str) -> Self {
        let mut out = Self::default();
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split('\t');
            // the netscape shape is domain, flag, path, secure, expiry, name,
            // value. a jar written by a header-dumping tool may use a
            // name=value; line, so that shape is handled too.
            let Some((name, value)) = fields.nth(5).zip(fields.next()) else {
                if let Some((name, value)) = line.split_once('=') {
                    match name.trim() {
                        "auth_token" => out.auth_token = Some(value.trim().to_owned()),
                        "ct0" => out.ct0 = Some(value.trim().to_owned()),
                        _ => {}
                    }
                }
                continue;
            };
            match name {
                "auth_token" => out.auth_token = Some(value.to_owned()),
                "ct0" => out.ct0 = Some(value.to_owned()),
                _ => {}
            }
        }
        out
    }
}

/// a bookmark folder id, which becomes a tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Folder {
    /// the folder's id, from the bookmarks url.
    pub id: String,
    /// a name to file it under.
    pub name: String,
}

/// the x client.
#[derive(Debug, Clone)]
pub struct XClient {
    http: Http,
    cookies: Cookies,
    folders: Vec<Folder>,
    query_id: String,
    graphql: String,
    post_url: String,
}

impl XClient {
    /// build a client.
    pub fn new(http: Http, cookies: Cookies) -> Self {
        Self {
            http,
            cookies,
            folders: Vec::new(),
            query_id: BOOKMARKS_QUERY_ID.to_owned(),
            graphql: DEFAULT_GRAPHQL.to_owned(),
            post_url: DEFAULT_POST_URL.to_owned(),
        }
    }

    /// point the client at a different server.
    ///
    /// `graphql` is the endpoint that answers the bookmarks query, and
    /// `post_url` is the host a permalink is built from. a server that answers
    /// the same protocol somewhere else is then a configuration change rather
    /// than a code change.
    #[must_use]
    pub fn with_endpoints(
        mut self,
        graphql: impl Into<String>,
        post_url: impl Into<String>,
    ) -> Self {
        self.graphql = graphql.into();
        self.post_url = post_url.into();
        self
    }

    /// the graphql endpoint this client is pointed at.
    #[must_use]
    pub fn graphql(&self) -> &str {
        &self.graphql
    }

    /// the host a permalink is built from.
    #[must_use]
    pub fn post_url(&self) -> &str {
        &self.post_url
    }

    /// watch these bookmark folders, one fetch each.
    #[must_use]
    pub fn with_folders(mut self, folders: Vec<Folder>) -> Self {
        self.folders = folders;
        self
    }

    /// use a different graphql query id, for when x ships a change.
    #[must_use]
    pub fn with_query_id(mut self, id: impl Into<String>) -> Self {
        self.query_id = id.into();
        self
    }

    /// whether the client has the cookies it needs.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.cookies.is_complete()
    }

    /// fetch one page of bookmarks, from one folder or from the whole list.
    async fn fetch_bookmarks(&self, count: usize, folder: Option<&str>) -> Result<Page> {
        if !self.is_ready() {
            return Err(Error::Auth(
                "x needs `auth_token` and `ct0`. copy them out of your browser's cookies."
                    .to_owned(),
            ));
        }

        let mut variables = json!({
            "count": count,
            "includePromotedContent": false,
            "withCommunity": false,
            "withV2Timeline": true,
        });
        if let Some(folder) = folder {
            variables["bookmarkFolderId"] = Value::String(folder.to_owned());
        }

        let url = format!(
            "{}/{}/Bookmarks?variables={}",
            self.graphql,
            self.query_id,
            urlencode(&variables.to_string())
        );

        let response = self
            .http
            .send(
                &Request::get(url)
                    .header("authorization", format!("Bearer {BEARER}"))
                    .header("x-twitter-active-user", "yes")
                    .header("x-twitter-client-language", "en")
                    .header("cookie", self.cookies.header_value()),
            )
            .await?;

        Page::parse(&response.body)
    }

    /// fetch one page from the `bird` cli.
    async fn fetch_via_bird(&self, count: usize, folder: Option<&str>) -> Result<Vec<Bookmark>> {
        let mut command = std::process::Command::new("bird");
        command.arg("bookmarks").arg("-n").arg(count.to_string()).arg("--json");
        if let Some(folder) = folder {
            command.arg("--folder-id").arg(folder);
        }
        if let Some(token) = &self.cookies.auth_token {
            command.env("AUTH_TOKEN", token);
        }
        if let Some(ct0) = &self.cookies.ct0 {
            command.env("CT0", ct0);
        }

        let output = tokio::task::spawn_blocking(move || command.output())
            .await
            .map_err(|e| Error::Agent(format!("bird: {e}")))?
            .map_err(|e| Error::Agent(format!("cannot run bird: {e}")))?;

        if !output.status.success() {
            return Err(Error::Agent(format!(
                "bird exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        parse_bird(&output.stdout)
    }
}

/// one page of a graphql response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Page {
    /// the bookmarks in it.
    pub instructions: Vec<Value>,
    /// the cursor for the next page.
    pub cursor: Option<String>,
    /// how many were asked for.
    pub requested: usize,
}

impl Page {
    /// read a graphql response.
    ///
    /// an empty response is the shape x returns when a query id has gone stale,
    /// so that case is reported as a shape error rather than as an empty page.
    pub fn parse(body: &[u8]) -> Result<Self> {
        let value: Value =
            serde_json::from_slice(body).map_err(|e| Error::Ingest(format!("x: not json: {e}")))?;

        let data = value.get("data").ok_or_else(|| {
            Error::Ingest(format!(
                "x returned no data. the endpoint or the query id has probably changed: {}",
                truncate(&value.to_string(), 200)
            ))
        })?;

        let entries = data
            .get("bookmark_timeline_v2")
            .and_then(|t| t.get("instructions"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                Error::Ingest(
                    "x returned no bookmark_timeline_v2. the response shape has changed."
                        .to_owned(),
                )
            })?;

        let mut instructions = Vec::new();
        let mut cursor = None;
        let mut requested = 0usize;

        for entry in entries {
            if let Some(next) = entry.pointer("/entryId").and_then(Value::as_str) {
                let _ = next;
            }
            // the cursor rides on an `addEntries` entry at the tail
            if cursor.is_none()
                && let Some(value) = entry.pointer("/entry/content/itemContent/tweet_results/result/core/user_results/result/favorite_tweet")
            {
                let _ = value;
            }
            if let Some(entries) = entry.get("entries").and_then(Value::as_array) {
                for candidate in entries {
                    let has_tweet =
                        candidate.pointer("/content/itemContent/tweet_results").is_some();
                    if has_tweet {
                        instructions.push(candidate.clone());
                        requested += 1;
                    }
                }
            }
            if let Some(next) = cursor_value(entry) {
                cursor = Some(next);
            }
        }

        Ok(Self { instructions, cursor, requested })
    }

    /// the bookmarks in this page.
    pub fn bookmarks(&self) -> Vec<Bookmark> {
        self.bookmarks_from(DEFAULT_POST_URL)
    }

    /// the bookmarks in this page, with permalinks built from a given host.
    pub fn bookmarks_from(&self, post_url: &str) -> Vec<Bookmark> {
        self.instructions.iter().filter_map(|value| entry(value, post_url)).collect()
    }
}

/// the `cursor-bottom` value an entry carries.
fn cursor_value(entry: &Value) -> Option<String> {
    for key in ["cursor-bottom", "cursorTop", "cursor-bottom-value"] {
        if let Some(value) = entry.get(key).and_then(Value::as_str)
            && value != "-1"
        {
            return Some(value.to_owned());
        }
    }
    None
}

/// read one timeline entry into a bookmark.
fn entry(value: &Value, post_url: &str) -> Option<Bookmark> {
    let result = value.pointer("/content/itemContent/tweet_results/result")?;
    // a tombstone is a deleted post, with a `tweetText` saying so
    let tweet = pick_tweet(result)?;

    // the id is on the result in every shape. `legacy` calls it `id_str` and
    // drops it entirely in some responses, so the result is the only place
    // worth reading it from.
    let id = result
        .get("rest_id")
        .or_else(|| tweet.get("rest_id"))
        .or_else(|| tweet.get("id_str"))
        .and_then(Value::as_str)?
        .to_owned();
    let text = tweet.get("full_text")?.as_str().unwrap_or_default().to_owned();
    let created = tweet.get("created_at").and_then(Value::as_str).and_then(crate::json::parse_date);

    let user = pick_user(result, &tweet)?;
    let screen_name = user.get("screen_name")?.as_str()?.to_owned();
    // x writes a handle with its `@`, and the mention in the post that points at
    // this account is written the same way, so the archive stores it that way and
    // the two match when someone searches for the mention. a permalink is the
    // exception: the site writes the path without it.
    let handle = format!("@{screen_name}");
    let name = user.get("name").and_then(Value::as_str).map(str::to_owned);

    let url = Url::parse(&format!("{post_url}/{screen_name}/status/{id}")).ok();
    let source = SourceRef::new(SourceMedium::X, id, url.clone());
    let mut bookmark = Bookmark::new(source, text, created.unwrap_or_else(crate::net::now));
    bookmark.created_at = created;
    bookmark.url = url;
    bookmark.author = Some(mbm_core::Author::new(&handle).with_name_opt(name));
    bookmark.push_tag(&handle);
    bookmark.media = tweet_media(&tweet);
    bookmark.role = thread_role(&tweet);
    if quoted_context(&tweet) {
        bookmark.role = Some(ThreadRole::Quote);
    }
    // the whole result, not just the picked tweet, because the archive sink
    // exists to keep the fields no reader of the archive uses
    bookmark.raw = Some(result.clone());
    Some(bookmark)
}

/// the tweet itself, whichever of the two shapes x returned.
///
/// the timeline endpoint has shipped the post wrapped in `legacy.full_tweet`
/// and the post itself, and a tombstone has neither, which is how a deleted
/// bookmark is told apart from a live one.
fn pick_tweet(result: &Value) -> Option<Value> {
    // three shapes for the same post, in the order the endpoint returns them:
    // the detail view nests a whole tweet under `legacy.full_tweet`, the
    // timeline puts the post's own fields straight in `legacy`, and a
    // tombstone puts them on the result itself. an adapter that only knows one
    // of the three reports an empty bookmark list for the other two.
    for path in ["/legacy/full_tweet", "/legacy", ""] {
        let candidate =
            if path.is_empty() { result.clone() } else { result.pointer(path)?.clone() };
        if candidate.is_object() && candidate.get("full_text").is_some() {
            return Some(candidate);
        }
    }
    None
}

/// the account that wrote a post.
///
/// the author lives beside the post rather than inside it, so it is read from
/// the result in both shapes. `core` is where the timeline puts it, and the
/// detail view is the one that nests it under the tweet.
fn pick_user(result: &Value, tweet: &Value) -> Option<Value> {
    for source in [result.pointer("/core"), tweet.pointer("/core"), result.pointer("/user")] {
        if let Some(user) = source.and_then(|c| c.get("user_results")).and_then(|u| u.get("result"))
            && user.get("screen_name").is_some()
        {
            return Some(user.clone());
        }
    }
    None
}

fn tweet_media(tweet: &Value) -> Vec<Media> {
    let mut out = Vec::new();
    let Some(entities) = tweet.get("entities") else { return out };
    let media = entities
        .get("extended_entities")
        .and_then(|e| e.get("media"))
        .or_else(|| entities.get("media"))
        .and_then(Value::as_array);

    let Some(media) = media else { return out };
    for item in media {
        let kind = match item.get("type").and_then(Value::as_str).unwrap_or("photo") {
            "video" => MediaKind::Video,
            "animated_gif" => MediaKind::Gif,
            _ => MediaKind::Photo,
        };
        let url =
            item.get("media_url_https").and_then(Value::as_str).and_then(|u| Url::parse(u).ok());
        let Some(url) = url else { continue };
        out.push(Media {
            kind,
            url: url.clone(),
            preview_url: item
                .get("preview_url")
                .and_then(Value::as_str)
                .and_then(|u| Url::parse(u).ok()),
            width: item
                .get("original_info")
                .and_then(|i| i.get("width"))
                .and_then(Value::as_u64)
                .map(|w| w as u32),
            height: item
                .get("original_info")
                .and_then(|i| i.get("height"))
                .and_then(Value::as_u64)
                .map(|h| h as u32),
            duration_ms: item
                .get("video_info")
                .and_then(|v| v.get("duration_millis"))
                .and_then(Value::as_u64),
            alt_text: None,
        });
    }
    out
}

fn thread_role(tweet: &Value) -> Option<ThreadRole> {
    if tweet.get("in_reply_to_status_id_str").is_some() {
        return Some(ThreadRole::Reply);
    }
    if tweet.get("is_thread").and_then(Value::as_bool).unwrap_or(false) {
        return Some(ThreadRole::Thread);
    }
    None
}

fn quoted_context(tweet: &Value) -> bool {
    tweet.get("quoted_status_id_str").is_some_and(|v| !v.is_null())
}

/// read the `bird` cli's output.
pub fn parse_bird(body: &[u8]) -> Result<Vec<Bookmark>> {
    let (items, _skipped) = crate::json::parse(body, SourceMedium::XBird)?;
    Ok(items)
}

/// the x source.
#[derive(Debug)]
pub struct X {
    client: XClient,
    use_bird: bool,
}

impl X {
    /// build the source.
    pub fn new(client: XClient) -> Self {
        Self { client, use_bird: false }
    }

    /// shell out to `bird` instead of calling graphql directly.
    #[must_use]
    pub fn via_bird_if(mut self, use_bird: bool) -> Self {
        self.use_bird = use_bird;
        self
    }
}

#[async_trait]
impl Source for X {
    fn medium(&self) -> SourceMedium {
        SourceMedium::X
    }

    async fn preflight(&self) -> Result<()> {
        if self.use_bird {
            if !mbm_extract::api::which("bird") {
                return Err(Error::Agent(
                    "`bird` is not on the path. remove the bird setting, or install it.".to_owned(),
                ));
            }
            return Ok(());
        }
        if !self.client.is_ready() {
            return Err(Error::Auth(
                "x needs `auth_token` and `ct0`. copy them out of your browser's cookies."
                    .to_owned(),
            ));
        }
        Ok(())
    }

    async fn fetch(&self, request: &FetchRequest) -> Result<FetchPage> {
        let count = request.limit.unwrap_or(20).clamp(1, 100);
        let mut items = Vec::new();
        let mut skipped = 0usize;

        if self.use_bird {
            // each folder is its own request, and one that fails is counted
            // rather than allowed to end the whole fetch
            match self.client.folders.first() {
                None => items = self.client.fetch_via_bird(count, None).await?,
                Some(_) => {
                    for folder in &self.client.folders {
                        match self.client.fetch_via_bird(count, Some(&folder.id)).await {
                            Ok(mut found) => {
                                for bookmark in &mut found {
                                    bookmark.push_tag(&folder.name);
                                }
                                items.append(&mut found);
                            }
                            Err(_) => skipped += 1,
                        }
                    }
                }
            }
            let found = items.len();
            return Ok(FetchPage { has_more: found >= count, items, next_cursor: None, skipped });
        }

        match self.client.folders.first() {
            None => {
                let page =
                    self.client.fetch_bookmarks(count, request.collection.as_deref()).await?;
                let found = page.bookmarks_from(self.client.post_url());
                skipped += count.saturating_sub(found.len());
                items = found;
            }
            Some(_) => {
                for folder in &self.client.folders {
                    let page = self
                        .client
                        .fetch_bookmarks(count, Some(&folder.id))
                        .await
                        .map_err(|e| Error::Ingest(format!("folder {}: {e}", folder.name)))?;
                    items.extend(page.bookmarks_from(self.client.post_url()).into_iter().map(
                        |mut b| {
                            b.push_tag(&folder.name);
                            b
                        },
                    ));
                }
            }
        }

        let found = items.len();
        Ok(FetchPage { has_more: found >= count, items, next_cursor: None, skipped })
    }
}

fn truncate(raw: &str, max: usize) -> String {
    raw.chars().take(max).collect()
}

/// percent-encode a query string value.
fn urlencode(raw: &str) -> String {
    mbm_extract::links::percent_encode_query(raw)
}
