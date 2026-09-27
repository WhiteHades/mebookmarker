//! storage engine: schema, full-text index, fingerprints.

pub mod db;
pub mod fingerprint;
pub mod prefilter;
pub mod repo;
pub mod schema;
pub mod search;

use mbm_core::Result;

pub use repo::{Filter, Repo};
use rusqlite::Connection;
pub use search::{Hit, Mode, Searcher};

/// bring the database up to [`schema::SCHEMA_VERSION`].
pub fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 =
        conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| db::store_err(&e))?;
    if current >= schema::SCHEMA_VERSION {
        return Ok(());
    }
    for (i, sql) in schema::MIGRATIONS.iter().enumerate() {
        let target = i as i64 + 1;
        if target <= current {
            continue;
        }
        conn.execute_batch(sql).map_err(|e| db::store_err(&e))?;
        // PRAGMA does not take a bound parameter, and the value is a loop
        // index rather than anything a caller controls.
        conn.execute_batch(&format!("PRAGMA user_version = {target};"))
            .map_err(|e| db::store_err(&e))?;
    }
    Ok(())
}
