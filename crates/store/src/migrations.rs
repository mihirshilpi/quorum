//! Embedded, forward-only schema migrations versioned by `PRAGMA user_version`.
//!
//! Each entry of [`MIGRATIONS`] moves the schema from version `i` to `i + 1` and runs inside its
//! own transaction together with the `user_version` bump, so a failed migration leaves the
//! database at the previous version. Never edit a released migration; append a new one.
//!
//! Conventions: instants are `INTEGER` milliseconds since the Unix epoch (UTC); model types are
//! stored as JSON in a `json` column next to the indexed scalar columns queries need; encrypted
//! values are `BLOB`s produced by the store cipher. FTS5 tables are contentless
//! (`content=''`, `contentless_delete=1`): they hold the search index but not a second plaintext
//! copy of bodies, and are kept in sync by explicit writes keyed by the owning row's `rid` (an `INTEGER PRIMARY KEY`, so it is
//! stable across `VACUUM`).

use rusqlite::Connection;

use crate::error::{Error, ErrorKind, Result};

/// The schema version this build of the store migrates to.
pub const SCHEMA_VERSION: u32 = 6;

const MIGRATIONS: [&str; SCHEMA_VERSION as usize] = [
    // 1: core key/value, sync state, outbox, drafts, blob cache.
    r"
    CREATE TABLE setting (
        key TEXT PRIMARY KEY NOT NULL,
        value TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE ui_prefs (
        key TEXT PRIMARY KEY NOT NULL,
        value TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE delta_state (
        resource TEXT PRIMARY KEY NOT NULL,
        delta_link TEXT,
        next_link TEXT,
        backfill_cursor TEXT,
        backfill_complete INTEGER NOT NULL DEFAULT 0,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE outbox (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        kind TEXT NOT NULL,
        resource TEXT NOT NULL,
        local_id TEXT,
        payload BLOB NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('pending', 'claimed', 'failed')),
        attempts INTEGER NOT NULL DEFAULT 0,
        not_before INTEGER NOT NULL DEFAULT 0,
        last_error TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX outbox_resource ON outbox (resource, id);
    CREATE INDEX outbox_state ON outbox (state, not_before, id);
    CREATE INDEX outbox_local ON outbox (local_id);
    CREATE TABLE draft (
        id TEXT PRIMARY KEY NOT NULL,
        kind TEXT NOT NULL,
        context TEXT,
        payload BLOB NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX draft_kind ON draft (kind, updated_at DESC);
    CREATE TABLE blob (
        key TEXT PRIMARY KEY NOT NULL,
        file TEXT,
        data BLOB,
        size INTEGER NOT NULL,
        last_access INTEGER NOT NULL
    );
    CREATE INDEX blob_lru ON blob (last_access, key);
    ",
    // 2: mail.
    r"
    CREATE TABLE mail_folder (
        id TEXT PRIMARY KEY NOT NULL,
        parent_id TEXT,
        well_known TEXT,
        json TEXT NOT NULL
    );
    CREATE INDEX mail_folder_parent ON mail_folder (parent_id);
    CREATE TABLE message (
        rid INTEGER PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        folder_id TEXT,
        conversation_id TEXT,
        received_at INTEGER NOT NULL,
        is_read INTEGER NOT NULL,
        flag TEXT NOT NULL,
        is_focused INTEGER NOT NULL,
        has_attachments INTEGER NOT NULL,
        categories TEXT NOT NULL,
        fts_sig INTEGER NOT NULL DEFAULT 0,
        json TEXT NOT NULL
    );
    CREATE INDEX message_folder_received ON message (folder_id, received_at DESC, id DESC);
    CREATE INDEX message_folder_focused ON message (folder_id, is_focused, received_at DESC, id DESC);
    CREATE INDEX message_folder_unread ON message (folder_id, is_read, received_at DESC, id DESC);
    CREATE INDEX message_conversation ON message (conversation_id, received_at);
    CREATE INDEX message_received ON message (received_at);
    CREATE TABLE message_body (
        message_id TEXT PRIMARY KEY NOT NULL REFERENCES message (id) ON DELETE CASCADE,
        content_type TEXT NOT NULL,
        body BLOB NOT NULL,
        fetched_at INTEGER NOT NULL
    );
    CREATE TABLE attachment (
        message_id TEXT NOT NULL REFERENCES message (id) ON DELETE CASCADE,
        id TEXT NOT NULL,
        size INTEGER NOT NULL,
        is_inline INTEGER NOT NULL,
        blob_key TEXT,
        json TEXT NOT NULL,
        PRIMARY KEY (message_id, id)
    );
    ",
    // 3: calendar.
    r"
    CREATE TABLE calendar (
        id TEXT PRIMARY KEY NOT NULL,
        source TEXT NOT NULL,
        json TEXT NOT NULL
    );
    CREATE TABLE event (
        id TEXT PRIMARY KEY NOT NULL,
        calendar_id TEXT NOT NULL,
        start_utc INTEGER NOT NULL,
        end_utc INTEGER NOT NULL,
        is_all_day INTEGER NOT NULL,
        event_type TEXT NOT NULL,
        series_master_id TEXT,
        json TEXT NOT NULL
    );
    CREATE INDEX event_calendar_start ON event (calendar_id, start_utc, end_utc);
    CREATE INDEX event_series ON event (series_master_id);
    ",
    // 4: Teams chats and channels.
    r"
    CREATE TABLE chat (
        id TEXT PRIMARY KEY NOT NULL,
        chat_type TEXT NOT NULL,
        last_activity INTEGER NOT NULL,
        has_unread INTEGER NOT NULL,
        is_hidden INTEGER NOT NULL,
        json TEXT NOT NULL
    );
    CREATE INDEX chat_activity ON chat (last_activity DESC, id DESC);
    CREATE TABLE chat_member (
        chat_id TEXT NOT NULL REFERENCES chat (id) ON DELETE CASCADE,
        ordinal INTEGER NOT NULL,
        user_id TEXT,
        json TEXT NOT NULL,
        PRIMARY KEY (chat_id, ordinal)
    );
    CREATE INDEX chat_member_user ON chat_member (user_id);
    CREATE TABLE chat_message (
        rid INTEGER PRIMARY KEY,
        chat_id TEXT NOT NULL,
        id TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        is_deleted INTEGER NOT NULL,
        meta TEXT NOT NULL,
        body BLOB,
        body_pruned INTEGER NOT NULL DEFAULT 0,
        UNIQUE (chat_id, id)
    );
    CREATE INDEX chat_message_created ON chat_message (chat_id, created_at DESC, id DESC);
    CREATE INDEX chat_message_age ON chat_message (created_at);
    CREATE TABLE team (
        id TEXT PRIMARY KEY NOT NULL,
        json TEXT NOT NULL
    );
    CREATE TABLE channel (
        id TEXT PRIMARY KEY NOT NULL,
        team_id TEXT NOT NULL,
        json TEXT NOT NULL
    );
    CREATE INDEX channel_team ON channel (team_id);
    CREATE TABLE channel_message (
        rid INTEGER PRIMARY KEY,
        channel_id TEXT NOT NULL,
        id TEXT NOT NULL,
        team_id TEXT NOT NULL,
        reply_to_id TEXT,
        created_at INTEGER NOT NULL,
        is_deleted INTEGER NOT NULL,
        meta TEXT NOT NULL,
        body BLOB,
        body_pruned INTEGER NOT NULL DEFAULT 0,
        UNIQUE (channel_id, id)
    );
    CREATE INDEX channel_message_thread ON channel_message (channel_id, reply_to_id, created_at DESC, id DESC);
    CREATE INDEX channel_message_age ON channel_message (created_at);
    ",
    // 5: files, sites, lists, people, presence.
    r"
    CREATE TABLE drive (
        id TEXT PRIMARY KEY NOT NULL,
        kind TEXT NOT NULL,
        json TEXT NOT NULL
    );
    CREATE TABLE drive_item (
        rid INTEGER PRIMARY KEY,
        drive_id TEXT NOT NULL,
        id TEXT NOT NULL,
        parent_id TEXT,
        is_folder INTEGER NOT NULL,
        name_sort TEXT NOT NULL,
        modified_at INTEGER NOT NULL,
        json TEXT NOT NULL,
        UNIQUE (drive_id, id)
    );
    CREATE INDEX drive_item_parent ON drive_item (drive_id, parent_id, is_folder DESC, name_sort, id);
    CREATE INDEX drive_item_recent ON drive_item (is_folder, modified_at DESC);
    CREATE TABLE site (
        id TEXT PRIMARY KEY NOT NULL,
        json TEXT NOT NULL
    );
    CREATE TABLE list (
        id TEXT PRIMARY KEY NOT NULL,
        site_id TEXT,
        json TEXT NOT NULL
    );
    CREATE INDEX list_site ON list (site_id);
    CREATE TABLE list_item (
        list_id TEXT NOT NULL,
        id TEXT NOT NULL,
        modified_at INTEGER NOT NULL,
        json TEXT NOT NULL,
        PRIMARY KEY (list_id, id)
    );
    CREATE INDEX list_item_modified ON list_item (list_id, modified_at DESC);
    CREATE TABLE person (
        rid INTEGER PRIMARY KEY,
        key TEXT NOT NULL UNIQUE,
        user_id TEXT,
        name_lc TEXT NOT NULL,
        email_lc TEXT,
        json TEXT NOT NULL
    );
    CREATE INDEX person_name ON person (name_lc);
    CREATE INDEX person_email ON person (email_lc);
    CREATE INDEX person_user ON person (user_id);
    CREATE TABLE presence (
        user_id TEXT PRIMARY KEY NOT NULL,
        availability TEXT NOT NULL,
        fetched_at INTEGER NOT NULL,
        json TEXT NOT NULL
    );
    CREATE INDEX presence_fetched ON presence (fetched_at);
    ",
    // 6: full-text search (contentless; rowid = owning row's `rid`).
    r"
    CREATE VIRTUAL TABLE mail_fts USING fts5 (
        subject, sender, preview, body,
        content = '', contentless_delete = 1, tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE VIRTUAL TABLE chat_fts USING fts5 (
        body, content = '', contentless_delete = 1, tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE VIRTUAL TABLE channel_fts USING fts5 (
        subject, body, content = '', contentless_delete = 1, tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE VIRTUAL TABLE drive_fts USING fts5 (
        name, content = '', contentless_delete = 1, tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE VIRTUAL TABLE person_fts USING fts5 (
        name, email, content = '', contentless_delete = 1, tokenize = 'unicode61 remove_diacritics 2'
    );
    ",
];

/// Reads `PRAGMA user_version`.
pub(crate) fn current_version(conn: &Connection) -> Result<u32> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    u32::try_from(v).map_err(|_| Error::new(ErrorKind::Migration, "negative schema version"))
}

/// Migrates `conn` up to `target` (at most [`SCHEMA_VERSION`]).
///
/// # Errors
///
/// [`ErrorKind::Migration`] if the database is newer than this build or a migration fails.
pub(crate) fn migrate_to(conn: &mut Connection, target: u32) -> Result<()> {
    let target = target.min(SCHEMA_VERSION);
    let mut version = current_version(conn)?;
    if version > SCHEMA_VERSION {
        return Err(Error::new(
            ErrorKind::Migration,
            format!("database schema {version} is newer than this build ({SCHEMA_VERSION})"),
        ));
    }
    while version < target {
        let Some(sql) = MIGRATIONS.get(version as usize) else {
            return Err(Error::bug("missing migration"));
        };
        let next = version.saturating_add(1);
        let tx = conn.transaction()?;
        tx.execute_batch(sql).map_err(|e| {
            Error::new(ErrorKind::Migration, format!("migration to schema {next} failed")).with_source(e)
        })?;
        tx.pragma_update(None, "user_version", i64::from(next))?;
        tx.commit()?;
        tracing::info!(name: "store.migrated", version = next, "store schema migrated");
        version = next;
    }
    Ok(())
}
