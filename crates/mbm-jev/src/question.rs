//! the question types a caller can ask.
//!
//! three primitives, and nothing else. the model returns a value of the
//! requested type rather than prose, so a caller can put the answer straight
//! into a database column or a threshold comparison without parsing anything.
//!
//! - [`Question::Boolean`] answers P(true), a probability.
//! - [`Question::Choice`] answers one named option and the probability of each.
//! - [`Question::Score`] answers a position on an ordered scale plus the
//!   probability of each rung.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// most options a choice question may offer.
pub const MAX_CHOICE_OPTIONS: usize = 255;

/// fewest rungs a score question may have.
pub const MIN_SCORE_LEVELS: usize = 2;

/// most rungs a score question may have.
pub const MAX_SCORE_LEVELS: usize = 10;

/// a question to ask about some state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// does this hold?
    Boolean {
        /// what to decide.
        instructions: String,
        /// what the true and false cases mean.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<BooleanCriteria>,
    },
    /// which one of these applies?
    Choice {
        /// what to decide.
        instructions: String,
        /// the options, keyed by the value that comes back.
        criteria: BTreeMap<String, String>,
    },
    /// where does this sit on an ordered scale?
    Score {
        /// what to decide.
        instructions: String,
        /// the rungs, lowest first.
        criteria: Vec<String>,
    },
}

impl Question {
    /// a yes/no question.
    #[must_use]
    pub fn boolean(instructions: impl Into<String>) -> Self {
        Self::Boolean { instructions: instructions.into(), criteria: None }
    }

    /// a yes/no question with the two cases spelled out.
    #[must_use]
    pub fn boolean_with(
        instructions: impl Into<String>,
        yes: impl Into<String>,
        no: impl Into<String>,
    ) -> Self {
        Self::Boolean {
            instructions: instructions.into(),
            criteria: Some(BooleanCriteria { yes: yes.into(), no: no.into() }),
        }
    }

    /// a pick-one question from `(value, description)` pairs.
    #[must_use]
    pub fn choice<I, K, V>(instructions: impl Into<String>, options: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self::Choice {
            instructions: instructions.into(),
            criteria: options.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
        }
    }

    /// an ordered-scale question.
    #[must_use]
    pub fn score<I, S>(instructions: impl Into<String>, rungs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Score {
            instructions: instructions.into(),
            criteria: rungs.into_iter().map(Into::into).collect(),
        }
    }

    /// whether this question is well formed.
    ///
    /// checked before the request goes out, because the gateway rejects a
    /// malformed question with a message that does not say which one of the
    /// several questions in the request was at fault.
    pub fn validate(&self) -> Result<(), String> {
        if self.instructions().trim().is_empty() {
            return Err("a question needs instructions".to_owned());
        }
        match self {
            Self::Choice { criteria, .. } if criteria.is_empty() => {
                Err("a choice question needs at least one option".to_owned())
            }
            Self::Choice { criteria, .. } if criteria.len() > MAX_CHOICE_OPTIONS => Err(format!(
                "a choice question allows {MAX_CHOICE_OPTIONS} options, got {}",
                criteria.len()
            )),
            Self::Score { criteria, .. }
                if !(MIN_SCORE_LEVELS..=MAX_SCORE_LEVELS).contains(&criteria.len()) =>
            {
                Err(format!(
                    "a score question needs {MIN_SCORE_LEVELS} to {MAX_SCORE_LEVELS} rungs, got {}",
                    criteria.len()
                ))
            }
            // a boolean always validates once it has instructions, and a
            // choice or score is fine once its bounds check out
            Self::Boolean { .. } | Self::Choice { .. } | Self::Score { .. } => Ok(()),
        }
    }

    /// the instructions, whichever variant this is.
    #[must_use]
    pub fn instructions(&self) -> &str {
        match self {
            Self::Boolean { instructions, .. }
            | Self::Choice { instructions, .. }
            | Self::Score { instructions, .. } => instructions,
        }
    }
}

/// what true and false mean for a boolean question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BooleanCriteria {
    /// what makes the answer yes.
    pub yes: String,
    /// what makes the answer no.
    pub no: String,
}

/// what the model said.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    /// a probability that the statement holds.
    Boolean {
        /// P(true), in `[0, 1]`.
        probability: f64,
    },
    /// one option, with the probability of each.
    Choice {
        /// the winning option.
        choice: String,
        /// probability per option. the model rounds to two decimals, so these
        /// do not have to sum to exactly one.
        probabilities: BTreeMap<String, f64>,
    },
    /// a position on the scale, with the probability of each rung.
    Score {
        /// the interpolated position, where 0 is the first rung.
        score: f64,
        /// probability per rung index, keyed by the rung's stringified index.
        probabilities: BTreeMap<String, f64>,
    },
}

impl Answer {
    /// P(true) for a boolean, or `None` for the other two shapes.
    #[must_use]
    pub const fn probability(&self) -> Option<f64> {
        match self {
            Self::Boolean { probability } => Some(*probability),
            _ => None,
        }
    }

    /// the winning option, or `None` for the other shapes.
    #[must_use]
    pub fn choice(&self) -> Option<&str> {
        match self {
            Self::Choice { choice, .. } => Some(choice),
            _ => None,
        }
    }

    /// the interpolated score, or `None` for the other shapes.
    #[must_use]
    pub const fn score(&self) -> Option<f64> {
        match self {
            Self::Score { score, .. } => Some(*score),
            _ => None,
        }
    }

    /// the probability the model assigned to one named option or rung.
    #[must_use]
    pub fn probability_of(&self, key: &str) -> Option<f64> {
        match self {
            Self::Boolean { probability } => key.strip_prefix("prob:").map(|_| *probability),
            Self::Choice { probabilities, .. } | Self::Score { probabilities, .. } => {
                probabilities.get(key).copied()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boolean_serialises_with_a_lowercase_type_tag() {
        let json = serde_json::to_string(&Question::boolean("is it useful?")).unwrap();
        assert_eq!(json, r#"{"type":"boolean","instructions":"is it useful?"}"#);
    }

    #[test]
    fn a_choice_carries_its_options() {
        let q = Question::choice("what is it?", [("tool", "a tool"), ("article", "prose")]);
        let json = serde_json::to_value(&q).unwrap();
        assert_eq!(json["type"], "choice");
        assert_eq!(json["criteria"]["tool"], "a tool");
    }

    #[test]
    fn a_score_carries_its_rungs_in_order() {
        let q = Question::score("how good?", ["bad", "ok", "good"]);
        let json = serde_json::to_value(&q).unwrap();
        assert_eq!(json["type"], "score");
        assert_eq!(json["criteria"][0], "bad");
        assert_eq!(json["criteria"][2], "good");
    }

    #[test]
    fn boolean_criteria_are_included_when_given() {
        let q = Question::boolean_with("did it work?", "exit code 0", "anything else");
        let json = serde_json::to_value(&q).unwrap();
        assert_eq!(json["criteria"]["yes"], "exit code 0");
        assert_eq!(json["criteria"]["no"], "anything else");
    }

    #[test]
    fn every_question_type_validates_a_well_formed_question() {
        assert!(Question::boolean("x").validate().is_ok());
        assert!(Question::choice("x", [("a", "b")]).validate().is_ok());
        assert!(Question::score("x", ["low", "high"]).validate().is_ok());
    }

    #[test]
    fn empty_instructions_are_rejected() {
        assert!(Question::boolean("  ").validate().is_err());
    }

    #[test]
    fn a_choice_needs_at_least_one_option() {
        assert!(Question::choice("x", Vec::<(String, String)>::new()).validate().is_err());
    }

    #[test]
    fn a_score_needs_two_rungs_at_least() {
        assert!(Question::score("x", ["only"]).validate().is_err());
        assert!(Question::score("x", ["a", "b"]).validate().is_ok());
    }

    #[test]
    fn a_score_is_capped_at_ten_rungs() {
        let rungs: Vec<String> = (0..11).map(|i| i.to_string()).collect();
        assert!(Question::score("x", rungs).validate().is_err());
    }

    #[test]
    fn a_choice_is_capped_at_255_options() {
        let options: Vec<(String, String)> =
            (0..256).map(|i| (i.to_string(), i.to_string())).collect();
        assert!(Question::choice("x", options).validate().is_err());
    }

    #[test]
    fn a_boolean_answer_reads_back_its_probability() {
        let answer: Answer =
            serde_json::from_str(r#"{"type":"boolean","probability":0.97}"#).unwrap();
        assert_eq!(answer.probability(), Some(0.97));
        assert_eq!(answer.choice(), None);
        assert_eq!(answer.score(), None);
    }

    #[test]
    fn a_choice_answer_reads_back_the_winner_and_its_probabilities() {
        let answer: Answer = serde_json::from_str(
            r#"{"type":"choice","choice":"tool","probabilities":{"tool":0.9,"article":0.1}}"#,
        )
        .unwrap();
        assert_eq!(answer.choice(), Some("tool"));
        assert_eq!(answer.probability_of("tool"), Some(0.9));
        assert_eq!(answer.probability_of("missing"), None);
    }

    #[test]
    fn a_score_answer_reads_back_its_position() {
        let answer: Answer = serde_json::from_str(
            r#"{"type":"score","score":2.5,"probabilities":{"0":0.1,"1":0.4,"2":0.5}}"#,
        )
        .unwrap();
        assert_eq!(answer.score(), Some(2.5));
        assert_eq!(answer.probability_of("2"), Some(0.5));
    }

    #[test]
    fn an_answer_of_the_wrong_shape_is_an_error_not_a_panic() {
        assert!(serde_json::from_str::<Answer>(r#"{"type":"nonsense"}"#).is_err());
        assert!(serde_json::from_str::<Answer>(r#"{"type":"choice"}"#).is_err());
    }
}
