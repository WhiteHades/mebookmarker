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
        Self {
            path: path.into(),
            items: Mutex::new(Vec::new()),
            title: title.into(),
        }
    }
}

impl Clone for Opml {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            items: Mutex::new(Vec::new()),
            title: self.title.clone(),
        }
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
            let mut buffer = self
                .items
                .lock()
                .map_err(|_| mbm_core::Error::Sink("the opml sink's buffer is poisoned".to_owned()))?;
            std::mem::take(&mut *buffer)
        };
        let count = items.len();
        let refs: Vec<&Bookmark> = items.iter().collect();
        let body = opml_document(&refs, &self.title);
        write_to(&self.path, body.as_bytes())?;
        Ok(SinkReport { written: count, files: 1, ..SinkReport::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::bookmark::{CategoryAssignment, SourceRef};
    use mbm_core::medium::SourceMedium;
    use url::Url;

    fn one(text: &str) -> Bookmark {
        let url = Url::parse("https://x.com/a/status/1").unwrap();
        let mut b =
            Bookmark::new(SourceRef::new(SourceMedium::X, "1", Some(url.clone())), text, 1_767_400_000_000)
                .created_at(1_767_312_000_000);
        b.url = Some(url);
        b
    }

    #[test]
    fn the_document_is_well_formed() {
        let a = one("first");
        let mut b = one("second");
        b.source = SourceRef::new(SourceMedium::Reddit, "2", None);
        let doc = opml_document(&[&a, &b], "my archive");
        assert!(doc.starts_with("<?xml"));
        assert!(doc.contains("<title>my archive</title>"));
        assert_eq!(doc.matches("<outline").count(), 4, "two folders and two items:\n{doc}");
        assert!(doc.trim_end().ends_with("</opml>"));
    }

    #[test]
    fn an_item_is_filed_under_its_first_category() {
        let mut b = one("a post");
        assert_eq!(opml_folder(&b), "x");
        b.categories.push(CategoryAssignment {
            slug: "engineering".to_owned(),
            confidence: 1.0,
            assigned_by: mbm_core::bookmark::Assigner::Rule,
        });
        b.categories.push(CategoryAssignment {
            slug: "reading".to_owned(),
            confidence: 0.5,
            assigned_by: mbm_core::bookmark::Assigner::Jev,
        });
        assert_eq!(opml_folder(&b), "engineering");
    }

    #[test]
    fn a_source_collection_is_the_folder_when_there_is_no_category() {
        let mut b = one("a post");
        b.source = b.source.in_collection("rust");
        assert_eq!(opml_folder(&b), "rust");
    }

    #[test]
    fn an_outline_carries_a_url_and_a_date() {
        let line = opml_outline(&one("a post"), 1);
        assert!(line.contains(" url=\"https://x.com/a/status/1\""), "{line}");
        assert!(line.contains(" addDate=\"1767312000\""), "{line}");
        assert!(line.starts_with("    <outline"), "{line}");
    }

    #[test]
    fn a_title_with_a_quote_is_escaped() {
        let mut b = one("a post");
        b.title = Some(r#"he said "hi" & left"#.to_owned());
        let line = opml_outline(&b, 0);
        assert!(!line.contains(r#" "hi" "#), "{line}");
        assert!(line.contains("&amp;"), "{line}");
        assert!(line.contains("&quot;"), "{line}");
    }

    #[test]
    fn a_folder_name_with_a_quote_is_escaped() {
        let mut b = one("a post");
        b.categories.push(CategoryAssignment {
            slug: r#"a "folder""#.to_owned(),
            confidence: 1.0,
            assigned_by: mbm_core::bookmark::Assigner::Rule,
        });
        let doc = opml_document(&[&b], "archive");
        assert!(doc.contains("&quot;folder&quot;"), "{doc}");
    }

    #[test]
    fn an_item_with_no_url_still_has_an_outline() {
        let mut b = one("a post");
        b.url = None;
        let line = opml_outline(&b, 0);
        assert!(!line.contains(" url="), "{line}");
        assert!(line.contains("<outline"), "{line}");
    }

    #[test]
    fn a_summary_and_author_go_in_the_description() {
        let mut b = one("a post");
        b.author = Some(mbm_core::bookmark::Author::new("simonw"));
        b.links.push(mbm_core::bookmark::Link {
            original: Url::parse("https://example.com/a").unwrap(),
            resolved: Url::parse("https://example.com/a").unwrap(),
            kind: mbm_core::medium::LinkKind::Article,
            title: None,
            body: None,
            summary: Some("a summary".to_owned()),
            blocked: None,
        });
        let line = opml_outline(&b, 0);
        assert!(line.contains("<description>a summary — @simonw</description>"), "{line}");
    }

    #[test]
    fn tags_travel_in_the_tags_attribute() {
        let mut b = one("a post");
        b.push_tag("rust");
        let line = opml_outline(&b, 0);
        assert!(line.contains(" tags=\"rust\""), "{line}");
    }

    #[test]
    fn an_empty_archive_is_still_a_document() {
        let doc = opml_document(&[], "archive");
        assert!(doc.contains("<opml"));
        assert!(doc.contains("</opml>"));
        assert!(!doc.contains("<outline"));
    }

    #[tokio::test]
    async fn the_opml_sink_writes_a_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.opml");
        let sink = Opml::new(&path, "archive");
        let a = one("a post");
        sink.write(&[&a]).await.unwrap();
        let report = sink.finish().await.unwrap();
        assert_eq!(report.written, 1);
        assert!(std::fs::read_to_string(&path).unwrap().contains("<opml"));
    }

    #[tokio::test]
    async fn the_sink_declares_its_medium() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Opml::new(dir.path().join("a"), "t").kind(), SinkMedium::Opml);
    }
}
