//! Test helpers (feature `test-util`, M-TEST-UTIL): build databases at old schema versions,
//! run raw SQL, inspect query plans. Not for production use.

use std::path::Path;

use rusqlite::Connection;

use crate::error::Result;
use crate::migrations;
use crate::store::{DB_FILE, Store};

/// Creates (or migrates) the database in `dir` up to schema `version` only, as an older
/// build would have left it. Open it afterwards with [`Store::open`] to run the remaining
/// migrations.
///
/// # Errors
///
/// [`ErrorKind::Io`](crate::ErrorKind::Io) or [`ErrorKind::Migration`](crate::ErrorKind::Migration).
pub fn create_at_version(dir: &Path, version: u32) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut conn = Connection::open(dir.join(DB_FILE))?;
    conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))?;
    migrations::migrate_to(&mut conn, version)
}

/// Runs raw SQL against the database file in `dir` (e.g. to plant data at an old version or
/// to set `user_version`).
///
/// # Errors
///
/// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the SQL fails.
pub fn execute_file(dir: &Path, sql: &str) -> Result<()> {
    let conn = Connection::open(dir.join(DB_FILE))?;
    conn.execute_batch(sql)?;
    Ok(())
}

/// Runs raw SQL in one write transaction of `store` (e.g. to corrupt a row). Emits no
/// change notification.
///
/// # Errors
///
/// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the SQL fails.
pub async fn execute(store: &Store, sql: &str) -> Result<()> {
    let sql = sql.to_owned();
    store.write(move |tx| Ok(tx.tx.execute_batch(&sql)?)).await
}

/// Runs a query returning one text-convertible column per row.
///
/// # Errors
///
/// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the SQL fails.
pub async fn query_strings(store: &Store, sql: &str) -> Result<Vec<String>> {
    let sql = sql.to_owned();
    store
        .read(move |r| {
            let mut stmt = r.conn.prepare(&sql)?;
            let rows = stmt.query_map([], |row| {
                let v: rusqlite::types::Value = row.get(0)?;
                Ok(match v {
                    rusqlite::types::Value::Null => String::new(),
                    rusqlite::types::Value::Integer(i) => i.to_string(),
                    rusqlite::types::Value::Real(f) => f.to_string(),
                    rusqlite::types::Value::Text(t) => t,
                    rusqlite::types::Value::Blob(b) => format!("<{} bytes>", b.len()),
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
}

/// The query plan (`EXPLAIN QUERY PLAN` detail lines) of `sql`.
///
/// # Errors
///
/// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the SQL cannot be planned.
pub async fn query_plan(store: &Store, sql: &str) -> Result<Vec<String>> {
    let sql = sql.to_owned();
    store.read(move |r| r.explain(&sql, Vec::new())).await
}
