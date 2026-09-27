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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::category::{Category, CategoryRule};

    fn taxonomy() -> Taxonomy {
        let mut t = Taxonomy::empty();
        t.insert(
            Category::new("repository", "Repository", "#06b6d4", "Code.")
                .filed_to("tools")
                .with_action(Action::File),
        );
        t.insert(
            Category::new("article", "Article", "#ec4899", "Prose.")
                .filed_to("articles")
                .with_action(Action::File),
        );
        t.insert(Category::new("video", "Video", "#ef4444", "Video.").with_action(Action::Defer));
        t.insert(Category::new("general", "General", "#64748b", "Anything."));
        t.rules
            .push(CategoryRule::any("repository", ["github.com", "gitlab.com", "bitbucket.org"]));
        t.rules.push(CategoryRule::any("article", ["medium.com", "substack.com", "arxiv.org"]));
        t.rules.push(CategoryRule::any("video", ["youtube.com", "youtu.be", "vimeo.com"]));
        t
    }

    /// the compiled form must agree with the rule-by-rule form on every input.
    fn agrees(cases: &[&str]) {
        let t = taxonomy();
        let compiled = CompiledTaxonomy::new(&t);
        for case in cases {
            let (slug, action) = t.route(case);
            let (compiled_slug, compiled_action) = compiled.route(case);
            assert_eq!(
                (slug.as_str(), action),
                (compiled_slug.as_ref(), compiled_action),
                "disagreed on {case:?}"
            );
        }
    }

    #[test]
    fn the_compiled_form_agrees_with_the_loop() {
        agrees(&[
            "github.com/a/b",
            "https://github.com/a/b?tab=readme#top",
            "https://medium.com/some-post",
            "youtube.com/watch?v=abc",
            "https://youtu.be/abc",
            "arxiv.org/abs/1234.5678",
            "vimeo.com/12345",
            "example.com/whatever",
            "",
            "https://example.com/x?ref=github.com",
            "GITHUB.COM/Case/Sensitive",
            "gitlab.com/group/project",
        ]);
    }

    #[test]
    fn routing_finds_each_category() {
        let compiled = CompiledTaxonomy::new(&taxonomy());
        assert_eq!(compiled.route("github.com/a/b").0.as_ref(), "repository");
        assert_eq!(compiled.route("medium.com/x").0.as_ref(), "article");
        assert_eq!(compiled.route("youtube.com/x").0.as_ref(), "video");
    }

    #[test]
    fn routing_falls_back_when_nothing_matches() {
        let compiled = CompiledTaxonomy::new(&taxonomy());
        let (slug, action) = compiled.route("example.com/nothing");
        assert_eq!(slug.as_ref(), "general");
        assert_eq!(action, Action::Capture);
    }

    #[test]
    fn matching_ignores_case() {
        let compiled = CompiledTaxonomy::new(&taxonomy());
        assert_eq!(compiled.route("GitHub.com/A/B").0.as_ref(), "repository");
        assert_eq!(compiled.route("MEDIUM.COM/Post").0.as_ref(), "article");
    }

    #[test]
    fn a_pattern_does_not_match_inside_the_query_string() {
        let compiled = CompiledTaxonomy::new(&taxonomy());
        assert_eq!(compiled.route("https://example.com/x?ref=github.com").0.as_ref(), "general");
    }

    #[test]
    fn the_first_matching_rule_wins() {
        let mut t = taxonomy();
        // a later, broader rule must not steal a url an earlier rule claims
        t.rules.push(CategoryRule::any("general", ["github.com"]));
        let compiled = CompiledTaxonomy::new(&t);
        assert_eq!(compiled.route("github.com/a").0.as_ref(), "repository");
    }

    #[test]
    fn a_rule_with_match_all_needs_every_pattern() {
        let mut t = Taxonomy::empty();
        t.insert(Category::new("research", "Research", "#3b82f6", "Papers.").filed_to("papers"));
        t.rules.push(CategoryRule::any("research", ["arxiv.org"]).requiring(["/abs/"]));

        let compiled = CompiledTaxonomy::new(&t);
        assert_eq!(compiled.route("arxiv.org/abs/1234").0.as_ref(), "research");
        assert_eq!(compiled.route("arxiv.org/list/cs.AI").0.as_ref(), "general");
    }

    #[test]
    fn every_matching_rule_comes_back_in_order() {
        let mut t = taxonomy();
        t.rules.push(CategoryRule::any("general", ["github"]));
        let compiled = CompiledTaxonomy::new(&t);
        let all = compiled.matches("github.com/a/b");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].slug.as_ref(), "repository", "declaration order, not match order");
        assert_eq!(all[1].slug.as_ref(), "general");
    }

    #[test]
    fn a_matched_rule_reports_the_pattern_that_fired() {
        let compiled = CompiledTaxonomy::new(&taxonomy());
        let all = compiled.matches("https://medium.com/x");
        assert_eq!(all[0].pattern.as_ref(), "medium.com");
    }

    #[test]
    fn an_action_override_survives_compilation() {
        let mut t = taxonomy();
        t.rules.push(CategoryRule::any("article", ["medium.com"]).with_action(Action::Defer));
        let compiled = CompiledTaxonomy::new(&t);
        // the earlier article rule still wins, so its File action applies
        assert_eq!(compiled.route("medium.com/x").1, Action::File);
    }

    #[test]
    fn a_rule_naming_an_unknown_category_falls_back() {
        let mut t = taxonomy();
        t.rules.push(CategoryRule::any("does-not-exist", ["nowhere.example"]));
        let compiled = CompiledTaxonomy::new(&t);
        assert_eq!(compiled.route("nowhere.example/x").0.as_ref(), "general");
    }

    #[test]
    fn an_empty_taxonomy_compiles_and_routes_everything_to_the_fallback() {
        let compiled = CompiledTaxonomy::new(&Taxonomy::empty());
        assert!(compiled.is_empty());
        assert_eq!(compiled.pattern_count(), 0);
        assert_eq!(compiled.route("anything").0.as_ref(), "general");
    }

    #[test]
    fn a_taxonomy_with_categories_but_no_rules_is_still_empty() {
        let mut t = Taxonomy::empty();
        t.insert(Category::new("tool", "Tool", "#000000", "A tool."));
        let compiled = CompiledTaxonomy::new(&t);
        assert!(compiled.is_empty());
        assert_eq!(compiled.route("github.com/a").0.as_ref(), "general");
    }

    #[test]
    fn a_long_path_still_matches() {
        let compiled = CompiledTaxonomy::new(&taxonomy());
        let quiet = format!("https://example.com/{}", "a".repeat(8192));
        assert_eq!(compiled.route(&quiet).0.as_ref(), "general");
    }

    #[test]
    fn a_pattern_inside_a_long_path_still_claims_the_url() {
        // a path that literally contains a known host is that host's link for
        // practical purposes, so matching it is the right answer
        let compiled = CompiledTaxonomy::new(&taxonomy());
        let deep = format!("https://example.com/{}/github.com", "a".repeat(4096));
        assert_eq!(compiled.route(&deep).0.as_ref(), "repository");
    }

    #[test]
    fn multibyte_urls_are_handled() {
        let compiled = CompiledTaxonomy::new(&taxonomy());
        assert_eq!(compiled.route("https://example.com/héllo/wörld").0.as_ref(), "general");
        assert_eq!(compiled.route("https://medium.com/日本語").0.as_ref(), "article");
    }

    #[test]
    fn a_cloned_compiler_shares_one_automaton() {
        let compiled = CompiledTaxonomy::new(&taxonomy());
        let clone = compiled.clone();
        assert_eq!(clone.route("github.com/a").0.as_ref(), "repository");
        assert_eq!(clone.pattern_count(), compiled.pattern_count());
    }
}
