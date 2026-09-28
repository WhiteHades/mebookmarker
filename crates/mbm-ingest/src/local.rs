//! the file and clipboard adapters.
//!
//! four sources that read from disk rather than from a network: a browser's
//! bookmark export, an opml file, a folder of documents, and a plain text file
//! of urls.
//!
//! these are the ones that need no credentials at all, which makes them the
//! fastest way to get a working install and the only ones that can be verified
//! end to end without an account.

use mbm_core::bookmark::{Bookmark, MediaKind, SourceRef};
use mbm_core::error::Result;
use mbm_core::medium::SourceMedium;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use url::Url;

/// read a browser bookmark export.
///
/// both the netscape format every browser still writes and the opml subset
/// that some write alongside it. the netscape shape is not xml, it is a
/// javascript-in-html hybrid, so it is scanned line by line rather than parsed.
pub fn parse_netscape(body: &str) -> Result<Vec<Bookmark>> {
    let mut out = Vec::new();
    let mut folders: Vec<String> = Vec::new();
    let mut pending_folder: Option<String> = None;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // an entry is checked before the folder markers, because a compact
        // export puts `<DL><DT><A HREF=...>` on one line and a `continue` on
        // the `<DL>` would swallow the entry that follows it
        let url = attribute(line, "href");

        // a folder names itself in an `<H3>` and opens with the `<DL>` that
        // follows it, so the name is held until the list starts
        if line.contains("<H3") || line.contains("<h3") {
            pending_folder = anchor_text(line).filter(|n| !n.is_empty());
        }
        if line.contains("<DL") || line.contains("<dl") {
            folders.push(pending_folder.take().unwrap_or_default());
        }

        let Some(url) = url.filter(|u| u.starts_with("http")) else {
            if line.contains("</DL>") || line.contains("</dl>") {
                folders.pop();
            }
            continue;
        };
        let name = anchor_text(line).unwrap_or_else(|| url.clone());
        let created = attribute(line, "add_date")
            .and_then(|v| v.parse::<i64>().ok())
            // browser exports write seconds, sometimes with microseconds
            .map(|s| if s > 1_000_000_000_000 { s / 1_000_000 } else { s * 1_000 });

        let mut bookmark = make(SourceMedium::BrowserBookmarks, &url, &name, created);
        for folder in &folders {
            if !folder.is_empty() {
                bookmark.push_tag(folder.clone());
            }
        }
        out.push(bookmark);

        if line.contains("</DL>") || line.contains("</dl>") {
            folders.pop();
        }
    }
    Ok(out)
}

/// the visible text of a netscape entry, which is whatever follows the
/// element's own closing angle bracket.
///
/// finding the first `>` on the line does not work: a line usually opens with
/// `<DT><A HREF="...">`, so the first one belongs to the `<DT>`.
fn anchor_text(line: &str) -> Option<String> {
    // the name is the text of the last tag on the line that has text inside it.
    //
    // finding the first `>` does not work, because a line usually opens with
    // `<DT><A HREF="...">` and the first one belongs to the `<DT>`. looking for
    // the last `<A` does not work either, because a folder is named by an
    // `<H3>` and every bookmark in it would be filed outside the folder a
    // person put it in.
    let bytes = line.as_bytes();
    let mut found: Option<String> = None;
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at] != b'<' {
            at += 1;
            continue;
        }
        let Some(close) = line[at..].find('>').map(|o| at + o) else { break };
        let tag = &line[at + 1..close];
        let start = close + 1;
        let end = line[start..].find('<').map_or(line.len(), |o| start + o);
        let text = unescape(line[start..end].trim());
        if !text.is_empty() && !tag.starts_with('/') {
            found = Some(text);
        }
        at = start.max(close + 1);
    }
    found
}

/// the value of an attribute, matched without regard to case.
///
/// a netscape file is not xml and nothing in it is case-normalised: every
/// browser that writes one writes `HREF` and `ADD_DATE` in capitals and writes
/// `href` in lower case when the export was made by something else. a
/// case-sensitive reader keeps the urls and silently drops the names, the dates
/// and the folders, which is an import that looks like it worked.
fn attribute(line: &str, name: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let needle = format!("{name}=\"");
    let at = lower.find(&needle)?;
    let rest = &line[at + needle.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// build a bookmark for a url, using the url itself as the identifier.
fn make(medium: SourceMedium, url: &str, text: &str, created: Option<i64>) -> Bookmark {
    let parsed = Url::parse(url).ok();
    let id = parsed.as_ref().map_or_else(|| url.to_owned(), canonical_key);
    let source = SourceRef::new(medium, id, parsed.clone());
    let now = created.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
    });
    let mut bookmark = Bookmark::new(source, text, now);
    bookmark.created_at = created;
    bookmark.url.clone_from(&parsed);
    if let Some(url) = parsed {
        bookmark.links.push(mbm_core::bookmark::Link {
            original: url.clone(),
            resolved: url,
            kind: mbm_core::medium::LinkKind::Unknown,
            title: None,
            body: None,
            summary: None,
            blocked: None,
        });
    }
    bookmark
}

/// a stable key for a url, used as the external id.
///
/// hashing rather than the url itself, because a url is a poor primary key:
/// it is long, it can be far longer than a row, and two exports of the same
/// page differ in their tracking parameters.
fn canonical_key(url: &Url) -> String {
    let canonical = mbm_extract::canonical(url);
    let hash = blake3::hash(canonical.as_bytes());
    hash.to_hex()[..16].to_owned()
}

/// read an opml file.
pub fn parse_opml(body: &str) -> Result<Vec<Bookmark>> {
    let mut out = Vec::new();
    let mut folder: Option<String> = None;
    let mut cursor = 0usize;
    let lower = body.to_ascii_lowercase();

    while let Some(offset) = lower[cursor..].find("<outline") {
        let at = cursor + offset;
        let Some(end) = lower[at..].find('>').map(|e| at + e + 1) else { break };
        let tag = &body[at..end];
        cursor = end;

        let has_target = attribute(tag, "url").is_some() || attribute(tag, "xmlUrl").is_some();
        if !has_target
            && let Some(title) = attribute(tag, "text").or_else(|| attribute(tag, "title"))
        {
            folder = Some(title);
        }
        let feed = attribute(tag, "xmlUrl").is_some();
        let url = attribute(tag, "url").or_else(|| attribute(tag, "xmlUrl"));
        let Some(url) = url else { continue };
        if !url.starts_with("http") {
            continue;
        }
        let text = attribute(tag, "text")
            .or_else(|| attribute(tag, "title"))
            .unwrap_or_else(|| url.clone());
        let created = attribute(tag, "addDate")
            .and_then(|v| v.parse::<i64>().ok())
            .map(|s| if s > 1_000_000_000_000 { s / 1_000 } else { s * 1_000 });

        let mut bookmark = make(SourceMedium::BrowserBookmarks, &url, &text, created);
        if let Some(folder) = &folder {
            bookmark.push_tag(folder.clone());
        }
        if feed {
            // a subscription rather than a page, but still something the user
            // chose to keep
            bookmark.push_tag("feed");
        }
        out.push(bookmark);
    }
    Ok(out)
}

/// read a file of urls, one per line.
///
/// the shape people actually have: a list copied out of somewhere. `#` starts a
/// comment and a bare url with no scheme gets `https://`.
pub fn parse_url_list(body: &str) -> Result<Vec<Bookmark>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // a markdown link is `[text](url)`
        let candidate = if let (Some(close), Some(open)) = (line.find("]("), line.find("](")) {
            let _ = (close, open);
            match line.find("](") {
                Some(at) => line[at + 2..].trim_end_matches(')').to_owned(),
                None => line.to_owned(),
            }
        } else {
            line.split_whitespace().next().unwrap_or_default().to_owned()
        };

        // a bare `mailto:` or `ftp:` line is left alone and then rejected,
        // rather than being turned into `https://mailto:...`
        let has_scheme = candidate.split_once(':').is_some_and(|(scheme, _)| {
            !scheme.is_empty() && scheme.bytes().all(|b| b.is_ascii_alphanumeric())
        });
        let candidate = if has_scheme { candidate } else { format!("https://{candidate}") };
        let Ok(url) = Url::parse(&candidate) else { continue };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        if !seen.insert(url.to_string()) {
            continue;
        }
        out.push(make(SourceMedium::Manual, url.as_str(), url.as_str(), None));
    }
    Ok(out)
}

/// read a plain text or markdown file as a single bookmark.
pub fn parse_text_document(path: &Path, body: &str) -> Result<Bookmark> {
    let name = path
        .file_stem()
        .map_or_else(|| "untitled".to_owned(), |s| s.to_string_lossy().into_owned());
    let created = created_of(path);
    let url = Url::from_file_path(path).ok();

    let source = SourceRef::new(SourceMedium::LocalFile, path.to_string_lossy().into_owned(), url);
    let mut bookmark = Bookmark::new(source, body.trim(), created);
    bookmark.created_at = Some(created);
    bookmark.title = Some(name);
    bookmark.push_tag("local");
    if let Some(parent) = path.parent().and_then(|p| p.file_name()) {
        bookmark.push_tag(parent.to_string_lossy());
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        bookmark.push_tag(ext.to_ascii_lowercase());
    }
    Ok(bookmark)
}

/// a file's modification time, in unix milliseconds.
///
/// falls back to now when the filesystem will not say, which keeps an import
/// working on a path that has been moved or copied.
fn created_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(now_millis, |d| d.as_millis() as i64)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// read a folder of documents, skipping anything unreadable.
///
/// returns the bookmarks and the paths that could not be read, so the caller
/// can report the second without the first being silently short.
pub fn read_directory(root: &Path, recursive: bool) -> (Vec<Bookmark>, Vec<(PathBuf, String)>) {
    let mut out = Vec::new();
    let mut failed = Vec::new();
    walk(root, recursive, &mut out, &mut failed);
    // oldest first, so a folder lands in the store in the order it was written
    out.sort_by_key(Bookmark::sort_timestamp);
    (out, failed)
}

/// the file types worth reading, and how to read each.
fn classify(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "md" | "markdown" | "txt" | "text" | "org" | "rst" | "log" => Kind::Text,
        "html" | "htm" => Kind::Html,
        "opml" => Kind::Opml,
        "json" => Kind::Json,
        _ => return None,
    })
}

enum Kind {
    Text,
    Html,
    Opml,
    Json,
}

fn walk(dir: &Path, recursive: bool, out: &mut Vec<Bookmark>, failed: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        failed.push((dir.to_path_buf(), "cannot read directory".to_owned()));
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        // a dot directory, or a build artefact, is not content
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }

        if path.is_dir() {
            if recursive {
                walk(&path, recursive, out, failed);
            }
            continue;
        }

        let Some(kind) = classify(&path) else { continue };
        let body = match std::fs::read_to_string(&path) {
            Ok(body) => body,
            Err(e) => {
                failed.push((path, e.to_string()));
                continue;
            }
        };

        match kind {
            Kind::Text => match parse_text_document(&path, &body) {
                Ok(bookmark) => out.push(bookmark),
                Err(e) => failed.push((path, e.to_string())),
            },
            Kind::Html => {
                let article = mbm_extract::readability::read(&body, &path.to_string_lossy());
                let mut bookmark = make(
                    SourceMedium::LocalFile,
                    &format!("file:{}", path.display()),
                    &article.body,
                    Some(created_of(&path)),
                );
                bookmark.title.clone_from(&article.title);
                out.push(bookmark);
            }
            Kind::Opml => match parse_opml(&body) {
                Ok(mut found) => out.append(&mut found),
                Err(e) => failed.push((path, e.to_string())),
            },
            Kind::Json => match crate::json::parse(body.as_bytes(), SourceMedium::LocalFile) {
                Ok((mut found, _)) => out.append(&mut found),
                Err(e) => failed.push((path, e.to_string())),
            },
        }
    }
}

/// a media attachment, guessed from a path.
#[must_use]
pub fn media_for(path: &Path) -> Option<(PathBuf, MediaKind)> {
    let kind = MediaKind::from_url(&path.to_string_lossy());
    matches!(kind, MediaKind::Photo).then(|| (path.to_path_buf(), kind))
}

fn unescape(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}
