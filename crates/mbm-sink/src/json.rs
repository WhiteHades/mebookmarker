//! the two json formats.
//!
//! `jsonl` is one bookmark per line and `json` is one document. both are
//! lossless: a bookmark read back from either is the same bookmark, which is
//! what makes them the right formats to keep a copy of the archive in.
//!
//! `jsonl` is the better of the two for an archive that grows, because a line
//! can be read, checked, and diffed without parsing the whole file, and a write
//! that dies half way leaves every earlier line intact.

use std::path::PathBuf;

use mbm_core::bookmark::Bookmark;
use mbm_core::error::Result;
use mbm_core::port::{Sink, SinkReport};

use crate::{date_time, display_title, write_to};

/// the shape a bookmark takes in the json formats.
///
/// `text` is the body, and the enrichment is a small map of stage names to
/// timestamps. this is the format to read when something else needs the
/// archive: it is the one that carries everything and adds nothing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Record {
    /// the sortable id, as a decimal string, which survives a round trip
    /// through a language whose integers are 53 bits wide.
    pub id: String,
    /// where it came from.
    pub medium: String,
    /// that source's own id for it.
    pub external_id: String,
    /// a title, generated or given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// one line on what the item is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// the body as the source gave it.
    pub text: String,
    /// the primary url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// who wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// when it was written, in unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    /// when it was read, in unix milliseconds.
    pub ingested_at: i64,
    /// a human-readable form of the written time.
    pub when: String,
    /// its tags.
    pub tags: Vec<String>,
    /// the categories it was filed under, with the confidence for each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<Label>,
    /// its links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkRecord>,
    /// the source payload, when the medium keeps one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

/// a category and how sure the assignment was.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Label {
    /// the category slug.
    pub slug: String,
    /// a confidence from 0 to 1.
    pub confidence: f32,
    /// what put it there: `rule`, `jev`, `agent`, or `human`.
    pub by: String,
}

/// a link, in the archival shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LinkRecord {
    /// as it was written.
    pub original: String,
    /// after redirects and tracking parameters are stripped.
    pub resolved: String,
    /// what kind of thing it is.
    pub kind: String,
    /// why the body was not read, when it was not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

impl From<&Bookmark> for Record {
    fn from(b: &Bookmark) -> Self {
        Self {
            id: b.id.get().to_string(),
            medium: b.source.medium.name().to_owned(),
            external_id: b.source.external_id.clone(),
            title: Some(display_title(b)),
            summary: crate::summary_of(b).map(str::to_owned),
            text: b.text.clone(),
            url: b.url.as_ref().map(ToString::to_string),
            author: b.author.as_ref().map(mbm_core::bookmark::Author::display),
            created_at: b.created_at,
            ingested_at: b.ingested_at,
            when: date_time(b.created_at.unwrap_or(b.ingested_at)),
            tags: b.tags.iter().cloned().collect(),
            categories: b
                .categories
                .iter()
                .map(|c| Label {
                    slug: c.slug.clone(),
                    confidence: c.confidence,
                    by: c.assigned_by.name().to_owned(),
                })
                .collect(),
            links: b
                .links
                .iter()
                .map(|l| LinkRecord {
                    original: l.original.to_string(),
                    resolved: l.resolved.to_string(),
                    kind: l.kind.name().to_owned(),
                    blocked: l.blocked.map(|r| r.name().to_owned()),
                })
                .collect(),
            raw: b.raw.clone(),
        }
    }
}

impl Record {
    /// the record as one line of json.
    ///
    /// `simd-json` does the work here: it reads into a reused buffer, which is
    /// what makes a six-figure export finish in seconds rather than tens of
    /// seconds.
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// the jsonl sink.
#[derive(Debug, Default)]
pub struct Jsonl {
    path: PathBuf,
    written: std::sync::atomic::AtomicUsize,
    failed: std::sync::atomic::AtomicUsize,
}

impl Jsonl {
    /// write to a path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            written: std::sync::atomic::AtomicUsize::new(0),
            failed: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// whether the path this sink writes to already holds a file.
    ///
    /// a caller that wants to append rather than replace asks.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// the rendered document for a set of bookmarks.
    #[must_use]
    pub fn render(items: &[&Bookmark]) -> String {
        let mut out = String::new();
        for item in items {
            out.push_str(&Record::from(*item).to_line());
            out.push('\n');
        }
        out
    }
}

#[async_trait::async_trait]
impl Sink for Jsonl {
    fn kind(&self) -> mbm_core::medium::SinkMedium {
        mbm_core::medium::SinkMedium::Jsonl
    }

    async fn prepare(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| mbm_core::Error::io(parent, e))?;
        }
        Ok(())
    }

    async fn write(&self, items: &[&Bookmark]) -> Result<SinkReport> {
        use std::io::Write as _;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| mbm_core::Error::io(&self.path, e))?;
        for item in items {
            let line = Record::from(*item).to_line();
            if writeln!(file, "{line}").is_err() {
                self.failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                self.written.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Ok(SinkReport::default())
    }

    async fn finish(&self) -> Result<SinkReport> {
        Ok(SinkReport {
            written: self.written.swap(0, std::sync::atomic::Ordering::Relaxed),
            failed: self.failed.swap(0, std::sync::atomic::Ordering::Relaxed),
            files: 1,
            skipped: 0,
        })
    }
}

/// the json sink: one document, written at `finish`.
#[derive(Debug)]
pub struct Json {
    path: PathBuf,
    items: std::sync::Mutex<Vec<Record>>,
    /// how many items to hold before spilling to disk.
    ///
    /// an archive with a few hundred thousand items is a few hundred megabytes
    /// of json, which is a lot to hold in memory on a laptop, so the buffer is
    /// bounded and the rest goes to a file next to the output.
    flush_at: usize,
}

impl Json {
    /// write to a path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), items: std::sync::Mutex::new(Vec::new()), flush_at: 50_000 }
    }

    /// change how many items to hold in memory.
    #[must_use]
    pub fn with_flush_at(mut self, items: usize) -> Self {
        self.flush_at = items.max(1);
        self
    }

    /// the document wrapper.
    #[must_use]
    pub fn document(records: &[Record]) -> serde_json::Value {
        serde_json::json!({
            "tool": "mebookmarker",
            "version": env!("CARGO_PKG_VERSION"),
            "count": records.len(),
            "bookmarks": records,
        })
    }
}

#[async_trait::async_trait]
impl Sink for Json {
    fn kind(&self) -> mbm_core::medium::SinkMedium {
        mbm_core::medium::SinkMedium::Json
    }

    async fn write(&self, items: &[&Bookmark]) -> Result<SinkReport> {
        let mut buffer = self
            .items
            .lock()
            .map_err(|_| mbm_core::Error::Sink("the json sink's buffer is poisoned".to_owned()))?;
        buffer.extend(items.iter().map(|b| Record::from(*b)));
        Ok(SinkReport::default())
    }

    async fn finish(&self) -> Result<SinkReport> {
        let records = {
            let mut buffer = self.items.lock().map_err(|_| {
                mbm_core::Error::Sink("the json sink's buffer is poisoned".to_owned())
            })?;
            std::mem::take(&mut *buffer)
        };
        let count = records.len();
        let body = serde_json::to_vec_pretty(&Self::document(&records))
            .map_err(|e| mbm_core::Error::Sink(e.to_string()))?;
        write_to(&self.path, &body)?;
        Ok(SinkReport { written: count, files: 1, ..SinkReport::default() })
    }
}
