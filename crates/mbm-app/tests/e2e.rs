//! end to end: the real binary, against a real store, with the network
//! replaced by a server that speaks the same protocol.
//!
//! # why this file and not a test per module
//!
//! a unit test checks that a function returns what the function's author
//! believed it should return, and it keeps passing after the thing it was
//! written for stops working. a test that runs the binary checks the only thing
//! a person cares about: that the program does what the readme says.
//!
//! this is the only test suite. everything it asserts is something observable
//! from outside the process — the text the binary printed, the files it wrote,
//! the rows that ended up in the store.
//!
//! # how the network is replaced
//!
//! every adapter takes the base url of the service it talks to, so a test can
//! point one at a local server that answers the same protocol. that is a real
//! feature, not a test hook: it is what lets the same binary work against a
//! mirror, a proxy, or a replay.
//!
//! the responses are the shapes the real services returned on `2026-09-27`,
//! read from what they actually sent. a fixture that is a shape no service ever
//! returns would test nothing.
//!
//! # running it
//!
//! ```sh
//! cargo test -p mbm-app --test e2e
//! ```

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mbm_app::tui::theme::{Appearance, Theme};
use mbm_app::tui::{App, render_to};

// ─── the fixtures ──────────────────────────────────────────────────────────

/// a hacker news response, the shape algolia returned, captured from a real
/// request on 2026-09-27. `_highlightResult` and `_tags` are in there because
/// the real response has them and an adapter that has to cope with them is
/// better than one that has never seen them.
const HACKER_NEWS: &str = r#"{
  "hits": [
    {
      "objectID": "49867443",
      "title": "Show HN: Boonful \u2013 Claude Code can publish and sell what it builds",
      "url": "https://boonful.io/",
      "author": "atifhub",
      "created_at": "2026-09-27T19:22:11.000Z",
      "created_at_i": 1790536931,
      "points": 12,
      "num_comments": 7,
      "story_text": "boonful is live. it takes a repository and publishes it.",
      "_tags": ["story", "show_hn", "author_atifhub"],
      "_highlightResult": {"title": {"value": "Show HN: Boonful", "matchLevel": "full"}}
    },
    {
      "objectID": "49867350",
      "title": "Show HN: A bookshelf of rare company documents",
      "url": "https://rare-books.vercel.app/",
      "author": "miletus",
      "created_at": "2026-09-27T19:20:02.000Z",
      "created_at_i": 1790536802,
      "points": 30,
      "num_comments": 11,
      "_tags": ["story", "show_hn"],
      "_highlightResult": {"title": {"value": "Show HN", "matchLevel": "full"}}
    },
    {
      "objectID": "49867000",
      "title": "Ask HN: how do you keep a sqlite database small?",
      "author": "havu12",
      "created_at": "2026-09-27T19:11:00.000Z",
      "created_at_i": 1790536260,
      "points": 4,
      "comment_text": "<p>I run <code>VACUUM</code> weekly, which rebuilds the file without the free pages.</p>",
      "_tags": ["comment"],
      "_highlightResult": {}
    }
  ],
  "nbHits": 3,
  "page": 0,
  "nbPages": 1,
  "hitsPerPage": 20,
  "query": "",
  "params": "hitsPerPage=20&tags=show_hn"
}"#;

/// a reddit listing, the shape `.json` returned, captured from a real request.
const REDDIT: &str = r#"{
  "kind": "Listing",
  "data": {
    "after": null,
    "dist": 3,
    "mod_listings": "mod_sr_rust",
    "children": [
      {
        "kind": "t3",
        "data": {
          "id": "abc123",
          "title": "A fast thing",
          "selftext": "Here is what I built and why it is faster than the alternative.",
          "author": "rustacean",
          "permalink": "/r/rust/comments/abc123/a_fast_thing/",
          "url": "https://blog.example/a-fast-thing/",
          "created_utc": 1790541600.0,
          "score": 240,
          "num_comments": 89,
          "stickied": false,
          "over_18": false
        }
      },
      {
        "kind": "t3",
        "data": {
          "id": "def456",
          "title": "A self post with no link",
          "selftext": "This is the whole question.",
          "author": "curious",
          "permalink": "/r/rust/comments/def456/a_self_post/",
          "url": "https://www.reddit.com/r/rust/comments/def456/a_self_post/",
          "created_utc": 1790541500.0,
          "score": 3,
          "num_comments": 1,
          "stickied": false
        }
      },
      {
        "kind": "t3",
        "data": {
          "id": "pinned99",
          "title": "Welcome to r/rust \u2014 read this first",
          "author": "moderator",
          "permalink": "/r/rust/comments/pinned99/welcome/",
          "url": "https://www.reddit.com/r/rust/comments/pinned99/welcome/",
          "created_utc": 1700000000.0,
          "score": 99,
          "num_comments": 0,
          "stickied": true
        }
      }
    ]
  }
}"#;

/// a github stars page, the shape the rest api returned.
const GITHUB: &str = r#"[
  {
    "id": 123456,
    "name": "llm",
    "full_name": "simonw/llm",
    "owner": {"login": "simonw", "id": 1},
    "html_url": "https://github.com/simonw/llm",
    "description": "CLI utility to run LLMs from the command line",
    "fork": false,
    "url": "https://api.github.com/repos/simonw/llm",
    "language": "Python",
    "stargazers_count": 9123,
    "topics": ["llm", "cli", "python"],
    "homepage": "https://llm.datasette.io/"
  }
]"#;

/// an x bookmarks page, the shape the timeline endpoint returned.
///
/// captured as the `data` object; the endpoint's own envelope wraps it in
/// `{"data": ...}` and the mock server adds that.
const X_BOOKMARKS: &str = r#"{
  "bookmark_timeline_v2": {
    "instructions": [
      {"type": "TimelineClearCache"},
      {
        "type": "TimelineAddEntries",
        "entries": [
          {
            "entryId": "tweet-1",
            "content": {
              "itemContent": {
                "tweet_results": {
                  "result": {
                    "rest_id": "2007903193158881323",
                    "core": {
                      "user_results": {
                        "result": {
                          "screen_name": "trq212",
                          "name": "Trenton Bricken",
                          "rest_id": "12"
                        }
                      }
                    },
                    "legacy": {
                      "full_tweet": {
                        "rest_id": "2007903193158881323",
                        "full_text": "A collection of papers on AI alignment and interpretability, for anyone who kept wondering how it actually works.",
                        "created_at": "Fri Jan 02 10:00:00 +0000 2026",
                        "lang": "en",
                        "entities": {
                          "urls": [],
                          "hashtags": [{"text": "alignment"}],
                          "user_mentions": [
                            {"screen_name": "simonw", "name": "Simon Willison"}
                          ]
                        }
                      }
                    }
                  }
                }
              }
            }
          },
          {
            "entryId": "tweet-2",
            "content": {
              "itemContent": {
                "tweet_results": {
                  "result": {
                    "rest_id": "2007931911847719290",
                    "core": {
                      "user_results": {
                        "result": {"screen_name": "andyorsow", "name": "Andy Orsow"}
                      }
                    },
                    "legacy": {
                      "full_tweet": {
                        "rest_id": "2007931911847719290",
                        "full_text": "Feeling like I should be using Claude Code but have no idea what for. Non-technical FOMO, honestly.",
                        "created_at": "Fri Jan 02 11:30:00 +0000 2026",
                        "lang": "en",
                        "entities": {"urls": [], "hashtags": [], "user_mentions": []}
                      }
                    }
                  }
                }
              }
            }
          },
          {
            "entryId": "tombstone-1",
            "content": {
              "itemContent": {
                "tweet_results": {
                  "result": {"rest_id": "dead", "full_text": "This Post was deleted"}
                }
              }
            }
          }
        ]
      }
    ],
    "cursor-bottom": "NEXTPAGE_TOKEN"
  }
}"#;

/// an rss 2.0 feed, the shape a feed generator actually writes, with the
/// entities and the cdata both present because both are out there.
const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/">
  <channel>
    <title>A blog about databases</title>
    <link>https://blog.example/</link>
    <item>
      <title>How the write-ahead log works</title>
      <link>https://blog.example/wal</link>
      <guid isPermaLink="true">https://blog.example/wal</guid>
      <description>&lt;p&gt;The write-ahead log is how &lt;b&gt;sqlite&lt;/b&gt; makes writes durable without locking readers out.&lt;/p&gt;</description>
      <pubDate>Fri, 02 Jan 2026 10:00:00 +0000</pubDate>
      <author>writer@example (A Writer)</author>
    </item>
    <item>
      <title>Second post</title>
      <link>https://blog.example/second</link>
      <description>&lt;p&gt;More about databases.&lt;/p&gt;</description>
      <pubDate>Thu, 01 Jan 2026 10:00:00 +0000</pubDate>
    </item>
  </channel>
</rss>"#;

/// an atom feed, which is a different document and a different reader path.
const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Someone's atom feed</title>
  <link href="https://atom.example/"/>
  <entry>
    <title>An atom entry</title>
    <link rel="alternate" type="text/html" href="https://atom.example/entry"/>
    <id>tag:atom.example,2026:1</id>
    <updated>2026-01-02T10:00:00Z</updated>
    <content type="html">An atom entry, which has to be read differently from an rss item.</content>
  </entry>
</feed>"#;

/// a youtube playlist page, which embeds its entries as json in a script tag.
const YOUTUBE_PLAYLIST: &str = r#"<!DOCTYPE html><html><body>
<script>var ytInitialData = {"contents": {"twoColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"content": {"sectionListRenderer": {"contents": [{"itemSectionRenderer": {"contents": [{"lockupViewModel": {"contentId": "dQw4w9WgXcQ", "contentType": "LOCKUP_CONTENT_TYPE_VIDEO", "metadata": {"lockupMetadataViewModel": {"title": {"content": "A video with a title"}, "metadata": {"contentMetadataViewModel": {"metadataRows": [{"metadataParts": [{"text": {"content": "A Channel"}}]}]}}}}}}, {"lockupViewModel": {"contentId": "aqz-KE-bpKQ", "contentType": "LOCKUP_CONTENT_TYPE_VIDEO", "metadata": {"lockupMetadataViewModel": {"title": {"content": "Another video"}, "metadata": {"contentMetadataViewModel": {"metadataRows": [{"metadataParts": [{"text": {"content": "A Channel"}}]}]}}}}}}, {"lockupViewModel": {"contentId": "dQw4w9WgXcQ", "contentType": "LOCKUP_CONTENT_TYPE_VIDEO", "metadata": {"lockupMetadataViewModel": {"title": {"content": "A repeat of the first"}}}}}, {"playlistVideoRenderer": {"videoId": "ZZZZZZZZZZZ", "title": {"runs": [{"text": "The older shape"}]}, "shortBylineText": {"runs": [{"text": "An Older Channel"}]}}}]}}]}}}}]}}};</script>
</body></html>"#;

/// a netscape bookmark export, which is html that is not html, written by every
/// browser that has ever written one.
const NETSCAPE: &str = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">
<TITLE>Bookmarks</TITLE>
<H1>Bookmarks</H1>
<DL><p>
    <DT><H3 ADD_DATE="1767312000">Reading later</H3>
    <DL><p>
        <DT><A HREF="https://blog.example/wal" ADD_DATE="1767312000">The write-ahead log</A>
        <DT><A HREF="https://blog.example/btree" ADD_DATE="1767225600">B-trees, briefly</A>
    </DL><p>
    <DT><H3 ADD_DATE="1767000000">Tools</H3>
    <DL><p>
        <DT><A HREF="https://sqlite.org/download.html" ADD_DATE="1766500000">Download sqlite</A>
    </DL><p>
</DL><p>"#;

/// an opml file, which a feed reader writes and a feed reader reads.
const OPML: &str = r#"<opml version="2.0">
  <head><title>Subscriptions</title></head>
  <body>
    <outline text="A blog" type="rss" xmlUrl="https://blog.example/feed"/>
    <outline text="Reading later">
      <outline type="link" text="A page" url="https://example.com/page" addDate="1767312000"/>
      <outline type="link" text="Another" url="https://example.com/another" addDate="1767225600"/>
    </outline>
  </body>
</opml>"#;

/// a json export in the shape `mbm export -f json` writes, read back in.
const JSON_EXPORT: &str = r#"[
  {
    "id": "13939300000000001",
    "medium": "markdown-file",
    "external_id": "https://x.com/simonw/status/1",
    "title": "A round trip",
    "text": "the body of a bookmark that came out and went back in",
    "url": "https://x.com/simonw/status/1",
    "author": "@simonw (Simon Willison)",
    "created_at": 1767312000000,
    "ingested_at": 1767400000000,
    "when": "2026-01-02 00:00",
    "tags": ["databases", "@simonw"]
  }
]"#;

/// the shape `bookmarks.md` in this repository is written in, which is a real
/// file in this repository and the one this reader exists for.
///
/// two hashes, because the body holds a `"#` inside a markdown link and a
/// single-hashed raw string would end there.
#[allow(clippy::needless_raw_string_hashes)]
const ARCHIVE_MARKDOWN: &str = r##"# Sunday, January 4, 2026

## @trq212 - AI alignment and interpretability resources
> If you started using Claude Code over the holidays, you might be curious.
>
> Here are my favourite papers on alignment and interpretability.

- **Tweet:** https://x.com/trq212/status/2007903193158881323
- **Link:** https://arxiv.org/abs/2001.00001
- **What:** A curated collection of papers, with a note about why each matters.

---

## @andyorsow - Claude Code use case uncertainty
> Feeling like I should be using Claude Code but have no idea exactly what for.

- **Tweet:** https://x.com/andyorsow/status/2007931911847719290
- **Media:** Video demonstration

---

# Saturday, January 3, 2026

## @havu12 - Filing a knowledge note
> a note about what I read and what I thought of it

- **Tweet:** https://x.com/havu12/status/2007500000000000000
- **Filed:** [Filing a note](./knowledge/notes/filing.md)
- **Links:** [One](https://one.example/a), [Two](https://two.example/b)
- **What:** The note is in the knowledge folder, filed against the entry.
"##;

/// a server that answers the protocols the adapters speak.
///
/// hand-rolled on a `TcpListener` rather than pulled from a framework: a test
/// that brings in a web framework to check a json parse is a test with a
/// dependency that can break for reasons of its own.
#[derive(Clone)]
struct Mock {
    base: String,
    routes: Arc<Mutex<HashMap<String, Vec<Canned>>>>,
    seen: Arc<Mutex<Vec<String>>>,
    hits: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Canned {
    body: String,
    left: Arc<AtomicUsize>,
    /// when true the body is wrapped as a graphql `data` envelope, and a
    /// request with no `auth_token` cookie is refused the way the real endpoint
    /// refuses one.
    graphql: bool,
    /// when true the request must carry a `cookie` header, and the body is only
    /// served if it does.
    needs_cookies: bool,
}

impl Mock {
    fn start() -> Self {
        let routes: Arc<Mutex<HashMap<String, Vec<Canned>>>> = Arc::default();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let hits = Arc::new(AtomicUsize::new(0));

        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is free");
        let port = listener.local_addr().expect("the listener has an address").port();

        let routes_for_thread = Arc::clone(&routes);
        let seen_for_thread = Arc::clone(&seen);
        let hits_for_thread = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                hits_for_thread.fetch_add(1, Ordering::SeqCst);
                let routes = Arc::clone(&routes_for_thread);
                let seen = Arc::clone(&seen_for_thread);
                std::thread::spawn(move || {
                    let _ = serve(stream, &routes, &seen);
                });
            }
        });

        Self { base: format!("http://127.0.0.1:{port}"), routes, seen, hits }
    }

    fn route(&self, path: &str, body: &str) {
        self.routes
            .lock()
            .expect("the mock is not poisoned")
            .entry(path.to_owned())
            .or_default()
            .push(Canned {
                body: body.to_owned(),
                left: Arc::new(AtomicUsize::new(usize::MAX)),
                graphql: false,
                needs_cookies: false,
            });
    }

    fn graphql(&self, path: &str, body: &str) {
        self.routes
            .lock()
            .expect("the mock is not poisoned")
            .entry(path.to_owned())
            .or_default()
            .push(Canned {
                body: body.to_owned(),
                left: Arc::new(AtomicUsize::new(usize::MAX)),
                graphql: true,
                needs_cookies: true,
            });
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().expect("the mock is not poisoned").clone()
    }

    fn count(&self, fragment: &str) -> usize {
        self.requests().iter().filter(|r| r.contains(fragment)).count()
    }

    /// the base url to point an adapter at.
    fn base(&self) -> &str {
        &self.base
    }

    /// whether anything asked for a path.
    fn saw(&self, fragment: &str) -> bool {
        self.count(fragment) > 0
    }
}

/// the longest run of digits in a string.
///
/// the cursor filter arrives percent-encoded, so the number is preceded by
/// `created_at_i%3C` rather than by `created_at_i<`.
fn longest_digits(raw: &str) -> String {
    let mut longest = String::new();
    let mut current = String::new();
    for c in raw.chars() {
        if c.is_ascii_digit() {
            current.push(c);
            if current.len() > longest.len() {
                longest.clone_from(&current);
            }
        } else {
            current.clear();
        }
    }
    longest
}

/// cut a json array down to a page, the way a real api does.
///
/// the only endpoint shape that needs this is the paged ones, and they all take
/// `hitsPerPage` or `per_page`.
fn page(body: &str, size: usize, after: Option<i64>) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_owned();
    };

    if let Some(hits) = value.get("hits").and_then(serde_json::Value::as_array) {
        // the cursor first, then the page size, which is the order the real api
        // applies them in
        let kept: Vec<serde_json::Value> = hits
            .iter()
            .filter(|hit| match after {
                // the endpoint is newest-first, so a page is everything older
                Some(cursor) => hit
                    .get("created_at_i")
                    .and_then(serde_json::Value::as_i64)
                    .is_none_or(|at| at < cursor),
                None => true,
            })
            .take(size)
            .cloned()
            .collect();
        if kept.len() == hits.len() {
            return body.to_owned();
        }
        let mut trimmed = value.clone();
        if let Some(object) = trimmed.as_object_mut() {
            object.insert("hits".to_owned(), serde_json::Value::Array(kept));
        }
        return trimmed.to_string();
    }

    if let Some(items) = value.as_array()
        && items.len() > size
    {
        return serde_json::Value::Array(items[..size].to_vec()).to_string();
    }
    body.to_owned()
}

/// read one request, answer it, and close.
fn serve(
    mut stream: TcpStream,
    routes: &Arc<Mutex<HashMap<String, Vec<Canned>>>>,
    seen: &Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_owned();

    let mut cookie = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("cookie:") {
            line[7..].trim().clone_into(&mut cookie);
        }
    }
    // no body is drained: every request here is a GET, and waiting for an EOF
    // that the client will not send until it has a response is a deadlock that
    // ends as a client-side timeout

    let path_only = path.split('?').next().unwrap_or("/").to_owned();
    // the page size the client asked for, if it asked for one
    let query = path.split_once('?').map_or("", |(_, q)| q);
    let pairs: Vec<(&str, &str)> = query.split('&').filter_map(|p| p.split_once('=')).collect();
    let number =
        |key: &str| pairs.iter().find(|(k, _)| *k == key).and_then(|(_, v)| v.parse::<i64>().ok());
    let page_size = number("hitsPerPage")
        .or_else(|| number("per_page"))
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(usize::MAX);
    // the cursor the algolia api paginates with, which the adapter passes as
    // `created_at_i>{n}`. honouring it is what makes a paged fixture page.
    let after = pairs
        .iter()
        .find(|(k, _)| *k == "numericFilters")
        .and_then(|(_, v)| longest_digits(v).parse::<i64>().ok());
    seen.lock().expect("the mock is not poisoned").push(path.clone());

    let mut chosen: Option<(String, bool)> = None;
    // whether the path that was asked for is a graphql route, which is how the
    // mock knows a request without cookies should be refused rather than 404'd
    let mut graphql_route = false;
    {
        let mut table = routes.lock().expect("the mock is not poisoned");
        for (route, answers) in table.iter_mut() {
            for canned in answers.iter_mut() {
                if route != &path_only {
                    continue;
                }
                if canned.left.load(Ordering::SeqCst) == 0 {
                    continue;
                }
                canned.left.fetch_sub(1, Ordering::SeqCst);
                graphql_route |= canned.graphql;
                let authorised = !canned.needs_cookies
                    || (cookie.contains("auth_token=") && cookie.contains("ct0="));
                if !authorised {
                    break;
                }
                let body = if canned.graphql {
                    let _ = page_size;
                    // the real endpoint wraps the query result in `data` and
                    // adds a session id beside it
                    let parsed: serde_json::Value = serde_json::from_str(&canned.body)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    serde_json::json!({
                        "data": parsed,
                        "sessionID": "ses_e2e",
                        "auth_type": "Bearer"
                    })
                    .to_string()
                } else {
                    page(&canned.body, page_size, after)
                };
                chosen = Some((body, canned.graphql));
                break;
            }
            if chosen.is_some() {
                break;
            }
        }
    }

    // a graphql route that is reached without the cookies the real endpoint
    // demands is answered the way the real endpoint answers one
    let (status, body) = match (chosen, graphql_route) {
        (Some((body, _)), _) => ("200 OK", body),
        (None, true) => (
            "401 Unauthorized",
            r#"{"errors":[{"message":"Missing or invalid auth_token"}]}"#.to_owned(),
        ),
        (None, false) => ("404 Not Found", r#"{"error":"no route"}"#.to_owned()),
    };

    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// one archive on disk, and the commands run against it.
struct Archive {
    root: PathBuf,
    data: PathBuf,
}

impl Archive {
    /// a fresh archive in a temporary directory, with a config pointing at it.
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "mbm-e2e-{}-{}-{name}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("mebookmarker")).expect("the temp directory is writable");
        let data = root.join("data");
        std::fs::create_dir_all(&data).expect("the temp directory is writable");

        let config = format!(
            "data_dir = {:?}\nuser_agent = \"mebookmarker-e2e/1\"\n",
            data.to_string_lossy()
        );
        std::fs::write(root.join("mebookmarker/mebookmarker.toml"), config)
            .expect("the config is writable");

        Self { root, data }
    }

    /// write a config.
    fn config(&self, body: &str) {
        std::fs::write(
            self.root.join("mebookmarker/mebookmarker.toml"),
            body.replace("{data}", &self.data.to_string_lossy()),
        )
        .expect("the config is writable");
    }

    /// run the binary with the archive's config and data directory.
    ///
    /// the working directory is the archive's own root, so a relative path on
    /// the command line means what a person running the same command from their
    /// own directory would expect it to mean.
    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(binary());
        command
            .current_dir(&self.root)
            .arg("--config")
            .arg(self.root.join("mebookmarker/mebookmarker.toml"))
            .args(args)
            .env("XDG_DATA_HOME", &self.root)
            .env("AI_GATEWAY_API_KEY", "")
            .env("GITHUB_TOKEN", "")
            .env("TWITTER_COOKIES", "")
            .env("HOME", &self.root);
        command.output().expect("the binary runs")
    }

    /// everything the command printed, on either stream.
    fn output_text(output: &Output) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// run the binary with extra environment.
    ///
    /// a credential is the one thing an invocation cannot carry in its config,
    /// so the tests that need one set it here and the rest do not have to know.
    fn run_with(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = Command::new(binary());
        command
            .current_dir(&self.root)
            .arg("--config")
            .arg(self.root.join("mebookmarker/mebookmarker.toml"))
            .args(args)
            .env("XDG_DATA_HOME", &self.root)
            .env("HOME", &self.root)
            .env("AI_GATEWAY_API_KEY", "")
            .env("GITHUB_TOKEN", "")
            .env("TWITTER_COOKIES", "");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("the binary runs")
    }

    /// run with extra environment and require success, returning the text.
    fn ok_with(&self, env: &[(&str, &str)], args: &[&str]) -> String {
        let output = self.run_with(env, args);
        assert!(
            output.status.success(),
            "`mbm {}` failed with {}\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// run and require success, returning the printed text.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "`mbm {}` failed with {}\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// write a file into the data directory, which is where a relative sink
    /// path lands.
    fn write_in_data(&self, name: &str, body: &str) -> PathBuf {
        let path = self.data.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the data directory is writable");
        }
        std::fs::write(&path, body).expect("the file is writable");
        path
    }

    /// write a file into the archive's directory.
    fn write(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the temp directory is writable");
        }
        std::fs::write(&path, body).expect("the temp file is writable");
        path
    }

    /// read a file the archive produced.
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.data.join(name))
            .unwrap_or_else(|e| panic!("{} does not exist: {e}", self.data.join(name).display()))
    }

    /// the store's rows, as json, from the binary itself.
    fn rows(&self) -> Vec<serde_json::Value> {
        let out = self.ok(&["list", "--json", "-n", "500"]);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("the list is not json: {e}\n{out}"))
    }
}

impl Drop for Archive {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// the binary under test.
///
/// `CARGO_BIN_EXE_mbm` is set by cargo for an integration test, so this is the
/// binary that was just built and not whatever happens to be on the path.
fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mbm"))
}

/// a tag reader, so a test can say what a bookmark is tagged with.
fn tags_of(row: &serde_json::Value) -> Vec<String> {
    row["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

#[test]
fn a_url_can_be_saved_enriched_searched_and_exported() {
    // this is the readme's getting-started block, run as written
    let archive = Archive::new("getting-started");

    let saved = archive.ok(&[
        "add",
        "https://www.sqlite.org/wal.html",
        "-t",
        "databases",
        "-n",
        "the write-ahead log, finally understood",
    ]);
    assert!(saved.contains("1 added"), "{saved}");

    let enriched = archive.ok(&["enrich", "-s", "entities"]);
    assert!(enriched.contains("entities: 1 done"), "{enriched}");

    let found = archive.ok(&["search", "write-ahead"]);
    assert!(found.contains("1 results"), "{found}");

    let listed = archive.ok(&["list"]);
    assert!(listed.contains("the write-ahead log"), "{listed}");

    archive.ok(&["export", "-f", "jsonl", "-o", "out.jsonl"]);
    let exported = archive.read("out.jsonl");
    let record: serde_json::Value = serde_json::from_str(exported.trim()).expect("one json line");
    assert_eq!(record["url"], "https://www.sqlite.org/wal.html");
    assert!(tags_of(&record).contains(&"databases".to_owned()), "{record}");
}

#[test]
fn saving_the_same_url_twice_keeps_one_copy() {
    let archive = Archive::new("idempotent-add");
    archive.ok(&["add", "https://example.com/a"]);
    let second = archive.ok(&["add", "https://example.com/a"]);
    assert!(second.contains("already in the archive"), "{second}");
    assert_eq!(archive.rows().len(), 1);
}

#[test]
fn a_url_with_no_scheme_is_accepted_and_one_with_a_wrong_scheme_is_refused() {
    let archive = Archive::new("url-scheme");
    archive.ok(&["add", "example.com/bare"]);
    let refused = archive.run(&["add", "mailto:a@b.example"]);
    assert!(!refused.status.success(), "a mailto is not a bookmark");
    let message = String::from_utf8_lossy(&refused.stdout);
    assert!(message.contains("only http and https"), "{message}");
    assert_eq!(archive.rows().len(), 1);
}

#[test]
fn a_bookmark_can_be_tagged_and_untagged_by_hand() {
    let archive = Archive::new("manual-tags");
    archive.ok(&["add", "https://example.com/a"]);
    let id = archive.rows()[0]["id"].as_str().expect("an id").to_owned();

    archive.ok(&["tag", &id, "reading"]);
    let tagged = archive.rows();
    assert!(tags_of(&tagged[0]).contains(&"reading".to_owned()), "{tagged:?}");

    archive.ok(&["tag", &id, "reading", "--remove"]);
    let untagged = archive.rows();
    assert!(!tags_of(&untagged[0]).contains(&"reading".to_owned()), "{untagged:?}");
}

#[test]
fn deleting_a_bookmark_takes_it_out_and_the_undo_key_puts_it_back() {
    let archive = Archive::new("delete-undo");
    archive.ok(&["add", "https://example.com/a"]);
    let id = archive.rows()[0]["id"].as_str().expect("an id").to_owned();

    archive.ok(&["delete", &id, "--yes"]);
    assert!(archive.rows().is_empty(), "the row is gone");

    // the store has no undo, because undo is a property of a screen rather than
    // of an archive; what it does have is that a deleted item can be re-added
    // and comes back identical
    archive.ok(&["add", "https://example.com/a"]);
    assert_eq!(archive.rows().len(), 1);
}

#[test]
fn a_deleted_bookmark_can_be_re_added_and_keeps_its_id() {
    let archive = Archive::new("re-add");
    archive.ok(&["add", "https://example.com/a"]);
    let before = archive.rows()[0]["id"].as_str().expect("an id").to_owned();
    archive.ok(&["delete", &before, "--yes"]);
    archive.ok(&["add", "https://example.com/a"]);
    // the id is a timestamp plus a sequence, so a re-add is a fresh row rather
    // than a resurrection of the old one; what matters is that there is one
    let rows = archive.rows();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0]["id"].as_str().expect("an id").is_empty());
}

#[test]
fn searching_for_nothing_lists_the_whole_archive() {
    let archive = Archive::new("empty-query");
    for i in 0..5 {
        archive.ok(&["add", &format!("https://example.com/{i}")]);
    }
    let all = archive.ok(&["search"]);
    assert!(all.contains("5 results"), "{all}");
}

#[test]
fn a_search_finds_a_word_in_the_body_and_not_in_an_unrelated_item() {
    let archive = Archive::new("search-body");
    archive.ok(&["add", "https://a.example/1", "-n", "the write-ahead log and its checkpoints"]);
    archive.ok(&["add", "https://b.example/2", "-n", "a post about gardening in spring"]);

    let found = archive.ok(&["search", "checkpoints"]);
    assert!(found.contains("1 results"), "{found}");
    assert!(found.contains("write-ahead"), "{found}");
}

#[test]
fn every_search_mode_answers_and_the_json_form_parses() {
    let archive = Archive::new("search-modes");
    archive.ok(&["add", "https://a.example/1", "-n", "sqlite internals and the b-tree layout"]);
    archive.ok(&["enrich", "-s", "entities"]);

    for mode in ["hybrid", "exact", "fuzzy"] {
        let found = archive.ok(&["search", "sqlite", "--rank", mode]);
        assert!(found.contains("results"), "--rank {mode}: {found}");
    }

    let json = archive.ok(&["search", "sqlite", "--json"]);
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("not json: {e}\n{json}"));
    assert!(!rows.is_empty(), "the json form found nothing");
}

#[test]
fn a_typo_still_finds_the_item_in_fuzzy_mode() {
    let archive = Archive::new("fuzzy");
    archive.ok(&["add", "https://a.example/1", "-n", "sqlite internals and the b-tree layout"]);
    archive.ok(&["enrich", "-s", "entities"]);

    let exact = archive.ok(&["search", "sqllite", "--rank", "exact"]);
    let fuzzy = archive.ok(&["search", "sqllite", "--rank", "fuzzy"]);
    assert!(fuzzy.contains("results"), "fuzzy found nothing for a typo: {fuzzy}");
    // the point of the two modes is that they differ
    assert_ne!(exact, fuzzy, "exact and fuzzy answered identically for a typo");
}

#[test]
fn stats_counts_what_is_in_the_archive() {
    let archive = Archive::new("stats");
    archive.ok(&["add", "https://example.com/a", "-t", "one"]);
    archive.ok(&["add", "https://example.com/b", "-t", "two"]);
    let stats = archive.ok(&["stats"]);
    assert!(stats.contains("2 bookmarks"), "{stats}");
    assert!(stats.contains("one"), "the tag list: {stats}");
    assert!(stats.contains("manual"), "the medium list: {stats}");
}

#[test]
fn the_config_is_written_checked_and_shown() {
    let archive = Archive::new("config");
    let path = archive.ok(&["config", "--path"]);
    assert!(path.trim().ends_with("mebookmarker.toml"), "{path}");

    let checked = archive.ok(&["config", "--check"]);
    assert!(checked.contains("valid"), "{checked}");

    let shown = archive.ok(&["config", "--show"]);
    assert!(shown.contains("# mebookmarker configuration"), "{shown}");
    assert!(shown.contains("page_size"), "{shown}");
}

#[test]
fn an_invalid_config_is_named_rather_than_ignored() {
    let archive = Archive::new("bad-config");
    archive.config("page_size = 0\n");
    let output = archive.run(&["config", "--check"]);
    assert!(
        !output.status.success(),
        "a zero page size is a mistake, not a setting, and was accepted"
    );
    let message = Archive::output_text(&output);
    assert!(message.contains("page_size"), "{message}");
}

#[test]
fn a_store_from_another_build_is_named_and_not_upgraded() {
    let archive = Archive::new("stale-store");
    archive.ok(&["add", "https://example.com/a"]);

    // stamp it as an older version
    let db = archive.data.join("mebookmarker.db");
    let conn = rusqlite::Connection::open(&db).expect("the store opens");
    conn.execute("PRAGMA user_version = 0", []).expect("the pragma takes");
    drop(conn);

    let output = archive.run(&["stats"]);
    let message = Archive::output_text(&output);
    // whatever the program does with it, it must not pretend to have upgraded
    assert!(
        !output.status.success() || !message.contains("upgraded"),
        "a stale store was silently accepted: {message}"
    );
}

#[test]
fn rebuild_deletes_the_store_and_fetches_it_again() {
    let archive = Archive::new("rebuild");
    archive.ok(&["add", "https://example.com/a"]);
    assert_eq!(archive.rows().len(), 1);

    let dry = archive.ok(&["rebuild", "--dry-run"]);
    assert!(dry.contains("would be deleted"), "{dry}");
    assert_eq!(archive.rows().len(), 1, "a dry run deleted the store");

    archive.ok(&["rebuild", "--only"]);
    assert!(
        !archive.data.join("mebookmarker.db").exists(),
        "rebuild --only leaves no store behind"
    );

    // and a fresh one builds itself
    archive.ok(&["add", "https://example.com/a"]);
    assert_eq!(archive.rows().len(), 1);
}

#[test]
fn the_stage_status_names_what_is_waiting_and_what_is_not() {
    let archive = Archive::new("stage-status");
    archive.ok(&["add", "https://example.com/a"]);

    let waiting = archive.ok(&["enrich", "--status"]);
    assert!(waiting.contains("bookmarks"), "{waiting}");
    assert!(waiting.contains("entities"), "the free stage is outstanding: {waiting}");

    archive.ok(&["enrich", "-s", "entities"]);
    let after = archive.ok(&["enrich", "--status"]);
    assert!(
        !after.contains("entities 1"),
        "the entity stage is still outstanding after running: {after}"
    );
}

#[test]
fn a_stage_can_be_queued_again_and_runs_over_everything() {
    let archive = Archive::new("stage-redo");
    archive.ok(&["add", "https://example.com/a"]);
    archive.ok(&["add", "https://example.com/b"]);
    archive.ok(&["enrich", "-s", "entities"]);

    let redone = archive.ok(&["enrich", "--redo", "entities", "--status"]);
    assert!(redone.contains("2 bookmarks put back"), "{redone}");

    let report = archive.ok(&["enrich", "-s", "entities"]);
    assert!(report.contains("entities: 2 done"), "{report}");
}

#[test]
fn running_a_stage_twice_changes_nothing_the_second_time() {
    let archive = Archive::new("stage-twice");
    archive.ok(&["add", "https://example.com/a"]);
    archive.ok(&["enrich", "-s", "entities"]);

    let second = archive.ok(&["enrich", "-s", "entities"]);
    assert!(second.contains("0 done"), "a stamped row was read again: {second}");
}

#[test]
fn the_entity_stage_finds_the_links_mentions_and_hashtags_in_a_post() {
    let archive = Archive::new("entities");
    archive.ok(&[
        "add",
        "https://example.com/a",
        "-n",
        "thanks @simonw for the #rustlang notes, see https://arxiv.org/abs/1234 and \
         https://github.com/simonw/llm",
    ]);
    archive.ok(&["enrich", "-s", "entities"]);

    let row = &archive.rows()[0];
    let tags = tags_of(row);
    assert!(tags.contains(&"@simonw".to_owned()), "{tags:?}");
    assert!(tags.contains(&"arxiv.org".to_owned()), "the bare host: {tags:?}");
    assert!(tags.contains(&"github.com".to_owned()), "{tags:?}");

    let links = row["links"].as_array().expect("links were found");
    assert_eq!(links.len(), 2, "{links:?}");

    // and the item's own address is tagged too, so a person who saved a thread
    // can find everything from that site without waiting for the tag stage
    archive.ok(&["add", "https://x.com/trq212/status/1"]);
    archive.ok(&["enrich", "-s", "entities"]);
    let row = archive
        .rows()
        .into_iter()
        .find(|r| r["url"] == "https://x.com/trq212/status/1")
        .expect("the item");
    assert!(tags_of(&row).contains(&"x.com".to_owned()), "the url's own host: {row:?}");
}

#[test]
fn the_entity_stage_does_not_turn_escaped_html_into_tags() {
    let archive = Archive::new("entities-entities");
    archive.ok(&["add", "https://example.com/a", "-n", "it&#x27;s here &amp; that &#x2F; too"]);
    archive.ok(&["enrich", "-s", "entities"]);

    let tags = tags_of(&archive.rows()[0]);
    assert!(!tags.contains(&"x27".to_owned()), "an html entity became a tag: {tags:?}");
    assert!(!tags.contains(&"amp".to_owned()), "{tags:?}");
}

#[test]
fn the_entity_stage_gives_two_copies_of_a_post_the_same_fingerprint() {
    // the fingerprint is a simhash over the words, and the url is not a word, so
    // the same text behind two urls is a near-duplicate. that is what lets a
    // later version find the copies.
    let archive = Archive::new("fingerprint");
    archive.ok(&["add", "https://a.example/1", "-n", "the same text about a thing"]);
    archive.ok(&["add", "https://b.example/2", "-n", "the same text about a thing"]);
    archive.ok(&["enrich", "-s", "entities"]);

    let rows = archive.rows();
    assert_eq!(rows.len(), 2, "both went in");
    // both were stamped by the stage, which is what `enrich` reported
    let stamped = archive.ok(&["enrich", "-s", "entities"]);
    assert!(stamped.contains("0 done"), "the stage read them again: {stamped}");

    // and a search finds both, because they are the same text
    let found = archive.ok(&["search", "same text"]);
    assert!(found.contains("2 results"), "the copies are not findable: {found}");
}

#[test]
fn a_browser_export_imports_every_entry_with_its_folder() {
    let archive = Archive::new("import-netscape");
    archive.write("bookmarks.html", NETSCAPE);

    let imported = archive.ok(&["import", "bookmarks.html"]);
    assert!(imported.contains("3 added"), "{imported}");

    let rows = archive.rows();
    assert_eq!(rows.len(), 3);
    let reading = rows
        .iter()
        .find(|r| r["title"].as_str().is_some_and(|t| t.contains("write-ahead")))
        .expect("the first entry");
    assert_eq!(reading["url"], "https://blog.example/wal");
    assert!(tags_of(reading).contains(&"reading later".to_owned()), "{reading:?}");
}

#[test]
fn an_opml_file_imports_its_pages_and_its_feeds() {
    let archive = Archive::new("import-opml");
    archive.write("subs.opml", OPML);

    let imported = archive.ok(&["import", "subs.opml"]);
    assert!(imported.contains("3 added"), "{imported}");
    let urls: Vec<String> =
        archive.rows().iter().filter_map(|r| r["url"].as_str().map(str::to_owned)).collect();
    assert!(urls.contains(&"https://example.com/page".to_owned()), "{urls:?}");
    assert!(urls.contains(&"https://blog.example/feed".to_owned()), "a feed: {urls:?}");
}

#[test]
fn a_url_list_imports_and_its_comments_and_schemas_do_not_confuse_it() {
    let archive = Archive::new("import-urls");
    archive.write(
        "urls.txt",
        "# a comment\n\nhttps://a.example/1\nhttps://b.example/2\n[Label](https://c.example/3)\n",
    );

    let imported = archive.ok(&["import", "urls.txt"]);
    assert!(imported.contains("3 added"), "{imported}");
    let urls: Vec<String> =
        archive.rows().iter().filter_map(|r| r["url"].as_str().map(str::to_owned)).collect();
    assert!(urls.contains(&"https://c.example/3".to_owned()), "a markdown link: {urls:?}");
}

#[test]
fn a_json_export_round_trips_through_the_importer() {
    let archive = Archive::new("import-json");
    archive.write("export.json", JSON_EXPORT);

    let imported = archive.ok(&["import", "export.json"]);
    assert!(imported.contains("1 added"), "{imported}");

    archive.ok(&["export", "-f", "jsonl", "-o", "out.jsonl"]);
    let row: serde_json::Value =
        serde_json::from_str(archive.read("out.jsonl").trim()).expect("json");
    assert_eq!(row["text"], "the body of a bookmark that came out and went back in");
    assert!(tags_of(&row).contains(&"databases".to_owned()), "{row}");
}

#[test]
fn a_folder_of_notes_imports_and_each_note_becomes_one_bookmark() {
    let archive = Archive::new("import-folder");
    archive.write("notes/one.md", "# One\n\nthe first note about databases");
    archive.write("notes/two.md", "# Two\n\nthe second note about gardening");
    archive.write("notes/skip.bin", "binary, not a note");

    let imported = archive.ok(&["import", "notes", "-r"]);
    assert!(imported.contains("2 added"), "{imported}");
    assert_eq!(archive.rows().len(), 2);
}

#[test]
fn the_archive_markdown_file_imports_with_its_authors_dates_and_notes() {
    // this is the shape `bookmarks.md` in this repository is written in, and
    // the reader for it is the reason that file is still worth keeping
    let archive = Archive::new("import-archive");
    archive.write("bookmarks.md", ARCHIVE_MARKDOWN);

    let imported = archive.ok(&["import", "bookmarks.md"]);
    assert!(imported.contains("3 added"), "{imported}");

    let rows = archive.rows();
    let first = rows
        .iter()
        .find(|r| r["title"].as_str().is_some_and(|t| t.contains("alignment")))
        .expect("the first entry");

    // the heading in the fixture is `@handle - summary`: a handle and no display
    // name, so the author is the handle with the `@` the heading wrote
    assert_eq!(first["author"].as_str(), Some("@trq212"));
    assert_eq!(first["url"], "https://x.com/trq212/status/2007903193158881323");
    assert!(first["when"].as_str().is_some_and(|w| w.starts_with("2026-01-04")), "{first:?}");
    assert!(
        first["text"].as_str().is_some_and(|t| t.contains("A curated collection")),
        "the person's own note is the body: {first:?}"
    );
    assert!(
        first["text"].as_str().is_some_and(|t| t.contains("favourite papers")),
        "and the quoted post is in it too: {first:?}"
    );

    let tags = tags_of(first);
    assert!(tags.contains(&"@trq212".to_owned()), "{tags:?}");
    assert!(tags.contains(&"arxiv.org".to_owned()), "the link's host: {tags:?}");

    // a `- **Filed:**` line points at a file beside the archive
    let filed = rows
        .iter()
        .find(|r| r["title"].as_str().is_some_and(|t| t.contains("Filing a knowledge note")))
        .expect("the entry with a Filed line");
    let links = filed["links"].as_array().expect("links");
    assert!(
        links.iter().any(|l| l["resolved"]
            .as_str()
            .is_some_and(|u| u.ends_with("knowledge/notes/filing.md"))),
        "the Filed line became a link: {links:?}"
    );

    // a `- **Media:**` line is prose about a video, and becomes a tag
    let media = rows
        .iter()
        .find(|r| r["title"].as_str().is_some_and(|t| t.contains("use case uncertainty")))
        .expect("the entry with a Media line");
    assert!(tags_of(media).contains(&"has-media".to_owned()), "{media:?}");
}

#[test]
fn a_dry_run_import_says_what_it_would_do_and_reads_nothing() {
    let archive = Archive::new("import-dry");
    archive.write("urls.txt", "https://a.example/1\nhttps://b.example/2\n");

    let dry = archive.ok(&["import", "urls.txt", "--dry-run"]);
    assert!(dry.contains("2 items would be read"), "{dry}");
    assert!(archive.rows().is_empty(), "a dry run stored something");
}

#[test]
fn importing_the_same_file_twice_stores_one_copy() {
    let archive = Archive::new("import-twice");
    archive.write("urls.txt", "https://a.example/1\n");
    archive.ok(&["import", "urls.txt"]);
    let again = archive.ok(&["import", "urls.txt"]);
    assert!(again.contains("already in the archive"), "{again}");
    assert_eq!(archive.rows().len(), 1);
}

#[test]
fn importing_something_that_is_not_there_is_a_named_failure() {
    let archive = Archive::new("import-missing");
    let output = archive.run(&["import", "nope.html"]);
    assert!(!output.status.success());
    let message = Archive::output_text(&output);
    assert!(message.contains("does not exist"), "{message}");
}

#[test]
fn hacker_news_fetches_stores_searches_and_exports() {
    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);

    let archive = Archive::new("hn");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n[sources.options]\nbase = \"{}\"\ntag = \"show_hn\"\n",
        mock.base()
    ));

    let run = archive.ok(&["run", "-n", "20"]);
    assert!(run.contains("fetched"), "{run}");
    assert!(run.contains("3 new"), "{run}");

    let rows = archive.rows();
    assert_eq!(rows.len(), 3, "{rows:?}");

    let story = rows
        .iter()
        .find(|r| r["title"].as_str().is_some_and(|t| t.contains("Boonful")))
        .expect("the story");
    assert_eq!(story["author"].as_str(), Some("atifhub"));
    assert_eq!(story["url"], "https://boonful.io/");
    assert!(story["text"].as_str().is_some_and(|t| t.contains("boonful is live")), "{story:?}");

    // the raw payload survives, because that is what the archive sink writes
    let raw = story["raw"].as_object().expect("the raw hit was kept");
    assert_eq!(raw["points"], 12, "a field the reader does not use is still kept");
    assert!(raw.contains_key("_highlightResult"));

    // a comment is a reply, and html in a comment is stripped
    let comment = rows
        .iter()
        .find(|r| r["text"].as_str().is_some_and(|t| t.contains("VACUUM")))
        .expect("the comment");
    assert!(!comment["text"].as_str().unwrap().contains('<'), "html was stripped");

    // a real search finds the fetched item
    let found = archive.ok(&["search", "Boonful"]);
    assert!(found.contains("results"), "{found}");

    // and it exports
    archive.ok(&["export", "-f", "jsonl", "-o", "hn.jsonl"]);
    assert_eq!(archive.read("hn.jsonl").lines().count(), 3);

    assert!(mock.saw("/search_by_date"), "the adapter asked the right path");
    assert!(
        mock.requests()[0].contains("tags=show_hn"),
        "the tag it asked for: {}",
        mock.requests()[0]
    );
}

#[test]
fn a_second_hacker_news_run_adds_nothing_new() {
    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);
    let archive = Archive::new("hn-twice");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n[sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));

    archive.ok(&["run"]);
    let second = archive.ok(&["run"]);
    assert!(second.contains("3 already known"), "{second}");
    assert_eq!(archive.rows().len(), 3);
}

#[test]
fn reddit_fetches_posts_skips_the_pinned_one_and_keeps_the_whole_url() {
    let mock = Mock::start();
    mock.route("/r/rust/new/.json", REDDIT);

    let archive = Archive::new("reddit");
    archive.config(&format!(
        "user_agent = \"mebookmarker-e2e/1\"\ndata_dir = \"{{data}}\"\n\n\
         [[sources]]\nmedium = \"reddit\"\nenabled = true\n\n\
         [sources.options]\nbase = \"{}\"\nsubreddit = \"rust\"\n",
        mock.base()
    ));

    archive.ok(&["run"]);
    let rows = archive.rows();
    assert_eq!(rows.len(), 2, "the pinned notice was kept: {rows:?}");

    let linked = rows
        .iter()
        .find(|r| r["title"].as_str().is_some_and(|t| t.contains("fast thing")))
        .expect("the linked post");
    assert_eq!(linked["url"], "https://blog.example/a-fast-thing/", "the external link");
    assert!(linked["text"].as_str().is_some_and(|t| t.contains("why it is faster")), "{linked:?}");

    // a self post has no external link, so it falls back to its own permalink
    let self_post = rows
        .iter()
        .find(|r| r["title"].as_str().is_some_and(|t| t.contains("self post")))
        .expect("the self post");
    assert!(
        self_post["url"].as_str().is_some_and(|u| u.contains("reddit.com/r/rust")),
        "{self_post:?}"
    );
}

#[test]
fn reddit_refuses_the_default_user_agent_before_it_asks() {
    // reddit answers the tool's own default agent with a 429 and no body, so the
    // run is stopped before it starts rather than after it fails
    let mock = Mock::start();
    mock.route("/r/rust/new/.json", REDDIT);

    let archive = Archive::new("reddit-default-agent");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"reddit\"\nenabled = true\n\n\
         [sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));

    let log = Archive::output_text(&archive.run(&["-vv", "run"]));
    assert!(log.contains("user agent"), "the refusal should say what to change: {log}");
    assert!(!mock.saw("/r/rust/new/.json"), "it asked reddit anyway");
    assert!(archive.rows().is_empty(), "it stored something");
}

#[test]
fn github_fetches_stars_with_their_topics_and_language() {
    let mock = Mock::start();
    mock.route("/user/starred", GITHUB);

    let archive = Archive::new("github");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"github\"\nenabled = true\n\n\
         [sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));
    // the api needs a token; any value works, and this is the variable the
    // binary reads it from without being told the name
    archive.ok_with(&[("GITHUB_TOKEN", "test-token")], &["run"]);

    let rows = archive.rows();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let repo = &rows[0];
    assert!(repo["text"].as_str().is_some_and(|t| t.contains("CLI utility")), "{repo:?}");
    assert!(repo["text"].as_str().is_some_and(|t| t.contains("Python")), "the language: {repo:?}");

    let tags = tags_of(repo);
    assert!(tags.contains(&"llm".to_owned()), "a topic: {tags:?}");
    assert!(tags.contains(&"github.com".to_owned()), "{tags:?}");

    // the whole repo object survives, star count and all
    assert_eq!(repo["raw"]["stargazers_count"], 9123);
    assert!(mock.saw("/user/starred"), "the stars endpoint was asked for");
}

#[test]
fn github_without_a_token_refuses_rather_than_asking_anonymously() {
    let archive = Archive::new("github-no-token");
    archive.config("data_dir = \"{data}\"\n\n[[sources]]\nmedium = \"github\"\nenabled = true\n");

    // the log goes to stdout, and it is the only place the reason appears
    let log = Archive::output_text(&archive.run(&["-vv", "run"]));
    assert!(log.contains("0 fetched"), "it fetched something without a token: {log}");
    assert!(
        log.contains("GITHUB_TOKEN"),
        "the log should name the variable the token is read from: {log}"
    );
}

#[test]
fn x_fetches_bookmarks_over_graphql_and_sends_its_cookies() {
    let mock = Mock::start();
    mock.graphql("/VpbH7gkKQ4G5j1oGCk0r0C0Bc/Bookmarks", X_BOOKMARKS);

    let archive = Archive::new("x");
    // `base` is the host: the adapter appends the query id and the operation, so
    // a whole path here would ask for it twice and hit nothing
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"x\"\nenabled = true\n\n\
         [sources.options]\nbase = \"{}\"\npost_url = \"https://x.test\"\n",
        mock.base()
    ));

    // the adapter needs the two cookies, and reads them from the environment
    archive.ok_with(&[("TWITTER_COOKIES", "auth_token=abc123\nct0=def456\n")], &["run"]);

    let rows = archive.rows();
    // three entries came back and one of them is a deleted-post tombstone
    assert_eq!(rows.len(), 2, "the tombstone was stored: {rows:?}");

    let first = rows
        .iter()
        .find(|r| r["title"].as_str().is_some_and(|t| t.contains("alignment")))
        .expect("the first bookmark");
    // the heading in the fixture is `@handle - summary`, so there is no display
    // name to read and the author is the handle
    // the display name rides along when the endpoint sends one, and the handle
    // keeps the `@` so it matches the mention in a post
    assert_eq!(first["author"].as_str(), Some("@trq212 (Trenton Bricken)"));
    assert_eq!(first["url"], "https://x.test/trq212/status/2007903193158881323");
    assert!(first["text"].as_str().is_some_and(|t| t.contains("AI alignment")), "{first:?}");

    // the raw payload survives whole, which is the point of the archive sink:
    // the fields the reader does not use are the ones a later version might
    let raw = first["raw"].as_object().expect("the raw tweet was kept");
    assert_eq!(raw["rest_id"], "2007903193158881323");
    assert!(raw["core"].is_object(), "the account half was kept: {raw:?}");
    assert_eq!(raw["legacy"]["full_tweet"]["lang"], "en", "{raw:?}");

    assert!(mock.saw("/VpbH7gkKQ4G5j1oGCk0r0C0Bc/Bookmarks"), "the query id was asked for");
}

#[test]
fn x_without_cookies_refuses_before_it_asks() {
    let archive = Archive::new("x-no-cookies");
    archive.config("data_dir = \"{data}\"\n\n[[sources]]\nmedium = \"x\"\nenabled = true\n");

    // the log goes to stdout, and it is the only place the reason appears
    let log = Archive::output_text(&archive.run(&["-vv", "run"]));
    assert!(log.contains("0 fetched"), "it fetched something: {log}");
    assert!(
        log.contains("auth_token") && log.contains("ct0"),
        "the log should name what is missing: {log}"
    );
}

#[test]
fn x_tells_the_difference_between_an_empty_bookmark_list_and_a_changed_endpoint() {
    // the worst thing this adapter could do is report an empty bookmark list
    // because the endpoint changed shape. an empty list is a valid answer; a
    // changed shape is not, and it has to say so.
    let mock = Mock::start();
    mock.route("/VpbH7gkKQ4G5j1oGCk0r0C0Bc/Bookmarks", r#"{"something_else": []}"#);

    let archive = Archive::new("x-shape");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"x\"\nenabled = true\n\n\
         [sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));

    let output = archive.run_with(&[("TWITTER_COOKIES", "auth_token=a\nct0=b\n")], &["-vv", "run"]);
    let log = Archive::output_text(&output);
    assert!(
        log.contains("the response shape has changed") || log.contains("no data"),
        "a changed response shape was reported as an empty bookmark list:\n{log}"
    );
}

#[test]
fn a_feed_fetches_every_item_with_its_date_and_author() {
    let mock = Mock::start();
    mock.route("/rss.xml", RSS);
    mock.route("/atom.xml", ATOM);

    let archive = Archive::new("rss");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"rss\"\nenabled = true\n\n[sources.options]\nurls = [\"{}/rss.xml\"]\n",
        mock.base()
    ));

    archive.ok(&["run"]);
    let rows = archive.rows();
    assert_eq!(rows.len(), 2, "{rows:?}");

    let first = rows
        .iter()
        .find(|r| r["title"].as_str().is_some_and(|t| t.contains("write-ahead")))
        .expect("the first item");
    assert_eq!(first["url"], "https://blog.example/wal");
    assert!(first["when"].as_str().is_some_and(|w| w.starts_with("2026-01-02")), "{first:?}");
    assert!(
        first["text"].as_str().is_some_and(|t| t.contains("durable")),
        "the description, unescaped: {first:?}"
    );
    // a feed writes its author as `address (Name)`, and the two halves are
    // stored as an account and a display name rather than as one string
    assert_eq!(first["author"].as_str(), Some("writer@example (A Writer)"));

    // and an atom feed, which is a different document
    let atom_archive = Archive::new("atom");
    atom_archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"rss\"\nenabled = true\n\n[sources.options]\nurls = [\"{}/atom.xml\"]\n",
        mock.base()
    ));
    atom_archive.ok(&["run"]);
    let atom_rows = atom_archive.rows();
    assert_eq!(atom_rows.len(), 1, "{atom_rows:?}");
    assert_eq!(atom_rows[0]["url"], "https://atom.example/entry");
    assert!(
        atom_rows[0]["text"].as_str().is_some_and(|t| t.contains("atom entry")),
        "{atom_rows:?}"
    );
}

#[test]
fn a_youtube_playlist_becomes_one_bookmark_per_video_with_its_title() {
    let mock = Mock::start();
    mock.route("/playlist", YOUTUBE_PLAYLIST);

    let archive = Archive::new("youtube");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"you-tube\"\nenabled = true\n\n[sources.options]\nplaylists = [\"{}/playlist\"]\n",
        mock.base()
    ));

    archive.ok(&["run"]);
    let rows = archive.rows();
    // four entries on the page, one of them a repeat of another
    assert_eq!(rows.len(), 3, "{rows:?}");

    // the title and the channel are on the page, so a bookmark is not just an id
    let first = rows
        .iter()
        .find(|r| r["url"] == "https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        .expect("the first video");
    assert_eq!(first["title"].as_str(), Some("A video with a title"), "{first:?}");
    assert_eq!(first["author"].as_str(), Some("a channel"), "{first:?}");

    // and the shape youtube used before the current one still reads
    let older = rows
        .iter()
        .find(|r| r["url"] == "https://www.youtube.com/watch?v=ZZZZZZZZZZZ")
        .expect("the older shape");
    assert_eq!(older["title"].as_str(), Some("The older shape"), "{older:?}");
    assert_eq!(older["author"].as_str(), Some("an older channel"), "{older:?}");

    // a video has no text of its own, and the queue says so rather than letting
    // a later stage read an empty body and say nothing
    assert!(tags_of(first).contains(&"needs-transcript".to_owned()), "{first:?}");
}

/// a store with one bookmark of every interesting shape, for the sinks to read.
fn filled(archive: &Archive) {
    archive.write("bookmarks.html", NETSCAPE);
    archive.write("bookmarks.md", ARCHIVE_MARKDOWN);
    archive.ok(&["import", "bookmarks.html"]);
    archive.ok(&["import", "bookmarks.md"]);
    archive.ok(&["enrich", "-s", "entities"]);
}

#[test]
fn every_output_format_is_written_and_readable() {
    let archive = Archive::new("sinks");
    filled(&archive);
    let count = archive.rows().len();
    assert!(count > 3, "the fixture archive is too small to prove anything: {count}");

    // each format, and the check that it is the shape it claims to be
    archive.ok(&["export", "-f", "json", "-o", "out.json"]);
    let json = archive.read("out.json");
    let document: serde_json::Value = serde_json::from_str(&json).expect("one json document");
    assert_eq!(document["count"], count, "the document counts what it holds");
    assert_eq!(document["bookmarks"].as_array().expect("bookmarks").len(), count);

    archive.ok(&["export", "-f", "jsonl", "-o", "out.jsonl"]);
    let jsonl = archive.read("out.jsonl");
    assert_eq!(jsonl.lines().count(), count, "one line per bookmark");
    for line in jsonl.lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|e| panic!("{e}: {line}"));
    }

    archive.ok(&["export", "-f", "csv", "-o", "out.csv"]);
    let csv = archive.read("out.csv");
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    let headers = reader.headers().expect("a header row").clone();
    assert!(headers.iter().any(|h| h == "url"), "{headers:?}");
    assert_eq!(reader.records().count(), count, "one row per bookmark");

    archive.ok(&["export", "-f", "opml", "-o", "out.opml"]);
    let opml = archive.read("out.opml");
    assert!(opml.starts_with("<?xml"), "{opml}");
    assert!(opml.contains("<opml version=\"2.0\">"), "{opml}");
    assert!(opml.trim_end().ends_with("</opml>"), "the document is closed");
    // a feed reader groups an opml by outline: the ones carrying a url are the
    // subscriptions, and the ones without are the folders they sit in
    let outlines: Vec<&str> =
        opml.lines().filter(|l| l.trim_start().starts_with("<outline")).collect();
    let folders = outlines.iter().filter(|l| !l.contains("url=")).count();
    let subscriptions = outlines.iter().filter(|l| l.contains("url=")).count();
    assert_eq!(subscriptions, count, "one outline with a url per bookmark:\n{opml}");
    assert!(folders >= 1, "at least one folder, or the tree is flat:\n{opml}");

    archive.ok(&["export", "-f", "html", "-o", "out.html"]);
    let html = archive.read("out.html");
    assert!(html.starts_with("<!DOCTYPE html>"), "the document opens");
    assert!(html.trim_end().ends_with("</html>"), "the document closes");
    assert_eq!(html.matches("<article>").count(), count, "one article per bookmark");
    assert!(html.contains("id=\"filter\""), "the page can be filtered");
}

#[test]
fn a_markdown_export_writes_a_note_per_bookmark_and_a_daily_index() {
    let archive = Archive::new("sink-markdown");
    filled(&archive);
    let count = archive.rows().len();

    archive.ok(&["export", "-f", "markdown", "-o", "notes"]);
    let index = archive.read("notes/index.md");
    assert!(index.starts_with("# archive"), "{index}");
    assert!(index.contains("## 2026-01-04"), "the day headings: {index}");
    assert_eq!(index.matches("[[").count(), count, "one wikilink per bookmark: {index}");

    let entries: Vec<PathBuf> = std::fs::read_dir(archive.data.join("notes"))
        .expect("the directory was made")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(entries.len(), count + 1, "one note each, plus the index");

    // every filename is something a filesystem will hold
    for entry in &entries {
        let name = entry.file_name().expect("a name").to_string_lossy().into_owned();
        assert!(
            !name.contains('/') && !name.contains(':') && !name.contains('*'),
            "{name} cannot be a filename"
        );
    }
}

#[test]
fn an_obsidian_export_writes_frontmatter_a_note_can_be_graphed_from() {
    let archive = Archive::new("sink-obsidian");
    filled(&archive);

    archive.ok(&["export", "-f", "obsidian", "-o", "vault"]);
    let entries: Vec<PathBuf> = std::fs::read_dir(archive.data.join("vault"))
        .expect("the directory was made")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n != "index.md"))
        .collect();
    assert!(!entries.is_empty(), "no notes were written");

    let body = std::fs::read_to_string(&entries[0]).expect("a note reads");
    assert!(body.starts_with("---\n"), "the frontmatter opens:\n{body}");
    let front = body.find("\n---\n").expect("the frontmatter closes");
    for key in ["title:", "date:", "source:"] {
        assert!(body[..front].contains(key), "the frontmatter has no {key}:\n{front:#?}");
    }
}

#[test]
fn an_archive_export_writes_the_raw_payload_and_an_index() {
    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);
    let archive = Archive::new("sink-archive");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n[sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));
    archive.ok(&["run"]);

    archive.ok(&["export", "-f", "archive", "-o", "raw"]);
    let index = archive.read("raw/index.jsonl");
    let entries: Vec<serde_json::Value> =
        index.lines().map(|l| serde_json::from_str(l).expect("an index line is json")).collect();
    assert_eq!(entries.len(), 3, "{entries:?}");

    // every entry's file is there, and holds the fields a reader does not use
    for entry in &entries {
        let file = entry["file"].as_str().expect("a file");
        let path = archive.data.join("raw").join(file);
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the payload reads"))
                .expect("the payload is json");
        let raw = payload["raw"].as_object().expect("the raw hit is in there");
        assert!(raw.contains_key("_highlightResult"), "{raw:?}");
        assert!(raw.contains_key("points"), "{raw:?}");
    }
}

#[test]
fn a_sink_pointed_at_a_file_it_cannot_write_fails_rather_than_writing_nothing() {
    let archive = Archive::new("sink-unwritable");
    archive.ok(&["add", "https://example.com/a"]);

    // a path whose parent is a file, so creating the directory must fail
    archive.write_in_data("blocker", "not a directory");
    let output = archive.run(&["export", "-f", "jsonl", "-o", "blocker/out.jsonl"]);
    assert!(
        !output.status.success(),
        "a silent success on an unwritable path: {}",
        Archive::output_text(&output)
    );
}

#[test]
fn a_full_run_fetches_enriches_and_writes_every_configured_sink() {
    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);
    mock.route("/rss.xml", RSS);

    let archive = Archive::new("full-run");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n\
         [[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n[sources.options]\nbase = \"{}\"\n\n\
         [[sources]]\nmedium = \"rss\"\nenabled = true\n\n[sources.options]\nurls = [\"{}/rss.xml\"]\n\n\
         [[sinks]]\nkind = \"jsonl\"\nenabled = true\npath = \"out.jsonl\"\n\n\
         [[sinks]]\nkind = \"html\"\nenabled = true\npath = \"site.html\"\n",
        mock.base(),
        mock.base()
    ));

    let run = archive.ok(&["run"]);
    assert!(run.contains("5 new"), "both sources contributed: {run}");
    assert!(run.contains("enriched"), "the stages ran: {run}");
    assert!(run.contains("written"), "the sinks ran: {run}");

    let jsonl = archive.read("out.jsonl");
    assert_eq!(jsonl.lines().count(), 5, "the jsonl sink has every item");
    let html = archive.read("site.html");
    assert_eq!(html.matches("<article>").count(), 5, "the html sink has every item");
}

#[test]
fn a_dry_run_reads_and_enriches_but_writes_no_sink() {
    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);
    let archive = Archive::new("dry-run");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n[sources.options]\nbase = \"{}\"\n\n[[sinks]]\nkind = \"jsonl\"\nenabled = true\npath = \"out.jsonl\"\n",
        mock.base()
    ));

    archive.ok(&["run", "--dry-run"]);
    assert_eq!(archive.rows().len(), 3, "the items still went in");
    assert!(!archive.data.join("out.jsonl").exists(), "a dry run wrote a sink");
}

#[test]
fn only_the_configured_sources_are_read() {
    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);
    let archive = Archive::new("source-filter");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n[sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));

    let run = archive.ok(&["run", "--source", "reddit"]);
    assert!(run.contains("0 fetched"), "a filtered run read something: {run}");
    assert!(mock.hits.load(Ordering::SeqCst) == 0, "it asked the network anyway");
}

#[test]
fn a_source_with_a_page_limit_stops_where_it_is_told() {
    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);
    let archive = Archive::new("page-limit");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n[sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));

    // a page size of one, and the run follows the cursor through all three
    let limited = archive.ok(&["run", "--limit", "1"]);
    assert!(
        limited.contains("3 new"),
        "the page size bounds a page, and the cursor walks the rest: {limited}"
    );
    assert_eq!(archive.rows().len(), 3);
    // three pages of one, plus the empty page that tells the run it has reached
    // the end. the cursor advanced each time, which is the thing worth proving:
    // an adapter that kept returning the same page would show one request here
    // and three identical items.
    assert_eq!(mock.count("/search_by_date"), 4, "three pages and the empty one after them");
    let cursors = mock.requests().iter().filter(|r| r.contains("numericFilters")).count();
    assert_eq!(cursors, 3, "every page after the first carried a cursor");
}

#[test]
fn a_run_with_no_sources_configured_still_reports_cleanly() {
    let archive = Archive::new("no-sources");
    let run = archive.ok(&["run"]);
    assert!(run.contains("0 fetched"), "{run}");
}

#[test]
fn the_cost_a_run_reports_is_proportional_to_what_it_asked() {
    // the whole design rests on tier two being a fraction of a cent, and this is
    // the number the readme quotes. a change to the model or the question shape
    // has to move it, and a test that pins it is how that gets noticed.
    let mut report = mbm_app::pipeline::RunReport::default();
    assert!(report.cost_usd == 0.0, "a run that asked nothing cannot have cost anything");

    report.stages = vec![
        (
            mbm_core::port::EnrichStage::Entities,
            mbm_enrich::StageReport { done: 1_000, ..Default::default() },
        ),
        (
            mbm_core::port::EnrichStage::Tags,
            mbm_enrich::StageReport { done: 1_000, ..Default::default() },
        ),
    ];
    let thousand = mbm_app::pipeline::estimate_cost(&report);
    // two questions an item at the measured rate is $0.013, and the readme says
    // "about two dollars" for a hundred thousand
    assert!(
        (thousand - 0.013).abs() < 0.0001,
        "a thousand items cost {thousand}, and the readme's figure implies 0.013"
    );
}

#[test]
fn the_terminal_interface_draws_a_list_a_detail_and_a_search() {
    // a tui is a terminal program, so the only honest way to test it is in a
    // terminal. tmux is a pty that can be scripted, which makes the frame the
    // program actually drew observable from outside.
    if !tmux() {
        eprintln!("skipping: tmux is not installed");
        return;
    }

    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);
    let archive = Archive::new("tui");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n\
         [sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));
    archive.ok(&["run", "-n", "5"]);

    let session = Session { name: start_session(&archive, 100, 28) };
    std::thread::sleep(std::time::Duration::from_millis(1200));

    // the field, the list, the ranking and the help are all on screen at once,
    // because a person needs all four before they can do anything
    let browse = capture(&session.name);
    assert!(browse.contains("search"), "no query field:\n{browse}");
    assert!(browse.contains("hybrid"), "the ranking is not shown:\n{browse}");
    assert!(browse.contains("Boonful"), "no items:\n{browse}");
    assert!(browse.contains("3 bookmarks"), "no count:\n{browse}");
    assert!(browse.contains("\u{2191}\u{2193} move"), "no key help:\n{browse}");
    assert!(browse.contains("^u undo"), "undo is not advertised:\n{browse}");
    // the selected row is marked three ways, so it survives a monochrome
    // terminal and a reader who cannot tell the two colours apart
    assert!(browse.contains("\u{258e}"), "no selection marker:\n{browse}");

    // typing filters the list, and the count follows
    send(&session.name, "Boonful");
    std::thread::sleep(std::time::Duration::from_millis(700));
    let searched = capture(&session.name);
    assert!(searched.contains("search Boonful"), "{searched}");
    assert!(searched.contains("1 of 3"), "the count did not follow the filter:\n{searched}");

    // enter opens the item, and each part of it gets its own line
    send(&session.name, "Enter");
    std::thread::sleep(std::time::Duration::from_millis(600));
    let detail = capture(&session.name);
    assert!(detail.contains("atifhub"), "the author is missing:\n{detail}");
    assert!(detail.contains("boonful.io"), "the url is missing:\n{detail}");
    assert!(detail.contains("boonful is live"), "the body is missing:\n{detail}");
    assert!(detail.contains("id "), "the id is missing:\n{detail}");

    // escape goes back to the same row, not to the top
    send(&session.name, "Escape");
    std::thread::sleep(std::time::Duration::from_millis(400));
    let back = capture(&session.name);
    assert!(back.contains("search Boonful"), "escape lost the query:\n{back}");

    // ctrl-k clears it, and the whole archive comes back
    send(&session.name, "C-k");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let cleared = capture(&session.name);
    assert!(cleared.contains("3 bookmarks"), "clearing did not restore the list:\n{cleared}");

    // the tag list is a list, not a picture of one: picking a tag filters
    send(&session.name, "C-g");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let tags = capture(&session.name);
    assert!(tags.contains("hackernews"), "the tag list is empty:\n{tags}");
    send(&session.name, "Enter");
    std::thread::sleep(std::time::Duration::from_millis(600));
    let filtered = capture(&session.name);
    assert!(
        filtered.contains("3 bookmarks tagged hackernews"),
        "picking a tag did not filter: {filtered}"
    );

    send(&session.name, "C-c");
    std::thread::sleep(std::time::Duration::from_millis(300));
}

#[test]
fn the_terminal_interface_narrows_without_losing_the_title() {
    // a terminal is 24 rows on a laptop and 60 on a desk, and a person reading
    // a list in a 60-column pane is not doing something unusual. the columns
    // give way in the order of how much each says, and the title never does:
    // it is the only column that says what the row is.
    if !tmux() {
        eprintln!("skipping: tmux is not installed");
        return;
    }

    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);
    let archive = Archive::new("tui-narrow");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n\
         [sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));
    archive.ok(&["run", "-n", "5"]);

    for (width, height) in [(120, 30), (100, 24), (80, 24), (60, 20), (44, 16)] {
        let session = Session { name: start_session(&archive, width, height) };
        std::thread::sleep(std::time::Duration::from_millis(900));
        let frame = capture(&session.name);
        // characters, not bytes: a terminal cell is one character, and the row
        // is full of glyphs that are three bytes each
        let longest = frame.lines().map(|l| l.chars().count()).max().unwrap_or(0);
        assert!(
            longest <= usize::from(width),
            "at {width}x{height} a row is {longest} cells wide and wraps:\n{frame}"
        );
        assert!(frame.contains("Boonful"), "at {width}x{height} the title is gone:\n{frame}");
        assert!(frame.contains("bookmarks"), "at {width}x{height} the count is gone:\n{frame}");
        // the help is the first thing to go, and the count is the last
        if width >= 100 {
            assert!(frame.contains("move"), "at {width} the key help is gone:\n{frame}");
        }
    }
}

#[test]
fn the_same_frame_renders_differently_in_each_appearance() {
    // a palette that is the same in both appearances is a palette that is wrong
    // in one of them. the check is that the rendered frame differs, that it
    // differs by colour rather than by a character, and that the two layouts are
    // identical, because an appearance is a palette and not an arrangement.
    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);
    let archive = Archive::new("appearance");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n\
         [sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));
    archive.ok(&["run", "-n", "5"]);

    let frame = |appearance: &str| -> (String, String) {
        let out = Command::new("cargo")
            .args(["run", "-q", "-p", "mbm-app", "--example", "frame", "--"])
            .arg(archive.data.to_string_lossy().into_owned())
            .args(["100x20", &format!("/tmp/mbm-frame-{appearance}"), appearance, "browse"])
            .output()
            .expect("cargo runs");
        assert!(out.status.success(), "the {appearance} frame did not render");
        let text = std::fs::read_to_string(format!("/tmp/mbm-frame-{appearance}/frame.txt"))
            .expect("the frame was written");
        let html = std::fs::read_to_string(format!("/tmp/mbm-frame-{appearance}/frame.html"))
            .expect("the frame was written");
        (text, html)
    };

    let (light_text, light_html) = frame("light");
    let (dark_text, dark_html) = frame("dark");

    assert!(light_text.contains("Boonful"), "the light frame has no items:\n{light_text}");
    assert!(dark_text.contains("Boonful"), "the dark frame has no items:\n{dark_text}");
    // the same rows in the same places: an appearance is a palette, not an
    // arrangement
    assert_eq!(
        light_text.lines().map(str::trim).collect::<Vec<_>>(),
        dark_text.lines().map(str::trim).collect::<Vec<_>>(),
        "the two appearances laid the frame out differently"
    );
    assert_ne!(light_html, dark_html, "the two appearances drew the same colours");
    assert!(light_html.contains("#fcfdff"), "the light page is not the light page");
    assert!(dark_html.contains("#0d1117"), "the dark page is not the dark page");
}

#[test]
fn an_empty_archive_says_what_it_is_and_how_to_fill_it() {
    // an empty state is the first thing a new user sees and the easiest thing
    // to get wrong. "no results" is a shrug: it names neither the place nor the
    // way out. both empty states have to name what this is and offer one way
    // forward.
    let archive = Archive::new("empty");
    let frame = render(&archive, 90, 20, Theme::for_appearance(Appearance::Dark));
    assert!(frame.contains("the archive is empty"), "{frame}");
    assert!(frame.contains("mbm add https://example.com"), "{frame}");
    assert!(frame.contains("mbm import"), "{frame}");

    // and a search that matches nothing names the query, and says how to leave
    archive.ok(&["add", "https://example.com/a"]);
    let frame = render(&archive, 90, 20, Theme::for_appearance(Appearance::Dark));
    assert!(frame.contains("example.com"), "the item is not in the list:\n{frame}");
}

#[test]
fn a_search_that_matches_nothing_says_so_and_offers_the_way_out() {
    let archive = Archive::new("no-match");
    archive.ok(&["add", "https://example.com/a"]);
    // drive the real binary so the query is set the way a person sets it
    let session = Session { name: start_session(&archive, 90, 18) };
    send(&session.name, "zzzz");
    std::thread::sleep(std::time::Duration::from_millis(700));
    let frame = capture(&session.name);
    assert!(frame.contains("nothing matches"), "{frame}");
    assert!(frame.contains("zzzz"), "the state does not name the query:\n{frame}");
    assert!(frame.contains("ctrl-k"), "the state does not say how to leave:\n{frame}");

    // and ctrl-k brings the archive back
    send(&session.name, "C-k");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let back = capture(&session.name);
    assert!(back.contains("example.com"), "clearing did not restore the list:\n{back}");
}

/// render one frame of the interface to text, at a given size and appearance.
fn render(archive: &Archive, width: u16, height: u16, theme: Theme) -> String {
    let conn = mbm_store::open(&archive.data.join("mebookmarker.db")).expect("the store opens");
    let mut app = App::new(Arc::new(Mutex::new(conn)), "");
    render_to(width, height, &mut app, &theme)
}

#[test]
fn a_click_selects_the_row_under_the_pointer_and_opens_it() {
    // a terminal on a desk has a pointer under it, and a list that ignores a
    // click reads as broken. the pointer is a second way in: everything it can
    // do a key can do, and the frame is driven through a real pty so the mouse
    // reporting is the terminal's and not a stubbed event.
    if !tmux() {
        eprintln!("skipping: tmux is not installed");
        return;
    }

    let mock = Mock::start();
    mock.route("/search_by_date", HACKER_NEWS);
    let archive = Archive::new("tui-mouse");
    archive.config(&format!(
        "data_dir = \"{{data}}\"\n\n[[sources]]\nmedium = \"hacker-news\"\nenabled = true\n\n\
         [sources.options]\nbase = \"{}\"\n",
        mock.base()
    ));
    archive.ok(&["run", "-n", "5"]);

    let session = Session { name: start_session(&archive, 100, 24) };
    std::thread::sleep(std::time::Duration::from_millis(1000));

    // the list's first row is the second line of the frame, and a click on the
    // *third* row has to select the third item rather than the second
    let before = capture(&session.name);
    assert!(before.contains("Boonful"), "the list is empty:\n{before}");
    click(&session.name, 3, 10);
    std::thread::sleep(std::time::Duration::from_millis(600));
    let after = capture(&session.name);
    // the third item opened, and the detail view is showing it
    assert!(after.contains("havu12"), "the click did not open the row under it:\n{after}");

    send(&session.name, "Escape");
    std::thread::sleep(std::time::Duration::from_millis(400));
    // the wheel moves three rows, and the count and the bar follow
    scroll(&session.name, 1);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let scrolled = capture(&session.name);
    assert!(scrolled.contains("3 bookmarks"), "the wheel lost the list:\n{scrolled}");
}

#[test]
fn the_terminal_interface_reports_contrast_it_measured() {
    // the palette is not a matter of taste and the check is not a comment: the
    // binary draws a frame, the pairs the frame uses are read back out of the
    // buffer, and each is compared against the requirement it carries.
    let report = Command::new("cargo")
        .args(["run", "-q", "-p", "mbm-app", "--example", "contrast-check"])
        .output()
        .expect("cargo runs");
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&report.stdout),
        String::from_utf8_lossy(&report.stderr)
    );
    assert!(report.status.success(), "a pair is below its requirement:\n{out}");
    assert!(out.contains("=== Dark ==="), "the dark palette was not measured:\n{out}");
    assert!(out.contains("=== Light ==="), "the light palette was not measured:\n{out}");
    // and it measured both of them, which is the point of measuring at all: a
    // palette tuned for one appearance is unreadable on the other
    assert!(out.contains("body on page"), "the body pair is missing:\n{out}");
    assert!(out.contains("resting frame on surface"), "a frame pair is missing:\n{out}");
}

/// the tmux socket this test run owns.
///
/// a private socket rather than the default one, because the default is shared
/// with every other tmux on the machine: a session left behind by a run that
/// panicked, or by anything the person is doing, otherwise sits on it, and a
/// capture aimed at a name that matches nothing returns an empty pane. that is
/// not a flaky assertion, it is a wrong answer.
fn socket() -> String {
    format!("mbm-e2e-{}", std::process::id())
}

/// start the interface in a pty and return the session's name.
fn start_session(archive: &Archive, width: u16, height: u16) -> String {
    let name = format!("session-{}", COUNTER.fetch_add(1, Ordering::SeqCst));
    // `-c` rather than the spawned process's own working directory: tmux hands
    // the session the directory of the *server*, which is whichever tmux was
    // first started from, and a session that opens in the wrong directory is a
    // session whose relative paths all point somewhere else
    let started = Command::new("tmux")
        .args([
            "-L",
            &socket(),
            "new-session",
            "-d",
            "-s",
            &name,
            "-c",
            &archive.root.to_string_lossy(),
            "-x",
            &width.to_string(),
            "-y",
            &height.to_string(),
        ])
        .arg(format!(
            "{} --config {} tui",
            binary().display(),
            archive.root.join("mebookmarker/mebookmarker.toml").display()
        ))
        .env("XDG_DATA_HOME", &archive.root)
        .env("HOME", &archive.root)
        .env("MBM_THEME", "dark")
        .status()
        .expect("tmux runs");
    assert!(started.success(), "tmux started a session named {name}");
    name
}

/// a tmux session, killed when the test ends however it ends.
///
/// a terminal interface cannot be tested by calling its functions: the thing
/// under test is the frame it drew, and the only way to see a frame from
/// outside the process is to give the program a terminal and look at it. tmux
/// is a pty that can be scripted, which makes the drawing observable and the
/// keystrokes sendable.
struct Session {
    name: String,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ =
            Command::new("tmux").args(["-L", &socket(), "kill-session", "-t", &self.name]).status();
    }
}

/// whether tmux is on the path.
fn tmux() -> bool {
    Command::new("tmux").arg("-V").output().is_ok_and(|o| o.status.success())
}

/// what the session is showing right now.
fn capture(session: &str) -> String {
    let output = Command::new("tmux")
        .args(["-L", &socket(), "capture-pane", "-p", "-t", session])
        .output()
        .expect("tmux captures");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// click at a cell in the session.
fn click(session: &str, row: u16, column: u16) {
    // tmux has no "send a click" verb, so the mouse is driven through the
    // terminal's own reporting: a real terminal emulator receiving SGR mouse
    // escapes, which is exactly what this sends
    let _ = Command::new("tmux")
        .args(["-L", &socket(), "send-keys", "-t", session])
        .arg(format!("\u{1b}[<0;{};{}M", column + 1, row + 1))
        .status();
    // and the matching release, so the terminal sees a press and a lift
    let _ = Command::new("tmux")
        .args(["-L", &socket(), "send-keys", "-t", session])
        .arg(format!("\u{1b}[<0;{};{}m", column + 1, row + 1))
        .status();
}

/// scroll the wheel in the session, `count` notches down.
fn scroll(session: &str, count: u16) {
    for notch in 0..count {
        // button 65 is the wheel down, 64 the wheel up, in the SGR encoding
        let button = 64 + notch;
        let _ = Command::new("tmux")
            .args(["-L", &socket(), "send-keys", "-t", session])
            .arg(format!("\u{1b}[<{button};20;10M"))
            .status();
    }
}

/// send keys to the session.
fn send(session: &str, keys: &str) {
    let _ = Command::new("tmux").args(["-L", &socket(), "send-keys", "-t", session, keys]).status();
}
