//! The outbox: user writes queued for replay against Graph, in order per resource.
//!
//! Lifecycle: [`Tx::outbox_enqueue`] (state `pending`) → [`Tx::outbox_claim_next`] (`claimed`)
//! → [`Tx::outbox_complete`] (row removed) or [`Tx::outbox_fail`] (back to `pending` with a
//! backoff, or `failed` after [`MAX_OUTBOX_ATTEMPTS`] or a permanent error). Failed entries
//! wait for the user: [`Tx::outbox_retry`] or [`Tx::outbox_discard`].
//!
//! Ordering: an entry is only claimable when no older entry for the same resource key exists
//! in any state, so a failed send blocks later edits of the same item instead of reordering
//! them. Claimed entries are made pending again when the store opens (the process stopped
//! mid-request), so replay is at-least-once; commands should be idempotent where Graph allows.
//!
//! Payloads are encrypted at rest (they can hold message text).

use std::fmt;
use std::time::Duration;

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::{Ctx, Reader, Store, Tx, tables};
use crate::util::{Page, clamp_limit, decode_all, ms, to_json, truncate, ts};

/// After this many failed attempts an entry becomes `failed` and waits for the user.
pub const MAX_OUTBOX_ATTEMPTS: u32 = 8;
/// The largest outbox payload (JSON bytes).
pub const MAX_OUTBOX_PAYLOAD: usize = 32 * 1024 * 1024;
const BASE_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
const MAX_ERROR_LEN: usize = 512;
const MAX_FIELD_LEN: usize = 1024;

/// The row id of an outbox entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutboxId(pub i64);

/// Where an outbox entry is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutboxState {
    /// Waiting to be sent (possibly after a backoff).
    Pending,
    /// Being sent by the sync engine.
    Claimed,
    /// Gave up; waiting for the user to retry or discard.
    Failed,
}

impl OutboxState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "claimed" => Ok(Self::Claimed),
            "failed" => Ok(Self::Failed),
            _ => Err(Error::corrupt("unknown outbox state")),
        }
    }
}

/// A write to queue.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewOutboxEntry {
    /// The command kind, e.g. `mail.send` or `chat.send`.
    pub kind: String,
    /// The resource key that orders entries, e.g. `message:<id>` or `chat:<id>`.
    pub resource: String,
    /// The local id of the optimistic item (draft id, temporary message id), if any.
    pub local_id: Option<String>,
    /// The command's parameters.
    pub payload: serde_json::Value,
}

impl fmt::Debug for NewOutboxEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewOutboxEntry").field("kind", &self.kind).finish_non_exhaustive()
    }
}

/// A queued write.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxEntry {
    /// The entry id.
    pub id: OutboxId,
    /// The command kind.
    pub kind: String,
    /// The ordering resource key.
    pub resource: String,
    /// The local id of the optimistic item, if any.
    pub local_id: Option<String>,
    /// The decrypted payload (may contain message text: never log it).
    pub payload: serde_json::Value,
    /// The lifecycle state.
    pub state: OutboxState,
    /// How many attempts failed so far.
    pub attempts: u32,
    /// Not to be retried before this instant.
    pub not_before: Timestamp,
    /// The last failure description (written by the caller; must not contain tenant data).
    pub last_error: Option<String>,
    /// When it was enqueued.
    pub created_at: Timestamp,
}

impl fmt::Debug for OutboxEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboxEntry")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("state", &self.state)
            .field("attempts", &self.attempts)
            .field("not_before", &self.not_before)
            .finish_non_exhaustive()
    }
}

/// Why an attempt failed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutboxFailure {
    /// A short description without tenant data (e.g. the Graph error code).
    pub error: String,
    /// The server's `Retry-After`, if any (overrides the exponential backoff when longer).
    pub retry_after: Option<Duration>,
    /// `true` when retrying cannot help (e.g. 400/403/404): the entry fails immediately.
    pub permanent: bool,
}

/// The exponential backoff before attempt `attempts + 1`: 2 s, 4 s, 8 s, … capped at 15 min.
#[must_use]
pub fn backoff(attempts: u32) -> Duration {
    let exp = attempts.saturating_sub(1).min(20);
    BASE_BACKOFF.saturating_mul(1u32 << exp).min(MAX_BACKOFF)
}

fn aad(kind: &str, resource: &str) -> Vec<u8> {
    format!("outbox:{kind}:{resource}").into_bytes()
}

fn check_field(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > MAX_FIELD_LEN || s.chars().any(char::is_control) {
        return Err(Error::invalid("invalid outbox kind, resource or local id"));
    }
    Ok(())
}

const SELECT: &str = "SELECT id, kind, resource, local_id, payload, state, attempts, not_before, last_error, created_at
                      FROM outbox";

type RawEntry = (i64, String, String, Option<String>, Vec<u8>, String, i64, i64, Option<String>, i64);

fn raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
}

fn decode(ctx: &Ctx, raw: RawEntry) -> Result<OutboxEntry> {
    let (mut entry, sealed) = decode_meta(raw)?;
    let json = ctx.cipher.open_string(&aad(&entry.kind, &entry.resource), &sealed)?;
    entry.payload = serde_json::from_str(&json)?;
    Ok(entry)
}

/// Decodes everything but the payload (left `null`).
fn decode_meta(raw: RawEntry) -> Result<(OutboxEntry, Vec<u8>)> {
    let (id, kind, resource, local_id, payload, state, attempts, not_before, last_error, created_at) = raw;
    let entry = OutboxEntry {
        id: OutboxId(id),
        payload: serde_json::Value::Null,
        kind,
        resource,
        local_id,
        state: OutboxState::parse(&state)?,
        attempts: u32::try_from(attempts).unwrap_or(0),
        not_before: ts(not_before),
        last_error,
        created_at: ts(created_at),
    };
    Ok((entry, payload))
}

/// Decodes an entry for listing: an undecodable payload becomes `null` (counted as corrupt)
/// so the user can still see and discard the entry.
fn decode_for_list(ctx: &Ctx, raw: RawEntry) -> Result<OutboxEntry> {
    let (mut entry, sealed) = decode_meta(raw)?;
    let payload = ctx
        .cipher
        .open_string(&aad(&entry.kind, &entry.resource), &sealed)
        .and_then(|json| Ok(serde_json::from_str(&json)?));
    match payload {
        Ok(p) => entry.payload = p,
        Err(e) => ctx.stats.record_corrupt(tables::OUTBOX, &e),
    }
    Ok(entry)
}

/// Makes entries that were claimed when the process stopped pending again.
pub(crate) fn reset_claimed(conn: &Connection) -> Result<()> {
    let n = conn.execute("UPDATE outbox SET state = 'pending' WHERE state = 'claimed'", [])?;
    if n > 0 {
        tracing::info!(name: "store.outbox_reclaimed", count = n, "outbox entries in flight at shutdown are pending again");
    }
    Ok(())
}

impl Tx<'_> {
    /// Queues a write. Call in the same transaction as the optimistic local change.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for an empty or overlong
    /// kind/resource/local id or a payload over [`MAX_OUTBOX_PAYLOAD`].
    pub fn outbox_enqueue(&mut self, entry: &NewOutboxEntry) -> Result<OutboxId> {
        self.outbox_enqueue_at(entry, Timestamp::UNIX_EPOCH)
    }

    /// Queues a write that must not be replayed before `not_before` (delayed send, undo
    /// send). Until then it can be withdrawn with [`Tx::outbox_discard_pending`]. Like any
    /// entry it also holds back later entries of the same resource.
    ///
    /// # Errors
    ///
    /// As [`Tx::outbox_enqueue`].
    pub fn outbox_enqueue_at(&mut self, entry: &NewOutboxEntry, not_before: Timestamp) -> Result<OutboxId> {
        check_field(&entry.kind)?;
        check_field(&entry.resource)?;
        if let Some(l) = &entry.local_id {
            check_field(l)?;
        }
        let json = to_json(&entry.payload)?;
        if json.len() > MAX_OUTBOX_PAYLOAD {
            return Err(Error::invalid("outbox payload too large"));
        }
        let sealed = self.ctx.cipher.seal(&aad(&entry.kind, &entry.resource), json.as_bytes())?;
        let now = ms(Timestamp::now());
        self.tx.execute(
            "INSERT INTO outbox (kind, resource, local_id, payload, state, attempts, not_before, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?6, ?5, ?5)",
            params![entry.kind, entry.resource, entry.local_id, sealed, now, ms(not_before)],
        )?;
        let id = self.tx.last_insert_rowid();
        self.touch(tables::OUTBOX);
        Ok(OutboxId(id))
    }

    /// Claims the oldest entry that is ready at `now`: pending, past its backoff, and with no
    /// older entry for the same resource. Returns `None` when nothing is ready.
    ///
    /// An entry that cannot be decrypted or decoded is marked `failed` (shown to the user for
    /// Discard) and counted in [`Store::corrupt_rows_skipped`]; it never stalls the queue.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a query fails.
    pub fn outbox_claim_next(&mut self, now: Timestamp) -> Result<Option<OutboxEntry>> {
        // Bounded: each undecodable candidate is marked failed and the next one is tried.
        for _ in 0..16 {
            let row = self
                .tx
                .prepare_cached(&format!(
                    "{SELECT} o WHERE o.state = 'pending' AND o.not_before <= ?1
                       AND NOT EXISTS (SELECT 1 FROM outbox p WHERE p.resource = o.resource AND p.id < o.id)
                     ORDER BY o.id LIMIT 1"
                ))?
                .query_row([ms(now)], raw)
                .optional()?;
            let Some(row) = row else { return Ok(None) };
            let id = row.0;
            match decode(self.ctx, row) {
                Ok(mut entry) => {
                    self.tx.execute(
                        "UPDATE outbox SET state = 'claimed', updated_at = ?2 WHERE id = ?1",
                        params![id, ms(now)],
                    )?;
                    entry.state = OutboxState::Claimed;
                    self.touch(tables::OUTBOX);
                    return Ok(Some(entry));
                }
                Err(e) => {
                    self.ctx.stats.record_corrupt(tables::OUTBOX, &e);
                    self.tx.execute(
                        "UPDATE outbox SET state = 'failed', last_error = 'undecodable entry', updated_at = ?2
                         WHERE id = ?1",
                        params![id, ms(now)],
                    )?;
                    self.touch(tables::OUTBOX);
                }
            }
        }
        Ok(None)
    }

    /// Removes a successfully replayed entry.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`](crate::ErrorKind::NotFound) if the entry does not exist.
    pub fn outbox_complete(&mut self, id: OutboxId) -> Result<()> {
        let n = self.tx.execute("DELETE FROM outbox WHERE id = ?1", [id.0])?;
        if n == 0 {
            return Err(Error::not_found("outbox entry"));
        }
        self.touch(tables::OUTBOX);
        Ok(())
    }

    /// Records a failed attempt. The entry goes back to `pending` after a backoff, or to
    /// `failed` for a permanent error or after [`MAX_OUTBOX_ATTEMPTS`]. Returns the new state.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`](crate::ErrorKind::NotFound) if the entry does not exist.
    pub fn outbox_fail(&mut self, id: OutboxId, failure: &OutboxFailure, now: Timestamp) -> Result<OutboxState> {
        let attempts: i64 = self
            .tx
            .query_row("SELECT attempts FROM outbox WHERE id = ?1", [id.0], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::not_found("outbox entry"))?;
        let attempts = u32::try_from(attempts).unwrap_or(0).saturating_add(1);
        let state = if failure.permanent || attempts >= MAX_OUTBOX_ATTEMPTS {
            OutboxState::Failed
        } else {
            OutboxState::Pending
        };
        let wait = backoff(attempts).max(failure.retry_after.unwrap_or_default().min(MAX_BACKOFF.saturating_mul(4)));
        let wait_ms = i64::try_from(wait.as_millis()).unwrap_or(i64::MAX);
        self.tx.execute(
            "UPDATE outbox SET state = ?2, attempts = ?3, not_before = ?4, last_error = ?5, updated_at = ?6 WHERE id = ?1",
            params![
                id.0,
                state.as_str(),
                attempts,
                ms(now).saturating_add(wait_ms),
                truncate(&failure.error, MAX_ERROR_LEN),
                ms(now)
            ],
        )?;
        self.touch(tables::OUTBOX);
        Ok(state)
    }

    /// Puts a failed (or pending) entry back in the queue immediately, resetting its attempts.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`](crate::ErrorKind::NotFound) if the entry does not exist.
    pub fn outbox_retry(&mut self, id: OutboxId) -> Result<()> {
        let n = self.tx.execute(
            "UPDATE outbox SET state = 'pending', attempts = 0, not_before = 0, updated_at = ?2 WHERE id = ?1",
            params![id.0, ms(Timestamp::now())],
        )?;
        if n == 0 {
            return Err(Error::not_found("outbox entry"));
        }
        self.touch(tables::OUTBOX);
        Ok(())
    }

    /// Removes an entry without replaying it and returns it, so the caller can revert its
    /// optimistic change in the same transaction. Returns `None` if it does not exist.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the write fails. An entry with an
    /// undecodable payload is still removed and returned with a `null` payload.
    pub fn outbox_discard(&mut self, id: OutboxId) -> Result<Option<OutboxEntry>> {
        self.discard_where(id, "")
    }

    /// Like [`Tx::outbox_discard`], but only while the entry is still `pending` (not yet
    /// claimed by the sync engine), so undo can never race a send in flight. Returns `None` if
    /// the entry does not exist or is claimed or failed.
    ///
    /// # Errors
    ///
    /// As [`Tx::outbox_discard`].
    pub fn outbox_discard_pending(&mut self, id: OutboxId) -> Result<Option<OutboxEntry>> {
        self.discard_where(id, " AND state = 'pending'")
    }

    fn discard_where(&mut self, id: OutboxId, filter: &'static str) -> Result<Option<OutboxEntry>> {
        let row =
            self.tx.prepare_cached(&format!("{SELECT} WHERE id = ?1{filter}"))?.query_row([id.0], raw).optional()?;
        let Some(row) = row else { return Ok(None) };
        self.tx.execute("DELETE FROM outbox WHERE id = ?1", [id.0])?;
        self.touch(tables::OUTBOX);
        match decode_for_list(self.ctx, row) {
            Ok(e) => Ok(Some(e)),
            Err(e) => {
                self.ctx.stats.record_corrupt(tables::OUTBOX, &e);
                Ok(None)
            }
        }
    }
}

impl Reader<'_> {
    /// Failed entries waiting for the user (Retry / Discard), oldest first. An entry whose
    /// payload cannot be decoded is listed with a `null` payload so it can still be discarded.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn outbox_failed(&self, limit: u32) -> Result<Page<OutboxEntry>> {
        self.outbox_where("WHERE state = 'failed'", limit)
    }

    /// All entries in queue order (for diagnostics), oldest first.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn outbox_entries(&self, limit: u32) -> Result<Page<OutboxEntry>> {
        self.outbox_where("", limit)
    }

    fn outbox_where(&self, filter: &str, limit: u32) -> Result<Page<OutboxEntry>> {
        let mut stmt = self.conn.prepare_cached(&format!("{SELECT} {filter} ORDER BY id LIMIT ?1"))?;
        let rows = stmt.query_map([clamp_limit(limit)], raw)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::OUTBOX, rows, |r| decode_for_list(self.ctx, r)))
    }

    /// The number of entries not yet failed (pending or claimed).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn outbox_pending_count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row("SELECT count(*) FROM outbox WHERE state != 'failed'", [], |r| r.get(0))?;
        Ok(crate::util::u64_from_i64(n))
    }

    /// The earliest instant at which some entry becomes claimable (to schedule a wake-up);
    /// entries queued behind an older entry of the same resource are not counted.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn outbox_next_ready_at(&self) -> Result<Option<Timestamp>> {
        let v: Option<i64> = self.conn.query_row(
            "SELECT min(o.not_before) FROM outbox o WHERE o.state = 'pending'
               AND NOT EXISTS (SELECT 1 FROM outbox p WHERE p.resource = o.resource AND p.id < o.id)",
            [],
            |r| r.get(0),
        )?;
        Ok(v.map(ts))
    }
}

impl Store {
    /// See [`Tx::outbox_enqueue`]. To enqueue together with an optimistic change, call the `Tx`
    /// method inside [`Store::write`].
    ///
    /// # Errors
    ///
    /// As [`Tx::outbox_enqueue`].
    pub async fn outbox_enqueue(&self, entry: NewOutboxEntry) -> Result<OutboxId> {
        self.write(move |tx| tx.outbox_enqueue(&entry)).await
    }

    /// See [`Tx::outbox_enqueue_at`].
    ///
    /// # Errors
    ///
    /// As [`Tx::outbox_enqueue_at`].
    pub async fn outbox_enqueue_at(&self, entry: NewOutboxEntry, not_before: Timestamp) -> Result<OutboxId> {
        self.write(move |tx| tx.outbox_enqueue_at(&entry, not_before)).await
    }

    /// See [`Tx::outbox_discard_pending`].
    ///
    /// # Errors
    ///
    /// As [`Tx::outbox_discard_pending`].
    pub async fn outbox_discard_pending(&self, id: OutboxId) -> Result<Option<OutboxEntry>> {
        self.write(move |tx| tx.outbox_discard_pending(id)).await
    }

    /// See [`Tx::outbox_claim_next`].
    ///
    /// # Errors
    ///
    /// As [`Tx::outbox_claim_next`].
    pub async fn outbox_claim_next(&self, now: Timestamp) -> Result<Option<OutboxEntry>> {
        self.write(move |tx| tx.outbox_claim_next(now)).await
    }

    /// See [`Tx::outbox_complete`].
    ///
    /// # Errors
    ///
    /// As [`Tx::outbox_complete`].
    pub async fn outbox_complete(&self, id: OutboxId) -> Result<()> {
        self.write(move |tx| tx.outbox_complete(id)).await
    }

    /// See [`Tx::outbox_fail`].
    ///
    /// # Errors
    ///
    /// As [`Tx::outbox_fail`].
    pub async fn outbox_fail(&self, id: OutboxId, failure: OutboxFailure, now: Timestamp) -> Result<OutboxState> {
        self.write(move |tx| tx.outbox_fail(id, &failure, now)).await
    }

    /// See [`Tx::outbox_retry`].
    ///
    /// # Errors
    ///
    /// As [`Tx::outbox_retry`].
    pub async fn outbox_retry(&self, id: OutboxId) -> Result<()> {
        self.write(move |tx| tx.outbox_retry(id)).await
    }

    /// See [`Tx::outbox_discard`].
    ///
    /// # Errors
    ///
    /// As [`Tx::outbox_discard`].
    pub async fn outbox_discard(&self, id: OutboxId) -> Result<Option<OutboxEntry>> {
        self.write(move |tx| tx.outbox_discard(id)).await
    }

    /// See [`Reader::outbox_failed`].
    ///
    /// # Errors
    ///
    /// As [`Reader::outbox_failed`].
    pub async fn outbox_failed(&self, limit: u32) -> Result<Page<OutboxEntry>> {
        self.read(move |r| r.outbox_failed(limit)).await
    }
}
