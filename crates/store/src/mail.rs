//! Mail: folders, message headers, encrypted bodies and attachment metadata.

use quorum_model::{
    Attachment, AttachmentId, BodyType, ConversationId, FlagStatus, GraphEnum, InferenceClassification, MailFolder,
    MailFolderId, Message, MessageBody, MessageId,
};
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::{Reader, Store, Tx, tables};
use crate::util::{
    Cursor, MAX_INDEXED_TEXT, MAX_PAGE, Page, clamp_limit, decode_all, from_json, html_to_text, keyset_page, ms,
    ms_or_zero, signature, to_json, truncate,
};

/// The largest message body the store accepts (bytes of content).
pub const MAX_BODY_LEN: usize = 32 * 1024 * 1024;

/// Which messages of a folder to list, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageQuery {
    /// The folder.
    pub folder: MailFolderId,
    /// `Some(true)` for the Focused tab, `Some(false)` for Other, `None` for both.
    #[serde(default)]
    pub focused: Option<bool>,
    /// Only unread messages.
    #[serde(default)]
    pub unread_only: bool,
    /// Continue after this cursor (from the previous page's [`Page::next`]).
    #[serde(default)]
    pub after: Option<Cursor>,
    /// Page size (clamped to `1..=500`).
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    50
}

impl MessageQuery {
    /// The first page of `folder`, 50 messages, no filters.
    #[must_use]
    pub fn folder(folder: MailFolderId) -> Self {
        Self { folder, focused: None, unread_only: false, after: None, limit: default_limit() }
    }
}

/// The number of unread cached messages in a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderCount {
    /// The folder.
    pub folder: MailFolderId,
    /// Unread messages cached locally.
    pub unread: u32,
}

/// Attachment metadata plus the blob cache key of its content, if downloaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAttachment {
    /// The message it belongs to.
    pub message_id: MessageId,
    /// The metadata.
    pub attachment: Attachment,
    /// The blob cache key, once the content was downloaded (see [`Store::put_blob`]).
    pub blob_key: Option<String>,
}

fn body_aad(id: &str) -> Vec<u8> {
    format!("message_body:{id}").into_bytes()
}

fn sender_text(m: &Message) -> String {
    m.from
        .as_ref()
        .or(m.sender.as_ref())
        .map(|a| format!("{} {}", a.name.as_str(), a.address.as_ref().map_or("", |x| x.as_str())))
        .unwrap_or_default()
}

fn mail_sig(m: &Message) -> i64 {
    signature(&[m.subject.as_str(), &sender_text(m), m.body_preview.as_str()])
}

impl Tx<'_> {
    /// Inserts or replaces mail folders by id.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_mail_folders(&mut self, folders: &[MailFolder]) -> Result<()> {
        if folders.is_empty() {
            return Ok(());
        }
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO mail_folder (id, parent_id, well_known, json) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (id) DO UPDATE SET parent_id = excluded.parent_id,
                    well_known = excluded.well_known, json = excluded.json",
            )?;
            for f in folders {
                stmt.execute(params![
                    f.id.as_str(),
                    f.parent_folder_id.as_ref().map(MailFolderId::as_str),
                    f.well_known_name.as_ref().map(GraphEnum::as_graph),
                    to_json(f)?
                ])?;
            }
        }
        self.touch(tables::MAIL_FOLDER);
        Ok(())
    }

    /// Deletes mail folders and every cached message in them.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_mail_folders(&mut self, ids: &[MailFolderId]) -> Result<()> {
        for id in ids {
            let blob_keys: Vec<String> = {
                let mut stmt = self.tx.prepare_cached(
                    "SELECT a.blob_key FROM attachment a JOIN message m ON m.id = a.message_id
                     WHERE m.folder_id = ?1 AND a.blob_key IS NOT NULL",
                )?;
                let rows = stmt.query_map([id.as_str()], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            self.delete_blob_rows(&blob_keys)?;
            self.tx.execute(
                "DELETE FROM mail_fts WHERE rowid IN (SELECT rid FROM message WHERE folder_id = ?1)",
                [id.as_str()],
            )?;
            let n = self.tx.execute("DELETE FROM message WHERE folder_id = ?1", [id.as_str()])?;
            self.tx.execute("DELETE FROM mail_folder WHERE id = ?1", [id.as_str()])?;
            if n > 0 {
                self.touch(tables::MESSAGE);
            }
        }
        if !ids.is_empty() {
            self.touch(tables::MAIL_FOLDER);
        }
        Ok(())
    }

    /// Inserts or updates message headers by id and keeps the mail search index current.
    ///
    /// Re-indexing happens only when the indexed text (subject, sender, preview) changed, so
    /// flag/read deltas stay cheap. Cached bodies are kept.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_messages(&mut self, messages: &[Message]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        for m in messages {
            let sig = mail_sig(m);
            let old_sig: Option<i64> = self
                .tx
                .prepare_cached("SELECT fts_sig FROM message WHERE id = ?1")?
                .query_row([m.id.as_str()], |r| r.get(0))
                .optional()?;
            let rid: i64 = self
                .tx
                .prepare_cached(
                    "INSERT INTO message (id, folder_id, conversation_id, received_at, is_read, flag, is_focused,
                        has_attachments, categories, fts_sig, json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                     ON CONFLICT (id) DO UPDATE SET folder_id = excluded.folder_id,
                        conversation_id = excluded.conversation_id, received_at = excluded.received_at,
                        is_read = excluded.is_read, flag = excluded.flag, is_focused = excluded.is_focused,
                        has_attachments = excluded.has_attachments, categories = excluded.categories,
                        fts_sig = excluded.fts_sig, json = excluded.json
                     RETURNING rid",
                )?
                .query_row(
                    params![
                        m.id.as_str(),
                        m.parent_folder_id.as_ref().map(MailFolderId::as_str),
                        m.conversation_id.as_ref().map(ConversationId::as_str),
                        ms_or_zero(m.received_date_time.or(m.sent_date_time)),
                        m.is_read,
                        m.flag_status.as_graph(),
                        m.inference_classification != InferenceClassification::Other,
                        m.has_attachments,
                        to_json(&m.categories)?,
                        sig,
                        to_json(m)?
                    ],
                    |r| r.get(0),
                )?;
            if old_sig != Some(sig) {
                let body = self.read().body_text(m.id.as_str())?;
                write_mail_fts(self, rid, m, &body)?;
            }
        }
        self.touch(tables::MESSAGE);
        Ok(())
    }

    /// Deletes messages by id, with their bodies, attachment metadata, cached attachment blobs
    /// and search index rows. Unknown ids are ignored.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_messages(&mut self, ids: &[MessageId]) -> Result<()> {
        let mut any = false;
        for id in ids {
            let Some(rid) = self.read().message_rid(id.as_str())? else { continue };
            let blob_keys: Vec<String> = {
                let mut stmt = self
                    .tx
                    .prepare_cached("SELECT blob_key FROM attachment WHERE message_id = ?1 AND blob_key IS NOT NULL")?;
                let rows = stmt.query_map([id.as_str()], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            self.delete_blob_rows(&blob_keys)?;
            self.tx.prepare_cached("DELETE FROM mail_fts WHERE rowid = ?1")?.execute([rid])?;
            self.tx.prepare_cached("DELETE FROM message WHERE rid = ?1")?.execute([rid])?;
            any = true;
        }
        if any {
            self.touch(tables::MESSAGE);
        }
        Ok(())
    }

    /// Stores (encrypted) the body of a cached message and indexes its plain text for search.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`](crate::ErrorKind::NotFound) if the message header is not cached,
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) if the body is larger than
    /// [`MAX_BODY_LEN`].
    pub fn put_message_body(&mut self, id: &MessageId, body: &MessageBody) -> Result<()> {
        if body.content.len() > MAX_BODY_LEN {
            return Err(Error::invalid("message body too large"));
        }
        let message = self.read().message(id)?.ok_or_else(|| Error::not_found("message"))?;
        let rid = self.read().message_rid(id.as_str())?.ok_or_else(|| Error::not_found("message"))?;
        let sealed = self.ctx.cipher.seal(&body_aad(id.as_str()), body.content.as_str().as_bytes())?;
        self.tx
            .prepare_cached(
                "INSERT INTO message_body (message_id, content_type, body, fetched_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (message_id) DO UPDATE SET content_type = excluded.content_type,
                    body = excluded.body, fetched_at = excluded.fetched_at",
            )?
            .execute(params![id.as_str(), body.content_type.as_graph(), sealed, ms(jiff::Timestamp::now())])?;
        let text = plain_text(&body.content_type, body.content.as_str());
        write_mail_fts(self, rid, &message, &text)?;
        self.touch(tables::MESSAGE_BODY);
        Ok(())
    }

    /// Optimistically marks a message read or unread. Returns `false` if it is not cached.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn set_message_read(&mut self, id: &MessageId, is_read: bool) -> Result<bool> {
        self.modify_message(id, |m| m.is_read = is_read)
    }

    /// Optimistically sets a message's follow-up flag. Returns `false` if it is not cached.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn set_message_flag(&mut self, id: &MessageId, flag: FlagStatus) -> Result<bool> {
        self.modify_message(id, |m| m.flag_status = flag)
    }

    /// Optimistically moves a message to another folder. Returns `false` if it is not cached.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn move_message(&mut self, id: &MessageId, folder: &MailFolderId) -> Result<bool> {
        self.modify_message(id, |m| m.parent_folder_id = Some(folder.clone()))
    }

    /// Loads, modifies and re-stores one message header. Returns `false` if it is not cached.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) if the stored header cannot be decoded.
    pub fn modify_message(&mut self, id: &MessageId, f: impl FnOnce(&mut Message)) -> Result<bool> {
        let Some(mut m) = self.read().message(id)? else { return Ok(false) };
        f(&mut m);
        m.id = id.clone();
        self.upsert_messages(std::slice::from_ref(&m))?;
        Ok(true)
    }

    /// Replaces the attachment metadata of a message (keeps blob keys of attachments that
    /// remain).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`](crate::ErrorKind::NotFound) if the message is not cached.
    pub fn put_attachments(&mut self, message: &MessageId, attachments: &[Attachment]) -> Result<()> {
        if self.read().message_rid(message.as_str())?.is_none() {
            return Err(Error::not_found("message"));
        }
        let keep: Vec<&str> = attachments.iter().map(|a| a.id.as_str()).collect();
        let existing: Vec<(String, Option<String>)> = {
            let mut stmt = self.tx.prepare_cached("SELECT id, blob_key FROM attachment WHERE message_id = ?1")?;
            let rows = stmt.query_map([message.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut gone_blobs = Vec::new();
        for (id, blob) in &existing {
            if !keep.contains(&id.as_str()) {
                self.tx
                    .prepare_cached("DELETE FROM attachment WHERE message_id = ?1 AND id = ?2")?
                    .execute(params![message.as_str(), id])?;
                if let Some(b) = blob {
                    gone_blobs.push(b.clone());
                }
            }
        }
        self.delete_blob_rows(&gone_blobs)?;
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO attachment (message_id, id, size, is_inline, json) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (message_id, id) DO UPDATE SET size = excluded.size,
                    is_inline = excluded.is_inline, json = excluded.json",
            )?;
            for a in attachments {
                stmt.execute(params![
                    message.as_str(),
                    a.id.as_str(),
                    crate::util::i64_from_u64(a.size),
                    a.is_inline,
                    to_json(a)?
                ])?;
            }
        }
        self.touch(tables::ATTACHMENT);
        Ok(())
    }

    /// Records (or clears) the blob cache key holding an attachment's downloaded content.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotFound`](crate::ErrorKind::NotFound) if the attachment is not cached.
    pub fn set_attachment_blob(
        &mut self,
        message: &MessageId,
        attachment: &AttachmentId,
        blob_key: Option<&str>,
    ) -> Result<()> {
        let n = self.tx.execute(
            "UPDATE attachment SET blob_key = ?3 WHERE message_id = ?1 AND id = ?2",
            params![message.as_str(), attachment.as_str(), blob_key],
        )?;
        if n == 0 {
            return Err(Error::not_found("attachment"));
        }
        self.touch(tables::ATTACHMENT);
        Ok(())
    }
}

pub(crate) fn plain_text(content_type: &BodyType, content: &str) -> String {
    match content_type {
        BodyType::Html => html_to_text(content),
        _ => truncate(content, MAX_INDEXED_TEXT).to_owned(),
    }
}

pub(crate) fn write_mail_fts(tx: &mut Tx<'_>, rid: i64, m: &Message, body: &str) -> Result<()> {
    tx.tx.prepare_cached("DELETE FROM mail_fts WHERE rowid = ?1")?.execute([rid])?;
    tx.tx
        .prepare_cached("INSERT INTO mail_fts (rowid, subject, sender, preview, body) VALUES (?1, ?2, ?3, ?4, ?5)")?
        .execute(params![
        rid,
        truncate(m.subject.as_str(), 4096),
        sender_text(m),
        truncate(m.body_preview.as_str(), 4096),
        body
    ])?;
    Ok(())
}

impl Reader<'_> {
    pub(crate) fn message_rid(&self, id: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .prepare_cached("SELECT rid FROM message WHERE id = ?1")?
            .query_row([id], |r| r.get(0))
            .optional()?)
    }

    /// The decrypted plain text of a cached body (empty if none), for indexing.
    pub(crate) fn body_text(&self, id: &str) -> Result<String> {
        let Some(body) = self.message_body(&MessageId::new(id)?)? else { return Ok(String::new()) };
        Ok(plain_text(&body.content_type, body.content.as_str()))
    }

    /// All cached mail folders.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn mail_folders(&self) -> Result<Page<MailFolder>> {
        let mut stmt = self.conn.prepare_cached("SELECT json FROM mail_folder ORDER BY rowid LIMIT 10000")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::MAIL_FOLDER, rows, |j| from_json(&j)))
    }

    /// One message header.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) if the stored row cannot be decoded.
    pub fn message(&self, id: &MessageId) -> Result<Option<Message>> {
        let json: Option<String> = self
            .conn
            .prepare_cached("SELECT json FROM message WHERE id = ?1")?
            .query_row([id.as_str()], |r| r.get(0))
            .optional()?;
        json.map(|j| from_json(&j)).transpose()
    }

    fn list_messages_sql(q: &MessageQuery) -> Result<(String, Vec<Value>)> {
        let mut sql = String::from("SELECT received_at, id, json FROM message WHERE folder_id = ?");
        let mut args = vec![Value::Text(q.folder.as_str().to_owned())];
        if let Some(focused) = q.focused {
            sql.push_str(" AND is_focused = ?");
            args.push(Value::Integer(i64::from(focused)));
        }
        if q.unread_only {
            sql.push_str(" AND is_read = 0");
        }
        if let Some(after) = &q.after {
            let (sort, id) = after.key()?;
            sql.push_str(" AND (received_at, id) < (?, ?)");
            args.push(Value::Integer(sort));
            args.push(Value::Text(id.to_owned()));
        }
        sql.push_str(" ORDER BY received_at DESC, id DESC LIMIT ?");
        args.push(Value::Integer(i64::from(clamp_limit(q.limit)).saturating_add(1)));
        Ok((sql, args))
    }

    /// Messages of a folder, newest first, keyset-paged.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for a malformed cursor.
    pub fn list_messages(&self, q: &MessageQuery) -> Result<Page<Message>> {
        let (sql, args) = Self::list_messages_sql(q)?;
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(keyset_page(&self.ctx.stats, tables::MESSAGE, rows, clamp_limit(q.limit), |j| from_json(&j)))
    }

    /// The SQLite query plan of [`Reader::list_messages`] (for index regression tests).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query cannot be planned.
    #[cfg(any(test, feature = "test-util"))]
    pub fn explain_list_messages(&self, q: &MessageQuery) -> Result<Vec<String>> {
        let (sql, args) = Self::list_messages_sql(q)?;
        self.explain(&sql, args)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn explain(&self, sql: &str, args: Vec<Value>) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// All cached messages of a conversation, oldest first (at most 500).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn conversation(&self, id: &ConversationId) -> Result<Page<Message>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT json FROM message WHERE conversation_id = ?1 ORDER BY received_at ASC, id ASC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![id.as_str(), MAX_PAGE], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::MESSAGE, rows, |j| from_json(&j)))
    }

    /// Unread cached messages per folder (folders with none are omitted).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn unread_counts(&self) -> Result<Vec<FolderCount>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT folder_id, count(*) FROM message WHERE is_read = 0 AND folder_id IS NOT NULL
             GROUP BY folder_id LIMIT 10000",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::MESSAGE, rows, |(f, n)| {
            Ok(FolderCount { folder: MailFolderId::new(f)?, unread: u32::try_from(n).unwrap_or(u32::MAX) })
        })
        .items)
    }

    /// The decrypted body of a message, if cached.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Crypto`](crate::ErrorKind::Crypto) if the body cannot be decrypted.
    pub fn message_body(&self, id: &MessageId) -> Result<Option<MessageBody>> {
        let row: Option<(String, Vec<u8>)> = self
            .conn
            .prepare_cached("SELECT content_type, body FROM message_body WHERE message_id = ?1")?
            .query_row([id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((content_type, sealed)) = row else { return Ok(None) };
        let content = self.ctx.cipher.open_string(&body_aad(id.as_str()), &sealed)?;
        Ok(Some(MessageBody { content_type: BodyType::from_graph(&content_type), content: content.into() }))
    }

    /// Ids of cached messages in `folder` received at or after `received_since` that have no
    /// cached body, newest first (for body prefetch). No decryption; at most 500.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn messages_without_body(
        &self,
        folder: &MailFolderId,
        received_since: jiff::Timestamp,
        limit: u32,
    ) -> Result<Vec<MessageId>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.id FROM message m INDEXED BY message_folder_received
             LEFT JOIN message_body b ON b.message_id = m.id
             WHERE m.folder_id = ?1 AND m.received_at >= ?2 AND b.message_id IS NULL
             ORDER BY m.received_at DESC, m.id DESC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![folder.as_str(), ms(received_since), clamp_limit(limit)], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::MESSAGE, rows, |s| Ok(MessageId::new(s)?)).items)
    }

    /// Whether a body is cached for the message (no decryption).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn has_message_body(&self, id: &MessageId) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT 1 FROM message_body WHERE message_id = ?1")?
            .query_row([id.as_str()], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Attachment metadata of a message.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn attachments(&self, message: &MessageId) -> Result<Page<StoredAttachment>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT json, blob_key FROM attachment WHERE message_id = ?1 ORDER BY rowid LIMIT 1000")?;
        let rows = stmt
            .query_map([message.as_str()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::ATTACHMENT, rows, |(j, blob_key)| {
            Ok(StoredAttachment { message_id: message.clone(), attachment: from_json(&j)?, blob_key })
        }))
    }
}

impl Store {
    /// See [`Reader::mail_folders`].
    ///
    /// # Errors
    ///
    /// As [`Reader::mail_folders`].
    pub async fn mail_folders(&self) -> Result<Page<MailFolder>> {
        self.read(|r| r.mail_folders()).await
    }

    /// See [`Reader::list_messages`].
    ///
    /// # Errors
    ///
    /// As [`Reader::list_messages`].
    pub async fn list_messages(&self, q: MessageQuery) -> Result<Page<Message>> {
        self.read(move |r| r.list_messages(&q)).await
    }

    /// See [`Reader::message`].
    ///
    /// # Errors
    ///
    /// As [`Reader::message`].
    pub async fn message(&self, id: MessageId) -> Result<Option<Message>> {
        self.read(move |r| r.message(&id)).await
    }

    /// See [`Reader::conversation`].
    ///
    /// # Errors
    ///
    /// As [`Reader::conversation`].
    pub async fn conversation(&self, id: ConversationId) -> Result<Page<Message>> {
        self.read(move |r| r.conversation(&id)).await
    }

    /// See [`Reader::unread_counts`].
    ///
    /// # Errors
    ///
    /// As [`Reader::unread_counts`].
    pub async fn unread_counts(&self) -> Result<Vec<FolderCount>> {
        self.read(|r| r.unread_counts()).await
    }

    /// See [`Reader::messages_without_body`].
    ///
    /// # Errors
    ///
    /// As [`Reader::messages_without_body`].
    pub async fn messages_without_body(
        &self,
        folder: MailFolderId,
        received_since: jiff::Timestamp,
        limit: u32,
    ) -> Result<Vec<MessageId>> {
        self.read(move |r| r.messages_without_body(&folder, received_since, limit)).await
    }

    /// See [`Reader::has_message_body`].
    ///
    /// # Errors
    ///
    /// As [`Reader::has_message_body`].
    pub async fn has_message_body(&self, id: MessageId) -> Result<bool> {
        self.read(move |r| r.has_message_body(&id)).await
    }

    /// See [`Reader::message_body`].
    ///
    /// # Errors
    ///
    /// As [`Reader::message_body`].
    pub async fn message_body(&self, id: MessageId) -> Result<Option<MessageBody>> {
        self.read(move |r| r.message_body(&id)).await
    }

    /// See [`Reader::attachments`].
    ///
    /// # Errors
    ///
    /// As [`Reader::attachments`].
    pub async fn attachments(&self, message: MessageId) -> Result<Page<StoredAttachment>> {
        self.read(move |r| r.attachments(&message)).await
    }

    /// Upserts headers and deletes removed ids in one transaction (one delta page).
    ///
    /// # Errors
    ///
    /// As [`Tx::upsert_messages`] and [`Tx::delete_messages`].
    pub async fn apply_message_delta(&self, upserts: Vec<Message>, deletes: Vec<MessageId>) -> Result<()> {
        self.write(move |tx| {
            tx.upsert_messages(&upserts)?;
            tx.delete_messages(&deletes)
        })
        .await
    }

    /// See [`Tx::put_message_body`].
    ///
    /// # Errors
    ///
    /// As [`Tx::put_message_body`].
    pub async fn put_message_body(&self, id: MessageId, body: MessageBody) -> Result<()> {
        self.write(move |tx| tx.put_message_body(&id, &body)).await
    }
}
