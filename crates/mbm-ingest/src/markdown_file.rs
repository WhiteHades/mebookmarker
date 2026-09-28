//! read the markdown shape `bookmarks.md` is written in.
//!
//! this is a real file with real data in it, so the parser is written to the
//! file rather than to a specification. the shape is:
//!
//! ```text
//! # Sunday, January 4, 2026
//!
//! ## @trq212 - AI alignment and interpretability resources
//! > the first line of the post
//! >
//! > the rest of it
//!
//! - **Tweet:** https://x.com/trq212/status/2007903193158881323
//! - **Link:** https://odysser.com/
//! - **What:** what the entry is about
//!
//! ---
//! ```
//!
//! three things the parser has to get right, all of them about the header:
//!
//! - `#` is a date and `##` is an entry. a `#` inside a quoted post is a
//!   heading in someone else's text, and it is indented or quoted so it never
//!   starts a line at column zero.
//! - the entry title is `@handle - summary`, and the handle is the author.
//! - the `> What:` line is a note the person wrote, which is worth more than
//!   anything a model would generate from the same text, so it is kept as the
//!   bookmark's own text when there is no other body.

use mbm_core::bookmark::{Author, Bookmark, Link, SourceRef};
use mbm_core::error::{Error, Result};
use mbm_core::medium::{LinkKind, SourceMedium};
use std::collections::BTreeSet;
use std::path::Path;
use url::Url;

/// one entry from the file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    /// the day heading the entry sat under.
    pub day: Option<String>,
    /// the author, without the `@`.
    pub author: Option<String>,
    /// the title, with the handle stripped off the front.
    pub title: String,
    /// the quoted post body.
    pub text: String,
    /// the person's own note, from `- **What:**`.
    pub note: Option<String>,
    /// the post itself, from `- **Tweet:**`.
    pub post: Option<String>,
    /// the post being replied to, from `- **Parent:**`.
    pub parent: Option<String>,
    /// the post being quoted, from `- **Quoted:**`.
    pub quoted: Option<String>,
    /// the person's own write-up, from `- **Filed:**`, as written.
    pub filed: Option<String>,
    /// a free-text note about attached media, from `- **Media:**`.
    pub media_note: Option<String>,
    /// the other links, from `- **Link:**` and `- **Links:**`.
    pub links: Vec<(String, String)>,
}

/// read the whole file.
pub fn parse(body: &str) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    let mut day: Option<String> = None;
    let mut current: Option<Entry> = None;

    for line in body.lines() {
        // a level-one heading is a date
        if let Some(rest) = line.strip_prefix("# ") {
            if let Some(previous) = current.take() {
                out.push(previous);
            }
            day = Some(rest.trim().to_owned());
            continue;
        }

        // a level-two heading opens an entry
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some(previous) = current.take() {
                out.push(previous);
            }
            current = Some(open(rest.trim(), day.clone()));
            continue;
        }

        let Some(entry) = current.as_mut() else { continue };
        let trimmed = line.trim();

        // `---` closes an entry, which is what the file uses as its separator
        if trimmed == "---" {
            continue;
        }

        // a `- **Label:** value` line
        if let Some((label, value)) = labelled(trimmed) {
            match label.as_str() {
                "what" => entry.note = Some(value),
                "tweet" | "post" => entry.post = Some(value),
                "parent" => entry.parent = Some(value),
                "quoted" => entry.quoted = Some(value),
                "filed" => entry.filed = Some(value),
                "media" => entry.media_note = Some(value),
                // a plural line holds several markdown links, comma separated
                "links" => entry.links.extend(markdown_links(&value)),
                _ => entry.links.push((label, value)),
            }
            continue;
        }

        // a `> ` line is the post's own text
        if let Some(quoted) = trimmed.strip_prefix("> ") {
            if !entry.text.is_empty() {
                entry.text.push('\n');
            }
            entry.text.push_str(quoted);
            continue;
        }
        if trimmed == ">" {
            if !entry.text.is_empty() {
                entry.text.push('\n');
            }
            continue;
        }

        // anything else inside an entry is a continuation of the body
        if !trimmed.is_empty() && !entry.text.is_empty() {
            entry.text.push('\n');
            entry.text.push_str(trimmed);
        }
    }
    if let Some(previous) = current {
        out.push(previous);
    }

    let out = dedupe(out);
    if out.is_empty() && !body.trim().is_empty() {
        return Err(Error::Ingest(
            "the file has content but nothing in it parsed as a bookmark. expected `#` date \
             headings and `## @handle - title` entries."
                .to_owned(),
        ));
    }
    Ok(out)
}

/// start an entry from its heading.
fn open(heading: &str, day: Option<String>) -> Entry {
    let (author, title) = split_author(heading);
    Entry { day, author, title, ..Entry::default() }
}

/// split `@handle - the rest` into its two halves.
///
/// the separator is a dash with spaces around it, which is what the file uses.
/// a handle with no dash after it has a title of nothing, which is still an
/// entry and still worth keeping.
fn split_author(heading: &str) -> (Option<String>, String) {
    let trimmed = heading.trim_start();
    if let Some(rest) = trimmed.strip_prefix('@') {
        let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len());
        let handle = &rest[..end];
        let title = rest[end..].trim_start().trim_start_matches('-').trim();
        return (
            Some(handle.to_owned()),
            if title.is_empty() { format!("@{handle}") } else { title.to_owned() },
        );
    }
    (None, trimmed.to_owned())
}

/// read a `- **Label:** value` line.
fn labelled(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("- ")?;
    let bold = rest.strip_prefix("**")?;
    let end = bold.find(":**")?;
    let label = bold[..end].to_ascii_lowercase();
    let value = bold[end + 3..].trim().to_owned();
    Some((label, value))
}

/// what a label means, in the link taxonomy.
#[must_use]
pub fn kind_of(label: &str) -> LinkKind {
    match label {
        "tweet" | "post" | "parent" => LinkKind::Post,
        "quoted" | "thread" => LinkKind::Thread,
        "paper" => LinkKind::Paper,
        "video" => LinkKind::Video,
        "repo" => LinkKind::Repository,
        "link" => LinkKind::Article,
        _ => LinkKind::Unknown,
    }
}

/// pull `[label](url)` pairs out of a comma-separated line.
///
/// a markdown link's own label may contain a comma, so the split is on `, ` at
/// the top level rather than on every comma. in practice the labels are one word
/// and the simple split is right; the loop exists so a label with a comma in it
/// does not produce a broken pair.
fn markdown_links(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(open) = rest.find("](") {
        let after = &rest[open + 2..];
        let Some(close) = after.find(')') else { break };
        let url = after[..close].trim().to_owned();
        // the label runs back to the `[` that opened it
        let label_start = rest[..open].rfind('[').map_or(0, |i| i + 1);
        let label = rest[label_start..open].trim().to_owned();
        if !url.is_empty() {
            out.push((if label.is_empty() { url.clone() } else { label }, url));
        }
        rest = &after[close + 1..];
    }
    out
}

/// turn one entry into a bookmark.
///
/// the note becomes the body when there is no quoted post, because a person's own
/// words about why they saved something is the most useful text in the file, and
/// a search for it should find the entry.
///
/// `archive_dir` is where the archive file sat, because `- **Filed:**` is a
/// relative markdown link to a file in a `knowledge/` folder beside it, and the
/// store wants a url.
#[must_use]
pub fn to_bookmark_in(entry: &Entry, created: i64, archive_dir: Option<&Path>) -> Bookmark {
    let post = entry.post.as_deref().or(entry.quoted.as_deref());
    let external_id = post.map_or_else(
        || format!("{}:{}", entry.author.as_deref().unwrap_or("entry"), entry.title),
        str::to_owned,
    );
    let parsed = post.and_then(|url| Url::parse(url).ok());

    let source = SourceRef::new(SourceMedium::MarkdownFile, external_id, parsed.clone());
    let text = match (&entry.note, entry.text.trim().is_empty()) {
        (Some(note), false) => format!("{note}\n\n{}", entry.text.trim()),
        (Some(note), true) => note.clone(),
        (None, false) => entry.text.trim().to_owned(),
        (None, true) => entry.title.clone(),
    };

    let mut bookmark = Bookmark::new(source, text, created).created_at(created);
    bookmark.title = Some(entry.title.clone());
    bookmark.url = parsed;

    if let Some(handle) = &entry.author {
        // the heading writes the handle with its `@`; the entry holds it without,
        // so both the author and the tag get it back
        let handle = format!("@{handle}");
        bookmark.author = Some(Author::new(&handle));
        bookmark.push_tag(handle);
    }

    // the post being replied to, and the post being quoted: both are context
    // for this entry and neither is the entry itself
    if let Some(parent) = &entry.parent
        && let Ok(parsed) = Url::parse(parent)
    {
        bookmark.role = Some(mbm_core::bookmark::ThreadRole::Reply);
        bookmark.push_tag("reply");
        push(&mut bookmark, parsed, LinkKind::Post, None);
    }
    if let Some(quoted) = &entry.quoted
        && let Ok(parsed) = Url::parse(quoted)
    {
        bookmark.push_tag("quote");
        push(&mut bookmark, parsed, LinkKind::Thread, None);
    }

    for (label, url) in &entry.links {
        let kind = kind_of(label);
        if let Ok(parsed) = Url::parse(url) {
            push(&mut bookmark, parsed, kind, None);
        } else if let Some(host) =
            Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned))
        {
            bookmark.push_tag(host);
        }
    }

    // the person's own write-up, which lives beside the archive file
    if let Some(filed) = &entry.filed {
        let target = filed_target(filed, archive_dir);
        if let Ok(parsed) = Url::parse(&target) {
            push(&mut bookmark, parsed, LinkKind::Article, Some(filed.clone()));
        }
        bookmark.push_tag("filed");
    }

    if let Some(media) = &entry.media_note {
        // free text, not a url: a note that the entry has a video or a set of
        // images attached
        bookmark.push_tag("has-media");
        for word in media.split_whitespace().filter(|w| w.len() > 3) {
            bookmark.push_tag(word.to_ascii_lowercase());
        }
    }

    bookmark
}

/// turn one entry into a bookmark, with no archive directory.
#[must_use]
pub fn to_bookmark(entry: &Entry, created: i64) -> Bookmark {
    to_bookmark_in(entry, created, None)
}

/// add a link, keeping the list free of duplicates.
fn push(bookmark: &mut Bookmark, url: Url, kind: LinkKind, title: Option<String>) {
    if bookmark.links.iter().any(|l| l.resolved == url) {
        return;
    }
    if let Some(host) = url.host_str() {
        bookmark.push_tag(host);
    }
    bookmark.links.push(Link {
        original: url.clone(),
        resolved: url,
        kind,
        title,
        body: None,
        summary: None,
        blocked: None,
    });
}

/// where a `- **Filed:**` line points.
///
/// the file writes a relative markdown link, `[label](./knowledge/articles/x.md)`.
/// resolved against the archive file's directory, that is a real path, and a
/// `file://` url is what a reader can open.
#[must_use]
pub fn filed_target(filed: &str, archive_dir: Option<&Path>) -> String {
    let raw = filed_target_path(filed);
    let Some(dir) = archive_dir.filter(|d| !d.as_os_str().is_empty()) else {
        return raw;
    };
    // a relative target is joined onto the directory the archive sat in, and an
    // absolute one is left alone. either way the result is a path, and a path
    // only becomes a link when it is a `file:` url.
    let path = Path::new(&raw);
    let joined = if path.is_absolute() { path.to_path_buf() } else { dir.join(path) };
    Url::from_file_path(&joined)
        .map_or_else(|()| joined.to_string_lossy().into_owned(), |u| u.to_string())
}

/// the path part of a `- **Filed:**` line.
#[must_use]
pub fn filed_target_path(filed: &str) -> String {
    let raw = filed.trim();
    if let Some(open) = raw.find("](")
        && let Some(close) = raw[open + 2..].find(')')
    {
        return raw[open + 2..open + 2 + close].trim().to_owned();
    }
    raw.to_owned()
}

/// the same post quoted in two entries is one bookmark.
///
/// the file quotes a post in the entry that discusses it and bookmarks the same
/// post on its own day, so the same url appears more than once. the first entry
/// wins, because that is the order the file is read in and the order a person
/// wrote it.
#[must_use]
pub fn dedupe(entries: Vec<Entry>) -> Vec<Entry> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let key = entry.post.as_deref().map_or_else(
            || format!("{}:{}", entry.author.as_deref().unwrap_or(""), entry.title),
            str::to_owned,
        );
        if seen.insert(key) {
            out.push(entry);
        }
    }
    out
}

/// the day heading, as a date.
///
/// the file writes `Sunday, January 4, 2026`; the month and day names are the
/// only part that has to be understood, and the answer is a unix millisecond
/// value so the store can order by it.
#[must_use]
pub fn day_to_unix_ms(day: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let lower = day.to_ascii_lowercase();
    // the heading opens with the weekday, so the month is somewhere in it
    let month = MONTHS.iter().position(|name| lower.contains(name)).map_or(0, |i| i as i64 + 1);

    // the first and second numbers in the string, in that order
    let numbers: Vec<i64> = lower
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    let (day_number, year) = (numbers.first().copied()?, numbers.get(1).copied()?);

    Some((days_from_civil(year, month, day_number) * 86_400) * 1_000)
}

/// days from 1970-01-01 to a civil date.
///
/// the same shift the sinks use, and the reason this file needs no date crate
/// for the only date the archive file carries.
#[must_use]
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
