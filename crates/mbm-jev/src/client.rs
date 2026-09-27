//! the http client for jev on vercel ai gateway.
//!
//! one call carries one state and any number of questions about it, so asking
//! three things about a bookmark costs one round trip. every question in a
//! request is answered against the same state, which is what makes a
//! categorise-and-score-and-check call a single request instead of three.
//!
//! the endpoint speaks `/v1/evaluate`. it is deliberately not the
//! openai-compatible or anthropic-compatible surface, which have no way to
//! express "answer with a probability".

use mbm_core::error::{Error, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

use crate::question::{Answer, Question};

/// the gateway's evaluation endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://ai-gateway.vercel.sh/v1/evaluate";

/// the model that answers typed questions.
pub const JEV: &str = "typesafe-ai/jev";

/// how long to wait before giving up on a request.
const TIMEOUT: Duration = Duration::from_secs(30);

/// a request against the evaluation endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateRequest {
    /// the model to call.
    pub model: String,
    /// the thing being judged. a string, an object, or an array.
    pub state: serde_json::Value,
    /// the questions, keyed by the name their answer comes back under.
    pub questions: BTreeMap<String, Question>,
    /// gateway options, such as pinning the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_options: Option<ProviderOptions>,
}

impl EvaluateRequest {
    /// build a request for the given state and questions.
    #[must_use]
    pub fn new(state: impl Into<serde_json::Value>, questions: BTreeMap<String, Question>) -> Self {
        Self { model: JEV.to_owned(), state: state.into(), questions, provider_options: None }
    }

    /// check the request before it leaves.
    pub fn validate(&self) -> Result<()> {
        if self.questions.is_empty() {
            return Err(Error::Invalid("a request needs at least one question".to_owned()));
        }
        for (name, question) in &self.questions {
            question
                .validate()
                .map_err(|e| Error::Invalid(format!("question `{name}`: {e}")))?;
        }
        Ok(())
    }
}

/// gateway-side options.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOptions {
    /// the gateway block, passed through untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayOptions>,
}

impl ProviderOptions {
    /// restrict the request to a zero-data-retention path.
    #[must_use]
    pub fn zero_data_retention() -> Self {
        Self { gateway: Some(GatewayOptions { zero_data_retention: Some(true), only: None }) }
    }

    /// pin which providers may serve the request.
    #[must_use]
    pub fn only(providers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            gateway: Some(GatewayOptions {
                zero_data_retention: None,
                only: Some(providers.into_iter().map(Into::into).collect()),
            }),
        }
    }
}

/// gateway routing and privacy settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayOptions {
    /// require a zero-data-retention path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zero_data_retention: Option<bool>,
    /// the only providers allowed to serve this request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
}

/// a response from the evaluation endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateResponse {
    /// the model that actually ran, which may be an alias resolved to a
    /// concrete version.
    pub model: String,
    /// one answer per question.
    pub answers: BTreeMap<String, Answer>,
    /// what it cost, when the caller asked for the metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// gateway routing, cost, and provider confidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

impl EvaluateResponse {
    /// the answer to a named question.
    #[must_use]
    pub fn answer(&self, name: &str) -> Option<&Answer> {
        self.answers.get(name)
    }

    /// P(true) for a named boolean question, or `None` if it is absent.
    #[must_use]
    pub fn probability(&self, name: &str) -> Option<f64> {
        self.answers.get(name).and_then(Answer::probability)
    }

    /// the winning option for a named choice question.
    #[must_use]
    pub fn choice(&self, name: &str) -> Option<&str> {
        self.answers.get(name).and_then(Answer::choice)
    }

    /// the interpolated score for a named score question.
    #[must_use]
    pub fn score(&self, name: &str) -> Option<f64> {
        self.answers.get(name).and_then(Answer::score)
    }

    /// the model's own confidence in a choice or score answer.
    #[must_use]
    pub fn confidence(&self, name: &str) -> Option<f64> {
        self.provider_metadata
            .as_ref()?
            .typesafe
            .as_ref()?
            .confidence
            .get(name)
            .copied()
    }

    /// the cost the gateway reported, in dollars.
    #[must_use]
    pub fn cost_usd(&self) -> Option<f64> {
        Some(self.provider_metadata.as_ref()?.gateway.as_ref()?.cost?.get())
    }

    /// the cost in microcents, which is the unit the run log stores.
    #[must_use]
    pub fn cost_micros(&self) -> Option<i64> {
        self.cost_usd().map(|c| (c * 1e8).round() as i64)
    }
}

/// token counts for a call.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// prompt tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// completion tokens.
    #[serde(default)]
    pub output_tokens: u64,
}

/// per-provider metadata the gateway attaches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderMetadata {
    /// the typesafe provider's own statistics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typesafe: Option<TypeSafeMetadata>,
    /// routing and cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayMetadata>,
}

impl ProviderMetadata {
    /// an empty set, for callers that do not request metadata.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

/// the evaluation provider's statistics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeSafeMetadata {
    /// per-question confidence, for choice and score answers.
    #[serde(default)]
    pub confidence: BTreeMap<String, f64>,
}

/// routing and cost metadata.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayMetadata {
    /// which provider actually served the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<Routing>,
    /// the cost in dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Dollars>,
    /// what the request would have cost without the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_cost: Option<Dollars>,
    /// the gateway's own fee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_cost: Option<Dollars>,
}

/// a dollar amount.
///
/// the gateway sends these as json strings, which is the usual way to keep
/// full decimal precision through a json encoder that stores floats as binary.
/// accepting a bare number too costs a few lines and keeps the type usable
/// against a proxy that reformats the body.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct Dollars(f64);

impl Dollars {
    /// the amount.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl std::fmt::Display for Dollars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<f64> for Dollars {
    fn from(value: f64) -> Self {
        Self(value)
    }
}

impl From<Dollars> for f64 {
    fn from(value: Dollars) -> Self {
        value.0
    }
}

impl Serialize for Dollars {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for Dollars {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Number(f64),
            Text(String),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Number(n) => Ok(Self(n)),
            Repr::Text(s) => {
                s.trim().parse().map(Self).map_err(|_| serde::de::Error::custom(format!("`{s}` is not a dollar amount")))
            }
        }
    }
}

/// how the gateway resolved a model id to a provider.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Routing {
    /// the id the caller asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_model_id: Option<String>,
    /// the provider that served it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_provider: Option<String>,
    /// the canonical slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_slug: Option<String>,
}

/// a configured client.
pub struct Jev {
    client: Client,
    endpoint: String,
    api_key: String,
    model: String,
}

impl std::fmt::Debug for Jev {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the api key never appears in a log line
        f.debug_struct("Jev").field("endpoint", &self.endpoint).field("model", &self.model).finish()
    }
}

impl Jev {
    /// build a client for the given gateway key.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("mebookmarker/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Jev(format!("cannot build the http client: {e}")))?;
        Ok(Self {
            client,
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            api_key: api_key.into(),
            model: JEV.to_owned(),
        })
    }

    /// read the key from the environment.
    ///
    /// two names are accepted because the gateway documents `AI_GATEWAY_API_KEY`
    /// while most local setups export `AI_GATEWAY_API_KEY` under a project
    /// prefix.
    pub fn from_env() -> Result<Self> {
        for name in ["AI_GATEWAY_API_KEY", "VERCEL_AI_GATEWAY_API_KEY", "JEV_API_KEY"] {
            if let Ok(key) = std::env::var(name)
                && !key.trim().is_empty()
            {
                return Self::new(key);
            }
        }
        Err(Error::Auth("no ai gateway key: set AI_GATEWAY_API_KEY".to_owned()))
    }

    /// point the client at a different endpoint, for a proxy or a test double.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// call a different model.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// ask the questions.
    pub async fn evaluate(&self, request: &EvaluateRequest) -> Result<EvaluateResponse> {
        request.validate()?;
        if self.api_key.trim().is_empty() {
            return Err(Error::Auth("the ai gateway key is empty".to_owned()));
        }

        let mut body = request.clone();
        body.model = self.model.clone();

        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(map_send_error)?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(classify(status.as_u16(), &text));
        }

        response
            .json::<EvaluateResponse>()
            .await
            .map_err(|e| Error::Jev(format!("cannot read the response: {e}")))
    }

    /// ask one question and return its answer.
    pub async fn ask(&self, state: &serde_json::Value, name: &str, question: Question) -> Result<Answer> {
        let mut questions = BTreeMap::new();
        questions.insert(name.to_owned(), question);
        let mut response = self.evaluate(&EvaluateRequest::new(state.clone(), questions)).await?;
        response
            .answers
            .remove(name)
            .ok_or_else(|| Error::Jev(format!("the response has no answer named `{name}`")))
    }
}

/// turn a transport failure into something the pipeline can act on.
fn map_send_error(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        return Error::Timeout(TIMEOUT);
    }
    if e.is_connect() {
        return Error::Jev(format!("cannot reach the gateway: {e}"));
    }
    if e.status().is_some_and(|s| s.as_u16() == 429) {
        return Error::RateLimited(format!("the gateway rate limited us: {e}"));
    }
    Error::Jev(e.to_string())
}

/// turn a non-2xx response into the right error class.
///
/// a 401 or 403 means the key is wrong, and retrying with the same key just
/// burns rate limit, so those get [`Error::Auth`]. a 400 means the question
/// was malformed, and retrying it unchanged fails the same way, so those get
/// [`Error::Invalid`]. everything else is worth another try.
fn classify(status: u16, body: &str) -> Error {
    let detail = if body.trim().is_empty() {
        String::new()
    } else {
        format!(": {}", body.trim().chars().take(400).collect::<String>())
    };
    match status {
        401 | 403 => Error::Auth(format!("the gateway rejected the key (status {status}){detail}")),
        400 | 404 | 422 => Error::Invalid(format!("the gateway rejected the request ({status}){detail}")),
        429 => Error::RateLimited(format!("the gateway rate limited us{detail}")),
        other => Error::Jev(format!("the gateway returned {other}{detail}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn questions() -> BTreeMap<String, Question> {
        let mut q = BTreeMap::new();
        q.insert("useful".to_owned(), Question::boolean("is it useful?"));
        q
    }

    #[test]
    fn a_request_defaults_to_the_jev_model() {
        let request = EvaluateRequest::new("some state", questions());
        assert_eq!(request.model, JEV);
    }

    #[test]
    fn a_request_with_no_questions_is_rejected() {
        let request = EvaluateRequest::new("some state", BTreeMap::new());
        assert!(request.validate().is_err());
    }

    #[test]
    fn a_malformed_question_names_itself() {
        let mut q = BTreeMap::new();
        q.insert("category".to_owned(), Question::choice("pick", Vec::<(String, String)>::new()));
        let request = EvaluateRequest::new("state", q);
        let err = request.validate().unwrap_err().to_string();
        assert!(err.contains("category"), "{err}");
    }

    #[test]
    fn a_request_serialises_with_the_shape_the_endpoint_expects() {
        let request = EvaluateRequest::new("a bookmark about rust", questions());
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "typesafe-ai/jev");
        assert_eq!(json["state"], "a bookmark about rust");
        assert_eq!(json["questions"]["useful"]["type"], "boolean");
    }

    #[test]
    fn structured_state_survives_serialisation() {
        let state = serde_json::json!({ "text": "hello", "tags": ["rust"] });
        let request = EvaluateRequest::new(state.clone(), questions());
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["state"], state);
    }

    #[test]
    fn a_response_exposes_each_answer_shape() {
        let response: EvaluateResponse = serde_json::from_str(
            r#"{
              "model": "typesafe-ai/jev",
              "answers": {
                "useful": {"type": "boolean", "probability": 0.83},
                "kind": {"type": "choice", "choice": "tool", "probabilities": {"tool": 0.9, "article": 0.1}},
                "quality": {"type": "score", "score": 1.4, "probabilities": {"0": 0.2, "1": 0.6, "2": 0.2}}
              },
              "usage": {"inputTokens": 477, "outputTokens": 85},
              "providerMetadata": {
                "typesafe": {"confidence": {"kind": 1.0, "quality": 0.59}},
                "gateway": {"routing": {"finalProvider": "typesafe-ai"}, "cost": 0.000020034}
              }
            }"#,
        )
        .unwrap();

        assert_eq!(response.probability("useful"), Some(0.83));
        assert_eq!(response.choice("kind"), Some("tool"));
        assert_eq!(response.score("quality"), Some(1.4));
        assert_eq!(response.answer("kind").unwrap().probability_of("article"), Some(0.1));
        assert_eq!(response.confidence("kind"), Some(1.0));
        assert_eq!(response.usage.unwrap().input_tokens, 477);
        assert_eq!(response.probability("missing"), None);
    }

    #[test]
    fn cost_converts_to_microcents() {
        let response: EvaluateResponse = serde_json::from_str(
            r#"{"model":"m","answers":{},"providerMetadata":{"gateway":{"cost":0.000020034}}}"#,
        )
        .unwrap();
        assert_eq!(response.cost_micros(), Some(2_003));
    }

    #[test]
    fn a_cost_sent_as_a_json_string_parses() {
        let response: EvaluateResponse = serde_json::from_str(
            r#"{"model":"m","answers":{},
                "providerMetadata":{"gateway":{"cost":"0.000011508","marketCost":"0.000011508"}}}"#,
        )
        .unwrap();
        assert_eq!(response.cost_usd(), Some(0.000011508));
        assert_eq!(response.cost_micros(), Some(1_151));
    }

    #[test]
    fn a_cost_sent_as_a_json_number_parses_too() {
        let response: EvaluateResponse = serde_json::from_str(
            r#"{"model":"m","answers":{},"providerMetadata":{"gateway":{"cost":0.5}}}"#,
        )
        .unwrap();
        assert_eq!(response.cost_usd(), Some(0.5));
    }

    #[test]
    fn a_cost_that_is_not_a_number_is_rejected() {
        let parsed = serde_json::from_str::<EvaluateResponse>(
            r#"{"model":"m","answers":{},"providerMetadata":{"gateway":{"cost":"free"}}}"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn a_response_without_metadata_still_answers() {
        let response: EvaluateResponse = serde_json::from_str(
            r#"{"model":"m","answers":{"a":{"type":"boolean","probability":0.5}}}"#,
        )
        .unwrap();
        assert_eq!(response.probability("a"), Some(0.5));
        assert_eq!(response.cost_usd(), None);
        assert_eq!(response.confidence("a"), None);
    }

    #[test]
    fn a_key_failure_is_classified_as_auth() {
        for status in [401, 403] {
            let err = classify(status, "invalid key");
            assert_eq!(err.class(), mbm_core::error::Class::Auth, "status {status}");
        }
    }

    #[test]
    fn a_malformed_request_is_classified_as_permanent() {
        for status in [400, 422] {
            let err = classify(status, "bad question");
            assert_eq!(err.class(), mbm_core::error::Class::Permanent, "status {status}");
        }
    }

    #[test]
    fn a_rate_limit_is_classified_as_retryable() {
        let err = classify(429, "slow down");
        assert_eq!(err.class(), mbm_core::error::Class::RateLimited);
        assert!(err.class().is_retryable());
    }

    #[test]
    fn a_server_error_is_retryable() {
        assert!(classify(500, "boom").class().is_retryable());
        assert!(classify(529, "overloaded").class().is_retryable());
    }

    #[test]
    fn a_long_error_body_is_truncated() {
        let err = classify(500, &"x".repeat(5_000));
        assert!(err.to_string().len() < 500);
    }

    #[test]
    fn the_client_never_prints_its_key() {
        let client = Jev::new("super-secret-key").unwrap();
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("super-secret-key"), "{rendered}");
    }

    #[tokio::test]
    async fn an_empty_key_is_refused_before_the_request() {
        let client = Jev::new("").unwrap();
        let err = client.evaluate(&EvaluateRequest::new("state", questions())).await.unwrap_err();
        assert_eq!(err.class(), mbm_core::error::Class::Auth);
    }

    #[tokio::test]
    async fn a_401_from_the_endpoint_becomes_an_auth_error() {
        let server = tiny_http_server(401, r#"{"error":"invalid key"}"#);
        let client = Jev::new("k").unwrap().with_endpoint(server.url.clone());
        let err = client.evaluate(&EvaluateRequest::new("state", questions())).await.unwrap_err();
        assert_eq!(err.class(), mbm_core::error::Class::Auth);
        server.stop();
    }

    #[tokio::test]
    async fn a_200_with_a_valid_body_parses() {
        let server = tiny_http_server(
            200,
            r#"{"model":"typesafe-ai/jev","answers":{"useful":{"type":"boolean","probability":0.9}}}"#,
        );
        let client = Jev::new("k").unwrap().with_endpoint(server.url.clone());
        let response = client.evaluate(&EvaluateRequest::new("state", questions())).await.unwrap();
        assert_eq!(response.probability("useful"), Some(0.9));
        server.stop();
    }

    /// a one-shot http server, so the client can be tested against a real socket.
    fn tiny_http_server(status: u16, body: &'static str) -> TinyServer {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/evaluate", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line.trim().is_empty() {
                    break;
                }
                line.clear();
            }
            let response =
                format!("HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            let _ = stream.write_all(response.as_bytes());
        });
        TinyServer { url, handle: Some(handle) }
    }

    struct TinyServer {
        url: String,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl TinyServer {
        fn stop(mut self) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}
