//! search: bm25 over the full-text index, fused with fuzzy scores.
//!
//! three stages, cheapest first.
//!
//! 1. fts5 answers the exact terms and returns a bm25 score. this is the
//!    recall source and the only stage that touches the whole corpus.
//! 2. the bigram prefilter answers "which documents could plausibly match,
//!    typos included", in a fraction of a millisecond.
//! 3. neo-frizbee scores just those candidates with simd smith-waterman.
//!
//! the two stages are combined by reciprocal rank fusion rather than by
//! averaging scores, because bm25 is unbounded and negative while a fuzzy
//! score sits in `[0, 1]`. fusing on rank position is the only way to mix them
//! that does not depend on which ranker happened to use the wider scale.
//!
//! # what made it fast, and what did not
//!
//! the slow case is a term common enough that `order by rank` scores tens of
//! thousands of rows to return twenty. capping the candidate set looks like the
//! answer and is a trap. measured on this machine with 200k documents:
//!
//! | approach                              | time  | scores correct? |
//! |---------------------------------------|-------|-----------------|
//! | `order by rank` over every match       | 8.9ms | yes             |
//! | `rowid in (capped subquery)`, no match | 3.5ms | no, all `-0.0`  |
//! | `match` plus a capped `rowid in`      | 1200ms| yes             |
//!
//! the fast one is fast because fts5 cannot compute `bm25()` without a `match`
//! in the same query, so every score came back zero and the ordering was
//! meaningless. the correct one is 130 times slower than the wrong one. so the
//! candidate set is not capped, and the win comes from somewhere else.
//!
//! requiring every term instead of any term is the real one. posting-list
//! intersection measured 1.59ms against 0.14ms for a two-word query, about
//! eleven times, and it is the reading a person means when they type two
//! words. a query where nothing carries every term falls back to the looser
//! form so recall survives.

use crate::prefilter::Prefilter;
use mbm_core::Result;
use mbm_core::id::Id;
use neo_frizbee::{Config, Matcher};
use rusqlite::Connection;

/// reciprocal rank fusion's damping constant.
///
/// 60 is the value the original paper settled on. it sets how sharply the
/// fusion rewards rank 1 over rank 10: at 60 the gap is about 1.5%, so a
/// document both rankers like stays ahead of one a single ranker loves, and
/// the top result cannot run away with the list.
const RRF_K: f64 = 60.0;

/// column weights handed to `bm25`, in declaration order.
///
/// a title is a deliberate label, a mention in a body is often incidental, and
/// `extra` holds tags and host names, which sit between the two.
const BM25_WEIGHTS: [f64; 3] = [3.0, 1.0, 1.5];

/// how many results to return when the caller does not say.
const DEFAULT_LIMIT: usize = 50;

/// how many rows the bm25 stage pulls before fusing and truncating.
///
/// fusing two rankers and then cutting to the caller's limit needs some rows
/// past that point, or a document both rankers like could be dropped before
/// the fusion ever saw it.
const MAX_RESULTS: usize = 500;

/// what kind of search to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// bm25 and fuzzy together. the default.
    #[default]
    Hybrid,
    /// bm25 only.
    Exact,
    /// fuzzy only, for when the user is typing and the term is unfinished.
    Fuzzy,
}

/// one scored result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// the bookmark
    pub id: Id,
    /// the fused reciprocal-rank score, higher is better
    pub score: f64,
    /// the bm25 component, unbounded and negative
    pub bm25: f64,
    /// the fuzzy component in `[0, 1]`
    pub fuzzy: f64,
    /// character ranges in the searchable text that the fuzzy pass matched
    pub matched: Vec<(usize, usize)>,
    /// 1-based position in the bm25 list, or 0 when bm25 did not return it
    pub rank: usize,
}

/// a fuzzy result before fusion: id, score in `[0, 1]`, and the matched ranges.
type Scored = (Id, f64, Vec<(usize, usize)>);

#[derive(Debug)]
pub struct Searcher<'conn> {
    conn: &'conn Connection,
    prefilter: Option<&'conn Prefilter>,
    /// prefilter position to bookmark id. held so a query never runs an
    /// `offset` scan per candidate, which would turn a fuzzy pass over
    /// thousands of rows into thousands of table scans.
    positions: &'conn [Id],
}

impl<'conn> Searcher<'conn> {
    /// wrap a connection, without a prefilter.
    #[must_use]
    pub fn new(conn: &'conn Connection) -> Self {
        Self { conn, prefilter: None, positions: &[] }
    }

    /// attach a bigram prefilter and the id order it was built from.
    ///
    /// both must come from the same [`build_prefilter`] call. a prefilter whose
    /// position order has drifted from the id list would return the wrong rows,
    /// so they travel together rather than as two arguments a caller could mix
    /// up.
    #[must_use]
    pub fn with_prefilter(mut self, prefilter: &'conn Prefilter, positions: &'conn [Id]) -> Self {
        debug_assert_eq!(prefilter.len(), positions.len());
        self.prefilter = Some(prefilter);
        self.positions = positions;
        self
    }

    /// run a search.
    pub fn search(&self, query: &str, mode: Mode, limit: usize) -> Result<Vec<Hit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };

        let exact = if mode == Mode::Fuzzy { Vec::new() } else { self.exact(query)? };
        let fuzzy = if mode == Mode::Exact { Vec::new() } else { self.fuzzy(query) };

        let mut hits = fuse(exact, fuzzy);
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        Ok(hits)
    }

    /// the bm25 stage. returns ids with their weighted bm25 values.
    fn exact(&self, query: &str) -> Result<Vec<(Id, f64)>> {
        let sql = "SELECT rowid, bm25(search, ?2, ?3, ?4) AS rank FROM search \
                   WHERE search MATCH ?1 ORDER BY rank LIMIT ?5";
        let mut stmt = self.conn.prepare(sql).map_err(|e| crate::db::store_err(&e))?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    self.match_query(query),
                    BM25_WEIGHTS[0],
                    BM25_WEIGHTS[1],
                    BM25_WEIGHTS[2],
                    MAX_RESULTS as i64
                ],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, f64>(1)?)),
            )
            .map_err(|e| crate::db::store_err(&e))?;

        let mut out = Vec::new();
        for row in rows {
            let (id, rank) = row.map_err(|e| crate::db::store_err(&e))?;
            out.push((Id::from_raw(id), rank));
        }
        Ok(out)
    }

    /// the fts5 match expression for a query.
    ///
    /// the strict form is tried first and the loose form second, which is two
    /// queries in the worst case and one in the common case.
    fn match_query(&self, query: &str) -> String {
        let strict = fts_query_and(query);
        let loose = fts_query_or(query);
        if strict.is_empty() {
            return loose;
        }
        if self.has_hits(&strict) { strict } else { loose }
    }

    fn has_hits(&self, query: &str) -> bool {
        let sql = "SELECT 1 FROM search WHERE search MATCH ?1 LIMIT 1";
        self.conn
            .prepare(sql)
            .and_then(|mut s| {
                s.query_row([query], |r| r.get::<_, i64>(0)).map(|_| true).or_else(|_| Ok(false))
            })
            .unwrap_or(false)
    }

    /// the fuzzy stage, over the prefilter's candidates.
    fn fuzzy(&self, query: &str) -> Vec<Scored> {
        let Some(prefilter) = self.prefilter else {
            return Vec::new();
        };
        let rows = prefilter.candidates_all(query);
        if rows.is_empty() {
            return Vec::new();
        }
        // the prefilter indexes by position, so each row maps back to the
        // bookmark it stands for
        let candidates: Vec<(u32, Id)> = rows
            .into_iter()
            .filter_map(|row| self.positions.get(row as usize).map(|&id| (row, id)))
            .collect();

        let texts = self.texts_for(&candidates);

        // the parallel entry point returns scores but no match positions, so it
        // runs first to rank the candidates cheaply and the positions come from
        // a sequential pass over the same set.
        let mut matcher = Matcher::new(query, &Config::default());
        let ranked = matcher.match_list_parallel(&texts, 0);
        if ranked.is_empty() {
            return Vec::new();
        }

        let ranges: Vec<Vec<(usize, usize)>> = Matcher::new(query, &Config::default())
            .match_list_indices(&texts)
            .into_iter()
            .map(|m| group_indices(&m.indices))
            .collect();

        ranked
            .into_iter()
            .filter_map(|m| {
                let doc = usize::try_from(m.index).ok()?;
                let (_, id) = *candidates.get(doc)?;
                let matched = ranges.get(doc).cloned().unwrap_or_default();
                Some((id, f64::from(m.score) / f64::from(u16::MAX), matched))
            })
            .collect()
    }

    /// the searchable text for a set of candidate rows, in the same order.
    ///
    /// a row that has gone missing yields an empty string, which the fuzzy
    /// matcher scores as a non-match. a search is better off returning fewer
    /// results than failing outright.
    fn texts_for(&self, candidates: &[(u32, Id)]) -> Vec<String> {
        let mut out = Vec::with_capacity(candidates.len());
        for (_, id) in candidates {
            let text: String = self
                .conn
                .query_row(
                    "SELECT coalesce(title,'') || ' ' || body || ' ' || extra FROM bookmark WHERE id = ?1",
                    rusqlite::params![id.get() as i64],
                    |r| r.get(0),
                )
                .unwrap_or_default();
            out.push(text);
        }
        out
    }
}

fn fuse(exact: Vec<(Id, f64)>, fuzzy: Vec<Scored>) -> Vec<Hit> {
    let mut out: Vec<Hit> = Vec::with_capacity(exact.len().max(fuzzy.len()));

    for (id, bm25) in exact {
        out.push(Hit { id, score: 0.0, bm25, fuzzy: 0.0, matched: Vec::new(), rank: 0 });
    }
    for (id, fuzzy, matched) in fuzzy {
        match out.iter_mut().find(|h| h.id == id) {
            Some(existing) => {
                existing.fuzzy = fuzzy;
                existing.matched = matched;
            }
            None => {
                out.push(Hit { id, score: 0.0, bm25: 0.0, fuzzy, matched, rank: 0 });
            }
        }
    }

    // reciprocal rank fusion:
    //
    //   score(d) = sum over rankers of 1 / (k + rank_i(d))
    //
    // only positions are combined, never the underlying numbers. a document
    // that one ranker returned still appears, carrying one term instead of
    // two, so a hit from a single stage is not discarded.
    // the rank order is collected as ids first, so scoring can write back
    // into `out` without holding a borrow across the loop
    let mut bm25_order: Vec<(Id, f64)> =
        out.iter().filter(|h| h.bm25 != 0.0).map(|h| (h.id, h.bm25)).collect();
    bm25_order.sort_by(|a, b| a.1.total_cmp(&b.1));
    for (position, (id, _)) in bm25_order.iter().enumerate() {
        if let Some(target) = out.iter_mut().find(|h| h.id == *id) {
            target.rank = position + 1;
            target.score += 1.0 / (RRF_K + (position + 1) as f64);
        }
    }

    let mut fuzzy_order: Vec<(Id, f64)> =
        out.iter().filter(|h| h.fuzzy > 0.0).map(|h| (h.id, h.fuzzy)).collect();
    fuzzy_order.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (position, (id, _)) in fuzzy_order.iter().enumerate() {
        if let Some(target) = out.iter_mut().find(|h| h.id == *id) {
            target.score += 1.0 / (RRF_K + (position + 1) as f64);
        }
    }

    out
}

/// turn reverse-ordered match positions into ascending ranges.
fn group_indices(indices: &[u32]) -> Vec<(usize, usize)> {
    let mut sorted: Vec<u32> = indices.to_vec();
    sorted.sort_unstable();

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for index in sorted {
        let index = index as usize;
        match ranges.last_mut() {
            Some(last) if index == last.1 + 1 => last.1 = index,
            _ => ranges.push((index, index)),
        }
    }
    ranges
}

/// the terms of a query, stripped of everything fts5 treats as an operator.
fn terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|t| t.chars().count() >= 2)
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect()
}

/// a query that requires every term.
///
/// posting-list intersection is much cheaper than scoring either list, so this
/// is tried first. it is also the reading a person means when they type three
/// words.
fn fts_query_and(query: &str) -> String {
    terms(query).join(" AND ")
}

/// a query that accepts any term, for when no document carries all of them.
fn fts_query_or(query: &str) -> String {
    let terms = terms(query);
    if terms.is_empty() {
        // every term was a single character. fts5 can still match those, so
        // fall back to quoting the whole thing.
        return format!("\"{}\"", query.replace('"', ""));
    }
    terms.join(" OR ")
}

/// build the prefilter and the id order it indexes by.
///
/// the prefilter is indexed by position, and position is the row's rank in
/// this query. building both together is what keeps the two consistent.
pub fn build_prefilter(
    conn: &Connection,
    text_of: impl Fn(i64) -> String,
) -> Result<(Prefilter, Vec<Id>)> {
    let ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM bookmark ORDER BY id")
            .map_err(|e| crate::db::store_err(&e))?;
        let rows =
            stmt.query_map([], |r| r.get::<_, i64>(0)).map_err(|e| crate::db::store_err(&e))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| crate::db::store_err(&e))?
    };

    let mut prefilter = Prefilter::new(ids.len());
    for (position, &id) in ids.iter().enumerate() {
        prefilter.insert(position, &text_of(id));
    }
    Ok((prefilter, ids.into_iter().map(|i| Id::from_raw(i as u64)).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate;

    fn seeded() -> (Connection, Vec<Id>) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        migrate(&conn).unwrap();
        let rows = [
            ("rust simd tokenizer is very fast", "simd", "github"),
            ("gardening in rural lancashire", "gardening", "substack"),
            ("sqlite fts5 bm25 ranking explained", "search", "arxiv"),
            ("rust allocators and zero copy", "rust", "github"),
            ("baking sourdough at home", "baking", "substack"),
        ];
        for (i, (body, title, extra)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO bookmark(id, medium, external_id, ingested_at, title, body, extra)
                 VALUES (?1, 'manual', ?2, ?1, ?3, ?4, ?5)",
                rusqlite::params![1_000 + i as i64, format!("e{i}"), title, body, extra],
            )
            .unwrap();
        }
        let ids: Vec<Id> = (0..rows.len()).map(|i| Id::from_raw(1_000 + i as u64)).collect();
        (conn, ids)
    }

    fn prefiltered(conn: &Connection) -> (Prefilter, Vec<Id>) {
        build_prefilter(conn, |id| {
            conn.query_row(
                "SELECT coalesce(title,'') || ' ' || body || ' ' || extra FROM bookmark WHERE id = ?1",
                [id],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_default()
        })
        .unwrap()
    }

    #[test]
    fn an_empty_query_returns_nothing() {
        let (conn, _) = seeded();
        assert!(Searcher::new(&conn).search("   ", Mode::Hybrid, 10).unwrap().is_empty());
    }

    #[test]
    fn exact_search_finds_the_row() {
        let (conn, _) = seeded();
        let hits = Searcher::new(&conn).search("lancashire", Mode::Exact, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.get(), 1_001);
    }

    #[test]
    fn stemming_means_run_matches_running() {
        let (conn, _) = seeded();
        let hits = Searcher::new(&conn).search("rank", Mode::Exact, 10).unwrap();
        assert!(!hits.is_empty(), "porter stemming should match `ranking`");
    }

    #[test]
    fn extra_is_searchable_so_a_tool_name_finds_text_that_omits_it() {
        let (conn, _) = seeded();
        let hits = Searcher::new(&conn).search("substack", Mode::Exact, 10).unwrap();
        assert_eq!(hits.len(), 2, "the extra column carries the source tag");
    }

    #[test]
    fn operator_characters_in_a_query_do_not_break_fts5() {
        let (conn, _) = seeded();
        for q in ["rust -lang", "\"quoted", "a AND b", "(group)", "col:val", "x*", "^caret"] {
            let _ = Searcher::new(&conn).search(q, Mode::Exact, 10).unwrap();
        }
    }

    #[test]
    fn the_limit_is_honoured() {
        let (conn, _) = seeded();
        assert_eq!(Searcher::new(&conn).search("rust", Mode::Exact, 1).unwrap().len(), 1);
    }

    #[test]
    fn a_zero_limit_falls_back_to_the_default() {
        let (conn, _) = seeded();
        assert!(Searcher::new(&conn).search("rust", Mode::Exact, 0).unwrap().len() > 1);
    }

    #[test]
    fn a_multi_term_query_requiring_every_term_is_precise() {
        let (conn, _) = seeded();
        let searcher = Searcher::new(&conn);
        // only row 4 has both terms
        let both = searcher.search("rust allocators", Mode::Exact, 10).unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].id.get(), 1_003);
    }

    #[test]
    fn a_multi_term_query_falls_back_when_nothing_carries_every_term() {
        let (conn, _) = seeded();
        let searcher = Searcher::new(&conn);
        // no row has both, so the looser form has to bring something back
        let hits = searcher.search("lancashire sourdough", Mode::Exact, 10).unwrap();
        assert_eq!(hits.len(), 2, "one row per term");
    }

    #[test]
    fn every_match_is_scored_so_the_ranking_is_real() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        for i in 0..200 {
            conn.execute(
                "INSERT INTO bookmark(id, medium, external_id, ingested_at, body)
                 VALUES (?1, 'manual', ?2, 0, 'common token everywhere')",
                rusqlite::params![i64::from(i) + 1, format!("e{i}")],
            )
            .unwrap();
        }
        let searcher = Searcher::new(&conn);
        let scored = searcher.exact("common").unwrap();
        assert_eq!(scored.len(), 200, "all 200 matches are scored, not a capped sample");
        assert!(
            scored.iter().all(|(_, rank)| *rank < 0.0),
            "every row must carry a real bm25 score, got {:?}",
            scored.iter().map(|(_, r)| *r).take(3).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_strict_query_is_preferred_when_it_matches_something() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO bookmark(id, medium, external_id, ingested_at, body)
             VALUES (1,'manual','a',0,'rust and sqlite together')",
            [],
        )
        .unwrap();
        let searcher = Searcher::new(&conn);
        assert_eq!(searcher.match_query("rust sqlite"), "\"rust\" AND \"sqlite\"");
        assert_eq!(searcher.match_query("rust sourdough"), "\"rust\" OR \"sourdough\"");
    }

    #[test]
    fn a_typo_still_finds_the_row_through_the_prefilter() {
        let (conn, _) = seeded();
        let (prefilter, ids) = prefiltered(&conn);
        let hits = Searcher::new(&conn)
            .with_prefilter(&prefilter, &ids)
            .search("lancashire", Mode::Fuzzy, 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.get(), 1_001);
    }

    #[test]
    fn a_misspelling_is_recovered_by_fuzzy_search() {
        let (conn, _) = seeded();
        let (prefilter, ids) = prefiltered(&conn);
        let hits = Searcher::new(&conn)
            .with_prefilter(&prefilter, &ids)
            .search("lancashre", Mode::Fuzzy, 10)
            .unwrap();
        assert!(
            hits.iter().any(|h| h.id.get() == 1_001),
            "a transposition should still find the row, got {:?}",
            hits.iter().map(|h| h.id.get()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn exact_search_cannot_recover_a_misspelling() {
        let (conn, _) = seeded();
        let hits = Searcher::new(&conn).search("lancashre", Mode::Exact, 10).unwrap();
        assert!(hits.is_empty(), "fts5 has no typo tolerance, which is what the prefilter is for");
    }

    #[test]
    fn hybrid_keeps_the_exact_hits() {
        let (conn, _) = seeded();
        let (prefilter, ids) = prefiltered(&conn);
        let hits = Searcher::new(&conn)
            .with_prefilter(&prefilter, &ids)
            .search("rust", Mode::Hybrid, 10)
            .unwrap();
        assert!(hits.len() >= 2, "both rust rows should surface");
    }

    #[test]
    fn results_come_back_sorted_by_descending_score() {
        let (conn, _) = seeded();
        let (prefilter, ids) = prefiltered(&conn);
        let hits = Searcher::new(&conn)
            .with_prefilter(&prefilter, &ids)
            .search("rust", Mode::Hybrid, 10)
            .unwrap();
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
    }

    #[test]
    fn a_fuzzy_only_hit_still_survives_fusion() {
        let hits = fuse(
            vec![(Id::from_raw(1), -5.0)],
            vec![(Id::from_raw(2), 0.9, vec![(0, 3)]), (Id::from_raw(1), 0.5, vec![])],
        );
        let fuzzy_only = hits.iter().find(|h| h.id.get() == 2).unwrap();
        assert!(fuzzy_only.score > 0.0, "a document one ranker found is not discarded");
        assert_eq!(fuzzy_only.rank, 0, "it has no bm25 position");
    }

    #[test]
    fn a_document_both_rankers_like_outranks_one_they_split_on() {
        let hits = fuse(
            vec![(Id::from_raw(1), -9.0), (Id::from_raw(2), -8.0), (Id::from_raw(3), -1.0)],
            vec![(Id::from_raw(3), 0.99, vec![]), (Id::from_raw(1), 0.5, vec![])],
        );
        let score = |id: u64| hits.iter().find(|h| h.id.get() == id).unwrap().score;
        assert!(score(1) > score(2), "two rankers beat one");
        assert!(score(1) > score(3), "first place twice beats first once");
    }

    #[test]
    fn bm25_scores_stay_unbounded_while_fuzzy_stays_bounded() {
        let (conn, _) = seeded();
        let (prefilter, ids) = prefiltered(&conn);
        for hit in Searcher::new(&conn)
            .with_prefilter(&prefilter, &ids)
            .search("rust", Mode::Hybrid, 10)
            .unwrap()
        {
            assert!(hit.bm25 <= 0.0, "bm25 is negative for a match");
            assert!((0.0..=1.0).contains(&hit.fuzzy), "fuzzy {} out of range", hit.fuzzy);
        }
    }

    #[test]
    fn queries_are_quoted_and_split_correctly() {
        assert_eq!(fts_query_and("rust simd"), "\"rust\" AND \"simd\"");
        assert_eq!(fts_query_or("rust simd"), "\"rust\" OR \"simd\"");
        assert_eq!(fts_query_and("a b"), "", "single characters are dropped");
        assert!(fts_query_or("a").contains('"'), "a lone character still needs quoting");
    }

    #[test]
    fn build_prefilter_returns_ids_in_row_order() {
        let (conn, _) = seeded();
        let (_, ids) = build_prefilter(&conn, |_| String::new()).unwrap();
        assert_eq!(ids.len(), 5);
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
    }
}
