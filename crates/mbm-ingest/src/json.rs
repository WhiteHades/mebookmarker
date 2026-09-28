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
    // rss writes `Fri, 02 Jan 2026 10:00:00 +0000` and every generator emits
    // that form for `pubDate`. a bookmark whose date falls back to the moment it
    // was fetched sorts in the wrong place in a list that is ordered by when
    // the thing was written, which is the only order worth having.
    if let Some(ms) = parse_rfc822(trimmed) {
        return Some(ms);
    }
    parse_twitter_date(trimmed)
}

/// the month and zone names rss and http both use.
const RFC822_MONTHS: [&str; 12] =
    ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// the zone names rfc 822 still allows, as minutes east of utc.
const RFC822_ZONES: [(&str, i32); 12] = [
    ("gmt", 0),
    ("ut", 0),
    ("utc", 0),
    ("z", 0),
    ("est", -5 * 60),
    ("edt", -4 * 60),
    ("cst", -6 * 60),
    ("cdt", -5 * 60),
    ("mst", -7 * 60),
    ("mdt", -6 * 60),
    ("pst", -8 * 60),
    ("pdt", -7 * 60),
];

/// an rfc 822 date, which is what a feed's `pubDate` is.
///
/// the shape is `Day, DD Mon YYYY HH:MM:SS +ZZZZ`, the weekday and the seconds
/// are both optional in practice, and the zone is a numeric offset about half
/// the time and a name the other half.
fn parse_rfc822(raw: &str) -> Option<i64> {
    // the weekday leads the field and carries no information the rest does not.
    // it is three letters, sometimes with a dot, and the split is on `, ` so a
    // date whose day happened to be followed by a comma is not mangled.
    let rest = match raw.split_once(", ") {
        Some((weekday, rest))
            if weekday.trim_end_matches('.').len() == 3
                && weekday.trim_end_matches('.').chars().all(char::is_alphabetic) =>
        {
            rest
        }
        _ => raw,
    };
    let rest =
        rest.trim_end_matches(|c: char| !c.is_ascii_digit() && c != ':' && c != '+' && c != '-');

    let (stamp, zone) = match rest.rsplit_once(' ') {
        Some((stamp, zone)) if zone.starts_with(['+', '-']) || zone_minutes(zone).is_some() => {
            (stamp, zone.trim())
        }
        _ => (rest, "+0000"),
    };

    let mut fields = stamp.split_whitespace();
    let day: i64 = fields.next()?.parse().ok()?;
    let name = fields.next()?;
    let month =
        RFC822_MONTHS.iter().position(|m| m.eq_ignore_ascii_case(name)).map_or(0, |i| i as i64 + 1);
    if month == 0 {
        return None;
    }
    let year: i64 = fields.next()?.parse().ok()?;
    let clock = fields.next().unwrap_or("00:00:00");

    let mut time = clock.split(':');
    let hour: i64 = time.next().unwrap_or("0").parse().ok()?;
    let minute: i64 = time.next().unwrap_or("0").parse().ok()?;
    let second: i64 = time
        .next()
        .unwrap_or("0")
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(0);

    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // a two-digit year is a window: rfc 822 has no century, and every feed that
    // writes one means this century
    let year = if year < 100 { 2000 + year } else { year };

    let offset = zone_minutes(zone)?;
    let days = days_from_civil(year, month, day);
    let seconds =
        (days * 86_400) + (hour * 3_600) + (minute * 60) + second - i64::from(offset) * 60;
    Some(seconds * 1_000)
}

/// a zone as minutes east of utc.
fn zone_minutes(zone: &str) -> Option<i32> {
    if let Some((_, minutes)) =
        RFC822_ZONES.iter().find(|(name, _)| name.eq_ignore_ascii_case(zone))
    {
        return Some(*minutes);
    }
    let (sign, digits) = match zone.split_at_checked(1)? {
        ("+", rest) => (1, rest),
        ("-", rest) => (-1, rest),
        _ => return None,
    };
    if digits.len() != 4 {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    Some(sign * (hours * 60 + minutes))
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
