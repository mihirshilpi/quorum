//! Teams: chats, chat members, chat messages, teams, channels and channel posts/replies.
//!
//! Message bodies are encrypted; the rest of each message (sender, times, mentions, reactions)
//! is stored as JSON with the body content blanked.

use jiff::Timestamp;
use quorum_model::{
    Channel, ChannelId, Chat, ChatId, ChatMessage, ChatMessageId, GraphEnum, MessageContainer, Team, TeamId, UserId,
};
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::mail::{MAX_BODY_LEN, plain_text};
use crate::store::{Ctx, Reader, Store, Tx, tables};
use crate::util::{
    Cursor, MAX_PAGE, Page, clamp_limit, decode_all, from_json, keyset_page, ms_or_zero, to_json, truncate, ts,
};

/// A channel post (top-level message) with its reply summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelPost {
    /// The post.
    pub post: ChatMessage,
    /// Number of cached replies.
    pub reply_count: u32,
    /// When the newest cached reply was created.
    pub last_reply_at: Option<Timestamp>,
}

fn chat_aad(chat: &str, id: &str) -> Vec<u8> {
    format!("chat_message:{chat}:{id}").into_bytes()
}

fn channel_aad(channel: &str, id: &str) -> Vec<u8> {
    format!("channel_message:{channel}:{id}").into_bytes()
}

fn chat_activity(c: &Chat) -> i64 {
    ms_or_zero(
        c.last_message_preview
            .as_ref()
            .and_then(|p| p.created_date_time)
            .or(c.last_updated_date_time)
            .or(c.created_date_time),
    )
}

/// Splits a message into (meta JSON without body content, sealed body, plain text for search).
fn split_message(ctx: &Ctx, aad: &[u8], m: &ChatMessage) -> Result<(String, Vec<u8>, String)> {
    if m.body.content.len() > MAX_BODY_LEN {
        return Err(Error::invalid("chat message body too large"));
    }
    let mut meta = m.clone();
    meta.body.content = Default::default();
    let sealed = ctx.cipher.seal(aad, m.body.content.as_str().as_bytes())?;
    let text = if m.is_deleted() { String::new() } else { plain_text(&m.body.content_type, m.body.content.as_str()) };
    Ok((to_json(&meta)?, sealed, text))
}

fn join_message(ctx: &Ctx, aad: &[u8], meta: &str, body: Option<&[u8]>) -> Result<ChatMessage> {
    let mut m: ChatMessage = from_json(meta)?;
    if let Some(sealed) = body {
        m.body.content = ctx.cipher.open_string(aad, sealed)?.into();
    }
    Ok(m)
}

impl Tx<'_> {
    /// Inserts or replaces chats by id; when a chat carries members, its member rows are
    /// replaced too.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_chats(&mut self, chats: &[Chat]) -> Result<()> {
        if chats.is_empty() {
            return Ok(());
        }
        let mut members_touched = false;
        for c in chats {
            self.tx
                .prepare_cached(
                    "INSERT INTO chat (id, chat_type, last_activity, has_unread, is_hidden, json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (id) DO UPDATE SET chat_type = excluded.chat_type,
                        last_activity = excluded.last_activity, has_unread = excluded.has_unread,
                        is_hidden = excluded.is_hidden, json = excluded.json",
                )?
                .execute(params![
                    c.id.as_str(),
                    c.chat_type.as_graph(),
                    chat_activity(c),
                    c.has_unread(),
                    c.is_hidden,
                    to_json(c)?
                ])?;
            if !c.members.is_empty() {
                self.tx.prepare_cached("DELETE FROM chat_member WHERE chat_id = ?1")?.execute([c.id.as_str()])?;
                let mut stmt = self.tx.prepare_cached(
                    "INSERT INTO chat_member (chat_id, ordinal, user_id, json) VALUES (?1, ?2, ?3, ?4)",
                )?;
                for (i, m) in c.members.iter().enumerate().take(10_000) {
                    stmt.execute(params![
                        c.id.as_str(),
                        i64::try_from(i).unwrap_or(i64::MAX),
                        m.user_id.as_ref().map(UserId::as_str),
                        to_json(m)?
                    ])?;
                }
                members_touched = true;
            }
        }
        self.touch(tables::CHAT);
        if members_touched {
            self.touch(tables::CHAT_MEMBER);
        }
        Ok(())
    }

    /// Deletes chats with their members and cached messages.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_chats(&mut self, ids: &[ChatId]) -> Result<()> {
        for id in ids {
            self.tx.execute(
                "DELETE FROM chat_fts WHERE rowid IN (SELECT rid FROM chat_message WHERE chat_id = ?1)",
                [id.as_str()],
            )?;
            self.tx.execute("DELETE FROM chat_message WHERE chat_id = ?1", [id.as_str()])?;
            self.tx.execute("DELETE FROM chat WHERE id = ?1", [id.as_str()])?;
        }
        if !ids.is_empty() {
            self.touch(tables::CHAT);
            self.touch(tables::CHAT_MEMBER);
            self.touch(tables::CHAT_MESSAGE);
        }
        Ok(())
    }

    /// Optimistically marks a chat read up to `read_at` (after `markChatReadForUser`).
    /// Returns `false` if the chat is not cached.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) if the stored chat cannot be decoded.
    pub fn mark_chat_read(&mut self, id: &ChatId, read_at: Timestamp) -> Result<bool> {
        let Some(mut chat) = self.read().chat(id)? else { return Ok(false) };
        chat.last_message_read_date_time = Some(read_at);
        self.upsert_chats(std::slice::from_ref(&chat))?;
        Ok(true)
    }

    /// Inserts or replaces chat messages, channel posts and channel replies (by container and
    /// id), encrypting bodies and indexing their text for search.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) if a body is larger than
    /// [`MAX_BODY_LEN`].
    pub fn upsert_chat_messages(&mut self, messages: &[ChatMessage]) -> Result<()> {
        let (mut chat, mut channel) = (false, false);
        for m in messages {
            match &m.container {
                MessageContainer::Chat(chat_id) => {
                    let (meta, sealed, text) = split_message(self.ctx, &chat_aad(chat_id.as_str(), m.id.as_str()), m)?;
                    let rid: i64 = self
                        .tx
                        .prepare_cached(
                            "INSERT INTO chat_message (chat_id, id, created_at, is_deleted, meta, body, body_pruned)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
                             ON CONFLICT (chat_id, id) DO UPDATE SET created_at = excluded.created_at,
                                is_deleted = excluded.is_deleted, meta = excluded.meta, body = excluded.body,
                                body_pruned = 0
                             RETURNING rid",
                        )?
                        .query_row(
                            params![
                                chat_id.as_str(),
                                m.id.as_str(),
                                ms_or_zero(m.created_date_time),
                                m.is_deleted(),
                                meta,
                                sealed
                            ],
                            |r| r.get(0),
                        )?;
                    self.tx.prepare_cached("DELETE FROM chat_fts WHERE rowid = ?1")?.execute([rid])?;
                    if !text.is_empty() {
                        self.tx
                            .prepare_cached("INSERT INTO chat_fts (rowid, body) VALUES (?1, ?2)")?
                            .execute(params![rid, text])?;
                    }
                    chat = true;
                }
                MessageContainer::Channel { team_id, channel_id } => {
                    let (meta, sealed, text) =
                        split_message(self.ctx, &channel_aad(channel_id.as_str(), m.id.as_str()), m)?;
                    let rid: i64 = self
                        .tx
                        .prepare_cached(
                            "INSERT INTO channel_message (channel_id, id, team_id, reply_to_id, created_at, is_deleted,
                                meta, body, body_pruned)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
                             ON CONFLICT (channel_id, id) DO UPDATE SET team_id = excluded.team_id,
                                reply_to_id = excluded.reply_to_id, created_at = excluded.created_at,
                                is_deleted = excluded.is_deleted, meta = excluded.meta, body = excluded.body,
                                body_pruned = 0
                             RETURNING rid",
                        )?
                        .query_row(
                            params![
                                channel_id.as_str(),
                                m.id.as_str(),
                                team_id.as_str(),
                                m.reply_to_id.as_ref().map(ChatMessageId::as_str),
                                ms_or_zero(m.created_date_time),
                                m.is_deleted(),
                                meta,
                                sealed
                            ],
                            |r| r.get(0),
                        )?;
                    self.tx.prepare_cached("DELETE FROM channel_fts WHERE rowid = ?1")?.execute([rid])?;
                    let subject = m.subject.as_ref().map_or("", |s| truncate(s.as_str(), 4096));
                    if !text.is_empty() || !subject.is_empty() {
                        self.tx
                            .prepare_cached("INSERT INTO channel_fts (rowid, subject, body) VALUES (?1, ?2, ?3)")?
                            .execute(params![rid, subject, text])?;
                    }
                    channel = true;
                }
            }
        }
        if chat {
            self.touch(tables::CHAT_MESSAGE);
        }
        if channel {
            self.touch(tables::CHANNEL_MESSAGE);
        }
        Ok(())
    }

    /// Deletes messages of one chat or channel by id (channel replies of a deleted post are
    /// deleted too).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_chat_messages(&mut self, container: &MessageContainer, ids: &[ChatMessageId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        match container {
            MessageContainer::Chat(chat) => {
                for id in ids {
                    self.tx.execute(
                        "DELETE FROM chat_fts WHERE rowid IN (SELECT rid FROM chat_message WHERE chat_id = ?1 AND id = ?2)",
                        params![chat.as_str(), id.as_str()],
                    )?;
                    self.tx.execute(
                        "DELETE FROM chat_message WHERE chat_id = ?1 AND id = ?2",
                        params![chat.as_str(), id.as_str()],
                    )?;
                }
                self.touch(tables::CHAT_MESSAGE);
            }
            MessageContainer::Channel { channel_id, .. } => {
                for id in ids {
                    let sel = "SELECT rid FROM channel_message WHERE channel_id = ?1 AND (id = ?2 OR reply_to_id = ?2)";
                    self.tx.execute(
                        &format!("DELETE FROM channel_fts WHERE rowid IN ({sel})"),
                        params![channel_id.as_str(), id.as_str()],
                    )?;
                    self.tx.execute(
                        "DELETE FROM channel_message WHERE channel_id = ?1 AND (id = ?2 OR reply_to_id = ?2)",
                        params![channel_id.as_str(), id.as_str()],
                    )?;
                }
                self.touch(tables::CHANNEL_MESSAGE);
            }
        }
        Ok(())
    }

    /// Inserts or replaces teams by id.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_teams(&mut self, teams: &[Team]) -> Result<()> {
        if teams.is_empty() {
            return Ok(());
        }
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO team (id, json) VALUES (?1, ?2) ON CONFLICT (id) DO UPDATE SET json = excluded.json",
            )?;
            for t in teams {
                stmt.execute(params![t.id.as_str(), to_json(t)?])?;
            }
        }
        self.touch(tables::TEAM);
        Ok(())
    }

    /// Deletes teams with their channels and channel messages.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_teams(&mut self, ids: &[TeamId]) -> Result<()> {
        for id in ids {
            self.tx.execute(
                "DELETE FROM channel_fts WHERE rowid IN (SELECT rid FROM channel_message WHERE team_id = ?1)",
                [id.as_str()],
            )?;
            self.tx.execute("DELETE FROM channel_message WHERE team_id = ?1", [id.as_str()])?;
            self.tx.execute("DELETE FROM channel WHERE team_id = ?1", [id.as_str()])?;
            self.tx.execute("DELETE FROM team WHERE id = ?1", [id.as_str()])?;
        }
        if !ids.is_empty() {
            self.touch(tables::TEAM);
            self.touch(tables::CHANNEL);
            self.touch(tables::CHANNEL_MESSAGE);
        }
        Ok(())
    }

    /// Inserts or replaces the channels of `team` by id.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_channels(&mut self, team: &TeamId, channels: &[Channel]) -> Result<()> {
        if channels.is_empty() {
            return Ok(());
        }
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO channel (id, team_id, json) VALUES (?1, ?2, ?3)
                 ON CONFLICT (id) DO UPDATE SET team_id = excluded.team_id, json = excluded.json",
            )?;
            for c in channels {
                let mut stored = c.clone();
                stored.team_id = Some(c.team_id.clone().unwrap_or_else(|| team.clone()));
                stmt.execute(params![c.id.as_str(), team.as_str(), to_json(&stored)?])?;
            }
        }
        self.touch(tables::CHANNEL);
        Ok(())
    }

    /// Deletes channels with their messages.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_channels(&mut self, ids: &[ChannelId]) -> Result<()> {
        for id in ids {
            self.tx.execute(
                "DELETE FROM channel_fts WHERE rowid IN (SELECT rid FROM channel_message WHERE channel_id = ?1)",
                [id.as_str()],
            )?;
            self.tx.execute("DELETE FROM channel_message WHERE channel_id = ?1", [id.as_str()])?;
            self.tx.execute("DELETE FROM channel WHERE id = ?1", [id.as_str()])?;
        }
        if !ids.is_empty() {
            self.touch(tables::CHANNEL);
            self.touch(tables::CHANNEL_MESSAGE);
        }
        Ok(())
    }
}

impl Reader<'_> {
    /// One chat.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) if the stored row cannot be decoded.
    pub fn chat(&self, id: &ChatId) -> Result<Option<Chat>> {
        let json: Option<String> = self
            .conn
            .prepare_cached("SELECT json FROM chat WHERE id = ?1")?
            .query_row([id.as_str()], |r| r.get(0))
            .optional()?;
        json.map(|j| from_json(&j)).transpose()
    }

    /// Chats ordered by last activity (newest first), keyset-paged. Hidden chats are left out
    /// unless `include_hidden`. Unread state is [`Chat::has_unread`].
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for a malformed cursor.
    pub fn chats(&self, after: Option<&Cursor>, limit: u32, include_hidden: bool) -> Result<Page<Chat>> {
        let mut sql = String::from("SELECT last_activity, id, json FROM chat WHERE 1");
        let mut args = Vec::new();
        if !include_hidden {
            sql.push_str(" AND is_hidden = 0");
        }
        if let Some(c) = after {
            let (sort, id) = c.key()?;
            sql.push_str(" AND (last_activity, id) < (?, ?)");
            args.push(Value::Integer(sort));
            args.push(Value::Text(id.to_owned()));
        }
        sql.push_str(" ORDER BY last_activity DESC, id DESC LIMIT ?");
        args.push(Value::Integer(i64::from(clamp_limit(limit)).saturating_add(1)));
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(keyset_page(&self.ctx.stats, tables::CHAT, rows, clamp_limit(limit), |j| from_json(&j)))
    }

    /// The number of chats with unread messages (excluding hidden chats).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn unread_chat_count(&self) -> Result<u32> {
        let n: i64 =
            self.conn
                .query_row("SELECT count(*) FROM chat WHERE has_unread = 1 AND is_hidden = 0", [], |r| r.get(0))?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Ids of the chats a user is a member of.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn chats_with_member(&self, user: &UserId) -> Result<Vec<ChatId>> {
        let mut stmt =
            self.conn.prepare_cached("SELECT DISTINCT chat_id FROM chat_member WHERE user_id = ?1 LIMIT ?2")?;
        let rows = stmt
            .query_map(params![user.as_str(), MAX_PAGE], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::CHAT_MEMBER, rows, |s| Ok(ChatId::new(s)?)).items)
    }

    /// Messages of a chat, newest first, keyset-paged. Bodies pruned by retention come back
    /// with empty content.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for a malformed cursor.
    pub fn chat_messages(&self, chat: &ChatId, after: Option<&Cursor>, limit: u32) -> Result<Page<ChatMessage>> {
        let limit = clamp_limit(limit);
        let (sort, id) = match after {
            Some(c) => {
                let (s, i) = c.key()?;
                (s, i.to_owned())
            }
            None => (i64::MAX, String::new()),
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT created_at, id, meta, body FROM chat_message
             WHERE chat_id = ?1 AND (created_at, id) < (?2, ?3)
             ORDER BY created_at DESC, id DESC LIMIT ?4",
        )?;
        // With no cursor, (i64::MAX, '') is above every key except an i64::MAX timestamp.
        let rows = stmt
            .query_map(params![chat.as_str(), sort, id, i64::from(limit).saturating_add(1)], |r| {
                Ok((r.get(0)?, r.get::<_, String>(1)?, (r.get::<_, String>(2)?, r.get::<_, Option<Vec<u8>>>(3)?)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let rows: Vec<(i64, String, (String, String, Option<Vec<u8>>))> =
            rows.into_iter().map(|(s, i, (m, b))| (s, i.clone(), (i, m, b))).collect();
        Ok(keyset_page(&self.ctx.stats, tables::CHAT_MESSAGE, rows, limit, |(id, meta, body)| {
            join_message(self.ctx, &chat_aad(chat.as_str(), &id), &meta, body.as_deref())
        }))
    }

    /// One chat or channel message.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Crypto`](crate::ErrorKind::Crypto) if the body cannot be decrypted.
    pub fn chat_message(&self, container: &MessageContainer, id: &ChatMessageId) -> Result<Option<ChatMessage>> {
        let (sql, scope, aad) = match container {
            MessageContainer::Chat(c) => (
                "SELECT meta, body FROM chat_message WHERE chat_id = ?1 AND id = ?2",
                c.as_str(),
                chat_aad(c.as_str(), id.as_str()),
            ),
            MessageContainer::Channel { channel_id, .. } => (
                "SELECT meta, body FROM channel_message WHERE channel_id = ?1 AND id = ?2",
                channel_id.as_str(),
                channel_aad(channel_id.as_str(), id.as_str()),
            ),
        };
        let row: Option<(String, Option<Vec<u8>>)> = self
            .conn
            .prepare_cached(sql)?
            .query_row(params![scope, id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        row.map(|(meta, body)| join_message(self.ctx, &aad, &meta, body.as_deref())).transpose()
    }

    /// Top-level posts of a channel, newest first, keyset-paged, with reply counts.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for a malformed cursor.
    pub fn channel_posts(&self, channel: &ChannelId, after: Option<&Cursor>, limit: u32) -> Result<Page<ChannelPost>> {
        let limit = clamp_limit(limit);
        let (sort, after_id) = match after {
            Some(c) => {
                let (s, i) = c.key()?;
                (s, i.to_owned())
            }
            None => (i64::MAX, String::new()),
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.created_at, p.id, p.meta, p.body,
                (SELECT count(*) FROM channel_message r WHERE r.channel_id = p.channel_id AND r.reply_to_id = p.id),
                (SELECT max(r.created_at) FROM channel_message r WHERE r.channel_id = p.channel_id AND r.reply_to_id = p.id)
             FROM channel_message p
             WHERE p.channel_id = ?1 AND p.reply_to_id IS NULL AND (p.created_at, p.id) < (?2, ?3)
             ORDER BY p.created_at DESC, p.id DESC LIMIT ?4",
        )?;
        type Raw = (String, String, Option<Vec<u8>>, i64, Option<i64>);
        let rows: Vec<(i64, String, Raw)> = stmt
            .query_map(params![channel.as_str(), sort, after_id, i64::from(limit).saturating_add(1)], |r| {
                let id: String = r.get(1)?;
                Ok((r.get(0)?, id.clone(), (id, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(keyset_page(&self.ctx.stats, tables::CHANNEL_MESSAGE, rows, limit, |(id, meta, body, count, last)| {
            Ok(ChannelPost {
                post: join_message(self.ctx, &channel_aad(channel.as_str(), &id), &meta, body.as_deref())?,
                reply_count: u32::try_from(count).unwrap_or(u32::MAX),
                last_reply_at: last.map(ts),
            })
        }))
    }

    /// Replies to a channel post, oldest first (at most 500).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn channel_replies(&self, channel: &ChannelId, post: &ChatMessageId) -> Result<Page<ChatMessage>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, meta, body FROM channel_message WHERE channel_id = ?1 AND reply_to_id = ?2
             ORDER BY created_at ASC, id ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![channel.as_str(), post.as_str(), MAX_PAGE], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<Vec<u8>>>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::CHANNEL_MESSAGE, rows, |(id, meta, body)| {
            join_message(self.ctx, &channel_aad(channel.as_str(), &id), &meta, body.as_deref())
        }))
    }

    /// All cached teams.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn teams(&self) -> Result<Page<Team>> {
        let mut stmt = self.conn.prepare_cached("SELECT json FROM team ORDER BY rowid LIMIT 2000")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::TEAM, rows, |j| from_json(&j)))
    }

    /// The cached channels of a team.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn channels(&self, team: &TeamId) -> Result<Page<Channel>> {
        let mut stmt =
            self.conn.prepare_cached("SELECT json FROM channel WHERE team_id = ?1 ORDER BY rowid LIMIT 2000")?;
        let rows = stmt.query_map([team.as_str()], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::CHANNEL, rows, |j| from_json(&j)))
    }
}

impl Store {
    /// See [`Reader::chats`].
    ///
    /// # Errors
    ///
    /// As [`Reader::chats`].
    pub async fn chats(&self, after: Option<Cursor>, limit: u32, include_hidden: bool) -> Result<Page<Chat>> {
        self.read(move |r| r.chats(after.as_ref(), limit, include_hidden)).await
    }

    /// See [`Reader::chat_messages`].
    ///
    /// # Errors
    ///
    /// As [`Reader::chat_messages`].
    pub async fn chat_messages(&self, chat: ChatId, after: Option<Cursor>, limit: u32) -> Result<Page<ChatMessage>> {
        self.read(move |r| r.chat_messages(&chat, after.as_ref(), limit)).await
    }

    /// See [`Reader::channel_posts`].
    ///
    /// # Errors
    ///
    /// As [`Reader::channel_posts`].
    pub async fn channel_posts(
        &self,
        channel: ChannelId,
        after: Option<Cursor>,
        limit: u32,
    ) -> Result<Page<ChannelPost>> {
        self.read(move |r| r.channel_posts(&channel, after.as_ref(), limit)).await
    }

    /// See [`Reader::channel_replies`].
    ///
    /// # Errors
    ///
    /// As [`Reader::channel_replies`].
    pub async fn channel_replies(&self, channel: ChannelId, post: ChatMessageId) -> Result<Page<ChatMessage>> {
        self.read(move |r| r.channel_replies(&channel, &post)).await
    }

    /// Upserts chat or channel messages in one transaction.
    ///
    /// # Errors
    ///
    /// As [`Tx::upsert_chat_messages`].
    pub async fn upsert_chat_messages(&self, messages: Vec<ChatMessage>) -> Result<()> {
        self.write(move |tx| tx.upsert_chat_messages(&messages)).await
    }

    /// Upserts chats in one transaction.
    ///
    /// # Errors
    ///
    /// As [`Tx::upsert_chats`].
    pub async fn upsert_chats(&self, chats: Vec<Chat>) -> Result<()> {
        self.write(move |tx| tx.upsert_chats(&chats)).await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn activity_falls_back() {
        // Covered end-to-end in tests/teams.rs; this keeps the helper honest for empty chats.
        let c: quorum_model::Chat = serde_json::from_str(r#"{"id":"c1","chatType":"group"}"#).unwrap();
        assert_eq!(super::chat_activity(&c), 0);
    }
}
