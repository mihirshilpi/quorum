//! The encrypted blob cache (attachment content, thumbnails, downloaded files) with a size
//! quota and least-recently-used eviction.
//!
//! File-backed stores keep each blob as one encrypted file under `<account>/blobs/` with a
//! random name (the key, which may contain ids, is only in the database). In-memory stores keep
//! the ciphertext in the `blob` table.

use std::collections::HashSet;
use std::io::Write;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::{Ctx, Store, Tx, random_hex, tables};
use crate::util::{i64_from_u64, ms, u64_from_i64};

/// The default blob cache quota (2 GiB, architecture §6).
pub const DEFAULT_BLOB_QUOTA: u64 = 2 * 1024 * 1024 * 1024;
const QUOTA_SETTING: &str = "store.blob_quota_bytes";
const MAX_KEY_LEN: usize = 1024;
const MAX_SWEEP_ENTRIES: usize = 200_000;
const MAX_EVICTIONS: usize = 100_000;

/// How much the blob cache holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlobUsage {
    /// Number of cached blobs.
    pub count: u64,
    /// Total stored bytes (ciphertext).
    pub bytes: u64,
    /// The configured quota in bytes.
    pub quota: u64,
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_LEN || key.chars().any(char::is_control) {
        return Err(Error::invalid("invalid blob key"));
    }
    Ok(())
}

fn aad(key: &str) -> Vec<u8> {
    format!("blob:{key}").into_bytes()
}

pub(crate) fn quota(conn: &Connection) -> Result<u64> {
    let v: Option<String> =
        conn.query_row("SELECT value FROM setting WHERE key = ?1", [QUOTA_SETTING], |r| r.get(0)).optional()?;
    Ok(v.and_then(|s| s.parse::<u64>().ok()).unwrap_or(DEFAULT_BLOB_QUOTA))
}

impl Tx<'_> {
    /// Deletes blob rows (and, after commit, their files).
    pub(crate) fn delete_blob_rows(&mut self, keys: &[String]) -> Result<()> {
        for key in keys {
            let file: Option<Option<String>> = self
                .tx
                .prepare_cached("DELETE FROM blob WHERE key = ?1 RETURNING file")?
                .query_row([key], |r| r.get(0))
                .optional()?;
            if let Some(file) = file {
                self.touch(tables::BLOB);
                if let (Some(name), Some(dir)) = (file, self.ctx.blob_dir.as_ref()) {
                    self.remove_after_commit(dir.join(name));
                }
            }
        }
        Ok(())
    }

    /// Evicts least-recently-used blobs (never `protect`) until the total fits the quota.
    /// Returns how many were evicted.
    pub(crate) fn enforce_blob_quota(&mut self, protect: Option<&str>) -> Result<u32> {
        let quota = quota(&self.tx)?;
        let total: i64 = self.tx.query_row("SELECT coalesce(sum(size), 0) FROM blob", [], |r| r.get(0))?;
        let mut total = u64_from_i64(total);
        if total <= quota {
            return Ok(0);
        }
        let victims: Vec<(String, i64)> = {
            let mut stmt =
                self.tx.prepare_cached("SELECT key, size FROM blob ORDER BY last_access ASC, key ASC LIMIT ?1")?;
            let rows = stmt.query_map([i64_from_u64(MAX_EVICTIONS as u64)], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut evict = Vec::new();
        for (key, size) in victims {
            if total <= quota {
                break;
            }
            if protect == Some(key.as_str()) {
                continue;
            }
            total = total.saturating_sub(u64_from_i64(size));
            evict.push(key);
        }
        let n = u32::try_from(evict.len()).unwrap_or(u32::MAX);
        self.delete_blob_rows(&evict)?;
        if n > 0 {
            tracing::info!(name: "store.blob_evicted", count = n, "evicted cached blobs over quota");
        }
        Ok(n)
    }
}

fn next_access(conn: &Connection) -> Result<i64> {
    let max: Option<i64> = conn.query_row("SELECT max(last_access) FROM blob", [], |r| r.get(0))?;
    let now = ms(jiff::Timestamp::now());
    Ok(max.map_or(now, |m| now.max(m.saturating_add(1))))
}

impl Store {
    /// Stores `bytes` under `key` (encrypted), replacing any previous value, then evicts
    /// least-recently-used blobs until the cache fits its quota.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for an empty, overlong or
    /// control-character key, or a blob larger than the quota; [`ErrorKind::Io`](crate::ErrorKind::Io)
    /// if the file cannot be written.
    pub async fn put_blob(&self, key: String, bytes: Vec<u8>) -> Result<()> {
        validate_key(&key)?;
        self.blocking(move |s| s.put_blob_blocking(&key, &bytes)).await
    }

    fn put_blob_blocking(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let ctx = self.ctx();
        let sealed = ctx.cipher.seal(&aad(key), bytes)?;
        let size = sealed.len() as u64;
        let quota = self.read_blocking(|r| quota(r.conn))?;
        if size > quota {
            return Err(Error::invalid("blob larger than the cache quota"));
        }
        let (file, data) = match &ctx.blob_dir {
            Some(dir) => {
                let name = format!("{}.bin", random_hex(16));
                let tmp = dir.join(format!("{name}.tmp"));
                let result = (|| -> Result<()> {
                    let mut f = std::fs::File::create(&tmp)?;
                    f.write_all(&sealed)?;
                    f.sync_all()?;
                    std::fs::rename(&tmp, dir.join(&name))?;
                    Ok(())
                })();
                if let Err(e) = result {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                (Some(name), None)
            }
            None => (None, Some(sealed)),
        };
        let written = file.clone();
        let result = self.write_blocking(|tx| {
            tx.delete_blob_rows(&[key.to_owned()])?;
            let access = next_access(&tx.tx)?;
            tx.tx.execute(
                "INSERT INTO blob (key, file, data, size, last_access) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![key, file, data, i64_from_u64(size), access],
            )?;
            tx.touch(tables::BLOB);
            tx.enforce_blob_quota(Some(key))?;
            Ok(())
        });
        if result.is_err()
            && let (Some(name), Some(dir)) = (written, &ctx.blob_dir)
        {
            let _ = std::fs::remove_file(dir.join(name));
        }
        result
    }

    /// Returns the decrypted blob stored under `key` and marks it recently used.
    ///
    /// A blob whose file disappeared is forgotten and reported as `None`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Crypto`](crate::ErrorKind::Crypto) if the content cannot be decrypted,
    /// [`ErrorKind::Io`](crate::ErrorKind::Io) if the file cannot be read.
    pub async fn get_blob(&self, key: String) -> Result<Option<Vec<u8>>> {
        validate_key(&key)?;
        self.blocking(move |s| s.get_blob_blocking(&key)).await
    }

    fn get_blob_blocking(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let ctx = self.ctx();
        let row: Option<(Option<String>, Option<Vec<u8>>)> = self.read_blocking(|r| {
            Ok(r.conn
                .prepare_cached("SELECT file, data FROM blob WHERE key = ?1")?
                .query_row([key], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?)
        })?;
        let Some((file, data)) = row else { return Ok(None) };
        let sealed = match (file, data, &ctx.blob_dir) {
            (Some(name), _, Some(dir)) => match std::fs::read(dir.join(name)) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.write_blocking(|tx| tx.delete_blob_rows(&[key.to_owned()]))?;
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            },
            (_, Some(data), _) => data,
            _ => return Err(Error::corrupt("blob row has no content")),
        };
        let plain = ctx.cipher.open(&aad(key), &sealed)?;
        // Access-time bumps are bookkeeping: not reported on the change feed.
        self.write_blocking(|tx| {
            let access = next_access(&tx.tx)?;
            tx.tx.execute("UPDATE blob SET last_access = ?2 WHERE key = ?1", params![key, access])?;
            Ok(())
        })?;
        Ok(Some(plain))
    }

    /// Removes the blob stored under `key`, if any.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the write fails.
    pub async fn delete_blob(&self, key: String) -> Result<()> {
        self.write(move |tx| tx.delete_blob_rows(&[key])).await
    }

    /// Current blob cache usage and quota.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub async fn blob_usage(&self) -> Result<BlobUsage> {
        self.read(|r| {
            let (count, bytes): (i64, i64) =
                r.conn.query_row("SELECT count(*), coalesce(sum(size), 0) FROM blob", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
            Ok(BlobUsage { count: u64_from_i64(count), bytes: u64_from_i64(bytes), quota: quota(r.conn)? })
        })
        .await
    }

    /// Sets the blob cache quota in bytes and evicts down to it. Returns how many blobs were
    /// evicted.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the write fails.
    pub async fn set_blob_quota(&self, bytes: u64) -> Result<u32> {
        self.write(move |tx| {
            tx.tx.execute(
                "INSERT INTO setting (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![QUOTA_SETTING, bytes.to_string(), ms(jiff::Timestamp::now())],
            )?;
            tx.enforce_blob_quota(None)
        })
        .await
    }

    /// Evicts least-recently-used blobs until the cache fits its quota. Returns how many were
    /// evicted.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the write fails.
    pub async fn enforce_blob_quota(&self) -> Result<u32> {
        self.write(|tx| tx.enforce_blob_quota(None)).await
    }
}

/// Removes blob files the database no longer references (left over from a crash between
/// writing a file and committing its row, or a crash before post-commit removal).
pub(crate) fn sweep_orphans(ctx: &Ctx, conn: &Connection) -> Result<()> {
    let Some(dir) = &ctx.blob_dir else { return Ok(()) };
    let known: HashSet<String> = {
        let mut stmt = conn.prepare("SELECT file FROM blob WHERE file IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut removed = 0u32;
    for entry in std::fs::read_dir(dir)?.take(MAX_SWEEP_ENTRIES) {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !known.contains(name)
            && entry.file_type().is_ok_and(|t| t.is_file())
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed = removed.saturating_add(1);
        }
    }
    if removed > 0 {
        tracing::info!(name: "store.blob_orphans_removed", count = removed, "removed orphaned blob files");
    }
    Ok(())
}
