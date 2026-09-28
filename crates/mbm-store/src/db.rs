//! opening the database and making it fast.
use mbm_core::error::{Error, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

// sqlite reads a negative count as kibibytes. 64mib holds the slice of the fts
// index a bm25 scan touches, which saves a disk round trip every few hundred
// candidates. the 2mib default costs one per candidate page.
const CACHE_KIB: i64 = -65_536;

// lets the os page in index sections on demand. no resident cost until touched.
const MMAP_BYTES: i64 = 512 * 1024 * 1024;

const BUSY_TIMEOUT_MS: u32 = 10_000;

pub fn open(path: &Path, create: bool) -> Result<Connection> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if create {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }

    let conn = Connection::open_with_flags(path, flags)
        .map_err(|e| Error::Store(format!("cannot open {}: {e}", path.display())))?;
    tune(&conn)?;
    Ok(conn)
}

pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()
        .map_err(|e| Error::Store(format!("cannot open in-memory database: {e}")))?;

    conn.execute_batch(&format!("PRAGMA page_size = 4096; PRAGMA cache_size = {CACHE_KIB};"))
        .map_err(|e| store_err(&e))?;
    Ok(conn)
}

fn tune(conn: &Connection) -> Result<()> {
    let _ = conn.execute_batch("PRAGMA page_size = 4096;");

    let _: String =
        conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)).map_err(|e| store_err(&e))?;

    conn.execute_batch(&format!(
        "PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = {CACHE_KIB};
         PRAGMA mmap_size = {MMAP_BYTES};
         PRAGMA temp_store = MEMORY;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};
         PRAGMA analysis_limit = 400;"
    ))
    .map_err(|e| store_err(&e))?;

    Ok(())
}

pub fn for_reading(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA query_only = ON; PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};"))
        .map_err(|e| store_err(&e))
}

// immediate acquires the write lock at the start, so two concurrent pipelines
// queue here instead of each finishing a long read pass and then colliding on
// the upgrade.
pub fn in_transaction<T>(
    conn: &mut Connection,
    body: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| store_err(&e))?;
    let out = body(&tx)?;
    tx.commit().map_err(|e| store_err(&e))?;
    Ok(out)
}

pub fn in_batches<T, F>(
    conn: &mut Connection,
    batch_size: usize,
    mut rows: T,
    mut insert: F,
) -> Result<usize>
where
    T: ExactSizeIterator + Iterator,
    F: FnMut(&Connection, T::Item) -> Result<()>,
{
    let batch_size = batch_size.max(1);
    let mut written = 0_usize;
    let mut pending: Vec<T::Item> = Vec::with_capacity(batch_size);

    for item in rows.by_ref() {
        pending.push(item);
        if pending.len() >= batch_size {
            written += flush(conn, &mut insert, &mut pending)?;
            pending = Vec::with_capacity(batch_size);
        }
    }

    if !pending.is_empty() {
        written += flush(conn, &mut insert, &mut pending)?;
    }

    Ok(written)
}

fn flush<R, F>(conn: &mut Connection, insert: &mut F, pending: &mut Vec<R>) -> Result<usize>
where
    F: FnMut(&Connection, R) -> Result<()>,
{
    let count = pending.len();
    let chunk = std::mem::take(pending);
    in_transaction(conn, |tx| {
        for row in chunk {
            insert(tx, row)?;
        }
        Ok(())
    })?;
    Ok(count)
}

#[must_use]
pub fn default_path() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("mebookmarker/bookmarks.db")
}

pub fn bulk_load_mode(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA journal_mode = MEMORY;
         PRAGMA locking_mode = EXCLUSIVE;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -262144;",
    )
    .map_err(|e| store_err(&e))
}

pub fn normal_mode(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA locking_mode = NORMAL; PRAGMA synchronous = NORMAL;")
        .map_err(|e| store_err(&e))
}

pub(crate) fn store_err(e: &rusqlite::Error) -> Error {
    Error::Store(e.to_string())
}

/// turn a `rusqlite::Result` into the workspace result type.
///
/// a `From` impl would be nicer, but `rusqlite::Error` is a foreign type and
/// so is `mbm_core::Error`, so neither orphan rule allows it. one extension
/// trait is the smallest thing that works, and it keeps the conversion in one
/// place rather than at two hundred `map_err` call sites.
pub trait SqlResultExt<T> {
    /// map the error into [`Error::Store`].
    fn sql(self) -> Result<T>;
}

impl<T> SqlResultExt<T> for rusqlite::Result<T> {
    fn sql(self) -> Result<T> {
        self.map_err(|e| store_err(&e))
    }
}
