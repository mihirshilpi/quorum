//! Sync state (delta links and backfill progress), settings and UI preferences.

use std::fmt;

use jiff::Timestamp;
use rusqlite::{OptionalExtension, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::{Reader, Store, Tx, tables};
use crate::util::{from_json, ms, to_json, ts};

const MAX_KEY_LEN: usize = 512;
const MAX_LINK_LEN: usize = 16 * 1024;
const MAX_VALUE_LEN: usize = 1024 * 1024;

/// The sync position of one resource (e.g. `mail:inbox`, `drive:<id>`, `calendar:<id>`).
///
/// Implements "download once, then only changes": `delta_link` is the stored
/// `@odata.deltaLink`; `next_link` is the `@odata.nextLink` of an interrupted delta round;
/// `backfill_cursor` is how far the background backfill of older items got (newest to oldest).
/// Links carry sync tokens, so `Debug` redacts them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeltaState {
    /// The resource key.
    pub resource: String,
    /// The delta link to poll next, once a full round finished.
    pub delta_link: Option<String>,
    /// The next page link of an unfinished round, to resume after a restart.
    pub next_link: Option<String>,
    /// The backfill position (opaque to the store, e.g. a timestamp or skip token).
    pub backfill_cursor: Option<String>,
    /// Whether the backfill reached the oldest item.
    pub backfill_complete: bool,
    /// When this state was last written (set by the store).
    pub updated_at: Timestamp,
}

impl fmt::Debug for DeltaState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeltaState")
            .field("resource", &self.resource)
            .field("has_delta_link", &self.delta_link.is_some())
            .field("has_next_link", &self.next_link.is_some())
            .field("has_backfill_cursor", &self.backfill_cursor.is_some())
            .field("backfill_complete", &self.backfill_complete)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl DeltaState {
    /// A fresh state for `resource` (nothing synced yet).
    #[must_use]
    pub fn new(resource: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            delta_link: None,
            next_link: None,
            backfill_cursor: None,
            backfill_complete: false,
            updated_at: Timestamp::UNIX_EPOCH,
        }
    }
}

fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_LEN || key.chars().any(char::is_control) {
        return Err(Error::invalid("invalid key"));
    }
    Ok(())
}

fn check_link(link: Option<&String>) -> Result<()> {
    if link.is_some_and(|l| l.len() > MAX_LINK_LEN) {
        return Err(Error::invalid("link too long"));
    }
    Ok(())
}

impl Tx<'_> {
    /// Stores the sync state of a resource (sets `updated_at` to now).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for an invalid key or a link
    /// longer than 16 KiB.
    pub fn set_delta_state(&mut self, state: &DeltaState) -> Result<()> {
        check_key(&state.resource)?;
        check_link(state.delta_link.as_ref())?;
        check_link(state.next_link.as_ref())?;
        check_link(state.backfill_cursor.as_ref())?;
        self.tx
            .prepare_cached(
                "INSERT INTO delta_state (resource, delta_link, next_link, backfill_cursor, backfill_complete, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (resource) DO UPDATE SET delta_link = excluded.delta_link,
                    next_link = excluded.next_link, backfill_cursor = excluded.backfill_cursor,
                    backfill_complete = excluded.backfill_complete, updated_at = excluded.updated_at",
            )?
            .execute(params![
                state.resource,
                state.delta_link,
                state.next_link,
                state.backfill_cursor,
                state.backfill_complete,
                ms(Timestamp::now())
            ])?;
        self.touch(tables::DELTA_STATE);
        Ok(())
    }

    /// Forgets the sync state of a resource (after a 410 / `syncStateNotFound`, before a
    /// resync that merges by id).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the write fails.
    pub fn clear_delta_state(&mut self, resource: &str) -> Result<()> {
        self.tx.execute("DELETE FROM delta_state WHERE resource = ?1", [resource])?;
        self.touch(tables::DELTA_STATE);
        Ok(())
    }

    /// Stores a setting as JSON.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for an invalid key or a value
    /// over 1 MiB.
    pub fn set_setting<T: Serialize>(&mut self, key: &str, value: &T) -> Result<()> {
        self.put_kv("setting", tables::SETTING, key, value)
    }

    /// Stores a UI preference as JSON.
    ///
    /// # Errors
    ///
    /// As [`Tx::set_setting`].
    pub fn set_ui_pref<T: Serialize>(&mut self, key: &str, value: &T) -> Result<()> {
        self.put_kv("ui_prefs", tables::UI_PREFS, key, value)
    }

    /// Removes a setting.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the write fails.
    pub fn delete_setting(&mut self, key: &str) -> Result<()> {
        self.tx.execute("DELETE FROM setting WHERE key = ?1", [key])?;
        self.touch(tables::SETTING);
        Ok(())
    }

    fn put_kv<T: Serialize>(&mut self, table: &str, name: &'static str, key: &str, value: &T) -> Result<()> {
        check_key(key)?;
        if key.starts_with("store.") {
            return Err(Error::invalid("the store. key prefix is reserved"));
        }
        let json = to_json(value)?;
        if json.len() > MAX_VALUE_LEN {
            return Err(Error::invalid("value too large"));
        }
        self.tx.execute(
            &format!(
                "INSERT INTO {table} (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at"
            ),
            params![key, json, ms(Timestamp::now())],
        )?;
        self.touch(name);
        Ok(())
    }
}

impl Reader<'_> {
    /// The sync state of a resource, if any.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn delta_state(&self, resource: &str) -> Result<Option<DeltaState>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT resource, delta_link, next_link, backfill_cursor, backfill_complete, updated_at
                 FROM delta_state WHERE resource = ?1",
            )?
            .query_row([resource], |r| {
                Ok(DeltaState {
                    resource: r.get(0)?,
                    delta_link: r.get(1)?,
                    next_link: r.get(2)?,
                    backfill_cursor: r.get(3)?,
                    backfill_complete: r.get(4)?,
                    updated_at: ts(r.get(5)?),
                })
            })
            .optional()?)
    }

    /// A setting decoded from JSON, if set.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) if the stored value does not decode as `T`.
    pub fn setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.get_kv("setting", key)
    }

    /// A UI preference decoded from JSON, if set.
    ///
    /// # Errors
    ///
    /// As [`Reader::setting`].
    pub fn ui_pref<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.get_kv("ui_prefs", key)
    }

    fn get_kv<T: DeserializeOwned>(&self, table: &str, key: &str) -> Result<Option<T>> {
        let v: Option<String> = self
            .conn
            .prepare_cached(&format!("SELECT value FROM {table} WHERE key = ?1"))?
            .query_row([key], |r| r.get(0))
            .optional()?;
        v.map(|j| from_json(&j)).transpose()
    }
}

impl Store {
    /// See [`Reader::delta_state`].
    ///
    /// # Errors
    ///
    /// As [`Reader::delta_state`].
    pub async fn delta_state(&self, resource: String) -> Result<Option<DeltaState>> {
        self.read(move |r| r.delta_state(&resource)).await
    }

    /// See [`Tx::set_delta_state`]. To store state together with the page it belongs to, call
    /// the `Tx` method inside [`Store::write`].
    ///
    /// # Errors
    ///
    /// As [`Tx::set_delta_state`].
    pub async fn set_delta_state(&self, state: DeltaState) -> Result<()> {
        self.write(move |tx| tx.set_delta_state(&state)).await
    }

    /// See [`Reader::setting`].
    ///
    /// # Errors
    ///
    /// As [`Reader::setting`].
    pub async fn setting<T: DeserializeOwned + Send + 'static>(&self, key: String) -> Result<Option<T>> {
        self.read(move |r| r.setting(&key)).await
    }

    /// See [`Tx::set_setting`].
    ///
    /// # Errors
    ///
    /// As [`Tx::set_setting`].
    pub async fn set_setting<T: Serialize + Send + 'static>(&self, key: String, value: T) -> Result<()> {
        self.write(move |tx| tx.set_setting(&key, &value)).await
    }

    /// See [`Reader::ui_pref`].
    ///
    /// # Errors
    ///
    /// As [`Reader::ui_pref`].
    pub async fn ui_pref<T: DeserializeOwned + Send + 'static>(&self, key: String) -> Result<Option<T>> {
        self.read(move |r| r.ui_pref(&key)).await
    }

    /// See [`Tx::set_ui_pref`].
    ///
    /// # Errors
    ///
    /// As [`Tx::set_ui_pref`].
    pub async fn set_ui_pref<T: Serialize + Send + 'static>(&self, key: String, value: T) -> Result<()> {
        self.write(move |tx| tx.set_ui_pref(&key, &value)).await
    }
}
