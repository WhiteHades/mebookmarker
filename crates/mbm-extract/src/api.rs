//! the api-backed extractors: github, oembed, and a headless browser.
//!
//! three different ways to get at a page's content, in increasing order of
//! cost and decreasing order of coverage.
//!
//! - **api**: structured, complete, and rate limited. used where it exists.
//! - **oembed**: one small request, gives a title and an author for media.
//! - **browser**: a real rendering engine, for the pages that are a single
//!   `<div id="root">` until javascript runs. this is the fix for the
//!   long-standing problem with X long-form articles, which serve no text at
//!   all to a plain http client.

use crate::http::{Http, Request};
use mbm_core::Result as CoreResult;
use mbm_core::bookmark::BlockedReason;
use mbm_core::error::{Error, Result};
use serde::Deserialize;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

/// what a github repository looks like.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Repo {
    /// `owner/name`.
    pub full_name: String,
    /// the repository description.
    pub description: Option<String>,
    /// star count.
    pub stars: u64,
    /// primary language.
    pub language: Option<String>,
    /// repository topics.
    pub topics: Vec<String>,
    /// the readme, truncated.
    pub readme: Option<String>,
    /// a one-line summary for indexing.
    pub summary: Option<String>,
}

/// split a github url into its owner and name.
#[must_use]
pub fn parse_repo(url: &str) -> Option<(String, String)> {
    let rest = url.split("github.com").nth(1)?;
    let rest = rest.split(['?', '#']).next()?;
    let mut parts = rest.split('/').filter(|p| !p.is_empty());
    let owner = parts.next()?;
    let name = parts.next()?;
    // a third segment means a path inside the repository, such as `/issues/1`.
    // a dotted name is fine, since a repository really can be called `llm.js`.
    if parts.next().is_some() {
        return None;
    }
    Some((owner.to_owned(), name.trim_end_matches(".git").to_owned()))
}

#[derive(Deserialize)]
struct ApiRepo {
    full_name: String,
    description: Option<String>,
    stargazers_count: u64,
    language: Option<String>,
    #[serde(default)]
    topics: Vec<String>,
    default_branch: String,
}

/// how much of a readme to keep. a long one costs prompt budget for no gain,
/// and the first few sections carry the project's purpose.
const README_BUDGET: usize = 5_000;

/// fetch a repository through the github api.
///
/// an unauthenticated request gets sixty requests an hour, which is enough for
/// a backfill and not enough for a large one. `token` raises that to five
/// thousand.
pub async fn github(http: &Http, owner: &str, name: &str, token: Option<&str>) -> Result<Repo> {
    let mut request = Request::get(format!("https://api.github.com/repos/{owner}/{name}"))
        .header("accept", "application/vnd.github+json");

    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }

    let response = http.send(&request).await?;
    if !response.is_success() {
        return Err(Error::NotFound(format!("github has no repository {owner}/{name}")));
    }
    let api: ApiRepo = response.decode()?;

    let readme = github_readme(http, owner, name, &api.default_branch, token).await;

    let stars = api.stargazers_count;
    let summary = api.description.clone().map(|d| {
        if stars == 0 {
            return d;
        }
        let mut s = d;
        let _ = write!(s, " ({stars} stars)");
        s
    });

    Ok(Repo {
        full_name: api.full_name,
        description: api.description,
        stars,
        language: api.language,
        topics: api.topics,
        readme,
        summary,
    })
}

async fn github_readme(
    http: &Http,
    owner: &str,
    name: &str,
    branch: &str,
    token: Option<&str>,
) -> Option<String> {
    let mut request = Request::get(format!(
        "https://raw.githubusercontent.com/{owner}/{name}/{branch}/README.md"
    ))
    .timeout(Duration::from_secs(15));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }

    let response = http.send_once(&request).await.ok()?;
    if !response.is_success() {
        // the readme is often lowercased or absent; try the other spelling
        let alt = http
            .send_once(
                &Request::get(format!(
                    "https://raw.githubusercontent.com/{owner}/{name}/{branch}/readme.md"
                ))
                .timeout(Duration::from_secs(15)),
            )
            .await
            .ok()?;
        if !alt.is_success() {
            return None;
        }
        return Some(truncate(&alt.text()));
    }
    Some(truncate(&response.text()))
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= README_BUDGET {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(README_BUDGET).collect();
    out.push_str("\n\n[truncated]");
    out
}

/// what an oembed provider said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Embed {
    /// the resource title.
    pub title: Option<String>,
    /// the author.
    pub author: Option<String>,
    /// the canonical url.
    pub url: Option<String>,
    /// a preview image.
    pub thumbnail: Option<String>,
}

/// ask an oembed endpoint about a url.
///
/// youtube, vimeo, soundcloud, flickr and the rest publish an oembed endpoint
/// that returns a title and an author for one small request. that is far
/// cheaper than fetching the page, and for a video it is the only thing worth
/// having without a transcript.
pub async fn oembed(http: &Http, url: &str) -> CoreResult<Embed> {
    /// the shape an oembed provider returns
    #[derive(Deserialize)]
    struct Body {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        author_name: Option<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        thumbnail_url: Option<String>,
    }

    let endpoint = oembed_endpoint(url)
        .ok_or_else(|| Error::NotFound(format!("no oembed provider for {url}")))?;
    let target = format!("{endpoint}?format=json&url={}", percent_encode(url));

    let response = http.send_once(&Request::get(target).timeout(Duration::from_secs(8))).await?;
    if !response.is_success() {
        return Err(Error::NotFound(format!("oembed declined {url}")));
    }

    let body: Body = response.decode()?;
    Ok(Embed {
        title: body.title,
        author: body.author_name,
        url: body.url,
        thumbnail: body.thumbnail_url,
    })
}

/// the oembed endpoint for a url's host, if there is one.
#[must_use]
pub fn oembed_endpoint(url: &str) -> Option<&'static str> {
    let host = url.split("//").nth(1)?.split('/').next()?.trim_start_matches("www.");
    Some(match host {
        "youtube.com" | "youtu.be" => "https://www.youtube.com/oembed",
        "vimeo.com" => "https://vimeo.com/api/oembed.json",
        "soundcloud.com" => "https://soundcloud.com/oembed",
        "flickr.com" => "https://www.flickr.com/services/oembed/",
        "ted.com" => "https://www.ted.com/services/v1/oembed.json",
        "spotify.com" => "https://open.spotify.com/oembed",
        "codepen.io" => "https://codepen.io/api/oembed",
        "substack.com" => "https://substack.com/api/v1/embed",
        "medium.com" => "https://medium.com/services/oembed",
        "reddit.com" => "https://www.reddit.com/oembed",
        _ => return None,
    })
}

fn percent_encode(raw: &str) -> String {
    percent_encoding::utf8_percent_encode(raw, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// render a page with a real browser engine and read the result.
///
/// x long-form articles are the motivating case. they serve an empty document
/// to an http client and populate it after javascript runs, which is why the
/// previous generation of this tool could only ever capture their title.
///
/// `agent-browser` does the work. it is a separate binary rather than a
/// library, so this shells out; the rendered html comes back on stdout, which
/// keeps the interface to one pipe.
#[derive(Debug, Clone)]
pub struct Browser {
    binary: PathBuf,
    timeout: Duration,
}

impl Browser {
    /// point at an `agent-browser` binary.
    #[must_use]
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self { binary: binary.into(), timeout: Duration::from_secs(45) }
    }

    /// use the binary on the path.
    #[must_use]
    pub fn from_path() -> Self {
        Self::new("agent-browser")
    }

    /// set how long a render may take.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// whether the browser is available at all.
    #[must_use]
    pub fn is_available(&self) -> bool {
        which(&self.binary.to_string_lossy())
    }

    /// render a page and return its html after scripts have run.
    pub async fn render(&self, url: &str) -> Result<String> {
        if !self.is_available() {
            return Err(Error::Agent("agent-browser is not installed".to_owned()));
        }

        let child = tokio::process::Command::new(&self.binary)
            .arg("get")
            .arg("--format")
            .arg("html")
            .arg("--timeout")
            .arg(self.timeout.as_secs().to_string())
            .arg(url)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Agent(format!("cannot run agent-browser: {e}")))?;

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| Error::Timeout(self.timeout))?
            .map_err(|e| Error::Agent(format!("agent-browser failed: {e}")))?;

        if !output.status.success() {
            return Err(Error::Agent(format!(
                "agent-browser exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// render a page and read the article out of it.
    pub async fn read(&self, url: &str) -> Result<crate::Article> {
        let html = self.render(url).await?;
        Ok(crate::readability::read(&html, url))
    }
}

/// whether a binary is on the path.
///
/// `std::env::var_os("PATH")` and a manual scan, because spawning a process to
/// find out would cost a fork per call and this is called once per adapter
/// start.
#[must_use]
pub fn which(binary: &str) -> bool {
    if binary.contains(std::path::MAIN_SEPARATOR) {
        return std::path::Path::new(binary).is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(binary);
        candidate.is_file()
    })
}

/// whether a page needs a browser at all.
///
/// the cheap test first: a document with almost no text and a root element is
/// a single-page app, and anything else will not improve with rendering.
#[must_use]
pub fn needs_rendering(html: &str) -> bool {
    // how much text the document actually carries, in characters. a shell
    // serves markup and no prose; an article serves thousands of characters.
    let text: usize = crate::readability::visible_text(html);
    let signals_spa = html.contains("id=\"root\"")
        || html.contains("id='root'")
        || html.contains("__NEXT_DATA__")
        || html.contains("data-reactroot")
        || html.contains("ng-app");
    signals_spa && text < MIN_TEXT_FOR_NO_RENDER
}

/// visible text below this many characters is a shell rather than a page.
const MIN_TEXT_FOR_NO_RENDER: usize = 200;

/// the reason a link could not be read, from an http status.
#[must_use]
pub fn blocked_from_status(status: u16) -> BlockedReason {
    match status {
        401..=403 => BlockedReason::Paywall,
        404 | 410 => BlockedReason::Gone,
        _ => BlockedReason::Refused,
    }
}
