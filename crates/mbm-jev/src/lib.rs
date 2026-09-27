//! typed evaluation through jev on vercel ai gateway.
//!
//! jev answers bounded questions with values rather than prose: a chosen
//! option, a score on a scale, a probability. that makes it the right tool for
//! the decisions a bookmark pipeline makes thousands of times, and the wrong
//! tool for anything that needs a sentence written.
//!
//! every question in one request is answered against the same state, so a
//! categorise-and-score call is one round trip.
//!
//! ```no_run
//! use std::collections::BTreeMap;
//! use mbm_jev::{EvaluateRequest, Jev, Question};
//!
//! # async fn run() -> mbm_core::Result<()> {
//! let jev = Jev::from_env()?;
//!
//! let mut questions = BTreeMap::new();
//! questions.insert(
//!     "kind".to_owned(),
//!     Question::choice(
//!         "What kind of thing is this?",
//!         [("tool", "A developer tool"), ("article", "A written article")],
//!     ),
//! );
//! questions.insert("worth_reading".to_owned(), Question::boolean("Is this worth reading later?"));
//!
//! let response = jev
//!     .evaluate(&EvaluateRequest::new("a bookmark about a rust cli", questions))
//!     .await?;
//!
//! assert_eq!(response.choice("kind"), Some("tool"));
//! println!("worth reading: {:?}", response.probability("worth_reading"));
//! # Ok(())
//! # }
//! ```

#![doc(html_no_source)]

pub mod client;
pub mod question;

pub use client::{
    DEFAULT_ENDPOINT, Dollars, EvaluateRequest, EvaluateResponse, GatewayMetadata, GatewayOptions,
    JEV, Jev, ProviderMetadata, ProviderOptions, Routing, TypeSafeMetadata, Usage,
};
pub use question::{
    Answer, BooleanCriteria, MAX_CHOICE_OPTIONS, MAX_SCORE_LEVELS, MIN_SCORE_LEVELS, Question,
};
