//! reading and writing bookmarks.
//!
//! [`Repo`] owns the statements and the row mapping. Everything above it works
//! with [`Bookmark`] values and never sees SQL.
//!
//! writes are upserts keyed on `(medium, external_id)`, which the schema
//! enforces with a unique index. that makes a re-import idempotent: running
//! `mbm ingest` twice leaves the store in the same state, and two concurrent
//! runs cannot both decide an item is new.

use ahash::{AHashMap, AHashSet};
use std::collections::BTreeSet;
use mbm_core::bookmark::{Assigner, BlockedReason, Bookmark, CategoryAssignment, Enrichment, Link, Media, MediaKind, ThreadRole};
use mbm_core::id::Id;
use mbm_core::medium::{LinkKind, SourceMedium};
use mbm_core::Result;
use rusqlite::{Connection, ToSql, params};
use serde_json::Value;
use std::str::FromStr;

use crate::db::{store_err, SqlResultExt};

/// rows written per transaction during a bulk import.
const IMPORT_BATCH: usize = 2_000;

#[derive(Debug)]
pub struct Repo<'conn> {
    conn: &'conn Connection,
}

impl<'conn> Repo<'conn> {
    /// wrap a connection.
    #[must_use]
    pub fn new(conn: &'conn Connection) -> Self {
        Self { conn }
    }

    /// the underlying connection.
    #[must_use]
    pub const fn conn(&self) -> &'conn Connection {
        self.conn
    }

    /// how many bookmarks are stored.
    pub fn count(&self) -> Result<usize> {
        self.conn
            .query_row("SELECT count(*) FROM bookmark", [], |r| r.get::<_, i64>(0))
            .sql()
            .map(|n| n as usize)
    }

    /// how many bookmarks are waiting on a stage.
    pub fn pending(&self, column: &str) -> Result<usize> {
        // `column` comes from a closed set of stage names in the caller, never
        // from user input, so interpolating it is safe. a bound parameter
        // cannot name a column, which is why this is not parameterised.
        debug_assert!(
            matches!(
                column,
                "entities_at" | "vision_at" | "tagged_at" | "categorized_at" | "described_at"
            ),
            "unknown stage column {column}"
        );
        self.conn
            .query_row(&format!("SELECT count(*) FROM bookmark WHERE {column} IS NULL"), [], |r| {
                r.get::<_, i64>(0)
            })
            .sql()
            .map(|n| n as usize)
    }

    /// ids waiting on a stage, in id order, one page at a time.
    ///
    /// keyset pagination on the primary key. `LIMIT`/`OFFSET` would get
    /// slower every page and would skip or repeat rows if the table changed
    /// mid-scan, which a scheduled run makes likely.
    pub fn pending_ids(
        &self,
        column: &str,
        after: Option<Id>,
        limit: usize,
    ) -> Result<Vec<Id>> {
        debug_assert!(
            matches!(
                column,
                "entities_at" | "vision_at" | "tagged_at" | "categorized_at" | "described_at"
            ),
            "unknown stage column {column}"
        );
        let sql = if after.is_some() {
            format!("SELECT id FROM bookmark WHERE {column} IS NULL AND id > ?1 ORDER BY id LIMIT ?2")
        } else {
            format!("SELECT id FROM bookmark WHERE {column} IS NULL ORDER BY id LIMIT ?2")
        };
        let mut stmt = self.conn.prepare(&sql).sql()?;
        let rows: Vec<i64> = match after {
            Some(after) => stmt
                .query_map(params![after.get() as i64, limit as i64], |r| r.get(0))
                .sql()?
                .collect::<rusqlite::Result<_>>()
                .sql()?,
            None => stmt
                .query_map(params![i64::MAX, limit as i64], |r| r.get(0))
                .sql()?
                .collect::<rusqlite::Result<_>>()
                .sql()?,
        };
        Ok(rows.into_iter().map(|id| Id::from_raw(id as u64)).collect())
    }

    /// the stored id for a source item, if we already have it.
    pub fn find_id(&self, medium: SourceMedium, external_id: &str) -> Result<Option<Id>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM bookmark WHERE medium = ?1 AND external_id = ?2")
            .sql()?;
        let mut rows = stmt.query(params![medium.name(), external_id]).sql()?;
        match rows.next().sql()? {
            Some(row) => {
                let id: i64 = row.get(0).sql()?;
                Ok(Some(Id::from_raw(id as u64)))
            }
            None => Ok(None),
        }
    }

    /// insert a bookmark, or leave the existing one alone.
    ///
    /// returns the stored id and whether this call was the one that wrote it.
    /// satellites are only written on a fresh insert, so a re-import never
    /// duplicates links or media.
    pub fn upsert(&self, bookmark: &Bookmark) -> Result<(Id, bool)> {
        let existing = self.find_id(bookmark.source.medium, &bookmark.source.external_id)?;
        if let Some(id) = existing {
            return Ok((id, false));
        }
        self.insert(bookmark)?;
        Ok((bookmark.id, true))
    }

    /// insert a bookmark and its satellites. assumes the identity is free.
    pub fn insert(&self, bookmark: &Bookmark) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| store_err(&e))?;
        insert_row(&tx, bookmark)?;
        tx.commit().map_err(|e| store_err(&e))
    }

    /// insert many bookmarks, committing in batches.
    ///
    /// an item already in the store is skipped, and so is a second item in the
    /// same batch with the same identity. that second case is a real one: a
    /// personal archive quotes the same post it bookmarks elsewhere, and a batch
    /// that aborted on the collision would lose every item after it.
    pub fn insert_many<'a>(&self, bookmarks: impl IntoIterator<Item = &'a Bookmark>) -> Result<usize> {
        let mut written = 0;
        let mut pending: Vec<&Bookmark> = Vec::with_capacity(IMPORT_BATCH);
        let mut queued: AHashSet<(SourceMedium, String)> = AHashSet::with_capacity(IMPORT_BATCH);

        for bookmark in bookmarks {
            let key = (bookmark.source.medium, bookmark.source.external_id.clone());
            if !queued.insert(key)
                || self.find_id(bookmark.source.medium, &bookmark.source.external_id)?.is_some()
            {
                continue;
            }
            pending.push(bookmark);
            if pending.len() >= IMPORT_BATCH {
                written += self.commit_batch(&mut pending)?;
                queued.clear();
            }
        }
        if !pending.is_empty() {
            written += self.commit_batch(&mut pending)?;
        }
        Ok(written)
    }

    fn commit_batch(&self, pending: &mut Vec<&Bookmark>) -> Result<usize> {
        let count = pending.len();
        let batch = std::mem::take(pending);
        let tx = self.conn.unchecked_transaction().map_err(|e| store_err(&e))?;
        for bookmark in &batch {
            insert_row(&tx, bookmark)?;
        }
        tx.commit().map_err(|e| store_err(&e))?;
        Ok(count)
    }

    /// load one bookmark with its satellites.
    pub fn load(&self, id: Id) -> Result<Option<Bookmark>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {ROW_COLUMNS} FROM bookmark b WHERE b.id = ?1"))
            .sql()?;
        let mut rows = stmt.query(params![id.get() as i64]).sql()?;
        let Some(row) = rows.next().sql()? else {
            return Ok(None);
        };
        let mut bookmark = map_row(row).sql()?;
        self.load_satellites(&mut bookmark)?;
        Ok(Some(bookmark))
    }

    /// fill in a bookmark's links, media, and categories from the store.
    pub fn load_satellites(&self, bookmark: &mut Bookmark) -> Result<()> {
        let id = bookmark.id.get() as i64;

        let mut stmt = self
            .conn
            .prepare(
                "SELECT original, resolved, kind, title, body, summary, blocked
                 FROM link WHERE bookmark = ?1 ORDER BY ordinal",
            )
            .sql()?;
        // the columns are NOT NULL in the schema, so a type mismatch here is a
        // bug rather than missing data. reading them with `ok` keeps the
        // closure in `rusqlite::Result` and lets the fallible url parse happen
        // outside it.
        let rows = stmt
            .query_map(params![id], |r| {
                Ok((
                    r.get::<_, String>(0).unwrap_or_default(),
                    r.get::<_, String>(1).unwrap_or_default(),
                    r.get::<_, String>(2).unwrap_or_default(),
                    r.get::<_, Option<String>>(3).unwrap_or_default(),
                    r.get::<_, Option<String>>(4).unwrap_or_default(),
                    r.get::<_, Option<String>>(5).unwrap_or_default(),
                    r.get::<_, Option<String>>(6).unwrap_or_default(),
                ))
            })
            .sql()?;
        for row in rows {
            let (original, resolved, kind, title, body, summary, blocked) =
                row.map_err(|e| store_err(&e))?;
            let (Some(original), Some(resolved)) =
                (parse_url_opt(&original), parse_url_opt(&resolved))
            else {
                continue;
            };
            bookmark.links.push(Link {
                original,
                resolved,
                kind: LinkKind::from_str(&kind).unwrap_or(LinkKind::Unknown),
                title,
                body,
                summary,
                blocked: blocked.as_deref().and_then(BlockedReason::parse),
            });
        }

        let mut stmt = self
            .conn
            .prepare(
                "SELECT kind, url, preview_url, width, height, duration_ms, alt_text
                 FROM media WHERE bookmark = ?1 ORDER BY ordinal",
            )
            .sql()?;
        let rows = stmt
            .query_map(params![id], |r| {
                Ok((
                    r.get::<_, String>(0).unwrap_or_default(),
                    r.get::<_, String>(1).unwrap_or_default(),
                    r.get::<_, Option<String>>(2).unwrap_or_default(),
                    r.get::<_, Option<i64>>(3).unwrap_or_default(),
                    r.get::<_, Option<i64>>(4).unwrap_or_default(),
                    r.get::<_, Option<i64>>(5).unwrap_or_default(),
                    r.get::<_, Option<String>>(6).unwrap_or_default(),
                ))
            })
            .sql()?;
        for row in rows {
            let (kind, url, preview, width, height, duration, alt) = row.map_err(|e| store_err(&e))?;
            let Some(url) = parse_url_opt(&url) else {
                continue;
            };
            bookmark.media.push(Media {
                kind: MediaKind::parse(&kind),
                url,
                preview_url: preview.and_then(|u| parse_url_opt(&u)),
                width: width.map(|v| v as u32),
                height: height.map(|v| v as u32),
                duration_ms: duration.map(|v| v as u64),
                alt_text: alt,
            });
        }

        let mut stmt = self
            .conn
            .prepare(
                "SELECT c.slug, bc.confidence, bc.assigned_by
                 FROM bookmark_category bc JOIN category c ON c.id = bc.category
                 WHERE bc.bookmark = ?1 ORDER BY bc.confidence DESC",
            )
            .sql()?;
        let rows = stmt
            .query_map(params![id], |r| {
                Ok((
                    r.get::<_, String>(0).unwrap_or_default(),
                    r.get::<_, f64>(1).unwrap_or_default(),
                    r.get::<_, String>(2).unwrap_or_default(),
                ))
            })
            .sql()?;
        for row in rows {
            let (slug, confidence, by) = row.map_err(|e| store_err(&e))?;
            bookmark.categories.push(CategoryAssignment {
                slug,
                confidence: confidence as f32,
                assigned_by: Assigner::parse(&by),
            });
        }

        // the tags are the one satellite the caller always wants, because a
        // list row shows them and a filter reads them
        let mut stmt = self
            .conn
            .prepare("SELECT tag FROM tag WHERE bookmark = ?1 ORDER BY tag")
            .sql()?;
        let rows = stmt
            .query_map(params![id], |r| r.get::<_, String>(0))
            .sql()?;
        for row in rows {
            bookmark.tags.insert(row.map_err(|e| store_err(&e))?);
        }

        Ok(())
    }

    /// load a page of bookmarks for a list view.
    pub fn list(&self, limit: usize, offset: usize) -> Result<Vec<Bookmark>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {ROW_COLUMNS} FROM bookmark b ORDER BY b.created_at DESC, b.id DESC LIMIT ?1 OFFSET ?2"
            ))
            .sql()?;
        let mut out = Vec::with_capacity(limit);
        let rows = stmt
            .query_map(params![limit as i64, offset as i64], map_row)
            .sql()?;
        for row in rows {
            out.push(row.map_err(|e| store_err(&e))?);
        }
        Ok(out)
    }

    /// the ids a search returned, loaded in rank order.
    pub fn load_ranked(&self, ids: &[Id]) -> Result<Vec<Bookmark>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(mut bookmark) = self.load(*id)? {
                self.load_satellites(&mut bookmark)?;
                out.push(bookmark);
            }
        }
        Ok(out)
    }

    /// record that a stage finished for these bookmarks.
    pub fn mark_stage(&self, column: &str, ids: &[Id], at: i64) -> Result<usize> {
        debug_assert!(
            matches!(
                column,
                "entities_at" | "vision_at" | "tagged_at" | "categorized_at" | "described_at"
            ),
            "unknown stage column {column}"
        );
        let mut changed = 0;
        for id in ids {
            changed += self
                .conn
                .execute(
                    &format!("UPDATE bookmark SET {column} = ?2 WHERE id = ?1"),
                    params![id.get() as i64, at],
                )
                .sql()?;
        }
        Ok(changed)
    }

    /// store a bookmark's fingerprint.
    pub fn set_fingerprint(&self, id: Id, fingerprint: u64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE bookmark SET fingerprint = ?2 WHERE id = ?1",
                params![id.get() as i64, fingerprint as i64],
            )
            .sql()?;
        Ok(())
    }

    /// store a generated title and summary.
    pub fn set_described(&self, id: Id, title: Option<&str>, summary: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE bookmark SET title = ?2, summary = ?3 WHERE id = ?1",
                params![id.get() as i64, title, summary],
            )
            .sql()?;
        Ok(())
    }

    /// store the text the full-text index reads.
    pub fn set_indexed_text(&self, id: Id, title: Option<&str>, extra: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE bookmark SET title = ?2, extra = ?3 WHERE id = ?1",
                params![id.get() as i64, title, extra],
            )
            .sql()?;
        Ok(())
    }

    /// replace a bookmark's tags.
    pub fn set_tags(&self, id: Id, tags: &AHashSet<String>) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| store_err(&e))?;
        tx.execute("DELETE FROM tag WHERE bookmark = ?1", params![id.get() as i64])
            .sql()?;
        for tag in tags {
            tx.execute(
                "INSERT OR IGNORE INTO tag(bookmark, tag) VALUES (?1, ?2)",
                params![id.get() as i64, tag],
            )
            .sql()?;
        }
        tx.commit().map_err(|e| store_err(&e))
    }

    /// upsert the category catalogue and return slug to row id.
    pub fn sync_categories(
        &self,
        categories: &mbm_core::category::Taxonomy,
    ) -> Result<AHashMap<String, i64>> {
        let tx = self.conn.unchecked_transaction().map_err(|e| store_err(&e))?;
        let mut out = AHashMap::new();
        for category in categories.iter() {
            tx.execute(
                "INSERT INTO category(slug, name, color, description, folder, action)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(slug) DO UPDATE SET
                     name = excluded.name,
                     color = excluded.color,
                     description = excluded.description,
                     folder = excluded.folder,
                     action = excluded.action",
                params![
                    category.slug,
                    category.name,
                    category.color,
                    category.description,
                    category.folder.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    category.action.to_string(),
                ],
            )
            .sql()?;
            let id: i64 = tx
                .query_row("SELECT id FROM category WHERE slug = ?1", params![category.slug], |r| r.get(0))
                .sql()?;
            out.insert(category.slug.clone(), id);
        }
        tx.commit().map_err(|e| store_err(&e))?;
        Ok(out)
    }

    /// replace a bookmark's category assignments.
    pub fn set_categories(
        &self,
        id: Id,
        assignments: &[CategoryAssignment],
        category_ids: &AHashMap<String, i64>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| store_err(&e))?;
        tx.execute("DELETE FROM bookmark_category WHERE bookmark = ?1", params![id.get() as i64])
            .sql()?;
        for assignment in assignments {
            let Some(&category_id) = category_ids.get(&assignment.slug) else {
                continue;
            };
            tx.execute(
                "INSERT OR REPLACE INTO bookmark_category(bookmark, category, confidence, assigned_by)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    id.get() as i64,
                    category_id,
                    assignment.confidence,
                    assignment.assigned_by.name()
                ],
            )
            .sql()?;
        }
        tx.commit().map_err(|e| store_err(&e))
    }

    /// every tag with its use count, for the TUI's tag list.
    pub fn tags_with_counts(&self, limit: usize) -> Result<Vec<(String, usize)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tag, count(*) FROM tag GROUP BY tag ORDER BY 2 DESC, 1 LIMIT ?1")
            .sql()?;
        let rows = stmt
            .query_map(params![limit as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)))
            .sql()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().sql()
    }

    /// bookmarks carrying a tag.
    pub fn by_tag(&self, tag: &str, limit: usize, offset: usize) -> Result<Vec<Bookmark>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {ROW_COLUMNS} FROM bookmark b \
                 JOIN tag t ON t.bookmark = b.id \
                 WHERE t.tag = ?1 ORDER BY b.created_at DESC LIMIT ?2 OFFSET ?3"
            ))
            .sql()?;
        let rows = stmt
            .query_map(params![tag, limit as i64, offset as i64], map_row)
            .sql()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().sql()
    }

    /// how many bookmarks there are in total, for paging.
    pub fn count_matching(&self, filter: &Filter) -> Result<usize> {
        let (where_clause, args): (String, Vec<Box<dyn ToSql>>) = filter.to_sql();
        let sql = format!("SELECT count(*) FROM bookmark b {where_clause}");
        let mut stmt = self.conn.prepare(&sql).sql()?;
        let refs: Vec<&dyn ToSql> = args.iter().map(AsRef::as_ref).collect();
        stmt.query_row(rusqlite::params_from_iter(refs), |r| r.get::<_, i64>(0)).sql().map(|n| n as usize)
    }

    /// a page of bookmarks matching a filter.
    pub fn query(&self, filter: &Filter, limit: usize, offset: usize) -> Result<Vec<Bookmark>> {
        let (where_clause, mut args): (String, Vec<Box<dyn ToSql>>) = filter.to_sql();
        args.push(Box::new(limit as i64));
        args.push(Box::new(offset as i64));
        let sql = format!(
            "SELECT {ROW_COLUMNS} FROM bookmark b {where_clause} \
             ORDER BY b.created_at DESC, b.id DESC LIMIT ?{} OFFSET ?{}",
            args.len() - 1,
            args.len()
        );
        let mut stmt = self.conn.prepare(&sql).sql()?;
        let refs: Vec<&dyn ToSql> = args.iter().map(AsRef::as_ref).collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(refs), map_row).sql()?;
        let mut out = Vec::new();
        for row in rows {
            let mut bookmark = row.map_err(|e| store_err(&e))?;
            self.load_satellites(&mut bookmark)?;
            out.push(bookmark);
        }
        Ok(out)
    }
}

/// a bookmark filter for list views.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// substring match over the body text.
    pub text: Option<String>,
    /// exact tag the bookmark must carry.
    pub tag: Option<String>,
    /// category slugs to include.
    pub categories: Vec<String>,
    /// only bookmarks from this source.
    pub medium: Option<SourceMedium>,
    /// only bookmarks in this date range, unix millis.
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// only bookmarks with or without a generated title.
    pub described: Option<bool>,
}

impl Filter {
    /// build the where clause and its bound arguments.
    fn to_sql(&self) -> (String, Vec<Box<dyn ToSql>>) {
        let mut clauses = Vec::new();
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        let mut n = 0;
        let mut next = || {
            n += 1;
            format!("?{n}")
        };

        if let Some(text) = self.text.as_deref().filter(|t| !t.trim().is_empty()) {
            // the same four columns the full-text index reads, so a filter and
            // a search agree on what is findable
            let p = next();
            clauses.push(format!(
                "(b.body LIKE {p} OR b.title LIKE {p} OR b.summary LIKE {p} OR b.extra LIKE {p})"
            ));
            // `%` and `_` are wildcards in LIKE, so a user typing one would
            // otherwise match everything. replacing them with a space keeps the
            // term literal without paying for an ESCAPE clause per column.
            let like = format!("%{}%", text.replace(['%', '_'], " "));
            for _ in 0..4 {
                args.push(Box::new(like.clone()));
            }
        }
        if let Some(tag) = self.tag.as_deref().filter(|t| !t.is_empty()) {
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM tag t WHERE t.bookmark = b.id AND t.tag = {})",
                next()
            ));
            args.push(Box::new(tag.to_owned()));
        }
        if !self.categories.is_empty() {
            let placeholders: Vec<String> = self.categories.iter().map(|_| next()).collect();
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM bookmark_category bc JOIN category c ON c.id = bc.category
                 WHERE bc.bookmark = b.id AND c.slug IN ({}))",
                placeholders.join(", ")
            ));
            for slug in &self.categories {
                args.push(Box::new(slug.clone()));
            }
        }
        if let Some(medium) = self.medium {
            clauses.push(format!("b.medium = {}", next()));
            args.push(Box::new(medium.name().to_owned()));
        }
        if let Some(since) = self.since {
            clauses.push(format!("b.created_at >= {}", next()));
            args.push(Box::new(since));
        }
        if let Some(until) = self.until {
            clauses.push(format!("b.created_at <= {}", next()));
            args.push(Box::new(until));
        }
        if let Some(described) = self.described {
            clauses.push(if described {
                "b.described_at IS NOT NULL".to_owned()
            } else {
                "b.described_at IS NULL".to_owned()
            });
        }

        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        (where_clause, args)
    }
}

const ROW_COLUMNS: &str = "b.id, b.medium, b.external_id, b.collection, b.url, b.author_handle, \
     b.author_name, b.title, b.summary, b.body, b.extra, b.role, b.created_at, b.ingested_at, \
     b.fingerprint, b.entities_at, b.vision_at, b.tagged_at, b.categorized_at, b.described_at, b.raw";

fn insert_row(conn: &Connection, bookmark: &Bookmark) -> Result<()> {
    conn.execute(
        "INSERT INTO bookmark(
            id, medium, external_id, collection, url, author_handle, author_name,
            title, summary, body, extra, role, created_at, ingested_at, fingerprint,
            entities_at, vision_at, tagged_at, categorized_at, described_at, raw)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
        params![
            bookmark.id.get() as i64,
            bookmark.source.medium.name(),
            bookmark.source.external_id,
            bookmark.source.collection,
            bookmark.url.as_ref().map(url::Url::as_str),
            bookmark.author.as_ref().map(|a| a.handle.as_str()),
            bookmark.author.as_ref().and_then(|a| a.name.as_deref()),
            bookmark.title,
            bookmark.raw.as_ref().and_then(extra_from_raw),
            bookmark.text,
            index_extra(bookmark),
            bookmark.role.map(ThreadRole::name),
            bookmark.created_at,
            bookmark.ingested_at,
            bookmark.fingerprint.map(|f| f as i64),
            bookmark.enrichment.entities_at,
            bookmark.enrichment.vision_at,
            bookmark.enrichment.tagged_at,
            bookmark.enrichment.categorized_at,
            bookmark.enrichment.described_at,
            bookmark.raw.as_ref().and_then(|v| serde_json::to_string(v).ok()),
        ],
    )
    .sql()?;

    let id = bookmark.id.get() as i64;
    for (ordinal, link) in bookmark.links.iter().enumerate() {
        conn.execute(
            "INSERT INTO link(bookmark, ordinal, original, resolved, kind, title, body, summary, blocked)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                ordinal as i64,
                link.original.as_str(),
                link.resolved.as_str(),
                link.kind.to_string(),
                link.title,
                link.body,
                link.summary,
                link.blocked.map(BlockedReason::name),
            ],
        )
        .sql()?;
    }

    for (ordinal, media) in bookmark.media.iter().enumerate() {
        conn.execute(
            "INSERT INTO media(bookmark, ordinal, kind, url, preview_url, width, height, duration_ms, alt_text)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                ordinal as i64,
                media.kind.name(),
                media.url.as_str(),
                media.preview_url.as_ref().map(url::Url::as_str),
                media.width.map(i64::from),
                media.height.map(i64::from),
                media.duration_ms.map(|d| d as i64),
                media.alt_text,
            ],
        )
        .sql()?;
    }

    for tag in &bookmark.tags {
        conn.execute(
            "INSERT OR IGNORE INTO tag(bookmark, tag) VALUES (?1, ?2)",
            params![id, tag],
        )
        .sql()?;
    }

    Ok(())
}

/// the text the full-text index reads alongside the body.
///
/// link hosts and link titles belong here. someone who bookmarked a post with
/// a github.com link expects a search for `github` to find it, and that word
/// appears nowhere in the post itself.
fn index_extra(bookmark: &Bookmark) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(bookmark.links.len() * 2 + 1);
    if let Some(handle) = bookmark.display_author() {
        parts.push(handle.to_owned());
    }
    for tag in &bookmark.tags {
        parts.push(tag.clone());
    }
    for link in &bookmark.links {
        if let Some(host) = link.resolved.host_str() {
            parts.push(host.trim_start_matches("www.").to_owned());
        }
        if let Some(title) = link.title.as_deref().filter(|t| !t.trim().is_empty()) {
            parts.push(title.to_owned());
        }
        if let Some(kind) = link.kind.title() {
            parts.push(kind.to_owned());
        }
    }
    parts.join(" ")
}

fn extra_from_raw(raw: &Value) -> Option<String> {
    raw.get("summary")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn parse_url_opt(raw: &str) -> Option<url::Url> {
    url::Url::parse(raw).ok()
}

type Row<'r> = rusqlite::Row<'r>;

fn map_row(row: &Row<'_>) -> rusqlite::Result<Bookmark> {
    let medium_name: String = row.get(1)?;
    let enrichment = Enrichment {
        entities_at: row.get(15)?,
        vision_at: row.get(16)?,
        tagged_at: row.get(17)?,
        categorized_at: row.get(18)?,
        described_at: row.get(19)?,
    };
    let raw: Option<String> = row.get(20)?;
    let role: Option<String> = row.get(11)?;

    Ok(Bookmark {
        id: Id::from_raw(row.get::<_, i64>(0)? as u64),
        source: mbm_core::bookmark::SourceRef {
            medium: SourceMedium::from_str(&medium_name).unwrap_or(SourceMedium::Manual),
            external_id: row.get(2)?,
            url: row.get::<_, Option<String>>(4)?.and_then(|u| parse_url_opt(&u)),
            collection: row.get(3)?,
        },
        author: row.get::<_, Option<String>>(5)?.map(|handle| mbm_core::bookmark::Author {
            handle,
            name: row.get::<_, Option<String>>(6).unwrap_or_default(),
            profile: None,
        }),
        title: row.get(7)?,
        text: row.get(9)?,
        url: row.get::<_, Option<String>>(4)?.and_then(|u| parse_url_opt(&u)),
        created_at: row.get(12)?,
        ingested_at: row.get(13)?,
        tags: BTreeSet::new(),
        links: Vec::new(),
        media: Vec::new(),
        categories: Vec::new(),
        role: role.as_deref().map(ThreadRole::parse),
        enrichment,
        fingerprint: row.get::<_, Option<i64>>(14)?.map(|f| f as u64),
        raw: raw.and_then(|r| serde_json::from_str(&r).ok()),
    })
}
