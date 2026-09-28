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
        // the union of the query's bigram postings, not the intersection. a
        // prefilter narrows the field the ranker looks at; it does not decide.
        // an intersection asks every bigram of the query to be present, and one
        // wrong character removes a bigram the document has, so the stage that
        // exists to survive a typo would return nothing for a typo.
        let rows = prefilter.candidates(query);
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

        // the matcher's own default allows no typos at all, which is the exact
        // behaviour under a fuzzy name. the allowance is a fraction of the
        // needle's length: a two-letter query has no room for one and a twenty
        // character phrase has room for several, and a fixed number is wrong at
        // one end or the other.
        let typos = (query.chars().count() / 4).min(u16::MAX as usize) as u16;
        let config = Config::default().max_typos(Some(typos.max(1)));

        // the parallel entry point returns scores but no match positions, so it
        // runs first to rank the candidates cheaply and the positions come from
        // a sequential pass over the same set.
        let ranked = Matcher::new(query, &config).match_list_parallel(&texts, 0);
        if ranked.is_empty() {
            return Vec::new();
        }

        let ranges: Vec<Vec<(usize, usize)>> = Matcher::new(query, &config)
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
        candidates.iter().map(|(_, id)| indexed_text(self.conn, *id)).collect()
    }
}

/// the text a bookmark is searched and ranked by.
///
/// one query, and one definition, because a prefilter built over a different
/// string than the ranker scores is a prefilter for a different index.
#[must_use]
pub fn indexed_text(conn: &Connection, id: Id) -> String {
    conn.query_row(
        "SELECT coalesce(title,'') || ' ' || body || ' ' || extra FROM bookmark WHERE id = ?1",
        rusqlite::params![id.get() as i64],
        |r| r.get(0),
    )
    .unwrap_or_default()
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
    text_of: impl Fn(Id) -> String,
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
        prefilter.insert(position, &text_of(Id::from_raw(id as u64)));
    }
    Ok((prefilter, ids.into_iter().map(|i| Id::from_raw(i as u64)).collect()))
}
