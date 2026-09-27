//! csv: the shape a spreadsheet still reads.
//!
//! one row per bookmark, a fixed column set, and a header row that names every
//! column. fixed matters: a csv with a variable column count is a csv nobody can
//! write a formula in. quoting follows rfc 4180, which the `csv` crate already
//! implements and every reader already parses.

use std::path::PathBuf;

use mbm_core::bookmark::Bookmark;
use mbm_core::error::Result;
use mbm_core::medium::SinkMedium;
use mbm_core::port::{Sink, SinkReport};

use crate::{display_title, iso8601, summary_of, write_to};

/// the columns, in order.
///
/// a fixed set, because a spreadsheet with a variable column count is a
/// spreadsheet nobody can write a formula in.
pub const COLUMNS: &[&str] = &[
    "id",
    "created_at",
    "medium",
    "author",
    "title",
    "url",
    "tags",
    "categories",
    "text",
    "summary",
    "links",
];

/// render one bookmark as a csv row.
#[must_use]
pub fn csv_row(bookmark: &Bookmark) -> String {
    let links = bookmark
        .links
        .iter()
        .map(|l| l.resolved.as_str())
        .collect::<Vec<_>>()
        .join(" ");

    let mut fields: Vec<String> = vec![
        bookmark.id.get().to_string(),
        bookmark
            .created_at
            .map_or_else(String::new, iso8601),
        bookmark.source.medium.name().to_owned(),
        bookmark
            .author
            .as_ref()
            .map_or_else(String::new, |a| a.handle.clone()),
        display_title(bookmark),
        bookmark.url.as_ref().map_or_else(String::new, ToString::to_string),
        bookmark.tags.iter().cloned().collect::<Vec<_>>().join(" "),
        bookmark
            .categories
            .iter()
            .map(|c| c.slug.clone())
            .collect::<Vec<_>>()
            .join(" "),
        bookmark.text.clone(),
        summary_of(bookmark).unwrap_or_default().to_owned(),
        links,
    ];
    // a row longer than the header would silently break every column after the
    // point where the two disagree
    fields.truncate(COLUMNS.len());
    fields.push(String::new());
    fields.truncate(COLUMNS.len());

    let mut row = String::new();
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            row.push(',');
        }
        row.push_str(&quote(field));
    }
    row
}

/// quote a csv field.
///
/// a field is quoted when it contains a delimiter, a quote, or a newline, and
/// an embedded quote is doubled. that is the whole of rfc 4180.
#[must_use]
pub fn quote(raw: &str) -> String {
    let needs = raw.contains([',', '"', '\n', '\r']);
    if !needs {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// the whole document, header included.
#[must_use]
pub fn csv_document(items: &[&Bookmark]) -> String {
    let mut out = String::new();
    for column in COLUMNS {
        if *column != COLUMNS[0] {
            out.push(',');
        }
        out.push_str(column);
    }
    out.push('\n');
    for item in items {
        out.push_str(&csv_row(item));
        out.push('\n');
    }
    out
}

/// the csv sink.
#[derive(Debug, Default)]
pub struct Csv {
    path: PathBuf,
    items: std::sync::Mutex<Vec<String>>,
}

impl Csv {
    /// write to a path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), items: std::sync::Mutex::new(Vec::new()) }
    }
}

#[async_trait::async_trait]
impl Sink for Csv {
    fn kind(&self) -> SinkMedium {
        SinkMedium::Csv
    }

    async fn write(&self, items: &[&Bookmark]) -> Result<SinkReport> {
        let mut buffer = self
            .items
            .lock()
            .map_err(|_| mbm_core::Error::Sink("the csv sink's buffer is poisoned".to_owned()))?;
        for item in items {
            buffer.push(csv_row(item));
        }
        Ok(SinkReport::default())
    }

    async fn finish(&self) -> Result<SinkReport> {
        let rows = {
            let mut buffer = self
                .items
                .lock()
                .map_err(|_| mbm_core::Error::Sink("the csv sink's buffer is poisoned".to_owned()))?;
            std::mem::take(&mut *buffer)
        };
        let count = rows.len();
        let mut body = String::new();
        for column in COLUMNS {
            if *column != COLUMNS[0] {
                body.push(',');
            }
            body.push_str(column);
        }
        body.push('\n');
        for row in rows {
            body.push_str(&row);
            body.push('\n');
        }
        write_to(&self.path, body.as_bytes())?;
        Ok(SinkReport { written: count, files: 1, ..SinkReport::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::bookmark::{Link, SourceRef};
    use mbm_core::medium::{LinkKind, SourceMedium};
    use url::Url;

    fn one(text: &str) -> Bookmark {
        let url = Url::parse("https://x.com/a/status/1").unwrap();
        let mut b = Bookmark::new(SourceRef::new(SourceMedium::X, "1", Some(url.clone())), text, 1_767_400_000_000)
            .created_at(1_767_312_000_000);
        b.url = Some(url);
        b
    }

    #[test]
    fn a_row_has_one_cell_per_column() {
        let b = one("a post");
        let row = csv_row(&b);
        let document = csv_document(&[&b]);
        let headers = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_reader(document.as_bytes())
            .headers()
            .unwrap()
            .clone();
        let mut records = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(row.as_bytes())
            .into_records();
        let record = records.next().unwrap().unwrap();
        assert_eq!(record.len(), headers.len());
        assert_eq!(&record[2], "x");
        assert_eq!(&record[3], "");
        assert_eq!(&record[8], "a post");
    }

    #[test]
    fn a_comma_in_the_text_is_quoted() {
        let row = csv_row(&one("one, two, three"));
        assert!(row.contains(r#""one, two, three""#), "{row}");
    }

    #[test]
    fn a_quote_in_the_text_is_doubled() {
        let row = csv_row(&one(r#"he said "hello""#));
        assert!(row.contains(r#""he said ""hello"""#), "{row}");
    }

    #[test]
    fn a_newline_in_the_text_is_quoted() {
        let row = csv_row(&one("one\ntwo"));
        assert!(row.contains("\"one\ntwo\""), "{row}");
    }

    #[test]
    fn a_plain_field_is_left_unquoted() {
        assert_eq!(quote("plain"), "plain");
    }

    #[test]
    fn the_document_starts_with_the_header() {
        let doc = csv_document(&[&one("a")]);
        assert!(doc.starts_with("id,created_at,medium,"), "{doc}");
    }

    #[test]
    fn the_tags_and_links_land_in_one_cell() {
        let mut b = one("a post");
        b.push_tag("rust");
        b.push_tag("databases");
        b.links.push(Link {
            original: Url::parse("https://example.com/a").unwrap(),
            resolved: Url::parse("https://example.com/a").unwrap(),
            kind: LinkKind::Article,
            title: None,
            body: None,
            summary: None,
            blocked: None,
        });
        let row = csv_row(&b);
        assert!(row.contains("databases rust"), "{row}");
        assert!(row.contains("https://example.com/a"), "{row}");
    }

    #[test]
    fn a_unicode_cell_survives_a_csv_parser() {
        let mut b = one("post");
        b.title = Some("日本語 — a title".to_owned());
        let doc = csv_document(&[&b]);
        let mut records = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(doc.as_bytes())
            .into_records();
        assert_eq!(&records.next().unwrap().unwrap()[4], "title", "the header row");
        let record = records.next().unwrap().unwrap();
        assert_eq!(&record[4], "日本語 — a title");
    }

}
