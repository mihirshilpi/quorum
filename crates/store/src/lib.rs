//! Local-first SQLite cache: migrations, delta state, outbox, drafts and full-text search.
//!
//! The UI reads only from this store; `quorum-sync` writes Graph data into it and replays the
//! outbox (architecture §1, §6, §7). One [`Store`] per account, opened with
//! [`Store::open`]`(dir, key_source)`.
//!
//! # Shape of the API
//!
//! * [`Store`] is a cheap `Clone` handle. Every method is `async` and runs SQLite on tokio's
//!   blocking pool. Typed convenience methods cover the common UI queries and writes.
//! * [`Store::read`] gives a [`Reader`] for any combination of queries on one snapshot;
//!   [`Store::write`] gives a [`Tx`] for any combination of writes in **one transaction**:
//!   a delta page (upserts, deletes and the new [`DeltaState`]) applies atomically, and an
//!   outbox entry is enqueued together with its optimistic local change.
//! * [`Store::subscribe`] yields one [`Changed`] per committed transaction, listing the tables
//!   it touched ([`tables`]).
//!
//! # Areas
//!
//! | Area | Writes ([`Tx`]) | Reads ([`Reader`]) |
//! |---|---|---|
//! | Mail | `upsert_mail_folders`, `upsert_messages`, `delete_messages`, `put_message_body`, `put_attachments`, `set_message_read`/`flag`, `move_message` | `list_messages` ([`MessageQuery`], keyset), `conversation`, `unread_counts`, `message_body`, `attachments` |
//! | Calendar | `upsert_calendars`, `upsert_events`, `delete_events`, `clear_event_range` | `calendars`, `events_in_range` (UTC, across calendars) |
//! | Teams | `upsert_chats`, `upsert_chat_messages` (chat and channel), `delete_chat_messages`, `upsert_teams`, `upsert_channels`, `mark_chat_read` | `chats` (by activity), `chat_messages` (keyset), `channel_posts` (with reply counts), `channel_replies`, `teams`, `channels` |
//! | Files | `upsert_drives`, `upsert_drive_items`, `delete_drive_items` (subtree), `upsert_sites`, `upsert_lists`, `upsert_list_items` | `drive_children`, `recent_drive_items`, `sites`, `lists`, `list_items` |
//! | People | `upsert_people`, `upsert_presences` | `people_by_prefix`, `person`, `presences`, `stale_presences` |
//! | Search | (kept in sync by the writes above) | [`Reader::search`] with [`SearchScope`]s |
//! | Sync | `set_delta_state`, `clear_delta_state` | `delta_state` |
//! | Outbox | `outbox_enqueue`, `outbox_claim_next`, `outbox_complete`, `outbox_fail`, `outbox_retry`, `outbox_discard` | `outbox_failed`, `outbox_pending_count`, `outbox_next_ready_at` |
//! | Drafts | `save_draft`, `delete_draft` | `draft`, `draft_for_context`, `drafts` |
//! | Settings | `set_setting`, `set_ui_pref` | `setting`, `ui_pref` |
//! | Blobs | [`Store::put_blob`], [`Store::get_blob`], [`Store::set_blob_quota`] (LRU eviction) | [`Store::blob_usage`] |
//! | Retention | `prune_message_bodies`, `prune_chat_bodies` | |
//!
//! # Safety properties
//!
//! * **Encryption at rest** (ADR 0005): mail bodies, chat/channel message bodies, drafts,
//!   outbox payloads and blob files are sealed with XChaCha20-Poly1305 under the account key
//!   from [`KeySource`], bound to their row by associated data. The search index is
//!   contentless (no second plaintext copy), but necessarily holds the indexed words.
//! * **Never crash**: no panics; corrupt rows are skipped and counted ([`Page::skipped`],
//!   [`Store::corrupt_rows_skipped`]); page sizes are capped at [`MAX_PAGE`]; hostile search
//!   input is escaped.
//! * **No PII in logs**: `tracing` events carry counts and kinds only; types holding content
//!   have redacting `Debug`.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing
)]
#![warn(missing_docs)]

mod blobs;
mod calendar;
mod crypto;
mod drafts;
mod error;
mod files;
mod mail;
mod migrations;
mod outbox;
mod people;
mod retention;
mod search;
mod state;
mod store;
mod teams;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;
mod util;

pub use blobs::{BlobUsage, DEFAULT_BLOB_QUOTA};
pub use calendar::{MAX_RANGE_CALENDARS, MAX_RANGE_EVENTS};
#[cfg(any(test, feature = "test-util"))]
pub use crypto::FixedKeySource;
pub use crypto::{KEY_LEN, KeySource, StoreKey};
pub use drafts::{Draft, DraftKind, MAX_DRAFT_LEN};
pub use error::{Error, ErrorKind, Result};
pub use mail::{FolderCount, MAX_BODY_LEN, MessageQuery, StoredAttachment};
pub use migrations::SCHEMA_VERSION;
pub use outbox::{
    MAX_OUTBOX_ATTEMPTS, MAX_OUTBOX_PAYLOAD, NewOutboxEntry, OutboxEntry, OutboxFailure, OutboxId, OutboxState, backoff,
};
pub use people::{CachedPresence, MAX_PRESENCE_LOOKUP};
pub use retention::RETENTION_BATCH;
pub use search::{HitItem, SearchHit, SearchResults, SearchScope, Snippet};
pub use state::DeltaState;
pub use store::{BLOB_DIR, Changed, DB_FILE, Reader, Store, Tx, tables};
pub use teams::ChannelPost;
pub use util::{Cursor, MAX_PAGE, Page};
