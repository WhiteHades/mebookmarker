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
    let links = bookmark.links.iter().map(|l| l.resolved.as_str()).collect::<Vec<_>>().join(" ");

    let mut fields: Vec<String> = vec![
        bookmark.id.get().to_string(),
        bookmark.created_at.map_or_else(String::new, iso8601),
        bookmark.source.medium.name().to_owned(),
        bookmark.author.as_ref().map_or_else(String::new, mbm_core::Author::display),
        display_title(bookmark),
        bookmark.url.as_ref().map_or_else(String::new, ToString::to_string),
        bookmark.tags.iter().cloned().collect::<Vec<_>>().join(" "),
        bookmark.categories.iter().map(|c| c.slug.clone()).collect::<Vec<_>>().join(" "),
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
            let mut buffer = self.items.lock().map_err(|_| {
                mbm_core::Error::Sink("the csv sink's buffer is poisoned".to_owned())
            })?;
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
