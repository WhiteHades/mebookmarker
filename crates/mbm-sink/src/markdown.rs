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
        meta.push(format!("author: {}", author.handle));
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
        let _ = writeln!(out, "author: {}", yaml_string(&author.handle));
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
        let _ = write!(out, "@{}\n\n", author.handle);
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
                bookmark.author.as_ref().map(|a| format!(" — @{}", a.handle)).unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::bookmark::{Link, Media, MediaKind, SourceRef};
    use mbm_core::medium::{LinkKind, SourceMedium};
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
    fn a_markdown_note_starts_with_its_title() {
        let doc = markdown(&one("a post"));
        assert!(doc.starts_with("# a post\n"), "{doc}");
        assert!(doc.contains("a post"));
    }

    #[test]
    fn a_markdown_note_lists_its_metadata() {
        let mut b = one("a post");
        b.author = Some(mbm_core::bookmark::Author::new("simonw"));
        b.push_tag("rust");
        let doc = markdown(&b);
        assert!(doc.contains("- date: 2026-01-02"), "{doc}");
        assert!(doc.contains("- author: simonw"), "{doc}");
        assert!(doc.contains("- source: x"), "{doc}");
        assert!(doc.contains("- tags: rust"), "{doc}");
    }

    #[test]
    fn a_markdown_note_links_its_url() {
        let doc = markdown(&one("a post"));
        assert!(doc.contains("<https://x.com/a/status/1>"), "{doc}");
    }

    #[test]
    fn a_generated_summary_is_a_blockquote() {
        let mut b = one("a post");
        b.links.push(Link {
            original: Url::parse("https://example.com/a").unwrap(),
            resolved: Url::parse("https://example.com/a").unwrap(),
            kind: LinkKind::Article,
            title: None,
            body: None,
            summary: Some("a one-line summary".to_owned()),
            blocked: None,
        });
        let doc = markdown(&b);
        assert!(doc.contains("> a one-line summary"), "{doc}");
    }

    #[test]
    fn a_markdown_note_lists_the_other_links() {
        let mut b = one("a post");
        b.links.push(Link {
            original: Url::parse("https://example.com/a").unwrap(),
            resolved: Url::parse("https://example.com/a").unwrap(),
            kind: LinkKind::Article,
            title: Some("The Article".to_owned()),
            body: None,
            summary: None,
            blocked: None,
        });
        let doc = markdown(&b);
        assert!(doc.contains("## links"), "{doc}");
        assert!(doc.contains("- [The Article](https://example.com/a)"), "{doc}");
    }

    #[test]
    fn a_blocked_link_says_why() {
        let mut b = one("a post");
        b.links.push(Link {
            original: Url::parse("https://nytimes.com/a").unwrap(),
            resolved: Url::parse("https://nytimes.com/a").unwrap(),
            kind: LinkKind::Article,
            title: None,
            body: None,
            summary: None,
            blocked: Some(mbm_core::bookmark::BlockedReason::Paywall),
        });
        let doc = markdown(&b);
        assert!(doc.contains("— paywall"), "{doc}");
    }

    #[test]
    fn a_markdown_note_embeds_its_media() {
        let mut b = one("a post");
        b.media.push(Media {
            kind: MediaKind::Photo,
            url: Url::parse("https://pbs.twimg.com/a.jpg").unwrap(),
            preview_url: None,
            width: None,
            height: None,
            duration_ms: None,
            alt_text: Some("a cat".to_owned()),
        });
        let doc = markdown(&b);
        assert!(doc.contains("## media"), "{doc}");
        assert!(doc.contains("![a cat](https://pbs.twimg.com/a.jpg)"), "{doc}");
    }

    #[test]
    fn an_obsidian_note_carries_frontmatter() {
        let mut b = one("a post");
        b.author = Some(mbm_core::bookmark::Author::new("simonw"));
        b.push_tag("rust");
        b.categories.push(mbm_core::bookmark::CategoryAssignment {
            slug: "engineering".to_owned(),
            confidence: 1.0,
            assigned_by: mbm_core::bookmark::Assigner::Rule,
        });
        let doc = obsidian(&b);
        assert!(doc.starts_with("---\n"), "{doc}");
        assert!(doc.contains("title: 'a post'"), "{doc}");
        assert!(doc.contains("author: 'simonw'"), "{doc}");
        assert!(doc.contains("categories: [engineering]"), "{doc}");
        assert!(doc.contains("tags: [rust]"), "{doc}");
        assert!(doc.contains("[[engineering]]"), "{doc}");
        assert!(doc.contains("@simonw"), "{doc}");
    }

    #[test]
    fn the_frontmatter_splits_on_the_first_double_dash_line() {
        let doc = obsidian(&one("a post"));
        let mut lines = doc.lines();
        assert_eq!(lines.next(), Some("---"));
        let end = doc.find("\n---\n").expect("a closing fence");
        let front = &doc[..end];
        assert!(front.contains("source: x"), "{front}");
    }

    #[test]
    fn a_yaml_string_doubles_its_apostrophes() {
        assert_eq!(yaml_string("it's here"), "'it''s here'");
        assert_eq!(yaml_string("plain"), "'plain'");
    }

    #[test]
    fn a_title_with_a_colon_does_not_break_the_frontmatter() {
        let mut b = one("a post");
        b.title = Some("Ratio: a study".to_owned());
        let doc = obsidian(&b);
        assert!(doc.contains("title: 'Ratio: a study'"), "{doc}");
    }

    #[test]
    fn a_note_name_starts_with_the_day() {
        assert!(note_name(&one("a post")).starts_with("2026-01-02 "));
    }

    #[test]
    fn a_note_name_drops_what_a_path_cannot_hold() {
        let mut b = one("a post");
        b.title = Some("a/b:c*d".to_owned());
        let name = note_name(&b);
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains(':'), "{name}");
    }

    #[test]
    fn the_index_groups_by_day_newest_first() {
        let mut a = one("first");
        a.created_at = Some(1_767_312_000_000);
        let mut b = one("second");
        b.created_at = Some(1_767_225_600_000);
        let names = vec![(note_name(&a), &a), (note_name(&b), &b)];
        let doc = index(&names);
        let first = doc.find("2026-01-02").unwrap();
        let second = doc.find("2026-01-01").unwrap();
        assert!(first < second, "the newer day comes first:\n{doc}");
        assert!(doc.contains("[[2026-01-02 first]]"), "{doc}");
    }

    #[tokio::test]
    async fn the_markdown_sink_writes_a_file_per_bookmark() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Markdown::new(dir.path());
        let a = one("first");
        let mut b = one("second");
        b.source = SourceRef::new(SourceMedium::Reddit, "2", None);
        let report = sink.write(&[&a, &b]).await.unwrap();
        assert_eq!(report.written, 2);
        assert_eq!(report.files, 3, "two notes and an index");

        let files: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(files.contains(&"index.md".to_owned()), "{files:?}");
        assert_eq!(files.len(), 3, "{files:?}");
    }

    #[tokio::test]
    async fn the_obsidian_sink_declares_its_medium() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Markdown::obsidian(dir.path());
        assert_eq!(sink.kind(), SinkMedium::Obsidian);
        assert!(sink.is_obsidian());
        let a = one("a post");
        sink.write(&[&a]).await.unwrap();
        let name = format!("{}.md", note_name(&a));
        let body = std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert!(body.starts_with("---"), "{body}");
    }

    #[tokio::test]
    async fn a_second_write_replaces_the_files_it_covers() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Markdown::new(dir.path());
        let mut a = one("first");
        a.title = Some("A title".to_owned());
        sink.write(&[&a]).await.unwrap();
        a.title = Some("A different title".to_owned());
        sink.write(&[&a]).await.unwrap();
        // both titles get a file, because a name is derived from the title and
        // a renamed note is a new file rather than an overwrite
        let files = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(files, 3, "one file per title, plus the index");
    }

    #[test]
    fn the_sink_render_matches_the_free_function() {
        let dir = tempfile::tempdir().unwrap();
        let a = one("a post");
        assert_eq!(Markdown::new(dir.path()).render(&a), markdown(&a));
        assert_eq!(Markdown::obsidian(dir.path()).render(&a), obsidian(&a));
    }
}
