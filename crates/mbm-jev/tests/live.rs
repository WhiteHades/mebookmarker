//! live checks against the real gateway.
//!
//! these run only when `MEBOOKMARKER_JEV_LIVE=1` and a key is present, so the
//! default `cargo test` stays offline and hermetic. the fixtures in the unit
//! tests pin the wire format; these confirm the wire format is still current.
//!
//! run with:
//!   MEBOOKMARKER_JEV_LIVE=1 AI_GATEWAY_API_KEY=$(cat ~/.secrets/vercel/vercel-ai-gateway) \
//!     cargo test -p mbm-jev --test live -- --nocapture

use std::collections::BTreeMap;

use mbm_jev::{EvaluateRequest, Jev, Question};

fn live() -> Option<Jev> {
    if std::env::var("MEBOOKMARKER_JEV_LIVE").as_deref() != Ok("1") {
        eprintln!("skipping: set MEBOOKMARKER_JEV_LIVE=1 to run against the gateway");
        return None;
    }
    match Jev::from_env() {
        Ok(client) => Some(client),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

const BOOKMARK: &str = "\
@simonw forked gistpreview.github.io into gisthost.github.io, a free github pages \
tool that renders html stored in gists. https://gisthost.github.io/";

fn three_questions() -> BTreeMap<String, Question> {
    let mut q = BTreeMap::new();
    q.insert(
        "kind".to_owned(),
        Question::choice(
            "What kind of thing is this?",
            [
                ("tool", "A developer tool, CLI, or open-source project"),
                ("article", "A written article or blog post"),
                ("meme", "A joke or meme"),
                ("news", "Breaking news"),
                ("media", "Video, podcast, or music"),
            ],
        ),
    );
    q.insert(
        "worth".to_owned(),
        Question::score("How likely is this worth reading later?", ["noise", "skim", "read", "must read"]),
    );
    q.insert("code".to_owned(), Question::boolean("Is it primarily about software development?"));
    q
}

#[tokio::test]
async fn the_gateway_answers_all_three_shapes_in_one_request() {
    let Some(jev) = live() else { return };

    let response = jev
        .evaluate(&EvaluateRequest::new(BOOKMARK, three_questions()))
        .await
        .expect("the gateway should answer");

    assert_eq!(response.answers.len(), 3, "every question gets an answer");
    assert_eq!(response.model, "typesafe-ai/jev");

    let kind = response.choice("kind").expect("a choice answer");
    assert_eq!(kind, "tool", "a gists renderer is a tool");

    let worth = response.score("worth").expect("a score answer");
    assert!((0.0..=3.0).contains(&worth), "score {worth} out of range");

    let code = response.probability("code").expect("a boolean answer");
    assert!((0.0..=1.0).contains(&code), "probability {code} out of range");
    assert!(code > 0.5, "a github pages tool is about software, got {code}");
}

#[tokio::test]
async fn structured_state_is_accepted() {
    let Some(jev) = live() else { return };

    let state = serde_json::json!({
        "text": "a rust cli that archives bookmarks",
        "author": "simonw",
        "links": [{ "url": "https://github.com/a/b", "kind": "repository" }],
    });

    let mut q = BTreeMap::new();
    q.insert("kind".to_owned(), Question::choice("what is it?", [("tool", "a tool"), ("article", "prose")]));

    let response = jev.evaluate(&EvaluateRequest::new(state, q)).await.expect("an answer");
    assert_eq!(response.choice("kind"), Some("tool"));
}

#[tokio::test]
async fn a_malformed_question_is_refused_locally() {
    let Some(jev) = live() else { return };

    let mut q = BTreeMap::new();
    q.insert("bad".to_owned(), Question::score("one rung", ["only"]));

    // this must not reach the network, and must not come back as a 400
    let err = jev.evaluate(&EvaluateRequest::new("state", q)).await.unwrap_err();
    assert_eq!(err.class(), mbm_core::error::Class::Permanent);
}

#[tokio::test]
async fn an_empty_key_is_refused_locally() {
    if std::env::var("MEBOOKMARKER_JEV_LIVE").as_deref() != Ok("1") {
        return;
    }
    let jev = Jev::new("").unwrap();
    let err = jev.evaluate(&EvaluateRequest::new(BOOKMARK, three_questions())).await.unwrap_err();
    assert_eq!(err.class(), mbm_core::error::Class::Auth);
}

#[tokio::test]
async fn a_real_request_reports_a_plausible_cost_and_latency() {
    let Some(jev) = live() else { return };

    let started = std::time::Instant::now();
    let response = jev.evaluate(&EvaluateRequest::new(BOOKMARK, three_questions())).await.unwrap();
    let elapsed = started.elapsed();

    let cost = response.cost_usd().expect("the gateway reports a cost");
    assert!(cost > 0.0, "cost {cost}");
    assert!(cost < 0.01, "three questions cost {cost}, which is far too much");

    let usage = response.usage.expect("the gateway reports usage");
    assert!(usage.input_tokens > 0 && usage.output_tokens > 0, "{usage:?}");

    eprintln!(
        "  3 questions: {elapsed:?}, {cost:.8} usd, {} in / {} out tokens",
        usage.input_tokens, usage.output_tokens
    );
    assert!(elapsed < std::time::Duration::from_secs(20), "took {elapsed:?}");
}

#[tokio::test]
async fn concurrent_requests_all_succeed() {
    let Some(jev) = live() else { return };

    let jev = std::sync::Arc::new(jev);
    let mut handles = Vec::new();
    for i in 0..8 {
        let jev = std::sync::Arc::clone(&jev);
        handles.push(tokio::spawn(async move {
            let mut q = BTreeMap::new();
            q.insert("kind".to_owned(), Question::choice("what is it?", [("tool", "a tool"), ("article", "prose")]));
            let state = format!("bookmark number {i} about a rust tool");
            jev.evaluate(&EvaluateRequest::new(state, q)).await.map(|r| r.choice("kind").map(str::to_owned))
        }));
    }

    for handle in handles {
        let answer = handle.await.unwrap().expect("a concurrent request");
        assert_eq!(answer.as_deref(), Some("tool"));
    }
}
