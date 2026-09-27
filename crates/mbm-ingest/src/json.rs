//! the json import path.
//!
//! this is the ingest route that needs no credentials and no network, and it
//! is the one that carries the most weight. five shapes are recognised, all of
//! them produced by something a real person would actually have:
//!
//! 1. **twitter-web-exporter**, a flat array keyed by its own csv headers.
//! 2. **the console script or bookmarklet**, the same tool the previous
//!    generation shipped, under a wrapper object.
//! 3. **raw twitter api v1.1**, using `id_str` and `full_text`.
//! 4. **our own round-trip export**, so an export can be re-imported.
//! 5. **anything else** with an array somewhere at the top level.
//!
//! detection is by shape, not by a declared format, because every exporter
//! that claims to write "json" writes something slightly different and a
//! required field name would reject real files.

use mbm_core::bookmark::{Bookmark, Media, MediaKind, SourceRef, ThreadRole};
use mbm_core::medium::SourceMedium;
use mbm_core::{Author, Result};
use serde_json::Value;
use url::Url;

/// read any of the supported json shapes into bookmarks.
///
/// `source` records where the file claims to have come from, which becomes the
/// bookmark's medium so a re-export keeps its provenance.
pub fn parse(raw: &[u8], source: SourceMedium) -> Result<(Vec<Bookmark>, usize)> {
    let value: Value = serde_json::from_slice(raw)
        .map_err(|e| mbm_core::Error::Invalid(format!("not json: {e}")))?;
    let (items, shape) = locate(&value);
    let Some(items) = items else {
        return Err(mbm_core::Error::Invalid(
            "no array of bookmarks found. expected an array, or an object with one.".to_owned(),
        ));
    };

    let mut out = Vec::with_capacity(items.len());
    let mut skipped = 0usize;
    for item in items {
        match one(item, source, shape) {
            Ok(Some(bookmark)) => out.push(bookmark),
            Ok(None) | Err(_) => skipped += 1,
        }
    }
    Ok((out, skipped))
}

/// the same thing, for text already in memory.
pub fn parse_str(raw: &str, source: SourceMedium) -> Result<(Vec<Bookmark>, usize)> {
    parse(raw.as_bytes(), source)
}

/// find the array of records, and say which shape it is.
fn locate(value: &Value) -> (Option<&Vec<Value>>, Shape) {
    match value {
        Value::Array(items) => (Some(items), Shape::Auto),
        Value::Object(map) => {
            // our own wrapper and the console script both nest under a key
            for key in ["bookmarks", "items", "data", "results", "tweets", "entries"] {
                if let Some(Value::Array(items)) = map.get(key) {
                    return (Some(items), Shape::Auto);
                }
            }
            // otherwise take the first array at the top level, which is what a
            // twitter-web-exporter file looks like
            for value in map.values() {
                if let Value::Array(items) = value {
                    return (Some(items), Shape::Auto);
                }
            }
            (None, Shape::Auto)
        }
        _ => (None, Shape::Auto),
    }
}

/// the shape of a record, once one has been picked apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Shape {
    /// not yet determined
    #[default]
    Auto,
    /// a flat row keyed by twitter-web-exporter's csv headers
    Flat,
    /// a raw api v1.1 tweet
    ApiV1,
    /// our own export
    RoundTrip,
    /// the console script's shape
    Console,
    /// a social post with an `author` object
    Post,
}

/// read one record, returning `None` when it has no usable identifier.
fn one(item: &Value, medium: SourceMedium, shape: Shape) -> Result<Option<Bookmark>> {
    let Value::Object(object) = item else {
        return Ok(None);
    };
    let shape = detect(object, shape);

    let external_id = external_id(object, shape)
        .ok_or_else(|| mbm_core::Error::Invalid("record has no id".to_owned()))?;
    if external_id.is_empty() {
        return Ok(None);
    }

    let text = text(object, shape);
    let created = created_at(object, shape);
    let ingested = created.unwrap_or_else(now_millis);
    let handle = author_handle(object, shape);
    let author = handle.as_deref().map(Author::new).map(|a| match author_name(object, shape) {
        Some(name) => a.with_name(name),
        None => a,
    });

    let url = permalink(object, shape, &external_id);
    let collection = string(object, &["collection", "folder", "list", "feed"]);

    let mut source_ref = SourceRef::new(medium, external_id, url.clone());
    if let Some(collection) = collection.filter(|c| !c.trim().is_empty()) {
        source_ref = source_ref.in_collection(collection);
    }

    let mut bookmark = Bookmark::new(source_ref, text, ingested);
    bookmark.url = url;
    bookmark.author = author;
    bookmark.created_at = created;
    bookmark.role = role(object, shape);
    bookmark.media = media(object, shape);
    bookmark.tags = tags(object);
    bookmark.raw = Some(item.clone());

    // the permalink is a good source for a tag when the file did not say
    if let Some(handle) = bookmark.author.as_ref().map(|a| a.handle.clone()) {
        bookmark.push_tag(handle);
    }
    Ok(Some(bookmark))
}

/// decide which shape a record is, from the keys it has.
fn detect(object: &serde_json::Map<String, Value>, hint: Shape) -> Shape {
    if hint != Shape::Auto {
        return hint;
    }
    // a record with no id field and a `text` field is a console export
    if object.contains_key("text") && !object.contains_key("id") && !object.contains_key("id_str") {
        return Shape::Console;
    }
    if object.contains_key("id_str") || object.contains_key("full_text") {
        return Shape::ApiV1;
    }
    if object.contains_key("tweetId") && object.contains_key("authorHandle") {
        return Shape::RoundTrip;
    }
    if object.contains_key("author") {
        return Shape::Post;
    }
    // keys that are plainly csv headers
    if object.contains_key("Tweet Id") || object.contains_key("Full Text") {
        return Shape::Flat;
    }
    Shape::Post
}

fn external_id(object: &serde_json::Map<String, Value>, shape: Shape) -> Option<String> {
    let keys: &[&str] = match shape {
        Shape::Flat => &["Tweet Id", "Tweet ID", "tweet_id"],
        Shape::ApiV1 => &["id_str", "id"],
        Shape::RoundTrip => &["tweetId", "id"],
        Shape::Console => &["id", "tweetId"],
        // `Auto` reaches here only if detection failed, in which case the
        // superset of every spelling is the right thing to try
        Shape::Post | Shape::Auto => &["id", "id_str", "tweetId", "externalId"],
    };
    first_string(object, keys)
}

fn text(object: &serde_json::Map<String, Value>, shape: Shape) -> String {
    let keys: &[&str] = match shape {
        Shape::Flat => &["Full Text", "full_text"],
        Shape::RoundTrip => &["text", "body"],
        _ => &["full_text", "text", "Full Text"],
    };
    first_string(object, keys).unwrap_or_default()
}

fn author_handle(object: &serde_json::Map<String, Value>, shape: Shape) -> Option<String> {
    match shape {
        Shape::Flat => first_string(object, &["User Screen Name", "user_screen_name"]),
        Shape::RoundTrip => first_string(object, &["authorHandle", "author"]),
        Shape::ApiV1 => {
            let user = object.get("user")?.as_object()?;
            first_string(user, &["screen_name", "username"])
        }
        _ => {
            // the modern shape nests an author object, and the console script
            // puts the handle in a bare string
            if let Some(author) = object.get("author") {
                match author {
                    Value::Object(map) => {
                        return first_string(map, &["username", "handle", "screen_name"]);
                    }
                    Value::String(s) => return Some(s.clone()),
                    _ => {}
                }
            }
            if let Some(handle) = object.get("handle") {
                return handle.as_str().map(str::to_owned);
            }
            first_string(object, &["screen_name", "username", "authorHandle"])
        }
    }
}

fn author_name(object: &serde_json::Map<String, Value>, shape: Shape) -> Option<String> {
    match shape {
        Shape::Flat => first_string(object, &["User Name", "user_name"]),
        Shape::RoundTrip => first_string(object, &["authorName"]),
        Shape::ApiV1 => {
            let user = object.get("user")?.as_object()?;
            first_string(user, &["name"])
        }
        _ => {
            if let Some(Value::Object(map)) = object.get("author") {
                return first_string(map, &["name", "displayName"]);
            }
            if let Some(Value::Object(map)) = object.get("user") {
                return first_string(map, &["name"]);
            }
            first_string(object, &["authorName", "name", "User Name"])
        }
    }
}

/// the upstream creation time, in unix milliseconds.
///
/// every exporter writes the date differently, and the shapes below cover the
/// ones that actually exist. a missing date falls back to import time rather
/// than to the epoch, so a bookmark always sorts somewhere sensible.
fn created_at(object: &serde_json::Map<String, Value>, shape: Shape) -> Option<i64> {
    let keys: &[&str] = match shape {
        Shape::Flat => &["Created At", "created_at"],
        Shape::RoundTrip => &["tweetCreatedAt", "createdAt", "created_at"],
        Shape::ApiV1 => &["created_at"],
        Shape::Console => &["timestamp", "createdAt"],
        Shape::Post | Shape::Auto => &["created_at", "createdAt", "timestamp", "date"],
    };

    for key in keys {
        let Some(value) = object.get(*key) else { continue };
        match value {
            Value::String(s) => {
                if let Some(ms) = parse_date(s) {
                    return Some(ms);
                }
            }
            Value::Number(n) => {
                if let Some(ms) = n.as_i64().and_then(normalise_epoch) {
                    return Some(ms);
                }
            }
            _ => {}
        }
    }
    None
}

/// turn a timestamp into milliseconds.
///
/// seconds, milliseconds, and microseconds all appear in the wild, and a
/// ten-digit number is unmistakably seconds while a thirteen-digit one is
/// unmistakably milliseconds. anything above microseconds is a nanosecond
/// count, which is divided down to keep the value in range.
fn normalise_epoch(raw: i64) -> Option<i64> {
    let magnitude = raw.unsigned_abs();
    if magnitude == 0 {
        return None;
    }
    Some(match magnitude {
        1_000_000_000_000_000.. => raw / 1_000_000,
        100_000_000_000.. => raw / 1_000,
        100_000_000.. => raw,
        _ => raw.saturating_mul(1_000),
    })
}

/// parse the date formats exporters actually write.
///
/// tried in order: rfc 3339, the `2024-01-02 15:04:05` that python and
/// javascript both emit, a bare date, and a unix epoch as a string.
pub fn parse_date(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        return normalise_epoch(n);
    }
    if let Some(ms) = parse_naive(trimmed) {
        return Some(ms);
    }
    parse_twitter_date(trimmed)
}

/// days from the civil epoch, for the `yyyy-mm-dd hh:mm:ss` form.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_twitter_date(raw: &str) -> Option<i64> {
    const MONTHS: [&str; 12] =
        ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    // parts[0] is the weekday, which is redundant and unverified
    let month = MONTHS.iter().position(|m| *m == parts[1].to_ascii_lowercase())? as i64 + 1;
    let day: i64 = parts[2].parse().ok()?;
    let time: Vec<&str> = parts[3].split(':').collect();
    let hour: i64 = time.first()?.parse().ok()?;
    let minute: i64 = time.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    let second: i64 = time.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
    // the offset and the year can be in either order
    let year: i64 = parts
        .iter()
        .skip(4)
        .find_map(|p| p.parse::<i64>().ok().filter(|n| (1970..=2100).contains(n)))?;

    let days = days_from_civil(year, month, day);
    Some(((days * 86_400) + (hour * 3_600) + (minute * 60) + second) * 1_000)
}

fn parse_naive(raw: &str) -> Option<i64> {
    let mut parts = raw.split(['-', 'T', ' ']);
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut hour = 0i64;
    let mut minute = 0i64;
    let mut second = 0i64;
    if let Some(time) = parts.next() {
        let time = time.trim_end_matches('Z');
        // drop a trailing numeric offset, which is not worth applying here
        let time = time.split(['+']).next().unwrap_or(time);
        let time = time.split(['-']).next().unwrap_or(time);
        let mut fields = time.split(':');
        hour = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        minute = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        second = fields
            .next()
            .and_then(|v| v.trim_end_matches(|c: char| !c.is_ascii_digit()).parse().ok())
            .unwrap_or(0);
    }

    let days = days_from_civil(year, month, day);
    Some(((days * 86_400) + (hour * 3_600) + (minute * 60) + second) * 1_000)
}

fn permalink(object: &serde_json::Map<String, Value>, shape: Shape, id: &str) -> Option<Url> {
    if let Some(url) = first_string(object, &["url", "link", "permalink", "source"])
        && let Ok(parsed) = Url::parse(&url)
    {
        return Some(parsed);
    }
    let handle = author_handle(object, shape);
    match handle {
        Some(handle) => Url::parse(&format!("https://x.com/{handle}/status/{id}")).ok(),
        None => Url::parse(&format!("https://x.com/i/status/{id}")).ok(),
    }
}

fn role(object: &serde_json::Map<String, Value>, _shape: Shape) -> Option<ThreadRole> {
    if object.contains_key("in_reply_to_status_id_str") || object.contains_key("inReplyTo") {
        return Some(ThreadRole::Reply);
    }
    if object.contains_key("quoted_status_id_str") || object.contains_key("quotedTweet") {
        return Some(ThreadRole::Quote);
    }
    if object.contains_key("self_thread") || object.contains_key("thread_id") {
        return Some(ThreadRole::Thread);
    }
    None
}

/// the media attached to a record.
fn media(object: &serde_json::Map<String, Value>, _shape: Shape) -> Vec<Media> {
    let mut out = Vec::new();

    // the modern shape, with structured media entries
    if let Some(Value::Array(items)) = object.get("media") {
        for item in items {
            if let Some(url) = item.as_str() {
                push_media(&mut out, url, None, None);
            } else if let Some(map) = item.as_object() {
                let url = first_string(map, &["url", "media_url_https", "src"])
                    .or_else(|| first_string(map, &["mediaUrl", "media_url", "previewUrl"]));
                let Some(url) = url else { continue };
                let kind = map
                    .get("type")
                    .and_then(Value::as_str)
                    .and_then(MediaKind::try_parse)
                    .unwrap_or_else(|| MediaKind::from_url(&url));
                push_media(&mut out, &url, Some(kind), first_string(map, &["previewUrl"]));
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    // the raw api shape, where media lives under extended_entities
    if let Some(entities) = object
        .get("extended_entities")
        .or_else(|| object.get("entities"))
        .and_then(Value::as_object)
        && let Some(Value::Array(items)) = entities.get("media")
    {
        for item in items {
            let Some(map) = item.as_object() else { continue };
            let kind = map
                .get("type")
                .and_then(Value::as_str)
                .and_then(MediaKind::try_parse)
                .unwrap_or(MediaKind::Photo);
            let direct = best_video_url(map);
            if let Some(url) = direct.or_else(|| first_string(map, &["media_url_https", "url"])) {
                push_media(&mut out, &url, Some(kind), first_string(map, &["preview_url"]));
            }
        }
    }
    out
}

/// the highest-bitrate mp4 for a video, which is what a viewer wants.
fn best_video_url(map: &serde_json::Map<String, Value>) -> Option<String> {
    let info = map.get("video_info")?.as_object()?;
    let variants = info.get("variants")?.as_array()?;
    let mut best: Option<(u64, String)> = None;
    for variant in variants {
        let Some(v) = variant.as_object() else { continue };
        if v.get("content_type").and_then(Value::as_str) != Some("video/mp4") {
            continue;
        }
        let Some(url) = first_string(v, &["url"]) else { continue };
        let bitrate = v.get("bitrate").and_then(Value::as_u64).unwrap_or(0);
        if best.as_ref().is_none_or(|(b, _)| bitrate > *b) {
            best = Some((bitrate, url));
        }
    }
    best.map(|(_, url)| url)
}

fn push_media(out: &mut Vec<Media>, url: &str, kind: Option<MediaKind>, preview: Option<String>) {
    let Ok(parsed) = Url::parse(url) else { return };
    if out.iter().any(|m| m.url == parsed) {
        return;
    }
    out.push(Media {
        kind: kind.unwrap_or_else(|| MediaKind::from_url(url)),
        url: parsed,
        preview_url: preview.and_then(|p| Url::parse(&p).ok()),
        width: None,
        height: None,
        duration_ms: None,
        alt_text: None,
    });
}

fn tags(object: &serde_json::Map<String, Value>) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for key in ["tags", "hashtags", "labels", "categories"] {
        match object.get(key) {
            Some(Value::Array(items)) => {
                for item in items {
                    if let Some(s) = item.as_str() {
                        out.insert(s.trim().trim_start_matches('#').to_ascii_lowercase());
                    }
                }
            }
            Some(Value::String(s)) => {
                for part in s.split(',') {
                    let part = part.trim();
                    if !part.is_empty() {
                        out.insert(part.to_ascii_lowercase());
                    }
                }
            }
            _ => {}
        }
    }
    out.remove("");
    out
}

fn first_string(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let value = object.get(*key)?;
        match value {
            Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    })
}

fn string(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    first_string(object, keys)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_of(raw: &str) -> Bookmark {
        let (items, skipped) = parse(raw.as_bytes(), SourceMedium::Json).expect("should parse");
        assert_eq!(skipped, 0, "nothing should be skipped");
        assert_eq!(items.len(), 1, "expected exactly one bookmark");
        items.into_iter().next().unwrap()
    }

    #[test]
    fn a_twitter_web_exporter_row_is_read() {
        let b = one_of(
            r#"[{
              "Tweet Id": "1234567890",
              "Full Text": "a post about rust and simd",
              "Created At": "2026-01-02 15:04:05",
              "User Screen Name": "simonw",
              "User Name": "Simon Willison",
              "Tweet Link": "https://x.com/simonw/status/1234567890"
            }]"#,
        );
        assert_eq!(b.source.external_id, "1234567890");
        assert_eq!(b.text, "a post about rust and simd");
        assert_eq!(b.author.as_ref().unwrap().handle, "simonw");
        assert_eq!(b.author.as_ref().unwrap().name.as_deref(), Some("Simon Willison"));
        assert_eq!(b.created_at, Some(parse_naive("2026-01-02 15:04:05").unwrap()));
    }

    #[test]
    fn an_api_v1_tweet_is_read() {
        let b = one_of(
            r#"[{
              "id_str": "987",
              "full_text": "hello from the api",
              "created_at": "Wed Jan 02 15:04:05 +0000 2026",
              "user": {"screen_name": "swyx", "name": "swyx"}
            }]"#,
        );
        assert_eq!(b.source.external_id, "987");
        assert_eq!(b.text, "hello from the api");
        assert_eq!(b.author.as_ref().unwrap().handle, "swyx");
        assert!(b.created_at.is_some(), "the twitter date format should parse");
    }

    #[test]
    fn a_console_export_is_read() {
        let b = one_of(
            r#"{"exportDate": "2026-01-02", "totalBookmarks": 1,
                "bookmarks": [{
                  "id": "555",
                  "author": "tom_doerr",
                  "handle": "@tom_doerr",
                  "timestamp": "2026-01-02T10:00:00Z",
                  "text": "whisper flow is real",
                  "media": [],
                  "hashtags": ["AI"],
                  "urls": ["https://github.com/x/y"]
                }]}"#,
        );
        assert_eq!(b.source.external_id, "555");
        assert_eq!(b.text, "whisper flow is real");
        assert_eq!(b.author.as_ref().unwrap().handle, "tom_doerr", "the @ is stripped");
        assert!(b.tags.contains("ai"), "hashtags become tags: {:?}", b.tags);
    }

    #[test]
    fn a_round_trip_export_is_read() {
        let b = one_of(
            r#"[{
              "tweetId": "777", "text": "round trip", "authorHandle": "simonw",
              "authorName": "Simon Willison",
              "tweetCreatedAt": "2026-01-02T10:00:00.000Z"
            }]"#,
        );
        assert_eq!(b.source.external_id, "777");
        assert_eq!(b.text, "round trip");
        assert_eq!(b.author.as_ref().unwrap().handle, "simonw");
    }

    #[test]
    fn a_modern_post_shape_is_read() {
        let b = one_of(
            r#"[{
              "id": "42", "text": "a modern shape",
              "author": {"username": "kelseyh", "name": "Kelsey"},
              "created_at": "2026-01-02T10:00:00.000Z"
            }]"#,
        );
        assert_eq!(b.source.external_id, "42");
        assert_eq!(b.author.as_ref().unwrap().handle, "kelseyh");
        assert_eq!(b.author.as_ref().unwrap().name.as_deref(), Some("Kelsey"));
    }

    #[test]
    fn the_url_is_built_when_the_record_has_none() {
        let b = one_of(r#"[{"id": "999", "text": "x", "author": {"username": "someone"}}]"#);
        let url = b.url.unwrap().to_string();
        assert!(url.contains("/someone/status/999"), "{url}");
    }

    #[test]
    fn a_declared_url_is_preferred() {
        let b = one_of(
            r#"[{"id": "1", "text": "x", "author": {"username": "a"}, "url": "https://example.com/real"}]"#,
        );
        assert_eq!(b.url.as_ref().unwrap().as_str(), "https://example.com/real");
    }

    #[test]
    fn structured_media_is_read() {
        let b = one_of(
            r#"[{"id":"1","text":"x","media":[
                {"type":"photo","url":"https://pbs.twimg.com/a.jpg","previewUrl":"https://pbs.twimg.com/s.jpg"}]}]"#,
        );
        assert_eq!(b.media.len(), 1);
        assert_eq!(b.media[0].kind, MediaKind::Photo);
        assert!(b.media[0].preview_url.is_some());
    }

    #[test]
    fn an_extended_entities_video_picks_the_best_bitrate() {
        let b = one_of(
            r#"[{"id":"1","text":"x","extended_entities":{"media":[{
              "type":"video","media_url_https":"https://video.twimg.com/thumb.jpg",
              "video_info":{"variants":[
                {"content_type":"video/mp4","bitrate":256000,"url":"https://video.twimg.com/low.mp4"},
                {"content_type":"video/mp4","bitrate":2176000,"url":"https://video.twimg.com/high.mp4"},
                {"content_type":"application/x-mpegURL","url":"https://video.twimg.com/playlist.m3u8"}
              ]}}]}}]"#,
        );
        assert_eq!(b.media.len(), 1);
        assert_eq!(b.media[0].url.as_str(), "https://video.twimg.com/high.mp4");
    }

    #[test]
    fn a_duplicate_media_url_is_kept_once() {
        let b = one_of(
            r#"[{"id":"1","text":"x","media":["https://a/1.jpg","https://a/1.jpg","https://a/2.jpg"]}]"#,
        );
        assert_eq!(b.media.len(), 2);
    }

    #[test]
    fn epoch_seconds_and_milliseconds_both_land_in_milliseconds() {
        let s = parse_date("1767225845");
        let ms = parse_date("1767225845000");
        assert_eq!(s, ms);
    }

    #[test]
    fn every_date_shape_parses_to_the_same_instant() {
        let rfc = parse_date("2026-01-02T15:04:05Z").unwrap();
        let naive = parse_date("2026-01-02 15:04:05").unwrap();
        let date_only = parse_date("2026-01-02").unwrap();
        assert_eq!(rfc, naive, "utc and naive forms should agree");
        assert_eq!(date_only, naive - ((15 * 3_600 + 4 * 60 + 5) * 1_000));
    }

    #[test]
    fn a_date_with_an_offset_still_parses() {
        let with_offset = parse_date("2026-01-02T15:04:05+02:00");
        assert_eq!(with_offset, Some(parse_date("2026-01-02T15:04:05Z").unwrap()));
    }

    #[test]
    fn an_unparseable_date_is_none_rather_than_a_wrong_value() {
        assert!(parse_date("not a date").is_none());
        assert!(parse_date("").is_none());
        assert!(parse_date("2026-13-45").is_none());
    }

    #[test]
    fn the_civil_date_conversion_agrees_with_a_known_instant() {
        // 1970-01-01 is the unix epoch
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), 10_957);
        assert_eq!(days_from_civil(2026, 1, 2), 20_455);
    }

    #[test]
    fn a_record_with_no_id_is_skipped_and_counted() {
        let (items, skipped) =
            parse_str(r#"[{"text":"no id"},{"id":"1","text":"ok"}]"#, SourceMedium::Json).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn a_malformed_file_is_an_error_not_a_panic() {
        assert!(parse(b"not json at all", SourceMedium::Json).is_err());
        assert!(parse(b"[]", SourceMedium::Json).is_ok());
        assert!(parse(b"{}", SourceMedium::Json).is_err());
        assert!(parse(b"\"a string\"", SourceMedium::Json).is_err());
    }

    #[test]
    fn a_file_with_several_records_reads_all_of_them() {
        let (items, skipped) = parse_str(
            r#"[{"id":"1","text":"a"},{"id":"2","text":"b"},{"id":"3","text":"c"}]"#,
            SourceMedium::Json,
        )
        .unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn a_reply_is_marked_as_one() {
        let b = one_of(r#"[{"id":"1","text":"x","in_reply_to_status_id_str":"99"}]"#);
        assert_eq!(b.role, Some(ThreadRole::Reply));
    }

    #[test]
    fn a_quote_is_marked_as_one() {
        let b = one_of(r#"[{"id":"1","text":"x","quoted_status_id_str":"99"}]"#);
        assert_eq!(b.role, Some(ThreadRole::Quote));
    }

    #[test]
    fn a_collection_becomes_a_tag_and_is_kept() {
        let b = one_of(r#"[{"id":"1","text":"x","collection":"ai-tools"}]"#);
        assert_eq!(b.source.collection.as_deref(), Some("ai-tools"));
    }

    #[test]
    fn an_explicit_tag_list_is_read() {
        let b = one_of(r#"[{"id":"1","text":"x","tags":["Rust","SIMD","" ]}]"#);
        assert!(b.tags.contains("rust"));
        assert!(b.tags.contains("simd"));
        assert!(!b.tags.contains(""));
    }

    #[test]
    fn a_comma_separated_tag_string_is_split() {
        let b = one_of(r#"[{"id":"1","text":"x","tags":"rust, simd, , post"}]"#);
        assert_eq!(b.tags.len(), 3, "{:?}", b.tags);
    }

    #[test]
    fn the_raw_payload_is_kept_for_reparsing() {
        let b = one_of(r#"[{"id":"1","text":"x","somethingNew":true}]"#);
        assert!(b.raw.is_some());
        assert_eq!(b.raw.unwrap()["somethingNew"], Value::Bool(true));
    }

    #[test]
    fn a_record_whose_date_is_missing_lands_at_import_time() {
        let b = one_of(r#"[{"id":"1","text":"x"}]"#);
        assert!(b.created_at.is_none());
        assert!(b.ingested_at > 1_700_000_000_000, "{}", b.ingested_at);
    }

    #[test]
    fn a_wrapper_object_under_an_unexpected_key_still_finds_its_records() {
        let (items, _) =
            parse_str(r#"{"somethingElse":[{"id":"1","text":"x"}]}"#, SourceMedium::Json).unwrap();
        assert_eq!(items.len(), 1);
    }
}
