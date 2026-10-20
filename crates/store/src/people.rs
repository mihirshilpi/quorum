//! People (directory users and contacts) and their presence.

use jiff::Timestamp;
use quorum_model::{GraphEnum, Person, Presence, UserId};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::store::{Reader, Store, Tx, tables};
use crate::util::{Page, clamp_limit, decode_all, fold, from_json, ms, prefix_upper, to_json, truncate, ts};

/// The most user ids one presence lookup accepts.
pub const MAX_PRESENCE_LOOKUP: usize = 650;

/// A cached presence with the time it was fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedPresence {
    /// The presence (its `user_id` is always set).
    pub presence: Presence,
    /// When it was fetched from Graph.
    pub fetched_at: Timestamp,
}

/// The storage key of a person: the user id, else `mail:` and the lower-cased address.
fn person_key(p: &Person) -> Option<String> {
    if let Some(id) = &p.id {
        return Some(id.as_str().to_owned());
    }
    p.email.as_ref().map(|e| format!("mail:{}", e.as_str().to_lowercase()))
}

impl Tx<'_> {
    /// Inserts or replaces people (keyed by user id, else email). Returns how many were skipped
    /// because they have neither.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_people(&mut self, people: &[Person]) -> Result<u32> {
        let mut skipped = 0u32;
        let mut any = false;
        for p in people {
            let Some(key) = person_key(p) else {
                skipped = skipped.saturating_add(1);
                continue;
            };
            let name = truncate(p.display_name.as_str(), 512);
            let email = p.email.as_ref().map(|e| e.as_str());
            let rid: i64 = self
                .tx
                .prepare_cached(
                    "INSERT INTO person (key, user_id, name_lc, email_lc, json) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (key) DO UPDATE SET user_id = excluded.user_id, name_lc = excluded.name_lc,
                        email_lc = excluded.email_lc, json = excluded.json
                     RETURNING rid",
                )?
                .query_row(
                    params![key, p.id.as_ref().map(UserId::as_str), fold(name), email.map(fold), to_json(p)?],
                    |r| r.get(0),
                )?;
            self.tx.prepare_cached("DELETE FROM person_fts WHERE rowid = ?1")?.execute([rid])?;
            self.tx
                .prepare_cached("INSERT INTO person_fts (rowid, name, email) VALUES (?1, ?2, ?3)")?
                .execute(params![rid, name, email.unwrap_or("")])?;
            any = true;
        }
        if any {
            self.touch(tables::PERSON);
        }
        Ok(skipped)
    }

    /// Stores presences fetched at `fetched_at` (entries without a user id are skipped).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_presences(&mut self, presences: &[Presence], fetched_at: Timestamp) -> Result<()> {
        let mut any = false;
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO presence (user_id, availability, fetched_at, json) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (user_id) DO UPDATE SET availability = excluded.availability,
                    fetched_at = excluded.fetched_at, json = excluded.json",
            )?;
            for p in presences {
                let Some(user) = &p.user_id else { continue };
                stmt.execute(params![user.as_str(), p.availability.as_graph(), ms(fetched_at), to_json(p)?])?;
                any = true;
            }
        }
        if any {
            self.touch(tables::PRESENCE);
        }
        Ok(())
    }
}

impl Reader<'_> {
    /// People whose display name or email address starts with `prefix` (case-insensitive),
    /// ordered by name. An empty prefix returns nothing.
    ///
    /// For word-prefix matches inside names (surnames), use [`Reader::search`] with
    /// [`SearchScope::People`](crate::SearchScope::People).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn people_by_prefix(&self, prefix: &str, limit: u32) -> Result<Page<Person>> {
        let p = fold(prefix.trim());
        if p.is_empty() {
            return Ok(Page::new());
        }
        let upper = prefix_upper(&p);
        let mut stmt = self.conn.prepare_cached(
            "SELECT json FROM (
                SELECT rid, name_lc, json FROM person WHERE name_lc >= ?1 AND name_lc < ?2
                UNION
                SELECT rid, name_lc, json FROM person WHERE email_lc >= ?1 AND email_lc < ?2
             ) ORDER BY name_lc, rid LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![p, upper, clamp_limit(limit)], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::PERSON, rows, |j| from_json(&j)))
    }

    /// A person by user id.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) if the stored row cannot be decoded.
    pub fn person(&self, id: &UserId) -> Result<Option<Person>> {
        let json: Option<String> = self
            .conn
            .prepare_cached("SELECT json FROM person WHERE key = ?1")?
            .query_row([id.as_str()], |r| r.get(0))
            .optional()?;
        json.map(|j| from_json(&j)).transpose()
    }

    /// Cached presences of `users` (unknown users are omitted). At most
    /// [`MAX_PRESENCE_LOOKUP`] ids are looked up.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn presences(&self, users: &[UserId]) -> Result<Page<CachedPresence>> {
        let mut stmt = self.conn.prepare_cached("SELECT fetched_at, json FROM presence WHERE user_id = ?1")?;
        let mut rows = Vec::new();
        for u in users.iter().take(MAX_PRESENCE_LOOKUP) {
            if let Some(row) =
                stmt.query_row([u.as_str()], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).optional()?
            {
                rows.push(row);
            }
        }
        Ok(decode_all(&self.ctx.stats, tables::PRESENCE, rows, |(f, j)| {
            Ok(CachedPresence { presence: from_json(&j)?, fetched_at: ts(f) })
        }))
    }

    /// User ids whose cached presence was fetched before `before` (to refresh), oldest first.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn stale_presences(&self, before: Timestamp, limit: u32) -> Result<Vec<UserId>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT user_id FROM presence WHERE fetched_at < ?1 ORDER BY fetched_at LIMIT ?2")?;
        let rows = stmt
            .query_map(params![ms(before), clamp_limit(limit)], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::PRESENCE, rows, |s| Ok(UserId::new(s)?)).items)
    }
}

impl Store {
    /// See [`Reader::people_by_prefix`].
    ///
    /// # Errors
    ///
    /// As [`Reader::people_by_prefix`].
    pub async fn people_by_prefix(&self, prefix: String, limit: u32) -> Result<Page<Person>> {
        self.read(move |r| r.people_by_prefix(&prefix, limit)).await
    }

    /// See [`Reader::presences`].
    ///
    /// # Errors
    ///
    /// As [`Reader::presences`].
    pub async fn presences(&self, users: Vec<UserId>) -> Result<Page<CachedPresence>> {
        self.read(move |r| r.presences(&users)).await
    }

    /// See [`Tx::upsert_presences`].
    ///
    /// # Errors
    ///
    /// As [`Tx::upsert_presences`].
    pub async fn upsert_presences(&self, presences: Vec<Presence>, fetched_at: Timestamp) -> Result<()> {
        self.write(move |tx| tx.upsert_presences(&presences, fetched_at)).await
    }

    /// See [`Tx::upsert_people`].
    ///
    /// # Errors
    ///
    /// As [`Tx::upsert_people`].
    pub async fn upsert_people(&self, people: Vec<Person>) -> Result<u32> {
        self.write(move |tx| tx.upsert_people(&people)).await
    }
}
