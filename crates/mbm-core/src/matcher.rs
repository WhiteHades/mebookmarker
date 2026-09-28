//! compiling a taxonomy into something worth running against a million urls.
//!
//! [`Taxonomy::route`] asks one question at a time: does this url match
//! `github.com`? `medium.com`? `/blog`? ...? With twenty-five patterns that is
//! twenty-five passes over the string, and a taxonomy is matched once per link
//! on every bookmark in the corpus.
//!
//! [`CompiledTaxonomy`] changes the shape of the problem. every pattern goes
//! into one aho-corasick automaton, and a single pass over the haystack reports
//! every pattern it contains. that is O(text + patterns) instead of
//! O(text x patterns), and the automaton is case-insensitive by construction,
//! so nothing has to be lowercased into a temporary.
//!
//! the compiled form is built once per run and reused, which is also why it is
//! a separate type from [`Taxonomy`]: the config is data, the automaton is a
//! derived index over it.

use crate::category::{Action, Taxonomy};
use aho_corasick::AhoCorasick;
use std::sync::Arc;

/// what a pattern match means for routing.
#[derive(Debug, Clone, PartialEq)]
struct CompiledRule {
    /// which category the pattern assigns to.
    slug: Arc<str>,
    /// automaton indices for the patterns this rule requires, so all of them
    /// must be present for the rule to fire.
    match_all: Vec<usize>,
    /// the action for a match, resolved against the category at compile time.
    action: Action,
    /// position in the original rule list, which decides who wins a tie.
    order: usize,
    /// this rule's index into the automaton's pattern list.
    pattern: usize,
}

/// a taxonomy with its patterns compiled into one automaton.
///
/// cheap to share: clone an [`Arc`] rather than rebuilding.
#[derive(Debug, Clone)]
pub struct CompiledTaxonomy {
    automaton: Arc<AhoCorasick>,
    by_pattern: Arc<Vec<CompiledRule>>,
    fallback: Arc<str>,
    fallback_action: Action,
    /// pattern text, kept so a caller can report which rule fired.
    texts: Arc<Vec<String>>,
}

impl CompiledTaxonomy {
    /// build the automaton for a taxonomy.
    ///
    /// patterns are inserted in rule order and the search is ordered, so the
    /// first match a url produces is the same answer the rule-by-rule loop
    /// would have given. that is what keeps [`route`](Self::route) agreeing
    /// with [`Taxonomy::route`].
    #[must_use]
    pub fn new(taxonomy: &Taxonomy) -> Self {
        let mut patterns: Vec<String> = Vec::new();
        let mut by_pattern: Vec<CompiledRule> = Vec::new();

        for (order, rule) in taxonomy.rules.iter().enumerate() {
            let action = rule
                .action
                .or_else(|| taxonomy.get(&rule.slug).map(|c| c.action))
                .unwrap_or(Action::Capture);
            let slug: Arc<str> = match taxonomy.get(&rule.slug) {
                Some(category) => Arc::from(category.slug.as_str()),
                None => Arc::from(taxonomy.fallback.as_str()),
            };
            // `match_all` is resolved first, because the `match_any` entries
            // clone the finished index list
            let mut match_all: Vec<usize> = Vec::with_capacity(rule.match_all.len());
            for needle in &rule.match_all {
                match_all.push(intern(&mut patterns, needle));
            }

            for needle in &rule.match_any {
                let pattern = intern(&mut patterns, needle);
                by_pattern.push(CompiledRule {
                    slug: Arc::clone(&slug),
                    match_all: match_all.clone(),
                    action,
                    order,
                    pattern,
                });
            }
        }

        let fallback_action =
            taxonomy.get(&taxonomy.fallback).map_or(Action::Capture, |c| c.action);

        let automaton =
            AhoCorasick::builder().ascii_case_insensitive(true).build(&patterns).unwrap_or_else(
                |_| AhoCorasick::new([""]).expect("the fallback pattern always builds"),
            );

        Self {
            automaton: Arc::new(automaton),
            by_pattern: Arc::new(by_pattern),
            fallback: Arc::from(taxonomy.fallback.as_str()),
            fallback_action,
            texts: Arc::new(patterns),
        }
    }

    /// route a url, answering with the category slug and its action.
    ///
    /// takes the same input as [`Taxonomy::route`]: a host with an optional
    /// path, a full url, or either with a query string. the scheme, query, and
    /// fragment are dropped first so a pattern cannot match inside them.
    #[must_use]
    pub fn route(&self, target: &str) -> (Arc<str>, Action) {
        self.match_rule(target).map_or_else(
            || (Arc::clone(&self.fallback), self.fallback_action),
            |m| (Arc::clone(&m.slug), m.action),
        )
    }

    /// the slug a url falls back to when no rule claims it.
    #[must_use]
    pub fn fallback(&self) -> &str {
        &self.fallback
    }

    /// every category this url matches, in rule order.
    ///
    /// cheaper than calling [`route`](Self::route) for each candidate
    /// category, which matters when a caller wants the top few rather than one.
    #[must_use]
    pub fn matches(&self, target: &str) -> Vec<MatchedRule> {
        let haystack = normalise(target);
        let found = self.found_set(haystack);

        let mut out: Vec<MatchedRule> = Vec::new();
        for rule in self.by_pattern.iter() {
            if !found.get(rule.pattern) {
                continue;
            }
            if !rule.match_all.iter().all(|&p| found.get(p)) {
                continue;
            }
            if out.iter().any(|m| m.order == rule.order) {
                continue;
            }
            out.push(MatchedRule {
                slug: Arc::clone(&rule.slug),
                action: rule.action,
                pattern: Arc::from(self.texts[rule.pattern].as_str()),
                order: rule.order,
            });
        }

        // `by_pattern` is already in rule order because it was built by walking
        // the rules in order, so this is a no-op guard against a caller having
        // constructed the slice some other way.
        out.sort_unstable_by_key(|m| m.order);
        out
    }

    /// which patterns the automaton found, as a bit per pattern.
    ///
    /// one pass over the haystack answers every question at once, which is the
    /// whole reason this type exists. a rule then needs a membership test
    /// rather than another scan.
    fn found_set(&self, haystack: &str) -> PatternSet {
        let mut set = PatternSet::new(self.texts.len());
        // overlapping, because a rule may require several patterns at once
        // and one of them can sit inside another. the plain iterator reports
        // only the first match at each position, which would hide a required
        // pattern sharing a prefix with the one that got there first.
        for found in self.automaton.find_overlapping_iter(haystack.as_bytes()) {
            set.insert(found.pattern().as_usize());
        }
        set
    }

    /// the first rule that claims a url.
    fn match_rule(&self, target: &str) -> Option<MatchedRule> {
        self.matches(target).into_iter().next()
    }

    /// how many patterns are in the automaton.
    #[must_use]
    pub fn pattern_count(&self) -> usize {
        self.by_pattern.len()
    }

    /// whether the taxonomy has no rules at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_pattern.is_empty()
    }
}

/// the automaton index for a pattern, adding it to the list if it is new.
fn intern(patterns: &mut Vec<String>, needle: &str) -> usize {
    let folded = needle.to_ascii_lowercase();
    if let Some(existing) = patterns.iter().position(|p| *p == folded) {
        return existing;
    }
    patterns.push(folded);
    patterns.len() - 1
}

/// a one-bit-per-pattern membership set.
struct PatternSet {
    words: Vec<u64>,
}

impl PatternSet {
    fn new(patterns: usize) -> Self {
        Self { words: vec![0; patterns.div_ceil(64).max(1)] }
    }

    fn insert(&mut self, pattern: usize) {
        if let Some(word) = self.words.get_mut(pattern / 64) {
            *word |= 1u64 << (pattern % 64);
        }
    }

    fn get(&self, pattern: usize) -> bool {
        self.words.get(pattern / 64).is_some_and(|w| w & (1u64 << (pattern % 64)) != 0)
    }
}

/// one rule that claimed a url.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchedRule {
    /// the category slug.
    pub slug: Arc<str>,
    /// the action for this match.
    pub action: Action,
    /// the pattern text that fired, for a log line.
    pub pattern: Arc<str>,
    /// position in the rule list, so a caller can explain the ordering.
    pub order: usize,
}

/// strip the scheme, query, and fragment from a url or host/path pair.
fn normalise(target: &str) -> &str {
    let trimmed = target.trim();
    let without_scheme = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    without_scheme.split(['?', '#']).next().unwrap_or(without_scheme)
}
