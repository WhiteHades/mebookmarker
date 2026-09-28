//! the command line.
//!
//! one binary, subcommands named after what a person is trying to do:
//!
//! ```text
//!   mbm add <url>...      save urls
//!   mbm import <path>     read a file: json, opml, netscape, markdown, a folder
//!   mbm run               fetch every configured source, enrich, export
//!   mbm search <query>    search the archive
//!   mbm show <id>         print one bookmark
//!   mbm list              list what is in the archive
//!   mbm tag <id> <tag>    add a tag by hand
//!   mbm stats             counts, by medium and by tag
//!   mbm export            write the archive out
//!   mbm enrich            run the enrichment stages
//!   mbm config            read and write the configuration
//!   mbm tui               the interactive browser
//! ```
//!
//! the shape rule: anything that changes the archive says so, and anything that
//! only reads it takes no lock. `add` and `import` are the two commands that
//! write, and both are idempotent — running either twice stores one copy.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use mbm_core::error::{Error, Result};
use mbm_core::medium::{SinkMedium, SourceMedium};
use mbm_core::port::{EnrichStage, FetchPage};
use mbm_store::{Filter, Repo};
use rusqlite::Connection;

use crate::config::Config;
use crate::pipeline::{self, Job};

/// mebookmarker: archive your bookmarks, search them, and keep them.
#[derive(Debug, Parser)]
#[command(
    name = "mbm",
    version,
    about = "archive bookmarks from every medium, search them, and keep them",
    long_about = "mebookmarker reads bookmarks from x, reddit, hacker news, github, \
                  feeds, youtube, your browser's export, and plain files; keeps them in \
                  one searchable store; and writes them back out to markdown, obsidian, \
                  html, csv, jsonl, json, opml, or a raw archive.\n\n\
                  every key in mebookmarker.toml is optional, and a missing file is a \
                  working install."
)]
pub struct Cli {
    /// the configuration file to use.
    #[arg(long, short = 'c', global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// print more about what is happening.
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// print nothing but the answer.
    #[arg(long, short = 'q', global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// the subcommand.
    #[command(subcommand)]
    pub command: Command,
}

/// what to do.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// save one or more urls.
    Add(AddArgs),

    /// read a file or a folder into the archive.
    Import(ImportArgs),

    /// fetch every configured source, enrich, and export.
    Run(RunArgs),

    /// search the archive.
    Search(SearchArgs),

    /// print one bookmark as json.
    Show(ShowArgs),

    /// list what is in the archive.
    List(ListArgs),

    /// add or remove a tag by hand.
    Tag(TagArgs),

    /// remove a bookmark.
    #[command(alias = "rm")]
    Delete(DeleteArgs),

    /// counts by medium, by tag, and by stage.
    Stats,

    /// write the archive out to the configured sinks.
    Export(ExportArgs),

    /// run the enrichment stages.
    Enrich(EnrichArgs),

    /// read and write the configuration.
    Config(ConfigArgs),

    /// delete the store and build it again from the sources.
    Rebuild(RebuildArgs),

    /// the interactive browser.
    Tui(TuiArgs),
}

/// save urls.
#[derive(Debug, Args)]
pub struct AddArgs {
    /// the urls to save.
    #[arg(value_name = "URL", required = true)]
    pub urls: Vec<String>,

    /// tag everything with this.
    #[arg(long, short = 't', value_name = "TAG")]
    pub tag: Vec<String>,

    /// a note to keep with the bookmark.
    #[arg(long, short = 'n', value_name = "TEXT")]
    pub note: Option<String>,

    /// read the page and keep its text.
    #[arg(long)]
    pub fetch: bool,
}

/// read a file or a folder.
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// the file or folder to read.
    #[arg(value_name = "PATH", required = true)]
    pub path: PathBuf,

    /// read subfolders too.
    #[arg(long, short = 'r')]
    pub recursive: bool,

    /// treat the input as this format rather than guessing.
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,

    /// dry run: say what would be read, read nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// fetch, enrich, and export.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// take at most this many items per source.
    #[arg(long, short = 'n', value_name = "COUNT")]
    pub limit: Option<usize>,

    /// take at most this many pages per source.
    #[arg(long, value_name = "COUNT")]
    pub pages: Option<usize>,

    /// read from this medium only.
    #[arg(long, short = 's', value_name = "MEDIUM")]
    pub source: Option<String>,

    /// read but write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// search.
#[derive(Debug, Args)]
pub struct SearchArgs {
    /// the query.
    #[arg(value_name = "QUERY", default_value = "")]
    pub query: String,

    /// how many results.
    #[arg(long, short = 'n', value_name = "COUNT", default_value_t = 20)]
    pub limit: usize,

    /// how the results are ranked.
    #[arg(long, value_enum, default_value_t = Rank::Hybrid)]
    pub rank: Rank,

    /// print json rather than a list.
    #[arg(long)]
    pub json: bool,
}

/// how search results are ranked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Rank {
    /// bm25 and fuzzy together. the default.
    Hybrid,
    /// bm25 only, for a query that is finished.
    Exact,
    /// fuzzy only, for a query that is still being typed.
    Fuzzy,
}

impl From<Rank> for mbm_store::Mode {
    fn from(rank: Rank) -> Self {
        match rank {
            Rank::Hybrid => Self::Hybrid,
            Rank::Exact => Self::Exact,
            Rank::Fuzzy => Self::Fuzzy,
        }
    }
}

/// print one bookmark.
#[derive(Debug, Args)]
pub struct ShowArgs {
    /// the bookmark's id, or part of its text.
    #[arg(value_name = "ID")]
    pub id: String,
}

/// list.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// how many.
    #[arg(long, short = 'n', value_name = "COUNT", default_value_t = 50)]
    pub limit: usize,

    /// how many to skip.
    #[arg(long, value_name = "COUNT", default_value_t = 0)]
    pub offset: usize,

    /// only these tags.
    #[arg(long, short = 't', value_name = "TAG")]
    pub tag: Vec<String>,

    /// only this medium.
    #[arg(long, short = 's', value_name = "MEDIUM")]
    pub source: Option<String>,

    /// print json rather than a list.
    #[arg(long)]
    pub json: bool,
}

/// add or remove a tag.
#[derive(Debug, Args)]
pub struct TagArgs {
    /// the bookmark's id.
    #[arg(value_name = "ID")]
    pub id: String,

    /// the tag.
    #[arg(value_name = "TAG")]
    pub tag: String,

    /// take the tag off.
    #[arg(long)]
    pub remove: bool,
}

/// remove a bookmark.
#[derive(Debug, Args)]
pub struct DeleteArgs {
    /// the bookmark's id.
    #[arg(value_name = "ID")]
    pub id: String,

    /// do not ask.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// export.
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// write to this path instead of the configured sinks.
    #[arg(long, short = 'o', value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// write this format instead of the configured sinks.
    #[arg(long, short = 'f', value_name = "FORMAT")]
    pub format: Option<String>,

    /// export only the items every enrichment stage has finished.
    ///
    /// off by default, because an export that silently wrote nothing would be
    /// indistinguishable from a bug, and a person who has not run the stages yet
    /// still wants their bookmarks.
    #[arg(long)]
    pub only_complete: bool,
}

/// run the enrichment stages.
#[derive(Debug, Args)]
pub struct EnrichArgs {
    /// run only these stages.
    #[arg(long, short = 's', value_name = "STAGE")]
    pub stage: Vec<EnrichStageArg>,

    /// take at most this many rows per stage.
    #[arg(long, short = 'n', value_name = "COUNT")]
    pub limit: Option<usize>,

    /// put every bookmark back in a stage's queue first.
    #[arg(long, value_name = "STAGE")]
    pub redo: Option<EnrichStageArg>,

    /// say what is waiting without running anything.
    #[arg(long)]
    pub status: bool,
}

/// a stage name on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EnrichStageArg {
    /// links, mentions, tags, fingerprints. free.
    Entities,
    /// media alt text and the description gate.
    Vision,
    /// topic tags from the vocabulary.
    Tags,
    /// categories from the taxonomy.
    Categorize,
    /// generated titles and summaries, through the local agent.
    Describe,
}

impl From<EnrichStageArg> for EnrichStage {
    fn from(stage: EnrichStageArg) -> Self {
        match stage {
            EnrichStageArg::Entities => Self::Entities,
            EnrichStageArg::Vision => Self::Vision,
            EnrichStageArg::Tags => Self::Tags,
            EnrichStageArg::Categorize => Self::Categorize,
            EnrichStageArg::Describe => Self::Describe,
        }
    }
}

/// the configuration.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// write a configuration file with the defaults.
    #[arg(long)]
    pub init: bool,

    /// print the effective configuration.
    #[arg(long)]
    pub show: bool,

    /// print where the configuration was looked for.
    #[arg(long)]
    pub path: bool,

    /// check the configuration and say what is wrong with it.
    #[arg(long)]
    pub check: bool,

    /// write a portable example into the current directory.
    #[arg(long)]
    pub example: bool,
}

/// rebuild the store from scratch.
#[derive(Debug, Args)]
pub struct RebuildArgs {
    /// say what would be deleted, delete nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// delete the store without fetching anything.
    #[arg(long, conflicts_with = "dry_run")]
    pub only: bool,
}

/// the interactive browser.
#[derive(Debug, Args)]
pub struct TuiArgs {
    /// open with this query already in the box.
    #[arg(value_name = "QUERY", default_value = "")]
    pub query: String,
}

impl Cli {
    /// the configuration this invocation uses.
    pub fn config(&self) -> Result<Config> {
        let path = self.config.clone().unwrap_or_else(|| Config::path_in(&default_config_dir()));
        Config::load(&path)
    }

    /// open the store.
    pub fn store(&self) -> Result<Connection> {
        pipeline::open(&self.config()?)
    }
}

/// where the configuration lives by default.
///
/// the data directory, so there is one thing to back up rather than two.
#[must_use]
pub fn default_config_dir() -> PathBuf {
    crate::config::default_data_dir()
}

/// the configuration path an invocation uses.
///
/// `--config` wins, and otherwise it is the one beside the store. every command
/// resolves it through this, because a subcommand that resolves it differently
/// is a subcommand where `--config` silently does nothing.
#[must_use]
pub fn config_path(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(|| Config::path_in(&default_config_dir()))
}

/// what a command did, ready to print.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    /// the lines to print.
    pub lines: Vec<String>,
    /// true when the command found something to report as a failure.
    pub failed: bool,
}

impl Output {
    /// an output with nothing in it.
    #[must_use]
    pub fn empty() -> Self {
        Self { lines: Vec::new(), failed: false }
    }

    /// one line.
    #[must_use]
    pub fn line(line: impl Into<String>) -> Self {
        Self { lines: vec![line.into()], failed: false }
    }

    /// add a line.
    #[must_use]
    pub fn with(mut self, line: impl Into<String>) -> Self {
        self.lines.push(line.into());
        self
    }

    /// mark this output as a failure.
    #[must_use]
    pub fn failed(mut self) -> Self {
        self.failed = true;
        self
    }

    /// everything as one string.
    #[must_use]
    pub fn render(&self) -> String {
        self.lines.join("\n")
    }
}

/// save urls into the archive.
pub async fn add(conn: &Connection, args: &AddArgs) -> Result<Output> {
    let repo = Repo::new(conn);
    let mut out = Output::empty();
    let mut added = 0usize;
    let mut known = 0usize;

    for raw in &args.urls {
        // a url that already names a scheme is taken as it is, so a `mailto:`
        // is recognised and refused rather than being turned into
        // `https://mailto:...`
        let names_a_scheme = raw.split_once(':').is_some_and(|(scheme, _)| {
            !scheme.is_empty() && scheme.bytes().all(|b| b.is_ascii_alphanumeric())
        });
        let candidate = if names_a_scheme { raw.clone() } else { format!("https://{raw}") };
        let Ok(url) = url::Url::parse(&candidate) else {
            out = out.with(format!("skipped `{raw}`: not a url")).failed();
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            out = out.with(format!("skipped `{raw}`: only http and https are kept")).failed();
            continue;
        }

        let text = args.note.clone().unwrap_or_else(|| url.to_string());
        let mut bookmark = mbm_core::bookmark::Bookmark::new(
            mbm_core::bookmark::SourceRef::new(
                SourceMedium::Manual,
                url.to_string(),
                Some(url.clone()),
            ),
            text,
            now_ms(),
        )
        .created_at(now_ms());
        bookmark.url = Some(url);
        for tag in &args.tag {
            bookmark.push_tag(tag.clone());
        }

        match repo.upsert(&bookmark) {
            Ok((_, true)) => added += 1,
            Ok((_, false)) => known += 1,
            Err(e) => {
                out = out.with(format!("could not save `{raw}`: {e}")).failed();
            }
        }
    }

    if args.fetch {
        // the note becomes the source text, and the entity stage is what finds
        // the links in it, so saving and enriching in one command is the useful
        // default
        let report = mbm_enrich::pipeline::run(
            conn,
            &mbm_enrich::pipeline::Plan::new(vec![Arc::new(mbm_enrich::Entities::new())]),
        )
        .await?;
        out = out.with(format!("{} enriched", report.total_done()));
    }

    out = out.with(format!("{added} added, {known} already in the archive"));
    Ok(out)
}

/// read a file or a folder.
pub fn import(conn: &Connection, args: &ImportArgs) -> Result<Output> {
    let path = &args.path;
    if !path.exists() {
        return Err(Error::Config(format!("{} does not exist", path.display())));
    }
    let repo = Repo::new(conn);
    let mut out = Output::empty();

    let (items, failed) = if path.is_dir() {
        mbm_ingest::read_directory(path, args.recursive)
    } else {
        read_one_file(path, args.format.as_deref())?
    };

    for (bad, reason) in &failed {
        out = out.with(format!("could not read {}: {reason}", bad.display()));
    }
    if args.dry_run {
        return Ok(out
            .with(format!("{} items would be read from {}", items.len(), path.display()))
            .failed_if(&failed));
    }

    let inserted = repo.insert_many(&items)?;
    let _ = args.format;
    Ok(out
        .with(format!("{inserted} added from {}", path.display()))
        .with(format!("{} already in the archive", items.len() - inserted))
        .failed_if(&failed))
}

impl Output {
    /// mark this output failed when anything in it is a failure.
    #[must_use]
    pub fn failed_if(mut self, failures: &[(PathBuf, String)]) -> Self {
        if !failures.is_empty() {
            self.failed = true;
        }
        self
    }
}

/// what a read produced: the bookmarks, and the paths that could not be read.
type Read = (Vec<mbm_core::bookmark::Bookmark>, Vec<(PathBuf, String)>);

/// whether a markdown file is the personal-archive shape.
///
/// whether a markdown file is a personal archive rather than a note.
///
/// the shape is a day heading and entries under it, so both have to be there.
/// the earlier test wanted two day headings, which quietly mis-read every
/// archive file a person had only started writing: one day is the common case,
/// not the edge one.
fn body_has_day_headings(body: &str) -> bool {
    let is_day = |line: &str| {
        let rest = line.strip_prefix("# ").unwrap_or_default();
        rest.contains(',') && rest.chars().filter(char::is_ascii_digit).count() >= 4
    };
    let days = body.lines().filter(|l| is_day(l)).count();
    let entries = body.lines().filter(|l| l.starts_with("## ")).count();
    days >= 1 && entries >= 1
}

/// read one file, guessing the format from what it holds.
fn read_one_file(path: &Path, format: Option<&str>) -> Result<Read> {
    let body = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let kind: &str = format.unwrap_or_else(|| {
        let extension = path
            .extension()
            .map(std::ffi::OsStr::to_string_lossy)
            .unwrap_or_default()
            .to_ascii_lowercase();
        match extension.as_str() {
            "opml" => "opml",
            "html" | "htm" => "netscape",
            "json" | "jsonl" => "json",
            "txt" => "urls",
            // a markdown file with `#` day headings and `## @handle` entries is
            // the personal-archive shape, and a plain note is not
            "md" | "markdown" if body_has_day_headings(&body) => "archive",
            "md" | "markdown" => "document",
            _ => "guess",
        }
    });

    let items = match kind {
        "opml" => mbm_ingest::parse_opml(&body)?,
        "netscape" | "html" => mbm_ingest::parse_netscape(&body)?,
        "urls" | "list" => mbm_ingest::parse_url_list(&body)?,
        "json" => {
            let (items, _skipped) =
                mbm_ingest::parse_json(body.as_bytes(), SourceMedium::LocalFile)?;
            items
        }
        "document" | "markdown" | "md" => {
            vec![mbm_ingest::parse_text_document(path, &body)?]
        }
        "archive" => {
            // a `- **Filed:**` line is relative to the archive file, so the
            // links it produces have to be resolved against where it sat. the
            // path is made absolute first: `bookmarks.md` has an empty parent,
            // and a link resolved against an empty parent is a relative path
            // that is not a url, so the entry's own note is silently dropped.
            let absolute = std::fs::canonicalize(path)
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(path));
            let dir = absolute.parent();
            mbm_ingest::parse_markdown_archive(&body)?
                .into_iter()
                .map(|entry| {
                    let created = entry
                        .day
                        .as_deref()
                        .and_then(mbm_ingest::day_to_unix_ms)
                        .unwrap_or_else(now_ms);
                    mbm_ingest::markdown_file::to_bookmark_in(&entry, created, dir)
                })
                .collect()
        }
        other => {
            // a json file whose extension says nothing still reads as json, and
            // a text file that holds a single url reads as a list
            match mbm_ingest::parse_json(body.as_bytes(), SourceMedium::LocalFile) {
                Ok((items, skipped)) if !items.is_empty() || skipped == 0 => items,
                _ => {
                    let listed = mbm_ingest::parse_url_list(&body)?;
                    if listed.is_empty() {
                        return Err(Error::Config(format!(
                            "{other} is not a format this program reads. try json, opml, \
                             html, urls, or markdown"
                        )));
                    }
                    vec![mbm_ingest::parse_text_document(path, &body)?]
                        .into_iter()
                        .chain(listed)
                        .collect()
                }
            }
        }
    };
    Ok((items, Vec::new()))
}

/// fetch, enrich, and export.
pub async fn run(conn: &Connection, config: &Config, args: &RunArgs) -> Result<Output> {
    let mut job = Job::full(config, conn)?;
    if let Some(limit) = args.limit {
        job = job.with_limit(Some(limit));
    }
    if let Some(pages) = args.pages {
        job = job.with_max_pages(Some(pages));
    }
    if let Some(medium) = &args.source {
        job = job
            .from_only(medium.parse::<SourceMedium>().map_err(|e| Error::Config(e.to_string()))?);
    }
    if args.dry_run {
        job = job.dry();
    }
    let report = pipeline::run(conn, config, &job).await?;
    Ok(Output::line(report.line()))
}

/// search the archive.
pub fn search(conn: &Connection, args: &SearchArgs) -> Result<Output> {
    let results = pipeline::search_mode(conn, &args.query, args.rank.into(), args.limit)?;
    if args.json {
        let records: Vec<mbm_sink::json::Record> = results.iter().map(|(b, _)| b.into()).collect();
        let body =
            serde_json::to_string_pretty(&records).map_err(|e| Error::Sink(e.to_string()))?;
        return Ok(Output::line(body));
    }

    let mut out = Output::empty();
    for (bookmark, score) in &results {
        out = out.with(format!(
            "{:<8} {:<10} {}",
            mbm_sink::date_only(bookmark.created_at.unwrap_or(bookmark.ingested_at)),
            bookmark.source.medium.name(),
            mbm_sink::display_title(bookmark)
        ));
        let _ = score;
    }
    Ok(out.with(format!("{} results", results.len())))
}

/// print one bookmark.
pub fn show(conn: &Connection, args: &ShowArgs) -> Result<Output> {
    let repo = Repo::new(conn);
    let found = match args.id.parse::<u64>() {
        Ok(id) => repo.load(mbm_core::id::Id::from_raw(id))?,
        Err(_) => pipeline::search(conn, &args.id, 1, 0)?.into_iter().next(),
    };
    let Some(bookmark) = found else {
        return Ok(Output::line(format!("nothing matches `{}`", args.id)).failed());
    };
    let body = serde_json::to_string_pretty(&mbm_sink::json::Record::from(&bookmark))
        .map_err(|e| Error::Sink(e.to_string()))?;
    Ok(Output::line(body))
}

/// list the archive.
pub fn list(conn: &Connection, args: &ListArgs) -> Result<Output> {
    // the store's filter takes one tag, so the first one narrows the scan and
    // the rest are checked in memory. `--tag a --tag b` means both, which is
    // what a person listing two tags means.
    let mut filter = Filter::default();
    if let Some(medium) = &args.source {
        filter.medium =
            Some(medium.parse::<SourceMedium>().map_err(|e| Error::Config(e.to_string()))?);
    }
    if let Some(tag) = args.tag.first() {
        filter.tag = Some(tag.clone());
    }
    let mut items = pipeline::list(conn, &filter, args.limit, args.offset)?;
    if args.tag.len() > 1 {
        items.retain(|bookmark| args.tag.iter().all(|tag| bookmark.tags.contains(tag)));
    }
    if args.json {
        let records: Vec<mbm_sink::json::Record> =
            items.iter().map(mbm_sink::json::Record::from).collect();
        let body =
            serde_json::to_string_pretty(&records).map_err(|e| Error::Sink(e.to_string()))?;
        return Ok(Output::line(body));
    }

    let mut out = Output::empty();
    for bookmark in &items {
        out = out.with(format!(
            "{:<20} {:<10} {}",
            bookmark.id.get(),
            bookmark.source.medium.name(),
            mbm_sink::display_title(bookmark)
        ));
    }
    Ok(out.with(format!("{} shown", items.len())))
}

/// add or remove a tag.
pub fn tag(conn: &Connection, args: &TagArgs) -> Result<Output> {
    let Ok(id) = args.id.parse::<u64>().map(mbm_core::id::Id::from_raw) else {
        return Err(Error::Config(format!("`{}` is not a bookmark id", args.id)));
    };
    let repo = Repo::new(conn);
    let Some(mut bookmark) = repo.load(id)? else {
        return Ok(Output::line(format!("no bookmark with id {id}")).failed());
    };

    let before = bookmark.tags.len();
    if args.remove {
        bookmark.tags.remove(&args.tag);
    } else {
        bookmark.push_tag(args.tag.clone());
    }
    if bookmark.tags.len() == before {
        return Ok(Output::line(format!(
            "{} already {} the tag `{}`",
            args.id,
            if args.remove { "lacks" } else { "has" },
            args.tag
        )));
    }

    let tags: ahash::AHashSet<String> = bookmark.tags.iter().cloned().collect();
    repo.set_tags(id, &tags)?;
    Ok(Output::line(format!("{} now has {} tags", args.id, bookmark.tags.len())))
}

/// remove a bookmark.
pub fn delete(conn: &Connection, args: &DeleteArgs) -> Result<Output> {
    let Ok(id) = args.id.parse::<u64>().map(mbm_core::id::Id::from_raw) else {
        return Err(Error::Config(format!("`{}` is not a bookmark id", args.id)));
    };
    if Repo::new(conn).load(id)?.is_none() {
        return Ok(Output::line(format!("no bookmark with id {id}")).failed());
    }
    conn.execute("DELETE FROM bookmark WHERE id = ?1", [id.get() as i64])
        .map_err(|e| Error::Store(e.to_string()))?;
    Ok(Output::line(format!("removed {id}")))
}

/// write the archive out.
pub async fn export(conn: &Connection, config: &Config, args: &ExportArgs) -> Result<Output> {
    let mut job = Job::full(config, conn)?;
    if args.format.is_some() || args.output.is_some() {
        let kind = match &args.format {
            Some(raw) => raw.parse::<SinkMedium>().map_err(|e| Error::Config(e.to_string()))?,
            None => SinkMedium::Jsonl,
        };
        // a relative path lands beside the store, which is where a person
        // expects their archive to be. an absolute one is taken as written, so a
        // destination outside the data directory works.
        let path = match &args.output {
            Some(path) => config.resolve(path),
            None => config.data_dir.join(format!("bookmarks.{}", kind.name())),
        };
        job.sinks = vec![build_one_sink(kind, path)?];
    }
    if args.only_complete {
        // only the items every stage has finished, so the export is one
        // consistent document rather than a mixture of finished and half-done
        job.only_complete = true;
    }
    let reports = pipeline::export(conn, &job).await?;
    let mut out = Output::empty();
    for (name, report) in &reports {
        out = out.with(format!("{name}: {} written, {} files", report.written, report.files));
    }
    Ok(out)
}

/// delete the store and fetch it again.
///
/// the store is derived data, so this is always a correct way to recover from a
/// store that is wrong, damaged, or written by a build that has since changed.
pub async fn rebuild(conn: &Connection, config: &Config, args: &RebuildArgs) -> Result<Output> {
    let before = mbm_store::schema::size_on_disk(conn).unwrap_or(0);
    if args.dry_run {
        return Ok(Output::line(format!(
            "{} would be deleted ({} bytes), and refetched from {} sources",
            config.database().display(),
            before,
            config.enabled_sources().len()
        )));
    }

    mbm_store::schema::reindex(conn)?;
    let _ = before;

    if args.only {
        pipeline::reset(config)?;
        return Ok(Output::line(format!(
            "deleted {}; the next run will build it again",
            config.database().display()
        )));
    }

    pipeline::reset(config)?;
    let fresh = pipeline::open(config)?;
    let job = Job::full(config, &fresh)?;
    let report = pipeline::run(&fresh, config, &job).await?;
    Ok(Output::line(format!("rebuilt: {}", report.line())))
}

fn build_one_sink(kind: SinkMedium, path: PathBuf) -> Result<Arc<dyn mbm_core::port::Sink>> {
    let sink: Arc<dyn mbm_core::port::Sink> = match kind {
        SinkMedium::Jsonl => Arc::new(mbm_sink::Jsonl::new(path)),
        SinkMedium::Json => Arc::new(mbm_sink::Json::new(path)),
        SinkMedium::Csv => Arc::new(mbm_sink::Csv::new(path)),
        SinkMedium::Opml => Arc::new(mbm_sink::Opml::new(path, "mebookmarker")),
        SinkMedium::Markdown => Arc::new(mbm_sink::Markdown::new(path)),
        SinkMedium::Obsidian => Arc::new(mbm_sink::Markdown::obsidian(path)),
        SinkMedium::Html => Arc::new(mbm_sink::Html::new(path)),
        SinkMedium::Archive => Arc::new(mbm_sink::Archive::new(path)),
        SinkMedium::Store => {
            return Err(Error::Config("the store is not an output format".to_owned()));
        }
    };
    Ok(sink)
}

/// run the enrichment stages.
pub async fn enrich(conn: &Connection, config: &Config, args: &EnrichArgs) -> Result<Output> {
    if let Some(stage) = args.redo {
        let stage = EnrichStage::from(stage);
        let requeued = mbm_enrich::requeue(conn, stage)?;
        if args.status {
            return Ok(Output::line(format!("{requeued} bookmarks put back in {stage}")));
        }
    }

    if args.status {
        return Ok(Output::line(mbm_enrich::backlog_line(conn)));
    }

    let mut job = Job::full(config, conn)?;
    if !args.stage.is_empty() {
        job = job.with_stages(args.stage.iter().map(|s| EnrichStage::from(*s)).collect());
    }
    if let Some(limit) = args.limit {
        job = job.with_stage_limit(Some(limit));
    }

    let report = mbm_enrich::pipeline::run(conn, &pipeline::build_plan(config, &job)).await?;
    let mut out = Output::empty();
    for (stage, counts) in &report.stages {
        out = out.with(format!(
            "{stage}: {} done, {} failed, {}ms",
            counts.done,
            counts.failed,
            counts.elapsed.as_millis()
        ));
    }
    Ok(out)
}

/// the configuration.
pub fn config_command(args: &ConfigArgs, path: &Path) -> Result<Output> {
    if args.init {
        Config::default().save(path)?;
        return Ok(Output::line(format!("wrote {}", path.display())));
    }
    if args.example {
        let target = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("mebookmarker.toml.example");
        Config::write_example(&target)?;
        return Ok(Output::line(format!("wrote {}", target.display())));
    }
    if args.path {
        return Ok(Output::line(path.display().to_string()));
    }

    let config = Config::load(path)?;
    if args.check {
        config.validate()?;
        return Ok(Output::line("the configuration is valid".to_owned()));
    }
    Ok(Output::line(config.to_toml()))
}

/// now, in unix milliseconds.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// how many items a fetch page holds, for reporting.
#[must_use]
pub fn page_size(page: &FetchPage) -> usize {
    page.items.len()
}
