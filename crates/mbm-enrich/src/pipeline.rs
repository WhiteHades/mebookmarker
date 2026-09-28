//! the pipeline: run the stages over what is waiting, in the cheap order.
//!
//! every stage keeps its own `NULL`-then-timestamp column on the bookmark row.
//! that single design choice is what makes the whole thing resumable: a run
//! that dies half way through leaves the rows it finished stamped and the rows
//! it did not untouched, and the next run picks up exactly where it stopped.
//! there is no queue to lose, no checkpoint file to go stale, and no separate
//! state to reconcile against the data.
//!
//! it also means a stage can be re-run on purpose. clearing a column puts
//! every bookmark back in that stage's queue, which is how a new taxonomy or a
//! new model gets applied to an archive that already exists.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mbm_core::bookmark::{Bookmark, CategoryAssignment, Link};
use mbm_core::error::{Error, Result};
use mbm_core::id::Id;
use mbm_core::port::{EnrichStage, Enricher};
use mbm_store::{Filter, Repo};
use rusqlite::Connection;
use rusqlite::params;

use crate::entities::Entities;
use crate::tags::{Categorizer, Tagger};
use crate::vision::Vision;

/// the column a stage stamps.
#[must_use]
pub fn column_for(stage: EnrichStage) -> &'static str {
    match stage {
        EnrichStage::Entities => "entities_at",
        EnrichStage::Vision => "vision_at",
        EnrichStage::Tags => "tagged_at",
        EnrichStage::Categorize => "categorized_at",
        EnrichStage::Describe => "described_at",
    }
}

/// what one stage did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageReport {
    /// how many rows the stage saw.
    pub seen: usize,
    /// how many it changed and stamped.
    pub done: usize,
    /// how many it left for a later run.
    pub failed: usize,
    /// wall time for the stage.
    pub elapsed: Duration,
}

/// what a whole run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// one entry per stage, in the order they ran.
    pub stages: Vec<(EnrichStage, StageReport)>,
}

impl Report {
    /// the total number of rows stamped.
    #[must_use]
    pub fn total_done(&self) -> usize {
        self.stages.iter().map(|(_, r)| r.done).sum()
    }

    /// the total number of failures.
    #[must_use]
    pub fn total_failed(&self) -> usize {
        self.stages.iter().map(|(_, r)| r.failed).sum()
    }
}

/// what one run is made of.
#[derive(Clone)]
pub struct Plan {
    stages: Vec<Arc<dyn Enricher>>,
    /// how many rows to take per page.
    page: usize,
    /// stop after this many rows in a stage, or `None` for no limit.
    limit: Option<usize>,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plan")
            .field("stages", &self.stages.iter().map(|s| s.stage()).collect::<Vec<_>>())
            .field("page", &self.page)
            .field("limit", &self.limit)
            .finish()
    }
}

impl Plan {
    /// build a plan from a list of stages, sorted into the cheap order.
    #[must_use]
    pub fn new(stages: Vec<Arc<dyn Enricher>>) -> Self {
        let mut stages = stages;
        stages.sort_by_key(|s| s.stage().order());
        Self { stages, page: 256, limit: None }
    }

    /// the ordinary plan: every stage, cheapest first.
    ///
    /// the free one always runs, because everything downstream reads the links
    /// it finds and a fingerprint.
    pub fn full(
        jev_key: Option<String>,
        vocabulary: Vec<String>,
        taxonomy: &mbm_core::category::Taxonomy,
    ) -> Result<Self> {
        Ok(Self::new(vec![
            Arc::new(Entities::new()),
            Arc::new(Vision::new(jev_key.clone())?),
            Arc::new(Tagger::new(jev_key.clone(), vocabulary)?),
            Arc::new(Categorizer::new(jev_key, taxonomy)?),
        ]))
    }

    /// the stages this plan will run.
    #[must_use]
    pub fn stages(&self) -> Vec<EnrichStage> {
        self.stages.iter().map(|s| s.stage()).collect()
    }

    /// add one stage.
    #[must_use]
    pub fn with(mut self, stage: Arc<dyn Enricher>) -> Self {
        self.stages.push(stage);
        self.stages.sort_by_key(|s| s.stage().order());
        self
    }

    /// change the page size.
    #[must_use]
    pub fn with_page(mut self, page: usize) -> Self {
        self.page = page.max(1);
        self
    }

    /// stop after this many rows in each stage.
    #[must_use]
    pub fn with_limit(mut self, limit: Option<usize>) -> Self {
        self.limit = limit;
        self
    }

    /// drop one stage from the plan.
    #[must_use]
    pub fn without(self, stage: EnrichStage) -> Self {
        Self {
            stages: self.stages.into_iter().filter(|s| s.stage() != stage).collect(),
            page: self.page,
            limit: self.limit,
        }
    }

    /// keep only the named stages.
    #[must_use]
    pub fn only(self, wanted: &[EnrichStage]) -> Self {
        Self {
            stages: self.stages.into_iter().filter(|s| wanted.contains(&s.stage())).collect(),
            page: self.page,
            limit: self.limit,
        }
    }
}

impl Default for Plan {
    /// the plan that runs the free stage and nothing else.
    ///
    /// this is what a caller gets when it cannot build the remote stages, which
    /// is what happens with no gateway key. the free stage needs nothing, so it
    /// is the one that always runs.
    fn default() -> Self {
        Self::new(vec![Arc::new(Entities::new())])
    }
}

/// run a plan over everything waiting, in one transaction per page.
pub async fn run(conn: &Connection, plan: &Plan) -> Result<Report> {
    let repo = Repo::new(conn);
    let mut report = Report::default();

    for enricher in &plan.stages {
        let stage = enricher.stage();
        if !enricher.is_enabled() {
            tracing::debug!(stage = %stage, "skipped: disabled");
            continue;
        }
        let column = column_for(stage);
        let started = Instant::now();
        let mut stage_report = StageReport::default();

        // keyset pagination, so a row inserted mid-run is picked up on the next
        // run rather than skipped, and a row already read is never read twice
        let mut after: Option<Id> = None;
        loop {
            if let Some(limit) = plan.limit
                && stage_report.seen >= limit
            {
                break;
            }

            let page_size =
                plan.limit.map_or(plan.page, |limit| plan.page.min(limit - stage_report.seen));
            let ids = repo.pending_ids(column, after, page_size)?;
            if ids.is_empty() {
                break;
            }
            after = ids.last().copied();
            stage_report.seen += ids.len();

            let tx = conn.unchecked_transaction().map_err(|e| store(&e))?;
            let mut done = Vec::with_capacity(ids.len());
            for id in &ids {
                let mut bookmark: Bookmark = match tx_rows(&tx, *id) {
                    Ok(Some(b)) => b,
                    // a row that vanished between the id scan and here is
                    // someone else's delete, not our failure
                    Ok(None) => continue,
                    Err(_) => {
                        stage_report.failed += 1;
                        continue;
                    }
                };
                match enricher.enrich(&mut bookmark).await {
                    Ok(()) => {
                        if write_back(&tx, *id, &bookmark, stage).is_ok() {
                            done.push(*id);
                        } else {
                            stage_report.failed += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%id, stage = %stage, error = %e, "stage failed");
                        stage_report.failed += 1;
                    }
                }
            }
            let now = now_ms();
            for id in &done {
                if tx
                    .execute(
                        &format!("UPDATE bookmark SET {column} = ?2 WHERE id = ?1"),
                        rusqlite::params![id.get() as i64, now],
                    )
                    .is_err()
                {
                    stage_report.failed += 1;
                }
            }
            if tx.commit().is_err() {
                stage_report.failed += done.len();
            } else {
                stage_report.done += done.len();
            }
        }

        stage_report.elapsed = started.elapsed();
        tracing::info!(
            stage = %stage,
            seen = stage_report.seen,
            done = stage_report.done,
            failed = stage_report.failed,
            ms = stage_report.elapsed.as_millis(),
            "stage finished"
        );
        report.stages.push((stage, stage_report));
    }

    Ok(report)
}

/// read one bookmark inside the page transaction.
///
/// [`Repo::load`] already fills the satellites, so there is no second read.
fn tx_rows(tx: &rusqlite::Transaction<'_>, id: Id) -> Result<Option<Bookmark>> {
    Repo::new(tx).load(id)
}

/// the text the full-text index reads alongside the body.
///
/// the body is what the author wrote. this is the rest of the searchable shape
/// of the item: the people involved, the tags, and the hosts it points at.
/// without it a search for `@simonw` or `arxiv.org` would miss every post
/// whose body happens not to mention either.
#[must_use]
pub fn indexed_text(bookmark: &Bookmark) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(bookmark.tags.len() + 4);
    if let Some(handle) = &bookmark.author {
        parts.push(format!("@{}", handle.handle));
        if let Some(name) = &handle.name {
            parts.push(name.clone());
        }
    }
    for link in &bookmark.links {
        if let Some(host) = link.resolved.host_str() {
            parts.push(host.to_owned());
        }
        if let Some(title) = &link.title {
            parts.push(title.clone());
        }
    }
    parts.extend(bookmark.tags.iter().cloned());
    parts.join(" ")
}

/// write one enriched bookmark back, in the shape its stage owns.
///
/// every stage that produces something writes it here. a stage whose output is
/// dropped is a stage that ran, cost money, and changed nothing, which is the
/// one failure this design cannot have.
fn write_back(
    tx: &rusqlite::Transaction<'_>,
    id: Id,
    bookmark: &Bookmark,
    stage: EnrichStage,
) -> Result<()> {
    let repo = Repo::new(tx);

    // the tags belong to whichever stage last touched them, and both the entity
    // and the tag stage add to the same set
    if matches!(stage, EnrichStage::Entities | EnrichStage::Tags) {
        write_tags(tx, id, &bookmark.tags)?;
        // the index reads `extra` for what the body does not spell out: who
        // wrote it, what it is tagged, where it came from
        repo.set_indexed_text(id, bookmark.title.as_deref(), &indexed_text(bookmark))?;
    }

    if stage == EnrichStage::Entities {
        // the links are the whole point of this stage. a stage that finds them
        // and does not write them has run, cost nothing, and changed nothing.
        write_links(tx, id, &bookmark.links)?;
        if let Some(fingerprint) = bookmark.fingerprint {
            repo.set_fingerprint(id, fingerprint)?;
        } else {
            tracing::warn!(%id, "the entity stage produced no fingerprint");
        }
    }

    if stage == EnrichStage::Categorize && !bookmark.categories.is_empty() {
        write_categories(tx, id, &bookmark.categories)?;
    }

    if stage == EnrichStage::Describe {
        repo.set_described(id, bookmark.title.as_deref(), summary_of(bookmark).as_deref())?;
    }

    Ok(())
}

/// replace a bookmark's links.
///
/// the ordinals are the order they appear in the text, which is the order a
/// person reading the post met them, and the order every sink writes them in.
fn write_links(tx: &rusqlite::Transaction<'_>, id: Id, links: &[Link]) -> Result<()> {
    tx.execute("DELETE FROM link WHERE bookmark = ?1", params![id.get() as i64])
        .map_err(|e| store(&e))?;
    for (ordinal, link) in links.iter().enumerate() {
        tx.execute(
            "INSERT INTO link(bookmark, ordinal, original, resolved, kind, title, body, summary, \
             blocked) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id.get() as i64,
                ordinal as i64,
                link.original.as_str(),
                link.resolved.as_str(),
                link.kind.name(),
                link.title,
                link.body,
                link.summary,
                link.blocked.map(|b| b.name().to_owned()),
            ],
        )
        .map_err(|e| store(&e))?;
    }
    Ok(())
}

/// replace a bookmark's tags.
///
/// written through the page transaction rather than through `Repo::set_tags`,
/// which opens its own and sqlite refuses a nested `begin`.
fn write_tags(tx: &rusqlite::Transaction<'_>, id: Id, tags: &BTreeSet<String>) -> Result<()> {
    tx.execute("DELETE FROM tag WHERE bookmark = ?1", params![id.get() as i64])
        .map_err(|e| store(&e))?;
    for tag in tags {
        tx.execute(
            "INSERT OR IGNORE INTO tag(bookmark, tag) VALUES (?1, ?2)",
            params![id.get() as i64, tag],
        )
        .map_err(|e| store(&e))?;
    }
    Ok(())
}

/// replace a bookmark's category assignments.
///
/// the catalogue rows are upserted first, so a taxonomy that gained a category
/// works against an archive that predates it, and the assignment points at a row
/// that exists.
fn write_categories(
    tx: &rusqlite::Transaction<'_>,
    id: Id,
    assignments: &[CategoryAssignment],
) -> Result<()> {
    for assignment in assignments {
        tx.execute(
            "INSERT INTO category(slug, name, color, description, action)
             VALUES (?1, ?1, '', '', 'capture')
             ON CONFLICT(slug) DO NOTHING",
            params![assignment.slug],
        )
        .map_err(|e| store(&e))?;
        let category: i64 = tx
            .query_row("SELECT id FROM category WHERE slug = ?1", params![assignment.slug], |r| {
                r.get(0)
            })
            .map_err(|e| store(&e))?;
        tx.execute(
            "INSERT OR REPLACE INTO bookmark_category(bookmark, category, confidence, assigned_by)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                id.get() as i64,
                category,
                assignment.confidence,
                assignment.assigned_by.name()
            ],
        )
        .map_err(|e| store(&e))?;
    }
    Ok(())
}

/// put every bookmark back in one stage's queue.
///
/// the way a taxonomy change is applied to an archive that already exists:
/// clear the column, run the stage, and every row goes through it again.
pub fn requeue(conn: &Connection, stage: EnrichStage) -> Result<usize> {
    let column = column_for(stage);
    conn.execute(&format!("UPDATE bookmark SET {column} = NULL"), []).map_err(|e| store(&e))
}

/// how many rows each stage is holding up, for the tui's status line.
#[must_use]
pub fn backlog(conn: &Connection) -> Vec<(EnrichStage, usize)> {
    let repo = Repo::new(conn);
    EnrichStage::ALL
        .iter()
        .map(|stage| {
            let count = repo.pending(column_for(*stage)).unwrap_or(0);
            (*stage, count)
        })
        .collect()
}

/// the total number of bookmarks.
#[must_use]
pub fn total(conn: &Connection) -> usize {
    Repo::new(conn).count().unwrap_or(0)
}

/// a one-line description of what is waiting, for a log.
#[must_use]
pub fn backlog_line(conn: &Connection) -> String {
    let counts = backlog(conn);
    let waiting: usize = counts.iter().map(|(_, n)| n).sum();
    let total = total(conn);
    let parts = counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(stage, n)| format!("{stage} {n}"))
        .collect::<Vec<_>>();
    if parts.is_empty() {
        format!("{total} bookmarks, all stages clear")
    } else {
        format!("{total} bookmarks, {waiting} waiting: {}", parts.join(", "))
    }
}

/// the rows a stage would take, for a caller that wants to drive the work itself.
pub fn next_batch(conn: &Connection, stage: EnrichStage, limit: usize) -> Result<Vec<Bookmark>> {
    let repo = Repo::new(conn);
    let ids = repo.pending_ids(column_for(stage), None, limit)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(mut b) = repo.load(id)? {
            repo.load_satellites(&mut b)?;
            out.push(b);
        }
    }
    Ok(out)
}

/// the categories and tags a store holds, for the tui.
pub fn facets(conn: &Connection) -> Result<(Vec<(String, usize)>, usize)> {
    let repo = Repo::new(conn);
    Ok((repo.tags_with_counts(200)?, repo.count_matching(&Filter::default())?))
}

/// the summary the describe stage produced.
///
/// the generated summary lives in the first link's summary field when the stage
/// filled one in, and otherwise is the opening of the text, which is the best
/// available answer for a short post that needs no summary at all.
fn summary_of(bookmark: &Bookmark) -> Option<String> {
    if let Some(generated) =
        bookmark.links.iter().find_map(|l| l.summary.as_deref()).filter(|s| !s.trim().is_empty())
    {
        return Some(generated.to_owned());
    }
    // the opening of the text is the best summary a post that needs none of
    // its own can have
    let first = bookmark.text.lines().find(|l| !l.trim().is_empty())?.trim();
    if first.is_empty() {
        return None;
    }
    Some(if first.chars().count() > 280 {
        let cut: String = first.chars().take(277).collect();
        format!("{cut}...")
    } else {
        first.to_owned()
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn store(e: &rusqlite::Error) -> Error {
    Error::Store(e.to_string())
}
