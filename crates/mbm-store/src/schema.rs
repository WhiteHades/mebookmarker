//! the schema, and the pragmas that make sqlite behave like a database rather
//! than like a file.
//!
//! # one schema, no migrations
//!
//! there is one version of this schema and it is the only one that exists. a
//! store written by an earlier build is not upgraded: it is deleted and rebuilt.
//!
//! that is a deliberate choice, and it is the right one here for three reasons.
//! the store is derived data — every row can be re-fetched from the source, so
//! nothing is lost by starting again. a migration path is code that only ever
//! runs once, on someone else's data, at the worst possible moment, and it is
//! the code most likely to be wrong. and a store that cannot be upgraded cannot
//! be half-upgraded either, which is the failure nobody tests for.
//!
//! what this costs is a `mbm reindex`-style command that rebuilds from the
//! sources, and that is a command worth having anyway.
//!
//! # the pragmas
//!
//! these matter more than the schema for how fast the thing feels, and each one
//! is a measured decision rather than a default:
//!
//! - `journal_mode = WAL`. a reader must not block the writer. an archive is
//!   read constantly and written in bursts, and the default rollback journal
//!   makes the two fight.
//! - `synchronous = NORMAL`. with WAL this survives a process crash and not a
//!   power cut. a bookmark archive is re-fetchable, and a full fsync per commit
//!   is the difference between an import that takes a minute and one that takes
//!   an hour.
//! - `mmap_size`. reading a large index is a memory-map read, not a copy.
//! - `cache_size`. the whole working set of a search is a few hundred pages, and
//!   the default of two megabytes is smaller than one FTS5 segment.
//! - `temp_store = MEMORY`. every sort and every join in a search would
//!   otherwise write a temporary file.

use rusqlite::Connection;

use mbm_core::Result;

/// the schema version this build writes.
///
/// stamped into `PRAGMA user_version` so a store from a different build is
/// recognisable. it is not read to drive a migration, because there is only one
/// version.
pub const SCHEMA_VERSION: i64 = 1;

/// every pragma, as one batch.
///
/// the numbers are the ones measured on the corpus in `benches/`: a 200,000
/// document store, 209 bytes a document, a rare two-letter stem answering in
/// 0.12ms.
const PRAGMAS: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 268435456;
PRAGMA cache_size = -65536;
PRAGMA busy_timeout = 5000;
PRAGMA optimize;
";

/// the ddl, in one authoritative statement.
///
/// the `IF NOT EXISTS` on every object is what makes this safe to run twice.
/// that is not a migration path — it is the same schema applied to a store that
/// already has it, which happens every time the program opens its database.
pub const SCHEMA: &str = r#"
-- ─── The hot row ─────────────────────────────────────────────────────────
-- One row per bookmark. Everything a list view needs is here; the links, media,
-- tags, and categories are satellites, and the text index is derived from these
-- three columns by a trigger.
CREATE TABLE IF NOT EXISTS bookmark (
    id            INTEGER PRIMARY KEY,   -- mbm_core::Id: ms timestamp << 16 | sequence
    medium        TEXT    NOT NULL,      -- mbm_core::SourceMedium
    external_id   TEXT    NOT NULL,      -- the source's own id for this item
    collection    TEXT,                  -- sub-stream: folder, subreddit, feed url
    url           TEXT,
    author_handle TEXT,
    author_name   TEXT,
    title         TEXT,
    summary       TEXT,                  -- one line on what the item is for
    body          TEXT NOT NULL DEFAULT '',
    extra         TEXT NOT NULL DEFAULT '',  -- author + tags + hosts, for the index
    role          TEXT,                  -- mbm_core::ThreadRole
    created_at    INTEGER,               -- unix ms, from the source
    ingested_at   INTEGER NOT NULL,      -- unix ms, from us
    fingerprint   INTEGER,               -- SimHash-64, stored signed for sqlite
    entities_at     INTEGER,             -- NULL until the stage has run
    vision_at       INTEGER,
    tagged_at       INTEGER,
    categorized_at  INTEGER,
    described_at    INTEGER,
    raw             TEXT                 -- the source payload, for re-parsing
) STRICT;

-- One row per (source, external id) is the dedup guarantee, enforced by the
-- schema rather than by a check-then-insert. this is what makes a second import
-- of the same file a no-op rather than a doubling.
CREATE UNIQUE INDEX IF NOT EXISTS bookmark_identity ON bookmark(medium, external_id);

-- The pipeline cursor. The rowid is the id, so this is free, and it lets SQLite
-- answer a page of pending ids without touching the table.
CREATE INDEX IF NOT EXISTS bookmark_cursor ON bookmark(id);

CREATE INDEX IF NOT EXISTS bookmark_created ON bookmark(created_at DESC);
CREATE INDEX IF NOT EXISTS bookmark_author  ON bookmark(author_handle);

-- One partial index per stage. Each covers only the rows that stage has not
-- finished, so the index is proportional to the work outstanding rather than to
-- the size of the archive. A six-figure archive with a full backlog is a few
-- hundred pages; the same archive with an empty backlog is nothing at all.
CREATE INDEX IF NOT EXISTS bookmark_pending ON bookmark(id) WHERE entities_at  IS NULL;
CREATE INDEX IF NOT EXISTS bookmark_vision_pending  ON bookmark(id) WHERE vision_at    IS NULL;
CREATE INDEX IF NOT EXISTS bookmark_tagged_pending  ON bookmark(id) WHERE tagged_at    IS NULL;
CREATE INDEX IF NOT EXISTS bookmark_categorized_pending ON bookmark(id) WHERE categorized_at IS NULL;
CREATE INDEX IF NOT EXISTS bookmark_described_pending ON bookmark(id) WHERE described_at IS NULL;

-- ─── Satellites ───────────────────────────────────────────────────────────
-- Split out because a list view reads none of them and a detail view reads all
-- of them, and one wide row with mostly-empty columns is worse on both.

CREATE TABLE IF NOT EXISTS link (
    bookmark INTEGER NOT NULL REFERENCES bookmark(id) ON DELETE CASCADE,
    ordinal   INTEGER NOT NULL,          -- the order they appear in the text
    original  TEXT    NOT NULL,
    resolved  TEXT    NOT NULL,          -- after redirects and tracking stripped
    kind      TEXT    NOT NULL,          -- mbm_core::LinkKind
    title     TEXT,
    body      TEXT,
    summary   TEXT,
    blocked   TEXT,                      -- mbm_core::BlockedReason
    PRIMARY KEY (bookmark, ordinal)
) STRICT;

CREATE INDEX IF NOT EXISTS link_resolved ON link(resolved);

CREATE TABLE IF NOT EXISTS media (
    bookmark    INTEGER NOT NULL REFERENCES bookmark(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    kind        TEXT    NOT NULL,        -- mbm_core::MediaKind
    url         TEXT    NOT NULL,
    preview_url TEXT,
    width       INTEGER,
    height      INTEGER,
    duration_ms INTEGER,
    alt_text    TEXT,
    PRIMARY KEY (bookmark, ordinal)
) STRICT;

CREATE INDEX IF NOT EXISTS media_url ON media(url);

CREATE TABLE IF NOT EXISTS tag (
    bookmark INTEGER NOT NULL REFERENCES bookmark(id) ON DELETE CASCADE,
    tag      TEXT    NOT NULL
) STRICT;

-- the covering index serves both directions: a bookmark's tags, and a tag's
-- bookmarks. the second is the one a `mbm list --tag` runs, and it is the one
-- that would otherwise be a table scan.
CREATE UNIQUE INDEX IF NOT EXISTS tag_name ON tag(tag, bookmark);

CREATE TABLE IF NOT EXISTS category (
    id          INTEGER PRIMARY KEY,
    slug        TEXT    NOT NULL UNIQUE,
    name        TEXT    NOT NULL,
    color       TEXT,
    description TEXT,
    folder      TEXT,
    action      TEXT    NOT NULL DEFAULT 'capture'   -- mbm_core::category::Action
) STRICT;

CREATE TABLE IF NOT EXISTS bookmark_category (
    bookmark   INTEGER NOT NULL REFERENCES bookmark(id)   ON DELETE CASCADE,
    category   INTEGER NOT NULL REFERENCES category(id)   ON DELETE CASCADE,
    confidence REAL    NOT NULL,
    assigned_by TEXT   NOT NULL,         -- rule | jev | agent | human
    PRIMARY KEY (bookmark, category)
) STRICT;

CREATE INDEX IF NOT EXISTS bookmark_category_cat ON bookmark_category(category);

-- ─── The text index ───────────────────────────────────────────────────────
-- An external-content fts5 table: it stores only the index, not a second copy
-- of the text, and a trigger keeps it in step. The trigger is what makes this
-- correct rather than merely fast; an index that can drift from its data is
-- worse than no index.
--
-- The tokenizer is porter + unicode61 with diacritics removed, because a
-- search for "cafe" should find "café" and a search for "running" should find
-- "run". remove_diacritics 2 folds the accents in the index only, so the text
-- column keeps them.
--
-- no prefix index, on purpose. a prefix index records where every short token
-- prefix occurs, which looked like the answer for an as-you-type search box,
-- and measuring it said otherwise: 30% more space, a rare two-letter stem going
-- from 0.06ms to 41ms, and about 10% on the queries it was meant to help. the
-- prefilter in `search.rs` serves that case instead, and it is faster.
CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
    title, body, extra,
    content='bookmark',
    content_rowid='id',
    tokenize='porter unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS search_ai AFTER INSERT ON bookmark BEGIN
    INSERT INTO search(rowid, title, body, extra) VALUES (new.id, new.title, new.body, new.extra);
END;

CREATE TRIGGER IF NOT EXISTS search_ad AFTER DELETE ON bookmark BEGIN
    INSERT INTO search(search, rowid, title, body, extra) VALUES('delete', old.id, old.title, old.body, old.extra);
END;

CREATE TRIGGER IF NOT EXISTS search_au AFTER UPDATE ON bookmark BEGIN
    INSERT INTO search(search, rowid, title, body, extra) VALUES('delete', old.id, old.title, old.body, old.extra);
    INSERT INTO search(rowid, title, body, extra) VALUES (new.id, new.title, new.body, new.extra);
END;

-- ─── Run log ──────────────────────────────────────────────────────────────
-- One row per pipeline run. This is a cost history: the whole design turns on
-- being able to say what a run cost, and a number in a log file cannot be
-- totalled six months later.
CREATE TABLE IF NOT EXISTS run (
    id          INTEGER PRIMARY KEY,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,
    outcome     TEXT,                -- ok | failed | cancelled
    bookmarks   INTEGER NOT NULL DEFAULT 0,
    written     INTEGER NOT NULL DEFAULT 0,
    skipped     INTEGER NOT NULL DEFAULT 0,
    failed      INTEGER NOT NULL DEFAULT 0,
    cost_micros INTEGER NOT NULL DEFAULT 0,
    detail      TEXT
) STRICT;

CREATE INDEX IF NOT EXISTS run_started ON run(started_at DESC);
"#;

/// open a connection with every pragma applied and the schema in place.
///
/// this is the only way to open a store. a caller that constructs a connection
/// itself gets sqlite's defaults, which are the slow ones.
pub fn open(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(path).map_err(|e| crate::db::store_err(&e))?;
    prepare(&conn)?;
    Ok(conn)
}

/// open a connection in memory, for a test or a throwaway query.
pub fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory().map_err(|e| crate::db::store_err(&e))?;
    prepare(&conn)?;
    Ok(conn)
}

/// apply the pragmas and the schema to a connection the caller opened.
pub fn prepare(conn: &Connection) -> Result<()> {
    conn.execute_batch(PRAGMAS)
        .map_err(|e| crate::db::store_err(&e))?;
    conn.execute_batch(SCHEMA)
        .map_err(|e| crate::db::store_err(&e))?;
    // PRAGMA does not take a bound parameter, and this is a compile-time
    // constant rather than anything a caller controls.
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
        .map_err(|e| crate::db::store_err(&e))?;
    Ok(())
}

/// whether a store was written by this build.
///
/// a store from another build is not upgraded, so the only honest response is to
/// say so and let the caller start again.
pub fn is_current(conn: &Connection) -> Result<bool> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| crate::db::store_err(&e))?;
    Ok(version == SCHEMA_VERSION)
}

/// rebuild the text index from the table it indexes.
///
/// an external-content fts5 table can be rebuilt exactly and cheaply, because
/// the text is still in `bookmark`. this is the repair for an index that has
/// drifted, and the second half of a schema change that only touches the index.
pub fn reindex(conn: &Connection) -> Result<()> {
    conn.execute_batch("INSERT INTO search(search) VALUES('rebuild');")
        .map_err(|e| crate::db::store_err(&e))?;
    // a fresh fts5 table is a pile of small segments; `optimize` merges them into
    // the smallest and fastest-to-query form
    conn.execute_batch("INSERT INTO search(search) VALUES('optimize');")
        .map_err(|e| crate::db::store_err(&e))?;
    Ok(())
}

/// the store's size on disk, in bytes.
pub fn size_on_disk(conn: &Connection) -> Result<u64> {
    conn.query_row(
        "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|bytes| bytes as u64)
    .map_err(|e| crate::db::store_err(&e))
}
