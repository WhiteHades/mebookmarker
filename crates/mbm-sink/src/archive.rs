//! the archive sink: the raw source payload, kept as it arrived.
//!
//! everything else in this crate is a rendering, and a rendering is a choice
//! that a later version may make differently. this one is not a rendering: it
//! is the bytes a source gave us, filed under the source's own id, so a future
//! mebookmarker can re-parse an item with a better parser and get back the item
//! it would have produced.
//!
//! the layout is the reason it is worth keeping:
//!
//! ```text
//! archive/
//!   index.jsonl          one line per file, for finding something without walking
//!   x/1.json             the payload, unmodified
//!   reddit/abc.json
//! ```
//!
//! one file per bookmark is a lot of files, and that is the point: it is a
//! format every other tool can read, and a partial copy is still a valid copy.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mbm_core::bookmark::Bookmark;
use mbm_core::error::{Error, Result};
use mbm_core::medium::{SinkMedium, SourceMedium};
use mbm_core::port::{Sink, SinkReport};
use serde::{Deserialize, Serialize};

use crate::{display_title, safe_filename, write_to};

/// the directory a medium's files go in.
///
/// one directory per medium, so a reader can take one source and leave the
/// rest. the name is the medium's own, which is stable across versions.
#[must_use]
pub fn medium_dir(medium: SourceMedium) -> &'static str {
    medium.name()
}

/// the filename one bookmark's payload gets.
///
/// the source's id is the right key and the wrong filename: ids contain slashes
/// (`t/1234`), colons, and occasionally a very long string. a readable prefix
/// plus a short hash keeps the path short, keeps a person able to recognise the
/// file, and makes the mapping unambiguous even when two ids share a prefix.
#[must_use]
pub fn payload_name(external_id: &str) -> String {
    let digest = blake3::hash(external_id.as_bytes());
    let short = &digest.to_hex()[..16];
    let stem: String = external_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect();
    if stem.is_empty() { format!("{short}.json") } else { format!("{stem}-{short}.json") }
}

/// one line of the index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// the source's own id.
    pub external_id: String,
    /// which medium it came from.
    pub medium: String,
    /// the file, relative to the archive root.
    pub file: String,
    /// the bookmark's id, as a decimal string.
    pub id: String,
    /// a title, so the index can be searched without opening anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// when it was written, in unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

/// build one index entry.
#[must_use]
pub fn index_entry(bookmark: &Bookmark) -> IndexEntry {
    let medium = bookmark.source.medium;
    IndexEntry {
        external_id: bookmark.source.external_id.clone(),
        medium: medium.name().to_owned(),
        file: format!("{}/{}", medium_dir(medium), payload_name(&bookmark.source.external_id)),
        id: bookmark.id.get().to_string(),
        title: Some(display_title(bookmark)),
        created_at: bookmark.created_at,
    }
}

/// what gets written for one bookmark.
///
/// the parsed fields plus the raw payload, because the parsed fields are what a
/// reader wants and the raw payload is what makes the archive future-proof.
#[must_use]
pub fn payload(bookmark: &Bookmark) -> serde_json::Value {
    serde_json::json!({
        "medium": bookmark.source.medium.name(),
        "external_id": bookmark.source.external_id,
        "collection": bookmark.source.collection,
        "id": bookmark.id.get().to_string(),
        "author": bookmark.author,
        "title": bookmark.title,
        "text": bookmark.text,
        "url": bookmark.url.as_ref().map(ToString::to_string),
        "created_at": bookmark.created_at,
        "ingested_at": bookmark.ingested_at,
        "tags": bookmark.tags.iter().cloned().collect::<Vec<_>>(),
        "role": bookmark.role,
        "fingerprint": bookmark.fingerprint,
        "raw": bookmark.raw,
    })
}

/// the archive sink.
#[derive(Debug, Default)]
pub struct Archive {
    root: PathBuf,
    entries: Mutex<Vec<IndexEntry>>,
    written: Mutex<usize>,
    skipped: Mutex<usize>,
}

impl Archive {
    /// write under a directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            entries: Mutex::new(Vec::new()),
            written: Mutex::new(0),
            skipped: Mutex::new(0),
        }
    }

    /// the archive root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// the path one bookmark's payload goes to.
    #[must_use]
    pub fn path_for(&self, bookmark: &Bookmark) -> PathBuf {
        self.root.join(index_entry(bookmark).file)
    }

    /// read an index back, for a caller that wants the archive as data.
    pub fn read_index(&self) -> Result<Vec<IndexEntry>> {
        let path = self.root.join("index.jsonl");
        let Ok(body) = std::fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).map_err(|e| Error::Sink(format!("{}: {e}", path.display())))
            })
            .collect()
    }

    /// read one payload back.
    pub fn read_payload(&self, entry: &IndexEntry) -> Result<serde_json::Value> {
        let path = self.root.join(&entry.file);
        let body = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
        serde_json::from_str(&body).map_err(|e| Error::Sink(format!("{}: {e}", path.display())))
    }
}

impl Clone for Archive {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            entries: Mutex::new(Vec::new()),
            written: Mutex::new(0),
            skipped: Mutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Sink for Archive {
    fn kind(&self) -> SinkMedium {
        SinkMedium::Archive
    }

    async fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(|e| Error::io(&self.root, e))?;
        for medium in SourceMedium::ALL {
            std::fs::create_dir_all(self.root.join(medium_dir(*medium)))
                .map_err(|e| Error::io(self.root.join(medium_dir(*medium)), e))?;
        }
        Ok(())
    }

    async fn write(&self, items: &[&Bookmark]) -> Result<SinkReport> {
        self.prepare().await?;
        let mut files = 0usize;
        let mut skipped = 0usize;
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| Error::Sink("the archive sink's index is poisoned".to_owned()))?;

        for item in items {
            // a payload with no raw source is a bookmark we re-read and rebuilt,
            // and there is nothing to archive that a json export would not
            // already carry
            if item.raw.is_none() {
                skipped += 1;
                continue;
            }
            let entry = index_entry(item);
            let body =
                serde_json::to_vec(&payload(item)).map_err(|e| Error::Sink(e.to_string()))?;
            write_to(&self.root.join(&entry.file), &body)?;
            entries.push(entry);
            files += 1;
        }

        *self
            .written
            .lock()
            .map_err(|_| Error::Sink("the archive sink's counter is poisoned".to_owned()))? +=
            files;
        *self
            .skipped
            .lock()
            .map_err(|_| Error::Sink("the archive sink's counter is poisoned".to_owned()))? +=
            skipped;

        Ok(SinkReport { written: files, files, skipped, failed: 0 })
    }

    async fn finish(&self) -> Result<SinkReport> {
        let entries = {
            let mut buffer = self
                .entries
                .lock()
                .map_err(|_| Error::Sink("the archive sink's index is poisoned".to_owned()))?;
            std::mem::take(&mut *buffer)
        };
        let mut body = String::new();
        for entry in &entries {
            body.push_str(&serde_json::to_string(entry).map_err(|e| Error::Sink(e.to_string()))?);
            body.push('\n');
        }
        write_to(&self.root.join("index.jsonl"), body.as_bytes())?;

        let written = self
            .written
            .lock()
            .map_err(|_| Error::Sink("the archive sink's counter is poisoned".to_owned()))?;
        let skipped = self
            .skipped
            .lock()
            .map_err(|_| Error::Sink("the archive sink's counter is poisoned".to_owned()))?;
        Ok(SinkReport { written: *written, files: *written + 1, skipped: *skipped, failed: 0 })
    }
}

/// a safe name for a directory entry, used by the callers that also write
/// human-facing copies.
#[must_use]
pub fn entry_name(title: &str) -> String {
    safe_filename(title, "bookmark")
}
