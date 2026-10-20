//! Local drafts of mail, chat messages and channel posts (never lose user text).
//!
//! Saving is one encrypted upsert, cheap enough to call on every pause in typing.

use std::fmt;

use jiff::Timestamp;
use quorum_model::Sensitive;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::{Ctx, Reader, Store, Tx, tables};
use crate::util::{Page, clamp_limit, decode_all, from_json, ms, to_json, ts};

/// The largest draft (JSON bytes of fields and body).
pub const MAX_DRAFT_LEN: usize = 32 * 1024 * 1024;
const MAX_ID_LEN: usize = 1024;

/// What a draft is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DraftKind {
    /// A mail message (new, reply or forward).
    Mail,
    /// A chat message.
    Chat,
    /// A channel post or reply.
    Channel,
}

impl DraftKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Chat => "chat",
            Self::Channel => "channel",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "mail" => Ok(Self::Mail),
            "chat" => Ok(Self::Chat),
            "channel" => Ok(Self::Channel),
            _ => Err(Error::corrupt("unknown draft kind")),
        }
    }
}

/// A locally saved draft.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    /// The local draft id (chosen by the caller, e.g. a UUID).
    pub id: String,
    /// What it is for.
    pub kind: DraftKind,
    /// What it belongs to, stored in clear for lookup: a chat id, `team/channel[/post]`, or the
    /// id of the message being replied to. Must not contain personal text.
    pub context: Option<String>,
    /// Structured fields (recipients, subject, importance, …), encrypted at rest.
    pub fields: serde_json::Value,
    /// The text being written, encrypted at rest.
    pub body: Sensitive,
    /// When it was last saved (set by the store).
    pub updated_at: Timestamp,
}

impl fmt::Debug for Draft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Draft")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("body", &self.body)
            .field("updated_at", &self.updated_at)
            .finish_non_exhaustive()
    }
}

impl Draft {
    /// A new empty draft.
    #[must_use]
    pub fn new(id: impl Into<String>, kind: DraftKind) -> Self {
        Self {
            id: id.into(),
            kind,
            context: None,
            fields: serde_json::Value::Null,
            body: Sensitive::default(),
            updated_at: Timestamp::UNIX_EPOCH,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    fields: serde_json::Value,
    body: Sensitive,
}

fn aad(id: &str) -> Vec<u8> {
    format!("draft:{id}").into_bytes()
}

fn check_id(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > MAX_ID_LEN || s.chars().any(char::is_control) {
        return Err(Error::invalid("invalid draft id or context"));
    }
    Ok(())
}

type RawDraft = (String, String, Option<String>, Vec<u8>, i64);

fn decode(ctx: &Ctx, (id, kind, context, payload, updated): RawDraft) -> Result<Draft> {
    let json = ctx.cipher.open_string(&aad(&id), &payload)?;
    let sealed: Sealed = from_json(&json)?;
    Ok(Draft {
        kind: DraftKind::parse(&kind)?,
        id,
        context,
        fields: sealed.fields,
        body: sealed.body,
        updated_at: ts(updated),
    })
}

impl Tx<'_> {
    /// Saves (inserts or replaces) a draft and returns the stored `updated_at`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for an invalid id/context or a
    /// draft over [`MAX_DRAFT_LEN`].
    pub fn save_draft(&mut self, draft: &Draft) -> Result<Timestamp> {
        check_id(&draft.id)?;
        if let Some(c) = &draft.context {
            check_id(c)?;
        }
        let json = to_json(&Sealed { fields: draft.fields.clone(), body: draft.body.clone() })?;
        if json.len() > MAX_DRAFT_LEN {
            return Err(Error::invalid("draft too large"));
        }
        let sealed = self.ctx.cipher.seal(&aad(&draft.id), json.as_bytes())?;
        let now = Timestamp::now();
        self.tx
            .prepare_cached(
                "INSERT INTO draft (id, kind, context, payload, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, context = excluded.context,
                    payload = excluded.payload, updated_at = excluded.updated_at",
            )?
            .execute(params![draft.id, draft.kind.as_str(), draft.context, sealed, ms(now)])?;
        self.touch(tables::DRAFT);
        Ok(now)
    }

    /// Deletes a draft (after sending or discarding). Returns `false` if it did not exist.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the write fails.
    pub fn delete_draft(&mut self, id: &str) -> Result<bool> {
        let n = self.tx.prepare_cached("DELETE FROM draft WHERE id = ?1")?.execute([id])?;
        if n > 0 {
            self.touch(tables::DRAFT);
        }
        Ok(n > 0)
    }
}

impl Reader<'_> {
    /// A draft by id.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Crypto`](crate::ErrorKind::Crypto) if it cannot be decrypted.
    pub fn draft(&self, id: &str) -> Result<Option<Draft>> {
        let row: Option<RawDraft> = self
            .conn
            .prepare_cached("SELECT id, kind, context, payload, updated_at FROM draft WHERE id = ?1")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
        row.map(|r| decode(self.ctx, r)).transpose()
    }

    /// The most recently saved draft of `kind` for `context` (e.g. the open chat), if any.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Crypto`](crate::ErrorKind::Crypto) if it cannot be decrypted.
    pub fn draft_for_context(&self, kind: DraftKind, context: &str) -> Result<Option<Draft>> {
        let row: Option<RawDraft> = self
            .conn
            .prepare_cached(
                "SELECT id, kind, context, payload, updated_at FROM draft WHERE kind = ?1 AND context = ?2
                 ORDER BY updated_at DESC LIMIT 1",
            )?
            .query_row(params![kind.as_str(), context], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
        row.map(|r| decode(self.ctx, r)).transpose()
    }

    /// Drafts of `kind` (or all kinds), most recently saved first.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn drafts(&self, kind: Option<DraftKind>, limit: u32) -> Result<Page<Draft>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, kind, context, payload, updated_at FROM draft WHERE ?1 IS NULL OR kind = ?1
             ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![kind.map(DraftKind::as_str), clamp_limit(limit)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<Vec<RawDraft>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::DRAFT, rows, |r| decode(self.ctx, r)))
    }
}

impl Store {
    /// See [`Tx::save_draft`].
    ///
    /// # Errors
    ///
    /// As [`Tx::save_draft`].
    pub async fn save_draft(&self, draft: Draft) -> Result<Timestamp> {
        self.write(move |tx| tx.save_draft(&draft)).await
    }

    /// See [`Reader::draft`].
    ///
    /// # Errors
    ///
    /// As [`Reader::draft`].
    pub async fn draft(&self, id: String) -> Result<Option<Draft>> {
        self.read(move |r| r.draft(&id)).await
    }

    /// See [`Reader::drafts`].
    ///
    /// # Errors
    ///
    /// As [`Reader::drafts`].
    pub async fn drafts(&self, kind: Option<DraftKind>, limit: u32) -> Result<Page<Draft>> {
        self.read(move |r| r.drafts(kind, limit)).await
    }

    /// See [`Tx::delete_draft`].
    ///
    /// # Errors
    ///
    /// As [`Tx::delete_draft`].
    pub async fn delete_draft(&self, id: String) -> Result<bool> {
        self.write(move |tx| tx.delete_draft(&id)).await
    }
}
