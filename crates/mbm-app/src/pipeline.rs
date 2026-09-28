//! the pipeline: everything a run does, in the order that makes it cheap.
//!
//! ```text
//!   sources ──▶ store ──▶ entities ──▶ vision ──▶ tags ──▶ categorize ──▶ describe
//!                            │
//!                            └──────────────────────────────▶ sinks
//! ```
//!
//! the ordering is the whole cost argument. the free stage runs first and last
//! writes nothing expensive; the two stages that cost a fraction of a cent per
//! item read what the free one found; the one stage that writes prose is last
//! and is off unless asked for. a run over a hundred thousand bookmarks with the
//! default switches costs two typed questions per item and nothing else.
//!
//! every step is resumable on its own. an ingest that dies half way leaves the
//! items it wrote, and the next run re-fetches and the store's identity index
//! rejects what it already has. a stage that dies leaves its rows unstamped.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mbm_core::bookmark::Bookmark;
use mbm_core::error::{Error, Result};
use mbm_core::medium::{SinkMedium, SourceMedium};
use mbm_core::port::{EnrichStage, FetchRequest, Sink, Source};
use mbm_enrich::pipeline::{self as stages, Plan};
use mbm_store::{Filter, Mode, Repo, Searcher};
use rusqlite::Connection;

use crate::config::Config;

/// what one run did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunReport {
    /// how many items the sources returned.
    pub fetched: usize,
    /// how many were new to the store.
    pub inserted: usize,
    /// how many the store already had.
    pub duplicates: usize,
    /// how many the sources could not give.
    pub skipped: usize,
    /// per-stage counts, in the order they ran.
    pub stages: Vec<(EnrichStage, mbm_enrich::StageReport)>,
    /// per-sink counts.
    pub sinks: Vec<(String, mbm_core::port::SinkReport)>,
    /// how long the whole run took.
    pub elapsed: Duration,
    /// what the gateway charged, in dollars, when it said.
    pub cost_usd: f64,
}

impl RunReport {
    /// a one-line summary for a log or a tui status bar.
    #[must_use]
    pub fn line(&self) -> String {
        let mut parts = vec![format!("{} fetched", self.fetched)];
        if self.inserted > 0 {
            parts.push(format!("{} new", self.inserted));
        }
        if self.duplicates > 0 {
            parts.push(format!("{} already known", self.duplicates));
        }
        let enriched: usize = self.stages.iter().map(|(_, r)| r.done).sum();
        if enriched > 0 {
            parts.push(format!("{enriched} enriched"));
        }
        let written: usize = self.sinks.iter().map(|(_, r)| r.written).sum();
        if written > 0 {
            parts.push(format!("{written} written"));
        }
        if self.cost_usd > 0.0 {
            parts.push(format!("${:.5}", self.cost_usd));
        }
        parts.push(format!("{}ms", self.elapsed.as_millis()));
        parts.join(", ")
    }
}

/// what one run should do.
#[derive(Clone)]
pub struct Job {
    /// the sources to read.
    pub sources: Vec<Arc<dyn Source>>,
    /// the sinks to write.
    pub sinks: Vec<Arc<dyn Sink>>,
    /// how many items one fetch may take.
    pub limit: Option<usize>,
    /// how many pages one source may fetch.
    pub max_pages: Option<usize>,
    /// only fetch from this medium.
    pub only: Option<SourceMedium>,
    /// stop before the enrichment stages.
    pub skip_enrich: bool,
    /// only run the stages named here.
    pub only_stages: Vec<EnrichStage>,
    /// take at most this many rows per stage.
    pub stage_limit: Option<usize>,
    /// do not write to any sink.
    pub dry_run: bool,
    /// write only the items every enrichment stage has finished.
    pub only_complete: bool,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("sources", &self.sources.iter().map(|s| s.name()).collect::<Vec<_>>())
            .field("sinks", &self.sinks.iter().map(|s| s.name()).collect::<Vec<_>>())
            .field("limit", &self.limit)
            .field("max_pages", &self.max_pages)
            .field("only", &self.only)
            .field("skip_enrich", &self.skip_enrich)
            .field("only_stages", &self.only_stages)
            .field("stage_limit", &self.stage_limit)
            .field("dry_run", &self.dry_run)
            .field("only_complete", &self.only_complete)
            .finish()
    }
}

impl Job {
    /// a job that does everything the config enables.
    pub fn full(config: &Config, store: &Connection) -> Result<Self> {
        let sources: Vec<Arc<dyn Source>> = build_sources(config)?;
        let sinks = if config.enabled_sinks().is_empty() {
            vec![Arc::new(mbm_sink::Jsonl::new(
                config.resolve(std::path::Path::new("bookmarks.jsonl")),
            )) as Arc<dyn Sink>]
        } else {
            build_sinks(config)?
        };
        let _ = store;
        Ok(Self {
            sources,
            sinks,
            limit: Some(config.page_size),
            max_pages: None,
            only: None,
            skip_enrich: false,
            only_stages: Vec::new(),
            stage_limit: None,
            dry_run: false,
            only_complete: false,
        })
    }

    /// take at most this many items per source.
    #[must_use]
    pub fn with_limit(mut self, limit: Option<usize>) -> Self {
        self.limit = limit;
        self
    }

    /// take at most this many pages per source.
    #[must_use]
    pub fn with_max_pages(mut self, pages: Option<usize>) -> Self {
        self.max_pages = pages;
        self
    }

    /// read from this medium only.
    #[must_use]
    pub fn from_only(mut self, medium: SourceMedium) -> Self {
        self.only = Some(medium);
        self
    }

    /// stop after the store.
    #[must_use]
    pub fn without_enrichment(mut self) -> Self {
        self.skip_enrich = true;
        self
    }

    /// run these stages only.
    #[must_use]
    pub fn with_stages(mut self, stages: Vec<EnrichStage>) -> Self {
        self.only_stages = stages;
        self
    }

    /// take at most this many rows per stage.
    #[must_use]
    pub fn with_stage_limit(mut self, limit: Option<usize>) -> Self {
        self.stage_limit = limit;
        self
    }

    /// read and enrich but write nothing.
    #[must_use]
    pub fn dry(mut self) -> Self {
        self.dry_run = true;
        self
    }

    /// write only the items every stage has finished.
    #[must_use]
    pub fn only_complete(mut self, only: bool) -> Self {
        self.only_complete = only;
        self
    }

    /// whether a source is in scope.
    #[must_use]
    pub fn takes(&self, source: &dyn Source) -> bool {
        self.only.is_none_or(|only| only == source.medium())
    }
}

/// build the sources a config describes.
pub fn build_sources(config: &Config) -> Result<Vec<Arc<dyn Source>>> {
    let http = mbm_extract::Http::new(&config.user_agent, 32)?
        .with_retries(config.retries)
        .with_timeout(Duration::from_secs(config.request_timeout_secs))?;

    let mut out: Vec<Arc<dyn Source>> = Vec::new();
    for entry in config.enabled_sources() {
        let source: Arc<dyn Source> = match entry.medium {
            SourceMedium::X | SourceMedium::XBird => {
                let cookies = config.twitter.cookies(&config.data_dir);
                let mut client = mbm_ingest::XClient::new(http.clone(), cookies);
                // a mirror, a proxy, or a replay: wherever the endpoint is, the
                // permalinks a person clicks are still x.com's
                if let Some(base) = entry.get("base") {
                    let post_url = entry.get("post_url").unwrap_or("https://x.com");
                    client = client.with_endpoints(base, post_url);
                }
                if !entry.list("folders").is_empty() {
                    let folders = entry
                        .list("folders")
                        .into_iter()
                        .map(|f| mbm_ingest::Folder { id: f.clone(), name: f })
                        .collect();
                    client = client.with_folders(folders);
                }
                let use_bird = config.twitter.use_bird || entry.flag("use_bird");
                Arc::new(mbm_ingest::X::new(client).via_bird_if(use_bird)) as Arc<dyn Source>
            }
            SourceMedium::HackerNews => {
                let mut source = mbm_ingest::HackerNews::new(http.clone());
                if let Some(base) = entry.get("base") {
                    source = source.with_base(base);
                }
                if let Some(tag) = entry.get("tag") {
                    source = source.in_collection(tag);
                }
                Arc::new(source) as Arc<dyn Source>
            }
            SourceMedium::Reddit => {
                let mut source = mbm_ingest::Reddit::new(http.clone());
                if let Some(base) = entry.get("base") {
                    source = source.with_base(base);
                }
                if let Some(name) = entry.get("subreddit") {
                    source = source.subreddit(name);
                }
                if let Some(sort) = entry.get("sort") {
                    source = source.sorted_by(sort);
                }
                Arc::new(source) as Arc<dyn Source>
            }
            SourceMedium::Github => {
                let mut source = mbm_ingest::GithubStars::new(http.clone(), config.github.token());
                // an enterprise instance has its own api host
                if let Some(base) = entry.get("base") {
                    source = source.with_base(base);
                }
                Arc::new(source) as Arc<dyn Source>
            }
            SourceMedium::Rss => {
                let urls = entry.list("urls");
                if urls.is_empty() {
                    return Err(Error::Config(format!(
                        "the {} source has no `urls`. add one under [sources.options].",
                        entry.medium.name()
                    )));
                }
                Arc::new(mbm_ingest::Feed::new(http.clone(), urls)) as Arc<dyn Source>
            }
            SourceMedium::YouTube => {
                let urls = entry.list("playlists");
                if urls.is_empty() {
                    return Err(Error::Config(format!(
                        "the {} source has no `playlists`.",
                        entry.medium.name()
                    )));
                }
                Arc::new(mbm_ingest::YouTube::new(http.clone(), urls)) as Arc<dyn Source>
            }
            // the file-shaped mediums need no adapter object, because the file
            // importer is called straight from the cli
            other => {
                tracing::debug!(medium = other.name(), "no network adapter; use `mbm import`");
                continue;
            }
        };
        out.push(source);
    }
    Ok(out)
}

/// build the sinks a config describes.
pub fn build_sinks(config: &Config) -> Result<Vec<Arc<dyn Sink>>> {
    let mut out: Vec<Arc<dyn Sink>> = Vec::new();
    for entry in config.enabled_sinks() {
        let path = config.resolve(&entry.path);
        let sink: Arc<dyn Sink> = match entry.kind {
            SinkMedium::Jsonl => Arc::new(mbm_sink::Jsonl::new(path)),
            SinkMedium::Json => Arc::new(mbm_sink::Json::new(path)),
            SinkMedium::Csv => Arc::new(mbm_sink::Csv::new(path)),
            SinkMedium::Opml => Arc::new(mbm_sink::Opml::new(path, "mebookmarker")),
            SinkMedium::Markdown => Arc::new(mbm_sink::Markdown::new(path)),
            SinkMedium::Obsidian => Arc::new(mbm_sink::Markdown::obsidian(path)),
            SinkMedium::Html => Arc::new(mbm_sink::Html::new(path)),
            SinkMedium::Archive => Arc::new(mbm_sink::Archive::new(path)),
            SinkMedium::Store => continue,
        };
        out.push(sink);
    }
    Ok(out)
}

/// the enrichment plan a config describes.
#[must_use]
pub fn build_plan(config: &Config, job: &Job) -> Plan {
    let mut plan = Plan::full(config.jev_key(), config.tag_vocabulary.clone(), &config.taxonomy)
        .unwrap_or_default()
        .with_page(config.enrich.page)
        .with_limit(job.stage_limit);

    let enrich = &config.enrich;
    if !enrich.entities {
        plan = plan.without(EnrichStage::Entities);
    }
    if !enrich.vision {
        plan = plan.without(EnrichStage::Vision);
    }
    if !enrich.tags {
        plan = plan.without(EnrichStage::Tags);
    }
    if !enrich.categorize {
        plan = plan.without(EnrichStage::Categorize);
    }
    if enrich.describe
        && let Some(driver) = config.agent.driver()
    {
        plan = plan.with(Arc::new(mbm_enrich::Describe::new(driver)));
    }
    if !job.only_stages.is_empty() {
        plan = plan.only(&job.only_stages);
    }
    plan
}

/// read every source into the store.
pub async fn ingest(conn: &Connection, job: &Job) -> Result<RunReport> {
    let started = Instant::now();
    let repo = Repo::new(conn);
    let mut report = RunReport::default();

    for source in &job.sources {
        if !job.takes(source.as_ref()) {
            continue;
        }
        if let Err(e) = source.preflight().await {
            tracing::warn!(source = source.name(), error = %e, "preflight failed; skipping");
            report.skipped += 1;
            continue;
        }

        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        loop {
            let request = FetchRequest {
                limit: job.limit,
                paginate: true,
                max_pages: job.max_pages,
                collection: cursor.clone(),
                only_ids: BTreeSet::new(),
                since: None,
            };
            let page = match source.fetch(&request).await {
                Ok(page) => page,
                Err(e) => {
                    tracing::warn!(source = source.name(), error = %e, "fetch failed");
                    report.skipped += 1;
                    break;
                }
            };

            report.fetched += page.items.len();
            report.skipped += page.skipped;
            let (inserted, duplicates) = store_many(&repo, &page.items)?;
            report.inserted += inserted;
            report.duplicates += duplicates;

            pages += 1;
            let more = page.has_more
                && (job.max_pages.is_none_or(|max| pages < max))
                && page.next_cursor.is_some()
                // a source that hands back the cursor it was given is not going
                // anywhere, and following it would spin until the run was
                // killed. an api that misbehaves should cost a page, not a night.
                && page.next_cursor != cursor;

            if !more {
                if page.has_more && page.next_cursor == cursor {
                    tracing::warn!(
                        source = source.name(),
                        "the source returned the same page twice; stopping here rather than                          fetching it again"
                    );
                }
                break;
            }
            cursor = page.next_cursor;
        }
    }

    report.elapsed = started.elapsed();
    Ok(report)
}

/// write a batch of bookmarks, counting what was new.
///
/// the return is `(inserted, duplicates)` rather than a report, because the
/// caller is the only one that knows how to phrase it. the whole batch goes
/// through one call, which commits in blocks: a transaction per item turns a
/// six-figure import into six-figure fsyncs, which is the difference between a
/// run that takes a minute and one that takes an hour.
fn store_many(repo: &Repo<'_>, items: &[Bookmark]) -> Result<(usize, usize)> {
    if items.is_empty() {
        return Ok((0, 0));
    }
    let mut duplicates = 0usize;
    for item in items {
        if repo.find_id(item.source.medium, &item.source.external_id)?.is_some() {
            duplicates += 1;
        }
    }
    let inserted = repo.insert_many(items)?;
    Ok((inserted, duplicates))
}

/// run the enrichment stages.
pub async fn enrich(
    conn: &Connection,
    config: &Config,
    job: &Job,
) -> Result<Vec<(EnrichStage, mbm_enrich::StageReport)>> {
    if job.skip_enrich {
        return Ok(Vec::new());
    }
    let plan = build_plan(config, job);
    let report = stages::run(conn, &plan).await?;
    Ok(report.stages)
}

/// write the store to every sink.
pub async fn export(
    conn: &Connection,
    job: &Job,
) -> Result<Vec<(String, mbm_core::port::SinkReport)>> {
    let mut out = Vec::with_capacity(job.sinks.len());
    let repo = Repo::new(conn);

    for sink in &job.sinks {
        sink.prepare().await?;
        // every sink is a whole-archive export, because every format here is a
        // document rather than a delta, and a document that is rewritten is one
        // a reader can open without knowing when it was written
        let mut offset = 0usize;
        let mut report = mbm_core::port::SinkReport::default();
        loop {
            // the satellites are what a sink writes: an export that dropped the
            // tags and the links would be a different document from the one
            // `mbm list` shows, and the whole point of an archive is that it
            // is the same archive
            let batch = if job.only_complete {
                repo.list_complete(500, offset)?
            } else {
                repo.list(500, offset)?
            };
            if batch.is_empty() {
                break;
            }
            let mut batch = batch;
            for item in &mut batch {
                repo.load_satellites(item)?;
            }
            let refs: Vec<&Bookmark> = batch.iter().collect();
            let written = sink.write(&refs).await?;
            report.absorb(written);
            offset += batch.len();
            if batch.len() < 500 {
                break;
            }
        }
        report.absorb(sink.finish().await?);
        tracing::info!(sink = sink.name(), written = report.written, files = report.files, "wrote");
        out.push((sink.name().to_owned(), report));
    }
    Ok(out)
}

/// run everything a job describes.
pub async fn run(conn: &Connection, config: &Config, job: &Job) -> Result<RunReport> {
    let started = Instant::now();
    let mut report = ingest(conn, job).await?;
    report.stages = enrich(conn, config, job).await?;
    if !job.dry_run {
        report.sinks = export(conn, job).await?;
    }
    report.cost_usd = estimate_cost(&report);
    report.elapsed = started.elapsed();
    Ok(report)
}

/// what a run probably cost.
///
/// measured against the gateway: 565 ms and $0.0000196 for three questions on
/// one item, with 466 tokens in and 83 out. the two stages that ask questions
/// are tags and categorize, so two questions per item is the assumption, and the
/// describe stage is free because it runs on a local agent.
#[must_use]
pub fn estimate_cost(report: &RunReport) -> f64 {
    const PER_QUESTION: f64 = 0.000_006_5;
    let asked: usize = report
        .stages
        .iter()
        .filter(|(stage, _)| matches!(stage, EnrichStage::Tags | EnrichStage::Categorize))
        .map(|(_, r)| r.done)
        .sum();
    asked as f64 * PER_QUESTION * 2.0
}

/// the prefilter a search ranks over, and when it was built.
///
/// the fuzzy stage is a scan of the whole archive unless something narrows it
/// first, and a terminal interface runs a search on every keystroke. building
/// the prefilter per query would make typing slower the larger the archive got,
/// which is the opposite of what a search is for. one process opens one store,
/// so one cache per thread is the whole of the state.
#[derive(Debug, Default)]
struct PrefilterCache {
    /// how many bookmarks there were, and the newest ingest time.
    ///
    /// either changing means the archive changed, and both come from a count
    /// and a max over an index, which is cheap next to a rebuild.
    seen: Option<(i64, i64)>,
    prefilter: Option<mbm_store::prefilter::Prefilter>,
    positions: Vec<mbm_core::id::Id>,
}

thread_local! {
    static PREFILTER: std::cell::RefCell<PrefilterCache> =
        std::cell::RefCell::new(PrefilterCache::default());
}

/// run a search with the prefilter attached, rebuilding it when the archive has
/// moved on.
///
/// the prefilter lives in a thread local rather than in the caller's hands, so
/// the search runs inside the closure: a borrow of it cannot outlive this call,
/// which is what stops a second store in the same process from being ranked
/// against the first one's index.
fn with_searcher(
    conn: &Connection,
    query: &str,
    mode: Mode,
    limit: usize,
) -> Result<Vec<mbm_store::search::Hit>> {
    let stamp: (i64, i64) = {
        let mut stmt = conn
            .prepare("SELECT count(*), coalesce(max(ingested_at), 0) FROM bookmark")
            .map_err(|e| Error::Store(e.to_string()))?;
        stmt.query_row([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .map_err(|e| Error::Store(e.to_string()))?
    };

    PREFILTER.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.seen != Some(stamp) || cache.prefilter.is_none() {
            let (prefilter, positions) =
                mbm_store::build_prefilter(conn, |id| mbm_store::indexed_text(conn, id))?;
            cache.prefilter = Some(prefilter);
            cache.positions = positions;
            cache.seen = Some(stamp);
        }
        let prefilter = cache.prefilter.as_ref().expect("just ensured");
        Searcher::new(conn).with_prefilter(prefilter, &cache.positions).search(query, mode, limit)
    })
}

/// the items a search returns, best first.
pub fn search(
    conn: &Connection,
    query: &str,
    limit: usize,
    offset: usize,
) -> Result<Vec<Bookmark>> {
    let repo = Repo::new(conn);
    if query.trim().is_empty() {
        return repo.list(limit, offset);
    }
    let hits = with_searcher(conn, query, Mode::Hybrid, limit.saturating_add(offset))?;
    let ids: Vec<mbm_core::id::Id> = hits.into_iter().skip(offset).map(|h| h.id).collect();
    repo.load_ranked(&ids)
}

/// search with a chosen mode, for the tui's toggle.
pub fn search_mode(
    conn: &Connection,
    query: &str,
    mode: Mode,
    limit: usize,
) -> Result<Vec<(Bookmark, f64)>> {
    let repo = Repo::new(conn);
    if query.trim().is_empty() {
        return Ok(repo.list(limit, 0)?.into_iter().map(|b| (b, 0.0)).collect());
    }
    let hits = with_searcher(conn, query, mode, limit)?;
    let ids: Vec<mbm_core::id::Id> = hits.iter().map(|h| h.id).collect();
    let scores: std::collections::HashMap<mbm_core::id::Id, f64> =
        hits.iter().map(|h| (h.id, h.score)).collect();
    let items = repo.load_ranked(&ids)?;
    Ok(items
        .into_iter()
        .map(|b| {
            let score = scores.get(&b.id).copied().unwrap_or(0.0);
            (b, score)
        })
        .collect())
}

/// a filtered listing, for the tui.
pub fn list(
    conn: &Connection,
    filter: &Filter,
    limit: usize,
    offset: usize,
) -> Result<Vec<Bookmark>> {
    Repo::new(conn).query(filter, limit, offset)
}

/// open the store, creating it if it is not there.
///
/// a store written by a different build is not upgraded and cannot be: the
/// store is derived data, so the honest response to one is to say so and let
/// the caller start again. `mbm rebuild` is the command that does that.
pub fn open(config: &Config) -> Result<Connection> {
    std::fs::create_dir_all(&config.data_dir).map_err(|e| Error::io(&config.data_dir, e))?;
    mbm_store::open(&config.database())
}

/// delete the store, so the next run builds a fresh one.
///
/// the re-fetch is the point: a store that is wrong is cheaper to rebuild than
/// to reason about, and every row in it came from a source that still has it.
pub fn reset(config: &Config) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let path = config.database().with_extension(format!("db{suffix}"));
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
        }
    }
    Ok(())
}
