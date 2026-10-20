//! The [`Store`] handle, its connection pool, transactions and the change feed.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use tokio::sync::broadcast;

use crate::crypto::{Cipher, KeySource, StoreKey};
use crate::error::{Error, ErrorKind, Result};
use crate::migrations::{self, SCHEMA_VERSION};
use crate::util::Stats;

/// The database file name inside the account directory.
pub const DB_FILE: &str = "quorum.db";
/// The blob cache directory name inside the account directory.
pub const BLOB_DIR: &str = "blobs";
const READERS: usize = 4;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const CHANGE_CAPACITY: usize = 256;
const KEY_CHECK_SETTING: &str = "store.key_check";
const KEY_CHECK_PLAIN: &[u8] = b"quorum-store key check v1";

/// Names of the tables reported in [`Changed`] notifications.
pub mod tables {
    /// Mail folders.
    pub const MAIL_FOLDER: &str = "mail_folder";
    /// Mail message headers.
    pub const MESSAGE: &str = "message";
    /// Mail message bodies.
    pub const MESSAGE_BODY: &str = "message_body";
    /// Attachment metadata.
    pub const ATTACHMENT: &str = "attachment";
    /// Calendars.
    pub const CALENDAR: &str = "calendar";
    /// Events.
    pub const EVENT: &str = "event";
    /// Chats.
    pub const CHAT: &str = "chat";
    /// Chat members.
    pub const CHAT_MEMBER: &str = "chat_member";
    /// Chat messages.
    pub const CHAT_MESSAGE: &str = "chat_message";
    /// Teams.
    pub const TEAM: &str = "team";
    /// Channels.
    pub const CHANNEL: &str = "channel";
    /// Channel posts and replies.
    pub const CHANNEL_MESSAGE: &str = "channel_message";
    /// Drives.
    pub const DRIVE: &str = "drive";
    /// Drive items.
    pub const DRIVE_ITEM: &str = "drive_item";
    /// SharePoint sites.
    pub const SITE: &str = "site";
    /// SharePoint lists.
    pub const LIST: &str = "list";
    /// SharePoint list items.
    pub const LIST_ITEM: &str = "list_item";
    /// People.
    pub const PERSON: &str = "person";
    /// Presence.
    pub const PRESENCE: &str = "presence";
    /// Delta / backfill state.
    pub const DELTA_STATE: &str = "delta_state";
    /// The outbox.
    pub const OUTBOX: &str = "outbox";
    /// Drafts.
    pub const DRAFT: &str = "draft";
    /// Settings.
    pub const SETTING: &str = "setting";
    /// UI preferences.
    pub const UI_PREFS: &str = "ui_prefs";
    /// The blob cache.
    pub const BLOB: &str = "blob";
}

/// A change notification: the tables one committed transaction wrote to.
///
/// Emitted once per committed write transaction (coalesced), after the commit, so a reader that
/// receives it sees the new data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// The touched table names (see [`tables`]), sorted and de-duplicated.
    pub tables: Vec<&'static str>,
}

impl Changed {
    /// Returns `true` if `table` was touched.
    #[must_use]
    pub fn touches(&self, table: &str) -> bool {
        self.tables.contains(&table)
    }
}

/// Shared per-store state used by readers and writers.
pub(crate) struct Ctx {
    pub(crate) cipher: Cipher,
    pub(crate) stats: Stats,
    pub(crate) blob_dir: Option<PathBuf>,
}

struct Inner {
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
    ctx: Ctx,
    changes: broadcast::Sender<Changed>,
    dir: Option<PathBuf>,
}

/// The local store of one account (M-SERVICES-CLONE: cheap to clone, `Arc` inside).
///
/// All methods are `async` and run SQLite work on tokio's blocking pool (M-YIELD-POINTS), so
/// they never block the runtime. There is one writer connection and a small pool of read-only
/// connections (WAL mode lets reads proceed while a write is in progress).
///
/// Compose several writes atomically with [`Store::write`]; read with [`Store::read`] or the
/// typed convenience methods.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("file_backed", &self.inner.dir.is_some())
            .field("readers", &self.inner.readers.len())
            .finish_non_exhaustive()
    }
}

fn lock(m: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    // A panic while holding the lock can only come from a bug; the connection itself is still
    // usable (an open transaction was rolled back when its guard dropped).
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn configure(conn: &Connection, read_only: bool) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.set_prepared_statement_cache_capacity(64);
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL; PRAGMA temp_store = MEMORY;")?;
    if read_only {
        conn.execute_batch("PRAGMA query_only = ON;")?;
    }
    Ok(())
}

impl Store {
    /// Opens (creating and migrating if needed) the store in `dir`, one directory per account.
    ///
    /// The key comes from `key_source`; on first open a key check value is written, and later
    /// opens verify it, so a wrong key fails here rather than on the first body read.
    /// Outbox entries that were claimed when the app last stopped are made pending again.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Io`] if the directory cannot be created, [`ErrorKind::Crypto`] if the key
    /// source fails or the key is wrong, [`ErrorKind::Migration`] if the schema is newer than
    /// this build or a migration fails, [`ErrorKind::Sqlite`] otherwise.
    pub async fn open(dir: impl AsRef<Path>, key_source: impl KeySource) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || Self::open_blocking(&dir, &key_source)).await?
    }

    fn open_blocking(dir: &Path, key_source: &dyn KeySource) -> Result<Self> {
        create_private_dir(dir)?;
        let key = key_source
            .load_or_create()
            .map_err(|e| Error::new(ErrorKind::Crypto, "key source failed").with_boxed_source(e))?;
        let db_path = dir.join(DB_FILE);
        let mut writer = Connection::open(&db_path)?;
        let mode: String = writer.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(name: "store.no_wal", "store could not switch to WAL mode");
        }
        configure(&writer, false)?;
        migrations::migrate_to(&mut writer, SCHEMA_VERSION)?;
        let blob_dir = dir.join(BLOB_DIR);
        create_private_dir(&blob_dir)?;
        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            let conn = Connection::open_with_flags(
                &db_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            configure(&conn, true)?;
            readers.push(Mutex::new(conn));
        }
        let store = Self::assemble(writer, readers, &key, Some(blob_dir), Some(dir.to_path_buf()))?;
        crate::blobs::sweep_orphans(&store.inner.ctx, &lock(&store.inner.writer))?;
        tracing::info!(name: "store.opened", "store opened");
        Ok(store)
    }

    /// Opens a fresh in-memory store with a random key (tests, `--mock-graph` runs).
    ///
    /// Reads and writes share one connection; blobs are kept in the database.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`] or [`ErrorKind::Migration`] if SQLite cannot be initialised.
    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        configure(&conn, false)?;
        migrations::migrate_to(&mut conn, SCHEMA_VERSION)?;
        Self::assemble(conn, Vec::new(), &StoreKey::generate(), None, None)
    }

    fn assemble(
        writer: Connection,
        readers: Vec<Mutex<Connection>>,
        key: &StoreKey,
        blob_dir: Option<PathBuf>,
        dir: Option<PathBuf>,
    ) -> Result<Self> {
        let cipher = Cipher::new(key);
        verify_key(&writer, &cipher)?;
        crate::outbox::reset_claimed(&writer)?;
        let (changes, _) = broadcast::channel(CHANGE_CAPACITY);
        Ok(Self {
            inner: Arc::new(Inner {
                writer: Mutex::new(writer),
                readers,
                next_reader: AtomicUsize::new(0),
                ctx: Ctx { cipher, stats: Stats::default(), blob_dir },
                changes,
                dir,
            }),
        })
    }

    /// Subscribes to change notifications (one [`Changed`] per committed write transaction).
    ///
    /// A slow receiver may observe `RecvError::Lagged`; it should then refresh everything.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Changed> {
        self.inner.changes.subscribe()
    }

    /// The number of stored rows skipped so far because they could not be decoded.
    #[must_use]
    pub fn corrupt_rows_skipped(&self) -> u64 {
        self.inner.ctx.stats.corrupt_rows.load(Ordering::Relaxed)
    }

    /// The account directory, for file-backed stores.
    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.inner.dir.as_deref()
    }

    /// The schema version of the open database (equals [`SCHEMA_VERSION`] after open).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`] if the pragma cannot be read.
    pub async fn schema_version(&self) -> Result<u32> {
        self.read(|r| migrations::current_version(r.conn)).await
    }

    /// Runs `f` with a read-only view on a blocking thread.
    ///
    /// # Errors
    ///
    /// Whatever `f` returns, or [`ErrorKind::Bug`] if the blocking task failed.
    pub async fn read<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&Reader<'_>) -> Result<R> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || inner.with_reader(f)).await?
    }

    /// Runs `f` inside one immediate write transaction on a blocking thread.
    ///
    /// If `f` returns `Ok`, the transaction commits and one [`Changed`] listing every touched
    /// table is broadcast; if it returns `Err`, everything `f` wrote is rolled back. Use this to
    /// apply a delta page atomically, or to enqueue an outbox entry together with its
    /// optimistic local change.
    ///
    /// # Errors
    ///
    /// Whatever `f` returns, [`ErrorKind::Sqlite`] if the commit fails, or [`ErrorKind::Bug`]
    /// if the blocking task failed.
    pub async fn write<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Tx<'_>) -> Result<R> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || inner.with_writer(f)).await?
    }

    /// Runs blocking work that needs the store context but no connection (blob file I/O).
    pub(crate) async fn blocking<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&Store) -> Result<R> + Send + 'static,
    {
        let me = self.clone();
        tokio::task::spawn_blocking(move || f(&me)).await?
    }

    pub(crate) fn ctx(&self) -> &Ctx {
        &self.inner.ctx
    }

    pub(crate) fn write_blocking<R>(&self, f: impl FnOnce(&mut Tx<'_>) -> Result<R>) -> Result<R> {
        self.inner.with_writer(f)
    }

    pub(crate) fn read_blocking<R>(&self, f: impl FnOnce(&Reader<'_>) -> Result<R>) -> Result<R> {
        self.inner.with_reader(f)
    }
}

impl Inner {
    fn with_reader<R>(&self, f: impl FnOnce(&Reader<'_>) -> Result<R>) -> Result<R> {
        let n = self.readers.len();
        if n == 0 {
            let conn = lock(&self.writer);
            return f(&Reader { conn: &conn, ctx: &self.ctx });
        }
        let start = self.next_reader.fetch_add(1, Ordering::Relaxed) % n;
        for i in 0..n {
            let idx = start.wrapping_add(i) % n;
            if let Some(Ok(conn)) = self.readers.get(idx).map(Mutex::try_lock) {
                return f(&Reader { conn: &conn, ctx: &self.ctx });
            }
        }
        let Some(m) = self.readers.get(start) else {
            return Err(Error::bug("reader index out of range"));
        };
        let conn = lock(m);
        f(&Reader { conn: &conn, ctx: &self.ctx })
    }

    fn with_writer<R>(&self, f: impl FnOnce(&mut Tx<'_>) -> Result<R>) -> Result<R> {
        let mut conn = lock(&self.writer);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut t = Tx { tx, ctx: &self.ctx, touched: BTreeSet::new(), removals: Vec::new() };
        let out = f(&mut t)?;
        let Tx { tx, touched, removals, .. } = t;
        tx.commit()?;
        for path in removals {
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(name: "store.blob_remove_failed", kind = ?e.kind(), "could not remove a cached blob file");
            }
        }
        if !touched.is_empty() {
            // No receivers is fine.
            let _ = self.changes.send(Changed { tables: touched.into_iter().collect() });
        }
        Ok(out)
    }
}

/// A read-only view of the store (inside [`Store::read`] or a [`Tx`]).
///
/// Typed query methods live in the per-area modules (mail, calendar, Teams, files, people,
/// search, sync state, outbox, drafts).
pub struct Reader<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) ctx: &'a Ctx,
}

impl fmt::Debug for Reader<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reader")
    }
}

/// One write transaction (inside [`Store::write`]).
///
/// Typed write methods live in the per-area modules. Every write records the tables it
/// touched; the change feed reports them once after commit.
pub struct Tx<'a> {
    pub(crate) tx: rusqlite::Transaction<'a>,
    pub(crate) ctx: &'a Ctx,
    touched: BTreeSet<&'static str>,
    removals: Vec<PathBuf>,
}

impl fmt::Debug for Tx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tx").field("touched", &self.touched).finish_non_exhaustive()
    }
}

impl Tx<'_> {
    /// A read view that sees this transaction's uncommitted writes.
    #[must_use]
    pub fn read(&self) -> Reader<'_> {
        Reader { conn: &self.tx, ctx: self.ctx }
    }

    /// Records that `table` was written (reported in the [`Changed`] notification).
    pub(crate) fn touch(&mut self, table: &'static str) {
        self.touched.insert(table);
    }

    /// Schedules a blob file for removal after a successful commit.
    pub(crate) fn remove_after_commit(&mut self, path: PathBuf) {
        self.removals.push(path);
    }
}

fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn verify_key(conn: &Connection, cipher: &Cipher) -> Result<()> {
    let stored: Option<Vec<u8>> = conn
        .query_row("SELECT value FROM setting WHERE key = ?1", [KEY_CHECK_SETTING], |r| r.get::<_, String>(0))
        .optional()?
        .map(|hex| decode_hex(&hex))
        .transpose()?;
    match stored {
        Some(sealed) => {
            let plain = cipher
                .open(KEY_CHECK_SETTING.as_bytes(), &sealed)
                .map_err(|_| Error::new(ErrorKind::Crypto, "store key does not match this database"))?;
            if plain != KEY_CHECK_PLAIN {
                return Err(Error::new(ErrorKind::Crypto, "store key does not match this database"));
            }
            Ok(())
        }
        None => {
            let sealed = cipher.seal(KEY_CHECK_SETTING.as_bytes(), KEY_CHECK_PLAIN)?;
            conn.execute(
                "INSERT INTO setting (key, value, updated_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![KEY_CHECK_SETTING, encode_hex(&sealed), crate::util::ms(jiff::Timestamp::now())],
            )?;
            Ok(())
        }
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len().saturating_mul(2));
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(Error::corrupt("bad hex"));
    }
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let txt = std::str::from_utf8(pair).map_err(|_| Error::corrupt("bad hex"))?;
            u8::from_str_radix(txt, 16).map_err(|_| Error::corrupt("bad hex"))
        })
        .collect()
}

/// Random lower-case hex, for blob file names.
pub(crate) fn random_hex(len_bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; len_bytes];
    rand::rng().fill_bytes(&mut buf);
    encode_hex(&buf)
}
