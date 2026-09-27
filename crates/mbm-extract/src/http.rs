//! the http client every adapter shares.
//!
//! three things the adapters would otherwise each reinvent: a connection that
//! is actually reused, a retry policy that knows the difference between a 429
//! and a 404, and a concurrency cap that does not open four hundred sockets.
//!
//! retries back off exponentially with jitter. without the jitter, every
//! worker that got rate limited at the same moment retries at the same moment,
//! and the second wave is rate limited too.

use mbm_core::error::{Error, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// what to send with a request.
#[derive(Debug, Clone)]
pub struct Request {
    /// where to send it.
    pub url: String,
    /// what to send.
    pub method: Method,
    /// request headers.
    pub headers: Vec<(String, String)>,
    /// a body, for methods that take one.
    pub body: Option<String>,
    /// how long this particular request may take.
    pub timeout: Duration,
}

impl Request {
    /// a GET.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: Method::Get,
            headers: Vec::new(),
            body: None,
            timeout: Duration::from_secs(20),
        }
    }

    /// a POST with a json body.
    pub fn post_json(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: Method::Post,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: Some(body.into()),
            timeout: Duration::from_secs(30),
        }
    }

    /// add a header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// set the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// the http methods an adapter needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// read.
    Get,
    /// create.
    Post,
    /// replace.
    Put,
    /// remove.
    Delete,
    /// check existence.
    Head,
}

impl Method {
    /// the method as it goes on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
        }
    }
}

/// what came back.
#[derive(Debug, Clone)]
pub struct Response {
    /// the status code.
    pub status: u16,
    /// the final url, after redirects.
    pub url: String,
    /// the response headers, lowercased.
    pub headers: Vec<(String, String)>,
    /// the body.
    pub body: Vec<u8>,
}

impl Response {
    /// whether the status is in the 2xx range.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    /// a header, if present.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }

    /// the body as text, replacing anything that is not valid utf-8.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// the body as json.
    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body).map_err(|e| Error::Http(format!("{}: {e}", self.url)))
    }

    /// the body as some typed shape.
    ///
    /// a separate method from [`json`](Self::json) because the typed form needs
    /// the target type's own `Deserialize`, and a generic wrapper cannot get at
    /// it from a `serde_json::Value`.
    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(|e| Error::Http(format!("{}: {e}", self.url)))
    }
}

/// shared http settings and counters.
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    user_agent: String,
    concurrency: usize,
    retries: u32,
    /// how many requests have been made, for the run summary.
    sent: Arc<AtomicU64>,
    /// how many bytes came back, for the run summary.
    received: Arc<AtomicU64>,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http")
            .field("user_agent", &self.user_agent)
            .field("concurrency", &self.concurrency)
            .field("retries", &self.retries)
            .field("sent", &self.sent.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Http {
    /// build a client.
    ///
    /// `concurrency` caps in-flight requests. the default of 32 is well above
    /// what most hosts like and well below the point where the local socket
    /// table or a remote rate limiter notices.
    pub fn new(user_agent: &str, concurrency: usize) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .pool_max_idle_per_host(concurrency.max(4))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(8))
            .build()
            .map_err(|e| Error::Http(format!("cannot build the client: {e}")))?;

        Ok(Self {
            client,
            user_agent: user_agent.to_owned(),
            concurrency: concurrency.max(1),
            retries: 3,
            sent: Arc::new(AtomicU64::new(0)),
            received: Arc::new(AtomicU64::new(0)),
        })
    }

    /// the default client, with this project's user agent.
    pub fn with_defaults() -> Result<Self> {
        Self::new(concat!("mebookmarker/", env!("CARGO_PKG_VERSION")), 32)
    }

    /// change how many times a transient failure is retried.
    #[must_use]
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    /// change the per-request timeout.
    ///
    /// rebuilds the client, because reqwest fixes the timeout at build time.
    /// this is called once at startup and never again.
    pub fn with_timeout(&self, timeout: Duration) -> Result<Self> {
        let mut out = Self::new(&self.user_agent, self.concurrency)?;
        out.retries = self.retries;
        out.client = reqwest::Client::builder()
            .user_agent(&self.user_agent)
            .pool_max_idle_per_host(self.concurrency.max(4))
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::limited(8))
            .build()
            .map_err(|e| Error::Http(format!("cannot build the client: {e}")))?;
        Ok(out)
    }

    /// how many requests may be in flight at once.
    #[must_use]
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// how many requests have been sent.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// how many bytes have come back.
    #[must_use]
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// the user agent these requests identify as.
    #[must_use]
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// send a request, retrying the failures worth retrying.
    pub async fn send(&self, request: &Request) -> Result<Response> {
        let mut attempt = 0;
        loop {
            match self.once(request).await {
                Ok(response) if response.is_success() => return Ok(response),
                Ok(response) => {
                    let error = classify(response.status, &response.url);
                    if attempt >= self.retries || !error.class().is_retryable() {
                        return Err(error);
                    }
                }
                Err(error) => {
                    if attempt >= self.retries || !error.class().is_retryable() {
                        return Err(error);
                    }
                }
            }
            attempt += 1;
            tokio::time::sleep(backoff(attempt)).await;
        }
    }

    /// send a request once, with no retry.
    pub async fn send_once(&self, request: &Request) -> Result<Response> {
        self.once(request).await
    }

    async fn once(&self, request: &Request) -> Result<Response> {
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
            Method::Head => reqwest::Method::HEAD,
        };

        let mut builder = self.client.request(method, &request.url).timeout(request.timeout);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }

        self.sent.fetch_add(1, Ordering::Relaxed);
        let response = builder.send().await.map_err(|e| map_transport(&e))?;
        let status = response.status().as_u16();
        let url = response.url().to_string();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (k.as_str().to_ascii_lowercase(), v.to_str().unwrap_or_default().to_owned())
            })
            .collect();
        let body = response.bytes().await.map_err(|e| map_transport(&e))?.to_vec();
        self.received.fetch_add(body.len() as u64, Ordering::Relaxed);

        Ok(Response { status, url, headers, body })
    }
}

/// exponential backoff with full jitter.
///
/// the jitter matters more than the exponent. a host that rate limits a burst
/// of workers will rate limit the retry burst too unless the retries are
/// spread out, and a fixed backoff is the same burst every time.
fn backoff(attempt: u32) -> Duration {
    let ceiling = Duration::from_millis(250u64 << attempt.min(6));
    let spread = ceiling.as_millis() as u64;
    Duration::from_millis(pseudo_random() % (spread.max(1)))
}

/// xorshift, for jitter. a real rng would need a dependency for one number.
fn pseudo_random() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0x9E37_79B9_7F4A_7C15) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}

/// turn a non-2xx status into the right error class.
///
/// the split is the whole point: a 404 or a 403 is a fact about the resource
/// and retrying it wastes a rate-limit slot, while a 429 or a 5xx is a
/// statement about right now and is worth another attempt.
fn classify(status: u16, url: &str) -> Error {
    let detail = format!("{url} returned {status}");
    match status {
        401 | 403 => Error::Auth(detail),
        404 | 410 => Error::NotFound(detail),
        408 | 425 | 429 => Error::RateLimited(detail),
        // a client error is a fact about the request, so retrying it unchanged
        // fails identically. `Error::Invalid` is classified as permanent,
        // which is what stops a loop from hammering a rejecting endpoint.
        400..=499 => Error::Invalid(detail),
        _ => Error::Http(detail),
    }
}

fn map_transport(e: &reqwest::Error) -> Error {
    if e.is_timeout() {
        return Error::Timeout(Duration::from_secs(0));
    }
    if e.is_connect() {
        return Error::Http(format!("cannot connect: {e}"));
    }
    if e.status().is_some_and(|s| s.as_u16() == 429) {
        return Error::RateLimited(e.to_string());
    }
    Error::Http(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::error::Class;

    #[test]
    fn a_request_defaults_to_get_with_a_timeout() {
        let r = Request::get("https://example.com");
        assert_eq!(r.method, Method::Get);
        assert!(r.body.is_none());
        assert!(r.timeout > Duration::ZERO);
    }

    #[test]
    fn a_json_post_sets_the_content_type() {
        let r = Request::post_json("https://example.com", "{}");
        assert_eq!(r.method, Method::Post);
        assert!(r.headers.iter().any(|(k, v)| k == "content-type" && v == "application/json"));
    }

    #[test]
    fn headers_and_timeouts_chain() {
        let r = Request::get("https://example.com")
            .header("authorization", "bearer x")
            .timeout(Duration::from_secs(3));
        assert!(r.headers.iter().any(|(k, _)| k == "authorization"));
        assert_eq!(r.timeout, Duration::from_secs(3));
    }

    #[test]
    fn methods_render_as_http_verbs() {
        assert_eq!(Method::Get.as_str(), "GET");
        assert_eq!(Method::Post.as_str(), "POST");
        assert_eq!(Method::Head.as_str(), "HEAD");
    }

    #[test]
    fn success_is_the_2xx_range() {
        let make =
            |status| Response { status, url: "u".into(), headers: Vec::new(), body: Vec::new() };
        assert!(make(200).is_success());
        assert!(make(204).is_success());
        assert!(!make(301).is_success());
        assert!(!make(404).is_success());
        assert!(!make(500).is_success());
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let r = Response {
            status: 200,
            url: "u".into(),
            headers: vec![("content-type".into(), "text/html".into())],
            body: b"hi".to_vec(),
        };
        assert_eq!(r.header("Content-Type"), Some("text/html"));
        assert_eq!(r.header("missing"), None);
    }

    #[test]
    fn a_body_with_invalid_utf8_does_not_panic() {
        let r = Response {
            status: 200,
            url: "u".into(),
            headers: Vec::new(),
            body: vec![0xff, 0xfe, b'o', b'k'],
        };
        assert!(r.text().contains("ok"));
    }

    #[test]
    fn auth_and_gone_are_not_retried() {
        assert_eq!(classify(401, "u").class(), Class::Auth);
        assert_eq!(classify(403, "u").class(), Class::Auth);
        assert_eq!(classify(404, "u").class(), Class::NotFound);
        assert_eq!(classify(410, "u").class(), Class::NotFound);
        for status in [401, 403, 404, 410] {
            assert!(!classify(status, "u").class().is_retryable(), "{status}");
        }
    }

    #[test]
    fn rate_limits_and_server_errors_are_retried() {
        assert_eq!(classify(429, "u").class(), Class::RateLimited);
        assert!(classify(429, "u").class().is_retryable());
        assert!(classify(500, "u").class().is_retryable());
        assert!(classify(503, "u").class().is_retryable());
    }

    #[test]
    fn a_client_error_is_permanent() {
        for status in [400, 422] {
            assert!(!classify(status, "u").class().is_retryable(), "{status}");
        }
    }

    #[test]
    fn an_error_message_carries_the_url_and_status() {
        let e = classify(404, "https://example.com/x");
        assert!(e.to_string().contains("https://example.com/x"), "{e}");
        assert!(e.to_string().contains("404"), "{e}");
    }

    #[test]
    fn backoff_grows_but_stays_bounded() {
        for attempt in 1..=6 {
            let d = backoff(attempt);
            assert!(d <= Duration::from_secs(16), "attempt {attempt} gave {d:?}");
        }
    }

    #[test]
    fn backoff_is_actually_spread_out() {
        // the jitter is the point: twenty retries of the same attempt should
        // not all land in the same millisecond
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            seen.insert(backoff(3).as_millis());
        }
        assert!(seen.len() > 8, "only {} distinct delays in 32 draws", seen.len());
    }

    #[test]
    fn the_concurrency_cap_is_at_least_one() {
        let http = Http::new("test", 0).unwrap();
        assert_eq!(http.concurrency(), 1);
    }

    #[test]
    fn counters_start_at_zero() {
        let http = Http::new("test", 4).unwrap();
        assert_eq!(http.sent(), 0);
        assert_eq!(http.received(), 0);
    }

    #[tokio::test]
    async fn a_transport_failure_on_an_unroutable_host_is_reported() {
        let http = Http::new("test", 1).unwrap();
        let err = http
            .send_once(
                &Request::get("http://127.0.0.1:1/nothing").timeout(Duration::from_millis(500)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Http(_) | Error::Timeout(_)), "got {err:?}");
    }
}
