//! the output formats.
//!
//! eight of them, one file per format, all of them pure: a sink takes
//! bookmarks and produces bytes. none of them touches the network, and none of
//! them holds state between calls except where a format genuinely needs to —
//! html opens a document at `prepare` and closes it at `finish`, and everything
//! else is a stream.
//!
//! the split that matters:
//!
//! - **structured** (`json`, `jsonl`, `csv`, `opml`) round-trips. a bookmark
//!   written to jsonl and read back is the same bookmark, so these are the
//!   formats to back an archive up with.
//! - **human** (`markdown`, `obsidian`, `html`) is for reading. these are the
//!   ones that want the enrichment, because a title and a summary are what make
//!   a page worth scrolling.
//! - **archive** is the raw source payload, so a future version can re-parse
//!   what an older one stored.

use std::collections::BTreeSet;
use std::path::Path;

use mbm_core::bookmark::Bookmark;
use mbm_core::error::{Error, Result};

pub mod archive;
pub mod csv;
pub mod html;
pub mod json;
pub mod markdown;
pub mod opml;

pub use archive::Archive;
pub use csv::Csv;
pub use html::Html;
pub use json::{Json, Jsonl};
pub use markdown::Markdown;
pub use opml::Opml;

/// the name a file should have, from a title.
///
/// the rules are the ones every filesystem already agrees on, so the same
/// archive produces the same names on linux, on macos, and on a windows share.
#[must_use]
pub fn safe_filename(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();

    // a name with no letter or digit in it is a legal filename and a useless
    // one, so it falls back with the rest
    if trimmed.is_empty() || !trimmed.chars().any(char::is_alphanumeric) {
        return fallback.to_owned();
    }

    // windows holds a path segment to 255 bytes and every other system is
    // happier with something shorter to read in a file listing
    let mut out = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        if out.len() + c.len_utf8() > 120 {
            break;
        }
        out.push(c);
    }
    let out = out.trim().trim_matches('.').trim();
    if out.is_empty() || !out.chars().any(char::is_alphanumeric) {
        fallback.to_owned()
    } else {
        out.to_owned()
    }
}

/// the title to show for a bookmark.
///
/// a generated title, then the first link's title, then the opening of the text,
/// then the url, then the source's own id. a bookmark always has something to be
/// called, because a blank heading in a list is worse than a ugly one.
#[must_use]
pub fn display_title(bookmark: &Bookmark) -> String {
    if let Some(title) = bookmark.title.as_deref().filter(|t| !t.trim().is_empty()) {
        return squash(title);
    }
    if let Some(link) = bookmark.links.iter().find_map(|l| l.title.as_deref()) {
        return squash(link);
    }
    if let Some(first) =
        bookmark.text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("http"))
    {
        return squash(first);
    }
    // a bare url is the last resort, and a whole one is unreadable as a heading.
    // the host plus its last path segment says which page it is.
    if let Some(url) = &bookmark.url {
        let host = url.host_str().unwrap_or_default();
        // a path usually ends in a slash, so the last split is usually empty;
        // the last one that is not is the page
        let last = url
            .path()
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .unwrap_or_default()
            .to_owned();
        if last.is_empty() {
            return squash(host);
        }
        return squash(&format!("{host} · {last}"));
    }
    format!("{} {}", bookmark.source.medium.name(), bookmark.source.external_id)
}

/// the one-line summary of a bookmark, if it has one.
#[must_use]
pub fn summary_of(bookmark: &Bookmark) -> Option<&str> {
    bookmark
        .links
        .iter()
        .find_map(|l| l.summary.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// collapse a string onto one line and cap it.
fn squash(raw: &str) -> String {
    let one = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= 120 {
        return one;
    }
    let cut: String = one.chars().take(117).collect();
    format!("{}…", cut.trim_end())
}

/// a date, in the local offset, as `2026-01-02`.
///
/// the day is what a reader means by "when" for a bookmark, and a full
/// timestamp in a heading is noise.
#[must_use]
pub fn date_only(unix_ms: i64) -> String {
    let (y, m, d) = civil_from(unix_ms.div_euclid(1000).div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// a timestamp with the hour, as `2026-01-02 10:00`.
#[must_use]
pub fn date_time(unix_ms: i64) -> String {
    let seconds = unix_ms.div_euclid(1000);
    let (y, m, d) = civil_from(seconds.div_euclid(86_400));
    let rest = seconds.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rest / 3600, (rest % 3600) / 60)
}

/// a `datetime` attribute value, in utc.
#[must_use]
pub fn iso8601(unix_ms: i64) -> String {
    let seconds = unix_ms.div_euclid(1000);
    let (y, m, d) = civil_from(seconds.div_euclid(86_400));
    let rest = seconds.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rest / 3600, (rest % 3600) / 60, rest % 60)
}

/// the unix day number to a `(year, month, day)` triple.
///
/// the inverse of the same conversion in `mbm-ingest`, and the reason that file
/// has no date dependency: an archive should be able to print a date without
/// pulling in a calendar.
#[must_use]
pub fn civil_from(days: i64) -> (i64, u32, u32) {
    // how many days from 0000-03-01 to `days`, the shift that puts the leap day
    // at the end of a year
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// write bytes to a path, creating the parent directories.
/// escape the five entities an xml or html document cares about.
///
/// `&` first: escaping it last would double-escape the ampersands the other four
/// replacements introduce.
#[must_use]
pub fn escape_xml(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// escape for html text, which does not need the apostrophe escaped.
#[must_use]
pub fn escape_html(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn write_to(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    std::fs::write(path, bytes).map_err(|e| Error::io(path, e))
}

/// the tags of a bookmark, sorted, as a `Vec`.
#[must_use]
pub fn tag_list(bookmark: &Bookmark) -> Vec<String> {
    let tags: BTreeSet<String> = bookmark.tags.iter().cloned().collect();
    tags.into_iter().collect()
}

/// join a list with a separator, skipping empties.
#[must_use]
pub fn join_all<I: IntoIterator<Item = String>>(items: I, sep: &str) -> String {
    items.into_iter().filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join(sep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mbm_core::bookmark::{Author, SourceRef};
    use mbm_core::medium::SourceMedium;
    use url::Url;

    fn one(text: &str) -> Bookmark {
        Bookmark::new(SourceRef::new(SourceMedium::X, "1", None), text, 0)
    }

    fn at(url: &str) -> Url {
        Url::parse(url).expect("a test url parses")
    }

    #[test]
    fn a_generated_title_wins() {
        let mut b = one("some text");
        b.title = Some("A good title".to_owned());
        assert_eq!(display_title(&b), "A good title");
    }

    #[test]
    fn a_links_title_comes_next() {
        let mut b = one("some text");
        b.links.push(mbm_core::bookmark::Link {
            original: Url::parse("https://example.com/a").unwrap(),
            resolved: Url::parse("https://example.com/a").unwrap(),
            kind: mbm_core::medium::LinkKind::Article,
            title: Some("From the page".to_owned()),
            body: None,
            summary: None,
            blocked: None,
        });
        assert_eq!(display_title(&b), "From the page");
    }

    #[test]
    fn the_opening_of_the_text_comes_after_that() {
        let b = one("\n\n  a post about databases  \nmore");
        assert_eq!(display_title(&b), "a post about databases");
    }

    #[test]
    fn a_bare_url_is_a_title_before_the_url_is_skipped() {
        let b = one("https://example.com/a\n\nand some words after");
        assert_eq!(display_title(&b), "and some words after");
    }

    #[test]
    fn a_bare_url_becomes_a_readable_title() {
        let mut b = one("");
        b.url = Some(at("https://blog.rust-lang.org/2024/01/01/release/"));
        assert_eq!(display_title(&b), "blog.rust-lang.org · release");
    }

    #[test]
    fn a_url_with_no_path_becomes_its_host() {
        let mut b = one("");
        b.url = Some(at("https://example.com/"));
        assert_eq!(display_title(&b), "example.com");
    }

    #[test]
    fn a_bookmark_with_nothing_still_has_a_title() {
        let b = one("");
        assert!(display_title(&b).contains('x'), "{}", display_title(&b));
    }

    #[test]
    fn a_title_is_collapsed_and_capped() {
        let b = one(&"w ".repeat(200));
        let title = display_title(&b);
        assert!(title.chars().count() <= 118, "{} chars", title.chars().count());
        assert!(title.ends_with('…'));
    }

    #[test]
    fn a_long_multiline_title_is_collapsed_to_one_line() {
        let mut b = one("x");
        b.title = Some("one\ntwo\n\nthree".to_owned());
        assert_eq!(display_title(&b), "one two three");
    }

    #[test]
    fn filenames_drop_the_characters_a_path_cannot_hold() {
        let name = safe_filename("a/b\\c:d*e?f\"g<h>i|j", "fallback");
        assert!(!name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']), "{name}");
    }

    #[test]
    fn a_title_that_is_all_punctuation_falls_back() {
        assert_eq!(safe_filename("///", "bookmark"), "bookmark");
        assert_eq!(safe_filename("   ", "bookmark"), "bookmark");
    }

    #[test]
    fn a_filename_is_capped() {
        let name = safe_filename(&"a".repeat(400), "bookmark");
        assert!(name.len() <= 120, "{} bytes", name.len());
    }

    #[test]
    fn dates_agree_with_a_known_instant() {
        // 2026-01-02T00:00:00Z
        assert_eq!(date_only(1_767_312_000_000), "2026-01-02");
        assert_eq!(iso8601(1_767_312_000_000), "2026-01-02T00:00:00Z");
        // 2026-01-02T10:30:00Z
        assert_eq!(date_time(1_767_348_000_000), "2026-01-02 10:00");
        assert_eq!(iso8601(1_767_348_000_000), "2026-01-02T10:00:00Z");
        // 2026-01-02T10:30:00Z
        assert_eq!(date_time(1_767_349_800_000), "2026-01-02 10:30");
        assert_eq!(iso8601(1_767_349_800_000), "2026-01-02T10:30:00Z");
    }

    #[test]
    fn a_pre_epoch_date_still_formats() {
        assert_eq!(date_only(-1_000), "1969-12-31");
        assert_eq!(iso8601(-1_000), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn a_leap_day_is_handled() {
        // 2024-02-29T00:00:00Z
        assert_eq!(date_only(1_709_164_800_000), "2024-02-29");
    }

    #[test]
    fn the_summary_is_read_from_the_first_link_that_has_one() {
        let b = one("text");
        assert_eq!(summary_of(&b), None);
        let mut with = b.clone();
        with.links.push(mbm_core::bookmark::Link {
            original: Url::parse("https://example.com/a").unwrap(),
            resolved: Url::parse("https://example.com/a").unwrap(),
            kind: mbm_core::medium::LinkKind::Article,
            title: None,
            body: None,
            summary: Some("  a summary  ".to_owned()),
            blocked: None,
        });
        assert_eq!(summary_of(&with), Some("a summary"));
    }

    #[test]
    fn tags_come_out_sorted_and_deduplicated() {
        let mut b = one("text");
        b.push_tag("rust");
        b.push_tag("databases");
        b.push_tag("rust");
        assert_eq!(tag_list(&b), vec!["databases".to_owned(), "rust".to_owned()]);
    }

    #[test]
    fn joining_skips_the_blanks() {
        let items = ["a".to_owned(), "  ".to_owned(), "b".to_owned()];
        assert_eq!(join_all(items, ", "), "a, b");
    }

    #[test]
    fn an_author_is_reported_by_handle() {
        let mut b = one("text");
        b.author = Some(Author::new("simonw").with_name("Simon Willison"));
        assert_eq!(b.author.as_ref().unwrap().handle, "simonw");
    }
}
