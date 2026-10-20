//! Retention: drop old bodies while keeping headers (architecture §6).
//!
//! Each call prunes at most one batch so a single transaction stays short; call again until
//! it returns 0. Pruned bodies are re-downloaded on demand.

use jiff::Timestamp;
use quorum_model::Message;
use rusqlite::params;

use crate::error::Result;
use crate::mail::write_mail_fts;
use crate::store::{Store, Tx, tables};
use crate::util::{from_json, ms};

/// The most rows one retention call prunes.
pub const RETENTION_BATCH: u32 = 2000;

impl Tx<'_> {
    /// Deletes cached mail bodies of messages received before `before` (headers, previews and
    /// header search stay). Returns how many bodies were pruned (at most [`RETENTION_BATCH`]).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn prune_message_bodies(&mut self, before: Timestamp) -> Result<u32> {
        let rows: Vec<(i64, String, String)> = {
            let mut stmt = self.tx.prepare_cached(
                "SELECT m.rid, m.id, m.json FROM message_body b JOIN message m ON m.id = b.message_id
                 WHERE m.received_at < ?1 LIMIT ?2",
            )?;
            let it = stmt.query_map(params![ms(before), RETENTION_BATCH], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            it.collect::<rusqlite::Result<_>>()?
        };
        let mut n = 0u32;
        for (rid, id, json) in rows {
            self.tx.prepare_cached("DELETE FROM message_body WHERE message_id = ?1")?.execute([&id])?;
            match from_json::<Message>(&json) {
                Ok(m) => write_mail_fts(self, rid, &m, "")?,
                Err(e) => {
                    self.ctx.stats.record_corrupt(tables::MESSAGE, &e);
                    self.tx.prepare_cached("DELETE FROM mail_fts WHERE rowid = ?1")?.execute([rid])?;
                }
            }
            n = n.saturating_add(1);
        }
        if n > 0 {
            self.touch(tables::MESSAGE_BODY);
            tracing::info!(name: "store.pruned_mail_bodies", count = n, "pruned old mail bodies");
        }
        Ok(n)
    }

    /// Drops the bodies (and search rows) of chat and channel messages created before
    /// `before`; the message rows stay so threads keep their shape. Returns how many were
    /// pruned (at most [`RETENTION_BATCH`] per table).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn prune_chat_bodies(&mut self, before: Timestamp) -> Result<u32> {
        let mut total = 0u32;
        for (table, fts, name) in [
            ("chat_message", "chat_fts", tables::CHAT_MESSAGE),
            ("channel_message", "channel_fts", tables::CHANNEL_MESSAGE),
        ] {
            let sel =
                format!("SELECT rid FROM {table} WHERE created_at < ?1 AND body_pruned = 0 ORDER BY rid LIMIT ?2");
            self.tx
                .execute(&format!("DELETE FROM {fts} WHERE rowid IN ({sel})"), params![ms(before), RETENTION_BATCH])?;
            let n = self.tx.execute(
                &format!("UPDATE {table} SET body = NULL, body_pruned = 1 WHERE rid IN ({sel})"),
                params![ms(before), RETENTION_BATCH],
            )?;
            let n = u32::try_from(n).unwrap_or(u32::MAX);
            if n > 0 {
                self.touch(name);
            }
            total = total.saturating_add(n);
        }
        if total > 0 {
            tracing::info!(name: "store.pruned_chat_bodies", count = total, "pruned old chat message bodies");
        }
        Ok(total)
    }
}

impl Store {
    /// Prunes mail bodies older than `days` days before `now`, in batches until done (each
    /// batch is its own transaction). Returns the total pruned.
    ///
    /// # Errors
    ///
    /// As [`Tx::prune_message_bodies`].
    pub async fn prune_message_bodies(&self, now: Timestamp, days: u32) -> Result<u64> {
        let before = cutoff(now, days);
        self.prune_loop(move |tx| tx.prune_message_bodies(before)).await
    }

    /// Prunes chat and channel message bodies older than `days` days before `now`, in batches
    /// until done. Returns the total pruned.
    ///
    /// # Errors
    ///
    /// As [`Tx::prune_chat_bodies`].
    pub async fn prune_chat_bodies(&self, now: Timestamp, days: u32) -> Result<u64> {
        let before = cutoff(now, days);
        self.prune_loop(move |tx| tx.prune_chat_bodies(before)).await
    }

    async fn prune_loop(&self, f: impl Fn(&mut Tx<'_>) -> Result<u32> + Send + Sync + Clone + 'static) -> Result<u64> {
        let mut total = 0u64;
        // Bounded: 10 000 batches is far more than any real cache holds.
        for _ in 0..10_000 {
            let f = f.clone();
            let n = self.write(move |tx| f(tx)).await?;
            total = total.saturating_add(u64::from(n));
            if n == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(total)
    }
}

fn cutoff(now: Timestamp, days: u32) -> Timestamp {
    let span = jiff::SignedDuration::from_hours(i64::from(days).saturating_mul(24));
    now.checked_sub(span).unwrap_or(Timestamp::MIN)
}
