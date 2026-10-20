//! Calendars and events, with UTC range queries across a set of calendars.

use jiff::Timestamp;
use quorum_model::{Calendar, CalendarId, DateTimeZone, Event, EventId, EventType, GraphEnum};
use rusqlite::{OptionalExtension, params};

use crate::error::{Error, Result};
use crate::store::{Reader, Store, Tx, tables};
use crate::util::{Page, decode_all, from_json, ms, to_json};

/// The most events one range query returns.
pub const MAX_RANGE_EVENTS: u32 = 5000;
/// The most calendars one range query accepts.
pub const MAX_RANGE_CALENDARS: usize = 200;

/// Resolves a Graph date-time to an instant; unknown zones fall back to UTC.
fn instant(dt: &DateTimeZone) -> Option<Timestamp> {
    dt.to_timestamp().ok().or_else(|| jiff::tz::TimeZone::UTC.to_timestamp(dt.date_time).ok())
}

impl Tx<'_> {
    /// Inserts or replaces calendars by id.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_calendars(&mut self, calendars: &[Calendar]) -> Result<()> {
        if calendars.is_empty() {
            return Ok(());
        }
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO calendar (id, source, json) VALUES (?1, ?2, ?3)
                 ON CONFLICT (id) DO UPDATE SET source = excluded.source, json = excluded.json",
            )?;
            for c in calendars {
                stmt.execute(params![c.id.as_str(), c.source.as_graph(), to_json(c)?])?;
            }
        }
        self.touch(tables::CALENDAR);
        Ok(())
    }

    /// Deletes calendars and their events.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_calendars(&mut self, ids: &[CalendarId]) -> Result<()> {
        for id in ids {
            self.tx.execute("DELETE FROM event WHERE calendar_id = ?1", [id.as_str()])?;
            self.tx.execute("DELETE FROM calendar WHERE id = ?1", [id.as_str()])?;
        }
        if !ids.is_empty() {
            self.touch(tables::CALENDAR);
            self.touch(tables::EVENT);
        }
        Ok(())
    }

    /// Inserts or replaces events by id. Events without a `calendar_id` are filed under
    /// `calendar`. Returns how many events were skipped because their times cannot be resolved.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn upsert_events(&mut self, calendar: &CalendarId, events: &[Event]) -> Result<u32> {
        let mut skipped = 0u32;
        if events.is_empty() {
            return Ok(0);
        }
        {
            let mut stmt = self.tx.prepare_cached(
                "INSERT INTO event (id, calendar_id, start_utc, end_utc, is_all_day, event_type, series_master_id, json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (id) DO UPDATE SET calendar_id = excluded.calendar_id,
                    start_utc = excluded.start_utc, end_utc = excluded.end_utc, is_all_day = excluded.is_all_day,
                    event_type = excluded.event_type, series_master_id = excluded.series_master_id,
                    json = excluded.json",
            )?;
            for e in events {
                let (Some(start), Some(end)) = (instant(&e.start), instant(&e.end)) else {
                    skipped = skipped.saturating_add(1);
                    tracing::warn!(name: "store.event_time_unresolved", "skipped an event whose times cannot be resolved");
                    continue;
                };
                let (start, end) = (ms(start), ms(end).max(ms(start)));
                let cal = e.calendar_id.as_ref().unwrap_or(calendar);
                let mut stored = e.clone();
                stored.calendar_id = Some(cal.clone());
                stmt.execute(params![
                    e.id.as_str(),
                    cal.as_str(),
                    start,
                    end,
                    e.is_all_day,
                    e.event_type.as_graph(),
                    e.series_master_id.as_ref().map(EventId::as_str),
                    to_json(&stored)?
                ])?;
            }
        }
        self.touch(tables::EVENT);
        Ok(skipped)
    }

    /// Deletes events by id (and, for series masters, their cached occurrences).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn delete_events(&mut self, ids: &[EventId]) -> Result<()> {
        for id in ids {
            self.tx.prepare_cached("DELETE FROM event WHERE series_master_id = ?1")?.execute([id.as_str()])?;
            self.tx.prepare_cached("DELETE FROM event WHERE id = ?1")?.execute([id.as_str()])?;
        }
        if !ids.is_empty() {
            self.touch(tables::EVENT);
        }
        Ok(())
    }

    /// Deletes every cached event of `calendar` overlapping `[from, to)`, before re-filling a
    /// calendar view window that is not delta-tracked (e.g. a shared calendar).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if a write fails.
    pub fn clear_event_range(&mut self, calendar: &CalendarId, from: Timestamp, to: Timestamp) -> Result<()> {
        self.tx.execute(
            "DELETE FROM event WHERE calendar_id = ?1 AND start_utc < ?3 AND end_utc > ?2 AND event_type != 'seriesMaster'",
            params![calendar.as_str(), ms(from), ms(to)],
        )?;
        self.touch(tables::EVENT);
        Ok(())
    }
}

impl Reader<'_> {
    /// All cached calendars.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the query fails.
    pub fn calendars(&self) -> Result<Page<Calendar>> {
        let mut stmt = self.conn.prepare_cached("SELECT json FROM calendar ORDER BY rowid LIMIT 2000")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(decode_all(&self.ctx.stats, tables::CALENDAR, rows, |j| from_json(&j)))
    }

    /// One event.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) if the stored row cannot be decoded.
    pub fn event(&self, id: &EventId) -> Result<Option<Event>> {
        let json: Option<String> = self
            .conn
            .prepare_cached("SELECT json FROM event WHERE id = ?1")?
            .query_row([id.as_str()], |r| r.get(0))
            .optional()?;
        json.map(|j| from_json(&j)).transpose()
    }

    /// Events of `calendars` overlapping the UTC range `[from, to)`, ordered by start.
    ///
    /// Overlap means `start < to && end > from`, so multi-day and all-day events that began
    /// before `from` are included; zero-length events at `from` are included too. Series
    /// masters are excluded (their occurrences are stored as separate events). At most
    /// [`MAX_RANGE_EVENTS`] events and [`MAX_RANGE_CALENDARS`] calendars.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) if more than
    /// [`MAX_RANGE_CALENDARS`] calendars are given.
    pub fn events_in_range(&self, calendars: &[CalendarId], from: Timestamp, to: Timestamp) -> Result<Page<Event>> {
        if calendars.len() > MAX_RANGE_CALENDARS {
            return Err(Error::invalid("too many calendars in one range query"));
        }
        if calendars.is_empty() || to < from {
            return Ok(Page::new());
        }
        let (from, to) = (ms(from), ms(to));
        let mut rows: Vec<(i64, String, String)> = Vec::new();
        let mut stmt = self.conn.prepare_cached(
            "SELECT start_utc, id, json FROM event INDEXED BY event_calendar_start
             WHERE calendar_id = ?1 AND start_utc < ?3 AND (end_utc > ?2 OR (end_utc = start_utc AND start_utc >= ?2))
               AND event_type != ?4
             ORDER BY start_utc LIMIT ?5",
        )?;
        for cal in calendars {
            let found = stmt.query_map(
                params![cal.as_str(), from, to, EventType::SeriesMaster.as_graph(), MAX_RANGE_EVENTS],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            for row in found {
                rows.push(row?);
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        rows.truncate(MAX_RANGE_EVENTS as usize);
        Ok(decode_all(&self.ctx.stats, tables::EVENT, rows, |(_, _, j)| from_json(&j)))
    }
}

impl Store {
    /// See [`Reader::calendars`].
    ///
    /// # Errors
    ///
    /// As [`Reader::calendars`].
    pub async fn calendars(&self) -> Result<Page<Calendar>> {
        self.read(|r| r.calendars()).await
    }

    /// See [`Reader::events_in_range`].
    ///
    /// # Errors
    ///
    /// As [`Reader::events_in_range`].
    pub async fn events_in_range(
        &self,
        calendars: Vec<CalendarId>,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Page<Event>> {
        self.read(move |r| r.events_in_range(&calendars, from, to)).await
    }

    /// Upserts events and deletes removed ids in one transaction (one delta page). Returns how
    /// many events were skipped (unresolvable times).
    ///
    /// # Errors
    ///
    /// As [`Tx::upsert_events`].
    pub async fn apply_event_delta(
        &self,
        calendar: CalendarId,
        upserts: Vec<Event>,
        deletes: Vec<EventId>,
    ) -> Result<u32> {
        self.write(move |tx| {
            let skipped = tx.upsert_events(&calendar, &upserts)?;
            tx.delete_events(&deletes)?;
            Ok(skipped)
        })
        .await
    }
}
