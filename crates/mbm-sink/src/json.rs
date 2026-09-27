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
            text: b.text.clone(),
            url: b.url.as_ref().map(ToString::to_string),
            author: b.author.as_ref().map(|a| match &a.name {
                Some(name) => format!("{} ({name})", a.handle),
                None => a.handle.clone(),
            }),
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
        Ok(SinkReport { written: count, ..SinkReport::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::bookmark::{Link, SourceRef};
    use mbm_core::medium::{LinkKind, SinkMedium, SourceMedium};
    use url::Url;

    fn one(text: &str) -> Bookmark {
        let url = Url::parse("https://x.com/a/status/1").unwrap();
        let mut b = Bookmark::new(
            SourceRef::new(SourceMedium::X, "1", Some(url.clone())),
            text,
            1_767_400_000_000,
        )
        .created_at(1_767_312_000_000);
        b.url = Some(url);
        b
    }

    #[test]
    fn a_record_carries_every_field() {
        let mut b = one("a post");
        b.author = Some(mbm_core::bookmark::Author::new("simonw").with_name("Simon"));
        b.push_tag("rust");
        b.links.push(Link {
            original: Url::parse("https://example.com/a?utm=x").unwrap(),
            resolved: Url::parse("https://example.com/a").unwrap(),
            kind: LinkKind::Article,
            title: None,
            body: None,
            summary: None,
            blocked: None,
        });
        let record = Record::from(&b);
        assert_eq!(record.medium, "x");
        assert_eq!(record.external_id, "1");
        assert_eq!(record.author.as_deref(), Some("simonw (Simon)"));
        assert_eq!(record.tags, vec!["rust".to_owned()]);
        assert_eq!(record.links[0].kind, "article");
        assert_eq!(record.when, "2026-01-02 00:00");
    }

    #[test]
    fn a_record_is_one_line() {
        let line = Record::from(&one("a\nmultiline\npost")).to_line();
        assert!(!line.contains('\n'), "{line}");
        serde_json::from_str::<Record>(&line).unwrap();
    }

    #[test]
    fn a_record_round_trips_through_json() {
        let mut b = one("a post");
        b.push_tag("rust");
        b.title = Some("A title".to_owned());
        let line = Record::from(&b).to_line();
        let back: Record = serde_json::from_str(&line).unwrap();
        assert_eq!(back.title.as_deref(), Some("A title"));
        assert_eq!(back.tags, vec!["rust".to_owned()]);
        assert_eq!(back.text, "a post");
    }

    #[test]
    fn a_unicode_title_survives_the_round_trip() {
        let mut b = one("post");
        b.title = Some("日本語のタイトル".to_owned());
        let line = Record::from(&b).to_line();
        let back: Record = serde_json::from_str(&line).unwrap();
        assert_eq!(back.title.as_deref(), Some("日本語のタイトル"));
    }

    #[test]
    fn jsonl_renders_one_line_per_bookmark() {
        let a = one("first");
        let mut b = one("second");
        b.source = SourceRef::new(SourceMedium::X, "2", None);
        let rendered = Jsonl::render(&[&a, &b]);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            serde_json::from_str::<Record>(line).unwrap();
        }
    }

    #[test]
    fn an_empty_jsonl_renders_to_nothing() {
        assert_eq!(Jsonl::render(&[]), "");
    }

    #[test]
    fn the_json_document_names_itself_and_counts() {
        let document = Json::document(&[Record::from(&one("a"))]);
        assert_eq!(document["count"], 1);
        assert_eq!(document["tool"], "mebookmarker");
        assert!(document["version"].is_string());
        assert_eq!(document["bookmarks"][0]["text"], "a");
    }

    #[tokio::test]
    async fn the_jsonl_sink_appends_to_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.jsonl");
        let sink = Jsonl::new(&path);

        let a = one("first");
        sink.write(&[&a]).await.unwrap();
        let mut b = one("second");
        b.source = SourceRef::new(SourceMedium::X, "2", None);
        sink.write(&[&b]).await.unwrap();

        let report = sink.finish().await.unwrap();
        assert_eq!(report.written, 2);
        assert_eq!(report.files, 1);
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 2);
    }

    #[tokio::test]
    async fn a_second_run_appends_rather_than_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.jsonl");
        let a = one("first");
        Jsonl::new(&path).write(&[&a]).await.unwrap();
        let mut b = one("second");
        b.source = SourceRef::new(SourceMedium::X, "2", None);
        Jsonl::new(&path).write(&[&b]).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    }

    #[tokio::test]
    async fn the_json_sink_writes_one_document_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        let sink = Json::new(&path);
        let a = one("first");
        sink.write(&[&a]).await.unwrap();
        assert!(!path.exists(), "nothing is written until the sink finishes");

        let report = sink.finish().await.unwrap();
        assert_eq!(report.written, 1);
        let body = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["count"], 1);
    }

    #[tokio::test]
    async fn the_json_sink_creates_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/nested/out.json");
        let sink = Json::new(&path);
        sink.prepare().await.unwrap();
        let a = one("first");
        sink.write(&[&a]).await.unwrap();
        sink.finish().await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn the_sinks_declare_their_own_medium() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Jsonl::new(dir.path().join("a")).kind(), SinkMedium::Jsonl);
        assert_eq!(Json::new(dir.path().join("a")).kind(), SinkMedium::Json);
    }
}
