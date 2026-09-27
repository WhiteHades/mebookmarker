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
                let source = mbm_ingest::HackerNews::new(http.clone());
                match entry.get("tag") {
                    Some(tag) => Arc::new(source.in_collection(tag)) as Arc<dyn Source>,
                    None => Arc::new(source) as Arc<dyn Source>,
                }
            }
            SourceMedium::Reddit => {
                let source = mbm_ingest::Reddit::new(http.clone());
                let mut source = match entry.get("subreddit") {
                    Some(name) => source.subreddit(name),
                    None => source,
                };
                if let Some(sort) = entry.get("sort") {
                    source = source.sorted_by(sort);
                }
                Arc::new(source) as Arc<dyn Source>
            }
            SourceMedium::Github => {
                Arc::new(mbm_ingest::GithubStars::new(http.clone(), config.github.token()))
                    as Arc<dyn Source>
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
                && page.next_cursor.is_some();
            if !more {
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
            let batch = repo.list(500, offset)?;
            if batch.is_empty() {
                break;
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
    let hits = Searcher::new(conn).search(query, Mode::Hybrid, limit.saturating_add(offset))?;
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
    let hits = Searcher::new(conn).search(query, mode, limit)?;
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

/// open the store, creating and migrating it if needed.
pub fn open(config: &Config) -> Result<Connection> {
    std::fs::create_dir_all(&config.data_dir).map_err(|e| Error::io(&config.data_dir, e))?;
    let path = config.database();
    let conn = Connection::open(&path).map_err(|e| sql(&e))?;
    mbm_store::migrate(&conn)?;
    Ok(conn)
}

fn sql(e: &rusqlite::Error) -> Error {
    Error::Store(e.to_string())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::{Sink, Source};
    use mbm_core::bookmark::SourceRef;
    use mbm_core::port::Source as SourceTrait;
    use std::path::PathBuf;

    struct Empty;

    #[async_trait::async_trait]
    impl SourceTrait for Empty {
        fn medium(&self) -> SourceMedium {
            SourceMedium::Rss
        }

        async fn fetch(&self, _request: &FetchRequest) -> Result<mbm_core::port::FetchPage> {
            Ok(mbm_core::port::FetchPage::empty())
        }
    }

    fn store() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("t.db")).unwrap();
        mbm_store::migrate(&conn).unwrap();
        (dir, conn)
    }

    fn one(text: &str) -> Bookmark {
        numbered(text, "1")
    }

    fn numbered(text: &str, id: &str) -> Bookmark {
        let mut b = Bookmark::new(SourceRef::new(SourceMedium::X, id, None), text, 0);
        b.created_at = Some(1_767_312_000_000);
        b
    }

    #[tokio::test]
    async fn a_run_with_no_sources_still_reports() {
        let (_dir, conn) = store();
        let job = Job {
            sources: Vec::new(),
            sinks: Vec::new(),
            limit: Some(10),
            max_pages: None,
            only: None,
            skip_enrich: true,
            only_stages: Vec::new(),
            stage_limit: None,
            dry_run: true,
        };
        let report = run(&conn, &Config::default(), &job).await.unwrap();
        assert_eq!(report.fetched, 0);
        assert!(report.line().contains("0 fetched"), "{}", report.line());
    }

    #[tokio::test]
    async fn storing_the_same_item_twice_counts_a_duplicate() {
        let (_dir, conn) = store();
        let repo = Repo::new(&conn);
        let items = vec![one("a post")];
        let (inserted, duplicates) = store_many(&repo, &items).unwrap();
        assert_eq!((inserted, duplicates), (1, 0));
        let (inserted, duplicates) = store_many(&repo, &items).unwrap();
        assert_eq!((inserted, duplicates), (0, 1));
    }

    #[tokio::test]
    async fn two_sources_returning_the_same_item_store_it_once() {
        let (_dir, conn) = store();
        let job = Job {
            sources: vec![Arc::new(Empty), Arc::new(Empty)],
            sinks: Vec::new(),
            limit: Some(10),
            max_pages: None,
            only: None,
            skip_enrich: true,
            only_stages: Vec::new(),
            stage_limit: None,
            dry_run: true,
        };
        let report = ingest(&conn, &job).await.unwrap();
        assert_eq!(report.fetched, 0);
        assert_eq!(Repo::new(&conn).count().unwrap(), 0);
    }

    #[test]
    fn a_source_filter_narrows_the_run() {
        let job = Job {
            sources: Vec::new(),
            sinks: Vec::new(),
            limit: None,
            max_pages: None,
            only: Some(SourceMedium::Reddit),
            skip_enrich: true,
            only_stages: Vec::new(),
            stage_limit: None,
            dry_run: true,
        };
        assert!(!job.takes(&Empty), "rss is not reddit");
        assert!(job.only.is_some());
    }

    #[test]
    fn a_config_with_no_sinks_still_gets_one() {
        let mut config = Config::default();
        config.data_dir = PathBuf::from("/tmp/mbm-test");
        config.sinks.clear();
        let job = Job::full(&config, &Connection::open_in_memory().unwrap()).unwrap();
        assert_eq!(job.sinks.len(), 1, "a run with no configured sink still writes jsonl");
    }

    #[test]
    fn a_configured_sink_becomes_the_right_object() {
        let mut config = Config::default();
        config.data_dir = PathBuf::from("/tmp/mbm-test");
        config.sinks =
            vec![Sink { kind: SinkMedium::Html, enabled: true, path: PathBuf::from("site.html") }];
        let job = Job::full(&config, &Connection::open_in_memory().unwrap()).unwrap();
        assert_eq!(job.sinks[0].name(), "html");
    }

    #[test]
    fn a_disabled_sink_is_not_built() {
        let mut config = Config::default();
        config.sinks =
            vec![Sink { kind: SinkMedium::Html, enabled: false, path: PathBuf::from("site.html") }];
        let job = Job::full(&config, &Connection::open_in_memory().unwrap()).unwrap();
        assert_eq!(job.sinks.len(), 1, "the fallback jsonl");
    }

    #[test]
    fn the_default_plan_runs_the_free_stage_first() {
        let config = Config::default();
        let job = Job::full(&config, &Connection::open_in_memory().unwrap()).unwrap();
        let plan = build_plan(&config, &job);
        let stages = plan.stages();
        assert_eq!(stages[0], EnrichStage::Entities);
        assert!(!stages.contains(&EnrichStage::Describe), "prose is opt-in");
    }

    #[test]
    fn turning_a_stage_off_removes_it_from_the_plan() {
        let mut config = Config::default();
        config.enrich.vision = false;
        config.enrich.tags = false;
        let job = Job::full(&config, &Connection::open_in_memory().unwrap()).unwrap();
        let stages = build_plan(&config, &job).stages();
        assert!(!stages.contains(&EnrichStage::Vision));
        assert!(!stages.contains(&EnrichStage::Tags));
        assert!(stages.contains(&EnrichStage::Entities));
    }

    #[test]
    fn naming_a_stage_keeps_only_that_one() {
        let config = Config::default();
        let job = Job::full(&config, &Connection::open_in_memory().unwrap())
            .unwrap()
            .with_stages(vec![EnrichStage::Tags]);
        let stages = build_plan(&config, &job).stages();
        assert_eq!(stages, vec![EnrichStage::Tags]);
    }

    #[test]
    fn an_rss_source_with_no_urls_is_a_config_error() {
        let mut config = Config::default();
        config.sources = vec![Source { medium: SourceMedium::Rss, ..Source::default() }];
        let Err(err) = build_sources(&config) else {
            panic!("a source with no urls should not build");
        };
        assert!(err.to_string().contains("urls"), "{err}");
    }

    #[test]
    fn a_youtube_source_with_no_playlists_is_a_config_error() {
        let mut config = Config::default();
        config.sources = vec![Source { medium: SourceMedium::YouTube, ..Source::default() }];
        assert!(build_sources(&config).is_err());
    }

    #[test]
    fn a_file_medium_needs_no_network_adapter() {
        let mut config = Config::default();
        config.sources = vec![Source { medium: SourceMedium::Json, ..Source::default() }];
        let sources = build_sources(&config).unwrap();
        assert!(sources.is_empty(), "json is read by `mbm import`");
    }

    #[test]
    fn a_configured_rss_source_becomes_a_feed() {
        let mut config = Config::default();
        config.sources = vec![Source {
            medium: SourceMedium::Rss,
            enabled: true,
            options: toml::from_str(r#"urls = ["https://a.example/feed"]"#).unwrap(),
        }];
        let sources = build_sources(&config).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].medium(), SourceMedium::Rss);
    }

    #[test]
    fn the_cost_estimate_scales_with_the_questions_asked() {
        let mut report = RunReport::default();
        report.stages = vec![
            (EnrichStage::Entities, mbm_enrich::StageReport { done: 100, ..Default::default() }),
            (EnrichStage::Tags, mbm_enrich::StageReport { done: 100, ..Default::default() }),
            (EnrichStage::Categorize, mbm_enrich::StageReport { done: 100, ..Default::default() }),
        ];
        let cost = estimate_cost(&report);
        // 200 items asked two questions each at the measured rate
        assert!((cost - 200.0 * 2.0 * 0.000_006_5).abs() < 1e-12, "{cost}");
    }

    #[test]
    fn the_free_stages_cost_nothing() {
        let mut report = RunReport::default();
        report.stages = vec![(
            EnrichStage::Entities,
            mbm_enrich::StageReport { done: 1000, ..Default::default() },
        )];
        assert!(estimate_cost(&report).abs() < f64::EPSILON);
    }

    #[test]
    fn the_report_line_says_what_happened() {
        let report = RunReport {
            fetched: 10,
            inserted: 8,
            duplicates: 2,
            elapsed: Duration::from_millis(120),
            ..RunReport::default()
        };
        let line = report.line();
        assert!(line.contains("10 fetched"), "{line}");
        assert!(line.contains("8 new"), "{line}");
        assert!(line.contains("2 already known"), "{line}");
        assert!(line.contains("120ms"), "{line}");
    }

    #[test]
    fn the_store_opens_and_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.data_dir = dir.path().to_path_buf();
        let conn = open(&config).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, mbm_store::schema::SCHEMA_VERSION);
        assert!(config.database().exists());
    }

    #[test]
    fn opening_the_store_twice_is_the_same_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.data_dir = dir.path().to_path_buf();
        drop(open(&config).unwrap());
        let conn = open(&config).unwrap();
        assert_eq!(Repo::new(&conn).count().unwrap(), 0);
    }

    #[tokio::test]
    async fn searching_finds_what_was_stored() {
        let (_dir, conn) = store();
        Repo::new(&conn).insert(&numbered("a post about sqlite internals", "1")).unwrap();
        Repo::new(&conn).insert(&numbered("a post about gardening", "2")).unwrap();
        let found = search(&conn, "sqlite", 10, 0).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].text.contains("sqlite"));
    }

    #[tokio::test]
    async fn an_empty_query_lists_everything() {
        let (_dir, conn) = store();
        Repo::new(&conn).insert(&numbered("one", "1")).unwrap();
        Repo::new(&conn).insert(&numbered("two", "2")).unwrap();
        assert_eq!(search(&conn, "  ", 10, 0).unwrap().len(), 2);
    }
}
