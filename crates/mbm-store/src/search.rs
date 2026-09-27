//! search: bm25 over the full-text index, fused with fuzzy scores.
//!
//! three stages, cheapest first.
//!
//! 1. fts5 answers the exact terms and returns a bm25 score. this is the
//!    recall source and it is the only stage that touches the whole corpus.
//! 2. the bigram prefilter answers "which documents could plausibly match,
//!    typos included", in a fraction of a millisecond.
//! 3. neo-frizbee scores just those candidates with simd smith-waterman.
//!
//! the final rank fuses 1 and 3. the previous generation threw away the bm25
//! ordering and let a language model do all the ranking, which made every
//! search cost money and made an offline search impossible.

use crate::prefilter::Prefilter;
use mbm_core::id::Id;
use mbm_core::Result;
use neo_frizbee::{Config, Matcher};
use rusqlite::Connection;
use std::str::FromStr;

/// how much weight bm25 carries against the fuzzy score.
const BM25_WEIGHT: f64 = 0.45;

/// how much the fuzzy score carries.
const FUZZY_WEIGHT: f64 = 0.55;

/// how many candidates the fts stage hands to the fuzzy stage.
const FTS_CANDIDATES: usize = 400;

/// how many results to return.
const DEFAULT_LIMIT: usize = 50;

/// what kind of search to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// bm25 and fuzzy together. the default.
    #[default]
    Hybrid,
    /// bm25 only, for exact queries and for the fast path.
    Exact,
    /// fuzzy only, for when the user is typing and the term is unfinished.
    Fuzzy,
}

/// a scored result before the two stages are fused: id, bm25 rank, fuzzy
/// score, and the character ranges the fuzzy pass matched.
type Scored = (Id, f64, Vec<(usize, usize)>);

/// one scored result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// the bookmark
    pub id: Id,
    /// the fused score, higher is better
    pub score: f64,
    /// the bm25 component
    pub bm25: f64,
    /// the fuzzy component in `[0, 1]`
    pub fuzzy: f64,
    /// character ranges in the searchable text that the fuzzy pass matched
    pub matched: Vec<(usize, usize)>,
}

#[derive(Debug)]
pub struct Searcher<'conn> {
    conn: &'conn Connection,
    prefilter: Option<&'conn Prefilter>,
    /// prefilter position to bookmark id. held so a query never has to run an
    /// `offset` scan per candidate, which turns a 13k-candidate fuzzy pass
    /// into 13k table scans.
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
    /// so they travel together rather than as two arguments a caller could
    /// mix up.
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

        let exact =
            if mode == Mode::Fuzzy { Vec::new() } else { self.exact(query, FTS_CANDIDATES)? };
        let fuzzy = if mode == Mode::Exact { Vec::new() } else { self.fuzzy(query) };

        let mut hits = fuse(exact, fuzzy);
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        Ok(hits)
    }

    /// the bm25 stage. returns ids with their raw bm25 values.
    fn exact(&self, query: &str, limit: usize) -> Result<Vec<(Id, f64)>> {
        let sql = "SELECT rowid, bm25(search) AS rank FROM search \
                   WHERE search MATCH ?1 ORDER BY rank LIMIT ?2";
        let mut stmt = self.conn.prepare(sql).map_err(|e| crate::db::store_err(&e))?;
        let rows = stmt
            .query_map(rusqlite::params![fts_query(query), limit as i64], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, f64>(1)?))
            })
            .map_err(|e| crate::db::store_err(&e))?;

        let mut out = Vec::new();
        for row in rows {
            let (id, rank) = row.map_err(|e| crate::db::store_err(&e))?;
            out.push((Id::from_raw(id), rank));
        }
        Ok(out)
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
        // the prefilter indexes by position, so each row has to be mapped back
        // to the bookmark id it stands for
        let candidates: Vec<(u32, Id)> = rows
            .into_iter()
            .filter_map(|row| self.positions.get(row as usize).map(|&id| (row, id)))
            .collect();

        let texts = self.texts_for(&candidates);

        // the parallel entry point returns scores but not match positions, so
        // it runs first to rank the candidates cheaply, and the positions come
        // from a sequential pass over just the survivors.
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
    let fuzzy_by_id: ahash::AHashMap<u64, (f64, Vec<(usize, usize)>)> =
        fuzzy.iter().map(|(id, score, ranges)| (id.get(), (*score, ranges.clone()))).collect();

    let mut out: Vec<Hit> = exact
        .into_iter()
        .map(|(id, bm25)| {
            let (score, ranges) =
                fuzzy_by_id.get(&id.get()).cloned().unwrap_or((0.0, Vec::new()));
            Hit { id, score: 0.0, bm25, fuzzy: score, matched: ranges }
        })
        .collect();

    for (id, score, ranges) in fuzzy {
        if !out.iter().any(|h| h.id == id) {
            out.push(Hit { id, score: 0.0, bm25: 0.0, fuzzy: score, matched: ranges });
        }
    }

    // bm25 runs negative, with a more negative value meaning a better match.
    // flipping and rescaling against the batch puts both components on one
    // scale, so a two-row result and a two-hundred-row result rank the same.
    let worst = out.iter().map(|h| h.bm25).fold(f64::INFINITY, f64::min);
    let best = out.iter().map(|h| h.bm25).fold(f64::NEG_INFINITY, f64::max);
    let span = (worst - best).abs();
    for hit in &mut out {
        let normalised = if span > f64::EPSILON { (worst - hit.bm25) / span } else { 1.0 };
        hit.score = normalised * BM25_WEIGHT + hit.fuzzy * FUZZY_WEIGHT;
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

/// escape a user query into something fts5 accepts.
///
/// fts5 has its own query syntax where `*`, `"`, `:`, `-`, `(`, `)`, and
/// `^` are all operators. a user typing `rust -lang` would otherwise get a
/// parse error, so every term is stripped to word characters and quoted.
fn fts_query(query: &str) -> String {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|t| t.chars().count() >= 2)
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect();

    if terms.is_empty() {
        // every term was a single character. fts5 can still match those, so
        // fall back to quoting the whole thing.
        return format!("\"{}\"", query.replace('"', ""));
    }
    terms.join(" OR ")
}

/// load the prefilter's document texts, so a searcher can score them.
///
/// the prefilter is indexed by position, and position is the row's rank in
/// this query. rebuilding both together is what keeps the two consistent.
pub fn build_prefilter(
    conn: &Connection,
    text_of: impl Fn(i64) -> String,
) -> Result<(Prefilter, Vec<Id>)> {
    let ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM bookmark ORDER BY id")
            .map_err(|e| crate::db::store_err(&e))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .map_err(|e| crate::db::store_err(&e))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| crate::db::store_err(&e))?
    };

    let mut prefilter = Prefilter::new(ids.len());
    for (position, &id) in ids.iter().enumerate() {
        prefilter.insert(position, &text_of(id));
    }
    Ok((prefilter, ids.into_iter().map(|i| Id::from_raw(i as u64)).collect()))
}

/// parse a source medium name, for callers that stored it as text.
#[must_use]
pub fn parse_medium(raw: &str) -> Option<mbm_core::medium::SourceMedium> {
    mbm_core::medium::SourceMedium::from_str(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{migrate, prefilter::Prefilter};

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
            let id = 1_000 + i as i64;
            conn.execute(
                "INSERT INTO bookmark(id, medium, external_id, ingested_at, title, body, extra)
                 VALUES (?1, 'manual', ?2, ?1, ?3, ?4, ?5)",
                rusqlite::params![id, format!("e{i}"), title, body, extra],
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
        let hits = Searcher::new(&conn).search("   ", Mode::Hybrid, 10).unwrap();
        assert!(hits.is_empty());
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
        let hits = Searcher::new(&conn).search("rust", Mode::Exact, 1).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn a_zero_limit_falls_back_to_the_default() {
        let (conn, _) = seeded();
        let hits = Searcher::new(&conn).search("rust", Mode::Exact, 0).unwrap();
        assert!(hits.len() > 1);
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
        assert!(hits.is_empty(), "fts5 has no typo tolerance, which is why the prefilter exists");
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
    fn scores_stay_within_the_fused_range() {
        let (conn, _) = seeded();
        let (prefilter, ids) = prefiltered(&conn);
        for hit in Searcher::new(&conn)
            .with_prefilter(&prefilter, &ids)
            .search("rust", Mode::Hybrid, 10)
            .unwrap()
        {
            assert!((0.0..=1.0).contains(&hit.fuzzy), "fuzzy {} out of range", hit.fuzzy);
        }
    }

    #[test]
    fn fts_query_quotes_every_term() {
        assert_eq!(fts_query("rust simd"), "\"rust\" OR \"simd\"");
        assert!(fts_query("a").contains('"'), "single characters still need quoting");
    }

    #[test]
    fn build_prefilter_returns_ids_in_row_order() {
        let (conn, _) = seeded();
        let (_, ids) = build_prefilter(&conn, |_| String::new()).unwrap();
        assert_eq!(ids.len(), 5);
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
    }
}
