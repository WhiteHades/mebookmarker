//! the schema and the migrations that produced it.
pub const SCHEMA_VERSION: i64 = 1;

pub const MIGRATIONS: &[&str] = &[V1];

const V1: &str = r#"
-- ─── The hot row ─────────────────────────────────────────────────────────
CREATE TABLE bookmark (
    id           INTEGER PRIMARY KEY,   -- mbm_core::Id: ms timestamp << 16 | sequence
    medium       TEXT    NOT NULL,      -- mbm_core::SourceMedium
    external_id  TEXT    NOT NULL,      -- the source's own id for this item
    collection   TEXT,                  -- sub-stream: folder, subreddit, feed url
    url          TEXT,
    author_handle TEXT,
    author_name  TEXT,
    title        TEXT,
    summary      TEXT,                  -- one-line description
    body         TEXT NOT NULL DEFAULT '',
    extra        TEXT NOT NULL DEFAULT '',  -- author + tags + entity names, for the index
    role         TEXT,                  -- mbm_core::ThreadRole
    created_at   INTEGER,               -- unix ms, from the source
    ingested_at  INTEGER NOT NULL,      -- unix ms, from us
    fingerprint  INTEGER,               -- SimHash-64, stored signed to keep SQLite happy
    entities_at  INTEGER,               -- NULL until the stage has run
    vision_at    INTEGER,
    tagged_at    INTEGER,
    categorized_at INTEGER,
    described_at INTEGER,
    raw          TEXT                   -- the source payload, for re-parsing
) STRICT;

-- One row per (source, external id) is the dedup guarantee. Enforced by the
-- schema rather than by a check-then-insert, so two concurrent runs cannot
-- both decide the item is new.
CREATE UNIQUE INDEX bookmark_identity ON bookmark(medium, external_id);

-- The pipeline cursor. Redundant with the primary key for ordering, but the
-- covering form lets SQLite answer a page without touching the table.
CREATE INDEX bookmark_cursor ON bookmark(id);

CREATE INDEX bookmark_created ON bookmark(created_at DESC);
CREATE INDEX bookmark_author  ON bookmark(author_handle);
CREATE INDEX bookmark_pending ON bookmark(id) WHERE entities_at  IS NULL;
CREATE INDEX bookmark_vision_pending  ON bookmark(id) WHERE vision_at    IS NULL;
CREATE INDEX bookmark_tagged_pending  ON bookmark(id) WHERE tagged_at    IS NULL;
CREATE INDEX bookmark_categorized_pending ON bookmark(id) WHERE categorized_at IS NULL;
CREATE INDEX bookmark_described_pending ON bookmark(id) WHERE described_at IS NULL;

-- ─── Satellites ──────────────────────────────────────────────────────────
CREATE TABLE link (
    id        INTEGER PRIMARY KEY,
    bookmark  INTEGER NOT NULL REFERENCES bookmark(id) ON DELETE CASCADE,
    ordinal   INTEGER NOT NULL,        -- position in the text
    original  TEXT    NOT NULL,
    resolved  TEXT    NOT NULL,
    kind      TEXT    NOT NULL,        -- mbm_core::LinkKind
    title     TEXT,
    body      TEXT,
    summary   TEXT,
    blocked   TEXT                     -- mbm_core::BlockedReason
) STRICT;

CREATE INDEX link_bookmark ON link(bookmark, ordinal);
-- Deduplicates the same shared link across many bookmarks, and powers the
-- "which bookmarks link here" lookup.
CREATE INDEX link_resolved ON link(resolved);

CREATE TABLE media (
    id          INTEGER PRIMARY KEY,
    bookmark    INTEGER NOT NULL REFERENCES bookmark(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    kind        TEXT    NOT NULL,      -- mbm_core::MediaKind
    url         TEXT    NOT NULL,
    preview_url TEXT,
    width       INTEGER,
    height      INTEGER,
    duration_ms INTEGER,
    alt_text    TEXT
) STRICT;

CREATE INDEX media_bookmark ON media(bookmark, ordinal);
-- Vision results are cached per URL, not per row: the same screenshot is
-- frequently bookmarked by three people and analysed once.
CREATE INDEX media_url ON media(url);

CREATE TABLE tag (
    bookmark INTEGER NOT NULL REFERENCES bookmark(id) ON DELETE CASCADE,
    tag      TEXT    NOT NULL,
    PRIMARY KEY (bookmark, tag)
) STRICT;

CREATE INDEX tag_name ON tag(tag);

CREATE TABLE category (
    id        INTEGER PRIMARY KEY,
    slug      TEXT    NOT NULL UNIQUE,
    name      TEXT    NOT NULL,
    color     TEXT    NOT NULL,
    description TEXT  NOT NULL,
    folder    TEXT,
    action    TEXT    NOT NULL DEFAULT 'capture'
) STRICT;

CREATE TABLE bookmark_category (
    bookmark   INTEGER NOT NULL REFERENCES bookmark(id) ON DELETE CASCADE,
    category   INTEGER NOT NULL REFERENCES category(id) ON DELETE CASCADE,
    confidence REAL    NOT NULL,
    assigned_by TEXT   NOT NULL,
    PRIMARY KEY (bookmark, category)
) STRICT;

CREATE INDEX bookmark_category_cat ON bookmark_category(category);

-- ─── Full-text index ─────────────────────────────────────────────────────
-- External content: only the inverted index is stored, never a second copy of
-- the text. `extra` carries the author, tags, and recognised tool names, so a
-- query for a tool name finds a bookmark whose prose never mentions it.
CREATE VIRTUAL TABLE search USING fts5(
    title, body, extra,
    content='bookmark',
    content_rowid='id',
    tokenize='porter unicode61 remove_daticritics 2'
);

CREATE TRIGGER search_ai AFTER INSERT ON bookmark BEGIN
    INSERT INTO search(rowid, title, body, extra) VALUES (new.id, new.title, new.body, new.extra);
END;

CREATE TRIGGER search_ad AFTER DELETE ON bookmark BEGIN
    INSERT INTO search(search, rowid, title, body, extra) VALUES('delete', old.id, old.title, old.body, old.extra);
END;

CREATE TRIGGER search_au AFTER UPDATE ON bookmark BEGIN
    INSERT INTO search(search, rowid, title, body, extra) VALUES('delete', old.id, old.title, old.body, old.extra);
    INSERT INTO search(rowid, title, body, extra) VALUES (new.id, new.title, new.body, new.extra);
END;

-- Rebuilding from a partially-updated index is cheap and exact:
--   INSERT INTO search(search) VALUES('rebuild');
CREATE VIRTUAL TABLE search_trigram USING fts5(
    title, body, extra,
    content='bookmark',
    content_rowid='id',
    tokenize='trigram'
);

-- ─── Run log ─────────────────────────────────────────────────────────────
-- One row per pipeline run, so `mbm status` can report what happened and
-- `mbm runs` can show a cost history without a separate metrics store.
CREATE TABLE run (
    id            INTEGER PRIMARY KEY,
    started_at    INTEGER NOT NULL,
    finished_at   INTEGER,
    outcome       TEXT,                -- ok | failed | cancelled
    bookmarks     INTEGER NOT NULL DEFAULT 0,
    written       INTEGER NOT NULL DEFAULT 0,
    skipped       INTEGER NOT NULL DEFAULT 0,
    failed        INTEGER NOT NULL DEFAULT 0,
    cost_micros   INTEGER NOT NULL DEFAULT 0,
    detail        TEXT
) STRICT;
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn migrated() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn a_fresh_database_reports_the_current_version() {
        let conn = migrated();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let conn = migrated();
        crate::migrate(&conn).unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn the_identity_index_rejects_a_duplicate_source_item() {
        let conn = migrated();
        let sql = "INSERT INTO bookmark(id, medium, external_id, ingested_at) VALUES (1,'x','a',0)";
        conn.execute(sql, []).unwrap();

        assert!(conn.execute(sql, []).is_err());
    }

    #[test]
    fn partial_indexes_only_cover_unfinished_rows() {
        let conn = migrated();
        conn.execute("INSERT INTO bookmark(id, medium, external_id, ingested_at, entities_at) VALUES (1,'x','a',0,123)", [])
            .unwrap();
        conn.execute("INSERT INTO bookmark(id, medium, external_id, ingested_at) VALUES (2,'x','b',0)", [])
            .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM bookmark WHERE entities_at IS NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn the_search_index_is_populated_by_a_trigger() {
        let conn = migrated();
        conn.execute(
            "INSERT INTO bookmark(id, medium, external_id, ingested_at, title, body, extra)
             VALUES (1,'x','a',0,'Simd things','we rewrote the tokenizer', 'GitHub')",
            [],
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM search WHERE search MATCH 'tokenizer'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "the insert trigger must have populated the index");
    }

    #[test]
    fn the_search_index_follows_an_update() {
        let conn = migrated();
        conn.execute(
            "INSERT INTO bookmark(id, medium, external_id, ingested_at, body) VALUES (1,'x','a',0,'cats')",
            [],
        )
        .unwrap();
        conn.execute("UPDATE bookmark SET body = 'dogs' WHERE id = 1", []).unwrap();
        let hits: i64 = conn
            .query_row("SELECT count(*) FROM search WHERE search MATCH 'cats'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 0, "the update trigger must have removed the stale entry");
        let hits: i64 = conn
            .query_row("SELECT count(*) FROM search WHERE search MATCH 'dogs'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn bm25_ranks_the_denser_match_first() {
        let conn = migrated();
        conn.execute(
            "INSERT INTO bookmark(id, medium, external_id, ingested_at, body) VALUES (1,'x','a',0,'rust rust rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO bookmark(id, medium, external_id, ingested_at, body) VALUES (2,'x','b',0,'rust and a great deal of other unrelated filler text')",
            [],
        )
        .unwrap();
        let ranked: Vec<i64> = conn
            .prepare("SELECT rowid FROM search WHERE search MATCH 'rust' ORDER BY rank")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(ranked, vec![1, 2], "a denser match should rank first");
    }

    #[test]
    fn stemming_and_diacritic_folding_both_work() {
        let conn = migrated();
        conn.execute(
            "INSERT INTO bookmark(id, medium, external_id, ingested_at, body) VALUES (1,'x','a',0,'Café naïve résumé')",
            [],
        )
        .unwrap();
        for term in ["cafe", "naive", "resume"] {
            let n: i64 = conn
                .query_row("SELECT count(*) FROM search WHERE search MATCH ?1", [term], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1, "`{term}` should fold onto the stored text");
        }
    }

    #[test]
    fn satellite_rows_are_cascaded_on_delete() {
        let conn = migrated();
        conn.execute("INSERT INTO bookmark(id, medium, external_id, ingested_at) VALUES (1,'x','a',0)", [])
            .unwrap();
        conn.execute("INSERT INTO tag(bookmark, tag) VALUES (1,'rust')", []).unwrap();
        conn.execute(
            "INSERT INTO link(bookmark, ordinal, original, resolved, kind) VALUES (1,0,'u','u','article')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM bookmark WHERE id = 1", []).unwrap();
        for table in ["tag", "link"] {
            let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{table} rows should cascade");
        }
    }
}
