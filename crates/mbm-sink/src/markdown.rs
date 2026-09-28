//! markdown and obsidian: the two formats meant to be read.
//!
//! they are the same document with two differences. obsidian gets yaml
//! frontmatter, wikilinks, and a filename derived from the title, because that
//! is what its note graph indexes. plain markdown gets a heading and a body,
//! because that is what everything else reads.
//!
//! both write one file per bookmark under a directory, with a daily index at
//! the top. a single file for the whole archive is unreadable past a few
//! hundred items, and a directory is also what a git repository wants.

use std::path::PathBuf;
use std::sync::Mutex;

use mbm_core::bookmark::Bookmark;
use mbm_core::error::Result;
use mbm_core::medium::SinkMedium;
use mbm_core::port::{Sink, SinkReport};

use crate::{date_only, display_title, safe_filename, summary_of, write_to};

/// one bookmark, as a markdown document.
#[must_use]
pub fn markdown(bookmark: &Bookmark) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bookmark.text.len() + 400);
    let title = display_title(bookmark);
    let _ = write!(out, "# {title}\n\n");

    let mut meta: Vec<String> = Vec::new();
    if let Some(when) = bookmark.created_at.or(Some(bookmark.ingested_at)) {
        meta.push(format!("date: {}", date_only(when)));
    }
    if let Some(author) = &bookmark.author {
        meta.push(format!("author: {}", author.display()));
    }
    meta.push(format!("source: {}", bookmark.source.medium.name()));
    for category in &bookmark.categories {
        meta.push(format!("category: {}", category.slug));
    }
    if !bookmark.tags.is_empty() {
        meta.push(format!(
            "tags: {}",
            bookmark.tags.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    for line in &meta {
        let _ = writeln!(out, "- {line}");
    }
    out.push('\n');

    if let Some(url) = &bookmark.url {
        let _ = write!(out, "<{url}>\n\n");
    }

    if let Some(summary) = summary_of(bookmark) {
        let _ = write!(out, "> {summary}\n\n");
    }

    let body = bookmark.text.trim();
    if !body.is_empty() {
        out.push_str(body);
        out.push_str("\n\n");
    }

    let other: Vec<&mbm_core::bookmark::Link> =
        bookmark.links.iter().filter(|l| Some(&l.resolved) != bookmark.url.as_ref()).collect();
    if !other.is_empty() {
        out.push_str("## links\n\n");
        for link in other {
            let label = link
                .title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| link.resolved.as_str());
            match link.blocked {
                Some(reason) => {
                    let _ = writeln!(out, "- [{label}]({}) — {}", link.resolved, reason.name());
                }
                None => {
                    let _ = writeln!(out, "- [{label}]({})", link.resolved);
                }
            }
        }
    }

    if !bookmark.media.is_empty() {
        out.push_str("\n## media\n\n");
        for media in &bookmark.media {
            let _ = write!(
                out,
                "- ![{}]({})",
                media.alt_text.as_deref().unwrap_or(media.kind.name()),
                media.url
            );
            if let Some(alt) = &media.alt_text {
                let _ = write!(out, " — {alt}");
            }
            out.push('\n');
        }
    }

    out
}

/// one bookmark, as an obsidian note.
#[must_use]
pub fn obsidian(bookmark: &Bookmark) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bookmark.text.len() + 400);
    out.push_str("---\n");
    let _ = writeln!(out, "title: {}", yaml_string(&display_title(bookmark)));

    if let Some(when) = bookmark.created_at.or(Some(bookmark.ingested_at)) {
        let _ = writeln!(out, "date: {}", date_only(when));
        let _ = writeln!(out, "created: {when}");
    }
    if let Some(author) = &bookmark.author {
        let _ = writeln!(out, "author: {}", yaml_string(&author.display()));
        if let Some(name) = &author.name {
            let _ = writeln!(out, "display: {}", yaml_string(name));
        }
    }
    let _ = writeln!(out, "source: {}", bookmark.source.medium.name());
    let _ = writeln!(out, "id: {}", bookmark.source.external_id);
    if let Some(url) = &bookmark.url {
        let _ = writeln!(out, "url: {url}");
    }
    let categories: Vec<&str> = bookmark.categories.iter().map(|c| c.slug.as_str()).collect();
    if !categories.is_empty() {
        let _ = writeln!(out, "categories: [{}]", categories.join(", "));
    }
    if !bookmark.tags.is_empty() {
        let _ = writeln!(
            out,
            "tags: [{}]",
            bookmark.tags.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    out.push_str("---\n\n");

    // a wikilink to each category is what makes the note graph useful
    if !categories.is_empty() {
        let _ = write!(
            out,
            "{}\n\n",
            categories.iter().map(|c| format!("[[{c}]]")).collect::<Vec<_>>().join(" ")
        );
    }
    if let Some(author) = &bookmark.author {
        let _ = write!(out, "{}\n\n", author.display());
    }

    let body = bookmark.text.trim();
    if !body.is_empty() {
        out.push_str(body);
        out.push_str("\n\n");
    }

    out
}

/// quote a string for a yaml scalar.
///
/// a single-quoted yaml string only needs the apostrophe doubled, which is a
/// smaller and more readable change than escaping every backslash.
#[must_use]
pub fn yaml_string(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "''"))
}

/// the filename a bookmark's note gets.
#[must_use]
pub fn note_name(bookmark: &Bookmark) -> String {
    let title = display_title(bookmark);
    let slug = safe_filename(&title, "bookmark");
    let day =
        bookmark.created_at.or(Some(bookmark.ingested_at)).map_or_else(String::new, date_only);
    if day.is_empty() { slug } else { format!("{day} {slug}") }
}

/// a daily index, newest day first.
#[must_use]
pub fn index(items: &[(String, &Bookmark)]) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("# archive\n\n");
    let mut days: Vec<String> = Vec::new();
    for (name, _) in items {
        if let Some((day, _)) = name.split_once(' ')
            && !days.contains(&day.to_owned())
        {
            days.push(day.to_owned());
        }
    }
    days.sort_unstable();
    days.reverse();

    for day in days {
        let _ = write!(out, "## {day}\n\n");
        for (name, bookmark) in items.iter().filter(|(_, b)| {
            b.created_at.or(Some(b.ingested_at)).is_some_and(|ms| date_only(ms) == day)
        }) {
            let author =
                bookmark.author.as_ref().map(|a| format!(" — {}", a.display())).unwrap_or_default();
            let _ = writeln!(out, "- [[{name}]]{author}");
        }
        out.push('\n');
    }
    out
}

/// the directory sink, shared by both formats.
#[derive(Debug, Default)]
pub struct Markdown {
    root: PathBuf,
    obsidian: bool,
    written: Mutex<usize>,
}

impl Markdown {
    /// write plain markdown under a directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), obsidian: false, written: Mutex::new(0) }
    }

    /// write obsidian notes under a directory.
    #[must_use]
    pub fn obsidian(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), obsidian: true, written: Mutex::new(0) }
    }

    /// the root directory.
    #[must_use]
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// whether this sink writes obsidian notes.
    #[must_use]
    pub fn is_obsidian(&self) -> bool {
        self.obsidian
    }

    /// render one bookmark.
    #[must_use]
    pub fn render(&self, bookmark: &Bookmark) -> String {
        if self.obsidian { obsidian(bookmark) } else { markdown(bookmark) }
    }
}

impl Clone for Markdown {
    fn clone(&self) -> Self {
        Self { root: self.root.clone(), obsidian: self.obsidian, written: Mutex::new(0) }
    }
}

#[async_trait::async_trait]
impl Sink for Markdown {
    fn kind(&self) -> SinkMedium {
        if self.obsidian { SinkMedium::Obsidian } else { SinkMedium::Markdown }
    }

    async fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(|e| mbm_core::Error::io(&self.root, e))?;
        Ok(())
    }

    async fn write(&self, items: &[&Bookmark]) -> Result<SinkReport> {
        self.prepare().await?;
        let mut files = 0usize;
        let mut names: Vec<(String, &Bookmark)> = Vec::with_capacity(items.len());

        for item in items {
            let name = note_name(item);
            let path = self.root.join(format!("{name}.md"));
            write_to(&path, self.render(item).as_bytes())?;
            names.push((name, item));
            files += 1;
        }

        write_to(&self.root.join("index.md"), index(&names).as_bytes())?;
        *self.written.lock().map_err(|_| {
            mbm_core::Error::Sink("the markdown sink's counter is poisoned".to_owned())
        })? += items.len();

        Ok(SinkReport { written: items.len(), files: files + 1, ..SinkReport::default() })
    }
}
