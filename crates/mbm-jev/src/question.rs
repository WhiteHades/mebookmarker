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
