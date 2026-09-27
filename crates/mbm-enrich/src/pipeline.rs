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

use mbm_core::bookmark::{Bookmark, CategoryAssignment};
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
        // a fingerprint is the whole point of this stage, so its absence is
        // worth a warning rather than a silent success
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

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::bookmark::SourceRef;
    use mbm_core::id::Id;
    use mbm_core::medium::SourceMedium;
    use mbm_core::port::FetchPage;

    /// an enricher that records which bookmarks it saw, and can be told to fail.
    struct Recorder {
        stage: EnrichStage,
        fail_on: Vec<String>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl Recorder {
        fn new(stage: EnrichStage) -> Self {
            Self { stage, fail_on: Vec::new(), seen: std::sync::Mutex::new(Vec::new()) }
        }

        fn failing_on(ids: &[&str]) -> Self {
            Self {
                stage: EnrichStage::Entities,
                fail_on: ids.iter().map(|s| (*s).to_owned()).collect(),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn count(&self) -> usize {
            self.seen.lock().map_or(0, |s| s.len())
        }
    }

    #[async_trait::async_trait]
    impl Enricher for Recorder {
        fn stage(&self) -> EnrichStage {
            self.stage
        }

        async fn enrich(&self, bookmark: &mut Bookmark) -> Result<()> {
            self.seen.lock().map(|mut s| s.push(bookmark.source.external_id.clone())).ok();
            if self.fail_on.contains(&bookmark.source.external_id) {
                return Err(Error::Pipeline("asked to fail".to_owned()));
            }
            Ok(())
        }
    }

    fn open() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("t.db")).unwrap();
        mbm_store::migrate(&conn).unwrap();
        (dir, conn)
    }

    fn seed(conn: &Connection, count: usize) {
        let repo = Repo::new(conn);
        for i in 0..count {
            let mut b = Bookmark::new(
                SourceRef::new(SourceMedium::X, format!("{i}"), None),
                format!("post number {i}"),
                0,
            );
            b.id = Id::from_parts(1_700_000_000_000 + i as u64, i as u16);
            repo.insert(&b).unwrap();
        }
    }

    #[tokio::test]
    async fn a_stage_runs_over_everything_and_stamps_it() {
        let (_dir, conn) = open();
        seed(&conn, 5);
        let plan = Plan::new(vec![Arc::new(Recorder::new(EnrichStage::Entities))]);
        let report = run(&conn, &plan).await.unwrap();
        assert_eq!(report.total_done(), 5, "report: {report:?}");
        assert_eq!(Repo::new(&conn).pending("entities_at").unwrap(), 0);
    }

    #[tokio::test]
    async fn a_second_run_has_nothing_to_do() {
        let (_dir, conn) = open();
        seed(&conn, 3);
        let stage = Arc::new(Recorder::new(EnrichStage::Entities));
        let plan = Plan::new(vec![stage.clone()]);
        run(&conn, &plan).await.unwrap();
        let second = run(&conn, &plan).await.unwrap();
        assert_eq!(second.total_done(), 0);
        assert_eq!(stage.count(), 3, "a stamped row is not read again");
    }

    /// a stage that adds one tag to whatever it is given.
    #[derive(Debug)]
    struct Tagger {
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Enricher for Tagger {
        fn stage(&self) -> EnrichStage {
            EnrichStage::Tags
        }

        async fn enrich(&self, bookmark: &mut Bookmark) -> Result<()> {
            self.seen.lock().map(|mut s| s.push(bookmark.source.external_id.clone())).ok();
            bookmark.push_tag("added-by-the-stage");
            Ok(())
        }
    }

    /// a stage that files one bookmark under a category.
    #[derive(Debug)]
    struct Filer;

    #[async_trait::async_trait]
    impl Enricher for Filer {
        fn stage(&self) -> EnrichStage {
            EnrichStage::Categorize
        }

        async fn enrich(&self, bookmark: &mut Bookmark) -> Result<()> {
            bookmark.categories.push(CategoryAssignment {
                slug: "engineering".to_owned(),
                confidence: 0.9,
                assigned_by: mbm_core::bookmark::Assigner::Jev,
            });
            Ok(())
        }
    }

    #[tokio::test]
    async fn the_tag_stage_saves_its_tags() {
        // a stage that runs, costs money, and writes nothing is the one failure
        // this design cannot have
        let (_dir, conn) = open();
        seed(&conn, 3);
        let plan = Plan::new(vec![Arc::new(Tagger { seen: std::sync::Mutex::new(Vec::new()) })]);
        run(&conn, &plan).await.unwrap();

        let repo = Repo::new(&conn);
        let found = repo.by_tag("added-by-the-stage", 10, 0).unwrap();
        assert_eq!(found.len(), 3, "every row kept the tag the stage added");
    }

    #[tokio::test]
    async fn the_tag_stage_keeps_the_tags_an_earlier_stage_added() {
        let (_dir, conn) = open();
        seed(&conn, 2);
        for bookmark in Repo::new(&conn).list(10, 0).unwrap() {
            let mut b = bookmark;
            b.push_tag("from-entities");
            Repo::new(&conn).set_tags(b.id, &b.tags.iter().cloned().collect()).unwrap();
        }

        let plan = Plan::new(vec![Arc::new(Tagger { seen: std::sync::Mutex::new(Vec::new()) })]);
        run(&conn, &plan).await.unwrap();

        let repo = Repo::new(&conn);
        assert_eq!(repo.by_tag("from-entities", 10, 0).unwrap().len(), 2);
        assert_eq!(repo.by_tag("added-by-the-stage", 10, 0).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_categorize_stage_saves_its_categories() {
        let (_dir, conn) = open();
        seed(&conn, 2);
        let plan = Plan::new(vec![Arc::new(Filer)]);
        run(&conn, &plan).await.unwrap();

        let stored = Repo::new(&conn).load(Id::from_parts(1_700_000_000_000, 0)).unwrap().unwrap();
        assert_eq!(stored.categories.len(), 1, "{:?}", stored.categories);
        assert_eq!(stored.categories[0].slug, "engineering");
        assert!((stored.categories[0].confidence - 0.9).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn a_failure_leaves_its_row_for_the_next_run() {
        let (_dir, conn) = open();
        seed(&conn, 3);
        let stage = Arc::new(Recorder::failing_on(&["1"]));
        let plan = Plan::new(vec![stage]);
        let report = run(&conn, &plan).await.unwrap();
        assert_eq!(report.total_done(), 2);
        assert_eq!(report.total_failed(), 1);
        assert_eq!(Repo::new(&conn).pending("entities_at").unwrap(), 1, "the failed row waits");
    }

    #[tokio::test]
    async fn stages_run_cheapest_first() {
        let (_dir, conn) = open();
        seed(&conn, 2);
        let plan = Plan::new(vec![
            Arc::new(Recorder::new(EnrichStage::Categorize)),
            Arc::new(Recorder::new(EnrichStage::Entities)),
        ]);
        let report = run(&conn, &plan).await.unwrap();
        let order: Vec<EnrichStage> = report.stages.iter().map(|(s, _)| *s).collect();
        assert_eq!(order, vec![EnrichStage::Entities, EnrichStage::Categorize]);
    }

    #[tokio::test]
    async fn requeue_puts_a_stage_back_in_the_queue() {
        let (_dir, conn) = open();
        seed(&conn, 4);
        let plan = Plan::new(vec![Arc::new(Recorder::new(EnrichStage::Tags))]);
        run(&conn, &plan).await.unwrap();
        assert_eq!(Repo::new(&conn).pending("tagged_at").unwrap(), 0);

        assert_eq!(requeue(&conn, EnrichStage::Tags).unwrap(), 4);
        assert_eq!(Repo::new(&conn).pending("tagged_at").unwrap(), 4);
    }

    #[tokio::test]
    async fn a_limit_stops_a_stage_early() {
        let (_dir, conn) = open();
        seed(&conn, 10);
        let plan =
            Plan::new(vec![Arc::new(Recorder::new(EnrichStage::Entities))]).with_limit(Some(4));
        let report = run(&conn, &plan).await.unwrap();
        assert_eq!(report.total_done(), 4);
        assert_eq!(Repo::new(&conn).pending("entities_at").unwrap(), 6);
    }

    /// a stage that must never be called.
    #[derive(Debug)]
    struct Off;

    #[async_trait::async_trait]
    impl Enricher for Off {
        fn stage(&self) -> EnrichStage {
            EnrichStage::Vision
        }

        fn is_enabled(&self) -> bool {
            false
        }

        async fn enrich(&self, _: &mut Bookmark) -> Result<()> {
            unreachable!("a disabled stage is never called")
        }
    }

    #[tokio::test]
    async fn a_disabled_stage_is_skipped() {
        let (_dir, conn) = open();
        seed(&conn, 2);
        let report = run(&conn, &Plan::new(vec![Arc::new(Off)])).await.unwrap();
        assert!(report.stages.is_empty());
        assert_eq!(Repo::new(&conn).pending("vision_at").unwrap(), 2);
    }

    #[tokio::test]
    async fn the_entity_stage_writes_its_output_through_the_repo() {
        let (_dir, conn) = open();
        seed(&conn, 3);
        let plan = Plan::new(vec![Arc::new(Entities::new())]);
        run(&conn, &plan).await.unwrap();

        let repo = Repo::new(&conn);
        let stored = repo.load(Id::from_parts(1_700_000_000_000, 0)).unwrap().unwrap();
        assert!(stored.fingerprint.is_some(), "the fingerprint survives the round trip");
        let tags = repo.by_tag("github.com", 10, 0).unwrap();
        assert!(tags.is_empty(), "a post whose text carries no host has no host tag");
    }

    #[tokio::test]
    async fn the_backlog_line_names_what_is_waiting() {
        let (_dir, conn) = open();
        seed(&conn, 3);
        let line = backlog_line(&conn);
        assert!(line.contains("3 bookmarks"), "{line}");
        assert!(line.contains("entities 3"), "{line}");
    }

    #[tokio::test]
    async fn a_clear_backlog_says_so() {
        let (_dir, conn) = open();
        seed(&conn, 1);
        let plan = Plan::new(vec![Arc::new(Recorder::new(EnrichStage::Entities))]);
        run(&conn, &plan).await.unwrap();
        for stage in EnrichStage::ALL {
            conn.execute(&format!("UPDATE bookmark SET {} = 1", column_for(*stage)), []).unwrap();
        }
        assert_eq!(backlog_line(&conn), "1 bookmarks, all stages clear");
    }

    #[test]
    fn every_stage_maps_to_a_real_column() {
        for stage in EnrichStage::ALL {
            let column = column_for(*stage);
            assert!(column.ends_with("_at"), "{stage} maps to {column}");
        }
    }

    #[tokio::test]
    async fn the_next_batch_reads_the_bookmarks_themselves() {
        let (_dir, conn) = open();
        seed(&conn, 3);
        let batch = next_batch(&conn, EnrichStage::Entities, 2).unwrap();
        assert_eq!(batch.len(), 2);
        assert!(batch[0].text.contains("post number"));
    }

    #[test]
    fn a_fetch_page_of_nothing_is_still_a_page() {
        let empty = FetchPage::empty();
        assert!(empty.items.is_empty());
        assert!(!empty.has_more);
    }
}
