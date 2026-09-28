//! opml: the shape every feed reader still reads.
//!
//! an opml file is a folder tree, and that is all a reader understands. it is
//! also enough: a bookmark is an outline with a title, a url, and a folder.
//! anything else the archive knows about a bookmark goes into the
//! `description`, where a reader shows it and a script can read it, because an
//! attribute nobody reads is a place to put data that gets lost.
//!
//! the folder is the bookmark's first category, then the source's own
//! collection, then the medium. the categories are a filing decision, so they
//! belong in the tree, and the reader will show the same shape the tui does.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;

use mbm_core::bookmark::Bookmark;
use mbm_core::error::Result;
use mbm_core::medium::SinkMedium;
use mbm_core::port::{Sink, SinkReport};

use crate::{display_title, escape_xml, summary_of, write_to};

/// the folder a bookmark is filed under.
#[must_use]
pub fn opml_folder(bookmark: &Bookmark) -> String {
    if let Some(category) = bookmark.categories.first() {
        return category.slug.clone();
    }
    if let Some(collection) = &bookmark.source.collection {
        return collection.clone();
    }
    bookmark.source.medium.name().to_owned()
}

/// one `<outline>` line, at a given nesting depth.
#[must_use]
pub fn opml_outline(bookmark: &Bookmark, depth: usize) -> String {
    let indent = "    ".repeat(depth);
    let title = escape_xml(&display_title(bookmark));

    let url = bookmark
        .url
        .as_ref()
        .map(ToString::to_string)
        .or_else(|| bookmark.links.first().map(|l| l.resolved.to_string()));
    let url_attr = url.map_or_else(String::new, |u| format!(" url=\"{}\"", escape_xml(&u)));
    // opml's `addDate` is in seconds, and every reader divides by a thousand
    // before showing it
    let date_attr = bookmark
        .created_at
        .map_or_else(String::new, |ms| format!(" addDate=\"{}\"", ms.div_euclid(1000)));
    let tag_attr = if bookmark.tags.is_empty() {
        String::new()
    } else {
        format!(
            " tags=\"{}\"",
            escape_xml(&bookmark.tags.iter().cloned().collect::<Vec<_>>().join(","))
        )
    };

    let mut note = String::new();
    if let Some(summary) = summary_of(bookmark) {
        let _ = write!(note, "{summary}");
    }
    if let Some(author) = &bookmark.author {
        let _ = write!(note, " — @{}", author.handle);
    }
    let body = if note.is_empty() {
        String::new()
    } else {
        format!("<description>{}</description>", escape_xml(&note))
    };

    format!(
        "{indent}<outline type=\"rss\" text=\"{title}\" title=\"{title}\"{url_attr}{date_attr}{tag_attr}>{body}</outline>\n"
    )
}

/// the whole document, with one folder per category.
#[must_use]
pub fn opml_document(items: &[&Bookmark], title: &str) -> String {
    let mut folders: Vec<String> = Vec::new();
    for item in items {
        let folder = opml_folder(item);
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    folders.sort();

    let mut out = String::with_capacity(items.len() * 200);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<opml version=\"2.0\">\n");
    let _ = writeln!(out, "  <head><title>{}</title></head>", escape_xml(title));
    out.push_str("  <body>\n");
    for folder in &folders {
        let _ = writeln!(out, "    <outline text=\"{0}\" title=\"{0}\">", escape_xml(folder));
        for item in items.iter().filter(|b| opml_folder(b) == *folder) {
            out.push_str(&opml_outline(item, 2));
        }
        out.push_str("    </outline>\n");
    }
    out.push_str("  </body>\n</opml>\n");
    out
}

/// the opml sink.
#[derive(Debug, Default)]
pub struct Opml {
    path: PathBuf,
    items: Mutex<Vec<Bookmark>>,
    title: String,
}

impl Opml {
    /// write to a path, under a document title.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, title: impl Into<String>) -> Self {
        Self { path: path.into(), items: Mutex::new(Vec::new()), title: title.into() }
    }
}

impl Clone for Opml {
    fn clone(&self) -> Self {
        Self { path: self.path.clone(), items: Mutex::new(Vec::new()), title: self.title.clone() }
    }
}

#[async_trait::async_trait]
impl Sink for Opml {
    fn kind(&self) -> SinkMedium {
        SinkMedium::Opml
    }

    async fn write(&self, items: &[&Bookmark]) -> Result<SinkReport> {
        let mut buffer = self
            .items
            .lock()
            .map_err(|_| mbm_core::Error::Sink("the opml sink's buffer is poisoned".to_owned()))?;
        buffer.extend(items.iter().map(|b| (*b).clone()));
        // the count is reported once, at `finish`, so a caller that adds two
        // reports together does not see every bookmark twice
        Ok(SinkReport::default())
    }

    async fn finish(&self) -> Result<SinkReport> {
        let items = {
            let mut buffer = self.items.lock().map_err(|_| {
                mbm_core::Error::Sink("the opml sink's buffer is poisoned".to_owned())
            })?;
            std::mem::take(&mut *buffer)
        };
        let count = items.len();
        let refs: Vec<&Bookmark> = items.iter().collect();
        let body = opml_document(&refs, &self.title);
        write_to(&self.path, body.as_bytes())?;
        Ok(SinkReport { written: count, files: 1, ..SinkReport::default() })
    }
}
