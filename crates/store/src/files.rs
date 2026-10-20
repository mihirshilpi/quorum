//! OneDrive / SharePoint: drives, drive items, sites, lists and list items.

use quorum_model::{
    Drive, DriveId, DriveItem, DriveItemId, GraphEnum, List, ListId, ListItem, ListItemId, Site, SiteId,
};
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, params};

use crate::error::Result;
use crate::store::{Reader, Store, Tx, tables};
use crate::util::{Cursor, Page, clamp_limit, decode_all, fold, from_json, keyset_page, ms_or_zero, to_json, truncate};

impl Tx<'_> {
    /// Inserts or replaces drives by id.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_drives(&mut self, drives: &[Drive]) -> Result<()> {
        if drives.is_empty() {
            return Ok(());
        }
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO drive (id, kind, json) VALUES (?1, ?2, ?3)
                 ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, json = excluded.json",
            )?;
            for d in drives {
                stmt.execute(params![d.id.as_str(), d.drive_type.as_graph(), to_json(d)?])?;
            }
        }
        self.touch(tables::DRIVE);
        Ok(())
    }

    /// Deletes drives with all their cached items.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_drives(&mut self, ids: &[DriveId]) -> Result<()> {
        for id in ids {
            self.tx.execute(
                "DELETE FROM drive_fts WHERE rowid IN (SELECT rid FROM drive_item WHERE drive_id = ?1)",
                [id.as_str()],
            )?;
            self.tx.execute("DELETE FROM drive_item WHERE drive_id = ?1", [id.as_str()])?;
            self.tx.execute("DELETE FROM drive WHERE id = ?1", [id.as_str()])?;
        }
        if !ids.is_empty() {
            self.touch(tables::DRIVE);
            self.touch(tables::DRIVE_ITEM);
        }
        Ok(())
    }

    /// Inserts or replaces drive items by (drive, id) and indexes their names for search.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_drive_items(&mut self, items: &[DriveItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        for it in items {
            let name = truncate(it.name.as_str(), 1024);
            let rid: i64 = self
                .tx
                .prepare_cached(
                    "INSERT INTO drive_item (drive_id, id, parent_id, is_folder, name_sort, modified_at, json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (drive_id, id) DO UPDATE SET parent_id = excluded.parent_id,
                        is_folder = excluded.is_folder, name_sort = excluded.name_sort,
                        modified_at = excluded.modified_at, json = excluded.json
                     RETURNING rid",
                )?
                .query_row(
                    params![
                        it.drive_id.as_str(),
                        it.id.as_str(),
                        it.parent_id.as_ref().map(DriveItemId::as_str),
                        it.is_folder(),
                        fold(name),
                        ms_or_zero(it.last_modified_date_time),
                        to_json(it)?
                    ],
                    |r| r.get(0),
                )?;
            self.tx.prepare_cached("DELETE FROM drive_fts WHERE rowid = ?1")?.execute([rid])?;
            self.tx
                .prepare_cached("INSERT INTO drive_fts (rowid, name) VALUES (?1, ?2)")?
                .execute(params![rid, name])?;
        }
        self.touch(tables::DRIVE_ITEM);
        Ok(())
    }

    /// Deletes drive items by id, including everything below deleted folders.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_drive_items(&mut self, drive: &DriveId, ids: &[DriveItemId]) -> Result<()> {
        // UNION (not UNION ALL) de-duplicates, so a corrupt parent cycle terminates.
        const SUBTREE: &str = "WITH RECURSIVE sub(id) AS (
                SELECT ?2 UNION SELECT d.id FROM drive_item d JOIN sub ON d.parent_id = sub.id WHERE d.drive_id = ?1
            )";
        for id in ids {
            self.tx.execute(
                &format!(
                    "{SUBTREE} DELETE FROM drive_fts WHERE rowid IN
                        (SELECT rid FROM drive_item WHERE drive_id = ?1 AND id IN (SELECT id FROM sub))"
                ),
                params![drive.as_str(), id.as_str()],
            )?;
            self.tx.execute(
                &format!("{SUBTREE} DELETE FROM drive_item WHERE drive_id = ?1 AND id IN (SELECT id FROM sub)"),
                params![drive.as_str(), id.as_str()],
            )?;
        }
        if !ids.is_empty() {
            self.touch(tables::DRIVE_ITEM);
        }
        Ok(())
    }

    /// Inserts or replaces SharePoint sites by id.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_sites(&mut self, sites: &[Site]) -> Result<()> {
        if sites.is_empty() {
            return Ok(());
        }
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO site (id, json) VALUES (?1, ?2) ON CONFLICT (id) DO UPDATE SET json = excluded.json",
            )?;
            for s in sites {
                stmt.execute(params![s.id.as_str(), to_json(s)?])?;
            }
        }
        self.touch(tables::SITE);
        Ok(())
    }

    /// Inserts or replaces SharePoint lists by id; lists without a `site_id` are filed under
    /// `site`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_lists(&mut self, site: &SiteId, lists: &[List]) -> Result<()> {
        if lists.is_empty() {
            return Ok(());
        }
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO list (id, site_id, json) VALUES (?1, ?2, ?3)
                 ON CONFLICT (id) DO UPDATE SET site_id = excluded.site_id, json = excluded.json",
            )?;
            for l in lists {
                let site_id = l.site_id.clone().unwrap_or_else(|| site.clone());
                let mut stored = l.clone();
                stored.site_id = Some(site_id.clone());
                stmt.execute(params![l.id.as_str(), site_id.as_str(), to_json(&stored)?])?;
            }
        }
        self.touch(tables::LIST);
        Ok(())
    }

    /// Inserts or replaces items of a SharePoint list by id.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_list_items(&mut self, list: &ListId, items: &[ListItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO list_item (list_id, id, modified_at, json) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (list_id, id) DO UPDATE SET modified_at = excluded.modified_at, json = excluded.json",
            )?;
            for it in items {
                let mut stored = it.clone();
                stored.list_id = Some(list.clone());
                stmt.execute(params![
                    list.as_str(),
                    it.id.as_str(),
                    ms_or_zero(it.last_modified_date_time.or(it.created_date_time)),
                    to_json(&stored)?
                ])?;
            }
        }
        self.touch(tables::LIST_ITEM);
        Ok(())
    }

    /// Deletes items of a SharePoint list by id.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_list_items(&mut self, list: &ListId, ids: &[ListItemId]) -> Result<()> {
        {
            let mut stmt = self.tx.prepare_cached("DELETE FROM list_item WHERE list_id = ?1 AND id = ?2")?;
            for id in ids {
                stmt.execute(params![list.as_str(), id.as_str()])?;
            }
        }
        if !ids.is_empty() {
            self.touch(tables::LIST_ITEM);
        }
        Ok(())
    }
}

impl Reader<'_> {
    /// All cached drives.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn drives(&self) -> Result<Page<Drive>> {
        let mut stmt = self.conn.prepare_cached("SELECT json FROM drive ORDER BY rowid LIMIT 2000")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::DRIVE, rows, |j| from_json(&j)))
    }

    /// One drive item.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) if the stored row cannot be decoded.
    pub fn drive_item(&self, drive: &DriveId, id: &DriveItemId) -> Result<Option<DriveItem>> {
        let json: Option<String> = self
            .conn
            .prepare_cached("SELECT json FROM drive_item WHERE drive_id = ?1 AND id = ?2")?
            .query_row(params![drive.as_str(), id.as_str()], |r| r.get(0))
            .optional()?;
        json.map(|j| from_json(&j)).transpose()
    }

    /// Children of `parent` in `drive` (or items without a parent, i.e. roots, for `None`):
    /// folders first, then by case-folded name, keyset-paged.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for a malformed cursor.
    pub fn drive_children(
        &self,
        drive: &DriveId,
        parent: Option<&DriveItemId>,
        after: Option<&Cursor>,
        limit: u32,
    ) -> Result<Page<DriveItem>> {
        let limit = clamp_limit(limit);
        let mut sql = String::from(
            "SELECT is_folder, name_sort, id, json FROM drive_item INDEXED BY drive_item_parent
             WHERE drive_id = ? AND parent_id IS ?",
        );
        let mut args = vec![
            Value::Text(drive.as_str().to_owned()),
            parent.map_or(Value::Null, |p| Value::Text(p.as_str().to_owned())),
        ];
        if let Some(c) = after {
            let (sort, id) = c.text_key()?;
            let (folder, name) = match sort.split_at_checked(1) {
                Some(("1", n)) => (1, n),
                Some(("0", n)) => (0, n),
                _ => return Err(crate::Error::invalid("malformed cursor")),
            };
            sql.push_str(" AND (is_folder < ? OR (is_folder = ? AND (name_sort, id) > (?, ?)))");
            args.extend([
                Value::Integer(folder),
                Value::Integer(folder),
                Value::Text(name.to_owned()),
                Value::Text(id.to_owned()),
            ]);
        }
        sql.push_str(" ORDER BY is_folder DESC, name_sort ASC, id ASC LIMIT ?");
        args.push(Value::Integer(i64::from(limit).saturating_add(1)));
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let mut rows = stmt
            .query_map(rusqlite::params_from_iter(args), |r| {
                Ok((r.get::<_, bool>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next = if has_more {
            rows.last().map(|(f, n, id, _)| Cursor::from_text_key(&format!("{}{n}", u8::from(*f)), id))
        } else {
            None
        };
        let mut page = decode_all(&self.ctx.stats, tables::DRIVE_ITEM, rows, |(_, _, _, j)| from_json(&j));
        page.next = next;
        Ok(page)
    }

    /// Recently modified files across all drives, newest first.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn recent_drive_items(&self, limit: u32) -> Result<Page<DriveItem>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT json FROM drive_item WHERE is_folder = 0 ORDER BY modified_at DESC LIMIT ?1")?;
        let rows =
            stmt.query_map([clamp_limit(limit)], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::DRIVE_ITEM, rows, |j| from_json(&j)))
    }

    /// All cached SharePoint sites.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn sites(&self) -> Result<Page<Site>> {
        let mut stmt = self.conn.prepare_cached("SELECT json FROM site ORDER BY rowid LIMIT 2000")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::SITE, rows, |j| from_json(&j)))
    }

    /// The cached lists of a site.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn lists(&self, site: &SiteId) -> Result<Page<List>> {
        let mut stmt =
            self.conn.prepare_cached("SELECT json FROM list WHERE site_id = ?1 ORDER BY rowid LIMIT 2000")?;
        let rows = stmt.query_map([site.as_str()], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::LIST, rows, |j| from_json(&j)))
    }

    /// Items of a list, most recently modified first, keyset-paged.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for a malformed cursor.
    pub fn list_items(&self, list: &ListId, after: Option<&Cursor>, limit: u32) -> Result<Page<ListItem>> {
        let limit = clamp_limit(limit);
        let (sort, id) = match after {
            Some(c) => {
                let (s, i) = c.key()?;
                (s, i.to_owned())
            }
            None => (i64::MAX, String::new()),
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT modified_at, id, json FROM list_item WHERE list_id = ?1 AND (modified_at, id) < (?2, ?3)
             ORDER BY modified_at DESC, id DESC LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(params![list.as_str(), sort, id, i64::from(limit).saturating_add(1)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(keyset_page(&self.ctx.stats, tables::LIST_ITEM, rows, limit, |j| from_json(&j)))
    }
}

impl Store {
    /// See [`Reader::drive_children`].
    ///
    /// # Errors
    ///
    /// As [`Reader::drive_children`].
    pub async fn drive_children(
        &self,
        drive: DriveId,
        parent: Option<DriveItemId>,
        after: Option<Cursor>,
        limit: u32,
    ) -> Result<Page<DriveItem>> {
        self.read(move |r| r.drive_children(&drive, parent.as_ref(), after.as_ref(), limit)).await
    }

    /// See [`Reader::recent_drive_items`].
    ///
    /// # Errors
    ///
    /// As [`Reader::recent_drive_items`].
    pub async fn recent_drive_items(&self, limit: u32) -> Result<Page<DriveItem>> {
        self.read(move |r| r.recent_drive_items(limit)).await
    }

    /// Upserts drive items and deletes removed ids in one transaction (one delta page).
    ///
    /// # Errors
    ///
    /// As [`Tx::upsert_drive_items`].
    pub async fn apply_drive_delta(
        &self,
        drive: DriveId,
        upserts: Vec<DriveItem>,
        deletes: Vec<DriveItemId>,
    ) -> Result<()> {
        self.write(move |tx| {
            tx.delete_drive_items(&drive, &deletes)?;
            tx.upsert_drive_items(&upserts)
        })
        .await
    }
}
