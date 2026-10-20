//! On-demand calendar lookups that cannot be served from the store: free/busy
//! (`getSchedule`, chunked by 20 and by 62-day windows), meeting time suggestions and rooms.
//! Results are cached briefly in memory so scrolling a view does not refetch.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use quorum_graph::api::calendar::{MAX_SCHEDULE_WINDOW, MeetingTimes, MeetingTimesQuery, Place};
use quorum_graph::api::mail::Mailbox;
use quorum_model::{MailAddress, ScheduleInformation};

use crate::{Ctx, Error, ErrorKind, Result};

/// How long free/busy answers are reused.
pub const FREE_BUSY_TTL: Duration = Duration::from_secs(120);
/// How long room lists are reused.
pub const ROOMS_TTL: Duration = Duration::from_secs(600);
/// The most addresses one free/busy lookup accepts.
pub const MAX_FREE_BUSY_USERS: usize = 100;
/// The longest free/busy range, in days (split into 62-day requests).
pub const MAX_FREE_BUSY_DAYS: i64 = 186;
const MAX_CACHE_ENTRIES: usize = 64;

struct Entry<T> {
    at: Instant,
    value: T,
}

#[derive(Default)]
struct Cache {
    schedules: HashMap<String, Entry<Vec<ScheduleInformation>>>,
    rooms: HashMap<String, Entry<Vec<Place>>>,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn with_cache<R>(f: impl FnOnce(&mut Cache) -> R) -> R {
    let mut g = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    f(g.get_or_insert_with(Cache::default))
}

/// Forgets every cached lookup (sign-in, sign-out, or after a write that changes availability).
pub fn clear_cache() {
    with_cache(|c| *c = Cache::default());
}

fn get_fresh<T: Clone>(map: &HashMap<String, Entry<T>>, key: &str, ttl: Duration) -> Option<T> {
    map.get(key).filter(|e| e.at.elapsed() < ttl).map(|e| e.value.clone())
}

fn put<T>(map: &mut HashMap<String, Entry<T>>, key: String, value: T, ttl: Duration) {
    map.retain(|_, e| e.at.elapsed() < ttl);
    if map.len() >= MAX_CACHE_ENTRIES
        && let Some(oldest) = map.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone())
    {
        map.remove(&oldest);
    }
    map.insert(key, Entry { at: Instant::now(), value });
}

/// Splits `[from, to)` into windows of at most 62 days (Graph's `getSchedule` limit).
#[must_use]
pub fn schedule_windows(from: Timestamp, to: Timestamp) -> Vec<(Timestamp, Timestamp)> {
    let max = jiff::SignedDuration::try_from(MAX_SCHEDULE_WINDOW).unwrap_or(jiff::SignedDuration::from_hours(24));
    let mut out = Vec::new();
    let mut start = from;
    while start < to && out.len() < 8 {
        let end = start.checked_add(max).unwrap_or(to).min(to);
        out.push((start, end));
        start = end;
    }
    out
}

/// Free/busy for `users` in `[from, to)` at `interval` minutes (5..=1440). Requests are
/// chunked by 20 addresses and by 62 days; windows are joined per user (availability strings
/// concatenated, items appended).
///
/// # Errors
/// [`ErrorKind::Invalid`] for an empty/oversized user list or range; Graph errors otherwise.
pub async fn free_busy(
    ctx: &Ctx,
    users: &[MailAddress],
    from: Timestamp,
    to: Timestamp,
    interval: u32,
) -> Result<Vec<ScheduleInformation>> {
    if users.is_empty() {
        return Ok(Vec::new());
    }
    if users.len() > MAX_FREE_BUSY_USERS {
        return Err(Error::new(ErrorKind::Invalid, "too many people for one free/busy lookup"));
    }
    if to <= from || from.duration_until(to) > jiff::SignedDuration::from_hours(24 * MAX_FREE_BUSY_DAYS) {
        return Err(Error::new(ErrorKind::Invalid, "free/busy range must be 1 minute to 186 days"));
    }
    if !(5..=1440).contains(&interval) {
        return Err(Error::new(ErrorKind::Invalid, "interval must be 5..=1440 minutes"));
    }
    let names: Vec<String> = users.iter().map(|u| u.as_str().to_ascii_lowercase()).collect();
    let key = format!("{}|{from}|{to}|{interval}", names.join(","));
    if let Some(hit) = with_cache(|c| get_fresh(&c.schedules, &key, FREE_BUSY_TTL)) {
        return Ok(hit);
    }
    let mut merged: Vec<ScheduleInformation> = Vec::new();
    for (i, (a, b)) in schedule_windows(from, to).into_iter().enumerate() {
        let got = ctx.graph.get_schedule(&Mailbox::Me, users, a, b, interval).await?;
        if i == 0 {
            merged = got.schedules;
            continue;
        }
        for s in got.schedules {
            match merged.iter_mut().find(|m| m.schedule_id.as_str().eq_ignore_ascii_case(s.schedule_id.as_str())) {
                Some(m) => {
                    m.availability_view.push_str(&s.availability_view);
                    m.schedule_items.extend(s.schedule_items);
                    if m.error.is_none() {
                        m.error = s.error;
                    }
                }
                None => merged.push(s),
            }
        }
    }
    let out = merged.clone();
    with_cache(|c| put(&mut c.schedules, key, merged, FREE_BUSY_TTL));
    Ok(out)
}

/// Meeting time suggestions (not cached: the answer depends on everyone's calendars now).
///
/// # Errors
/// As [`quorum_graph::Client::find_meeting_times`].
pub async fn find_times(ctx: &Ctx, query: &MeetingTimesQuery) -> Result<MeetingTimes> {
    Ok(ctx.graph.find_meeting_times(query).await?)
}

/// Rooms (all, or in one room list), cached for ten minutes.
///
/// # Errors
/// Graph errors.
pub async fn rooms(ctx: &Ctx, list: Option<&MailAddress>) -> Result<Vec<Place>> {
    let key = format!("rooms|{}", list.map(MailAddress::as_str).unwrap_or(""));
    if let Some(hit) = with_cache(|c| get_fresh(&c.rooms, &key, ROOMS_TTL)) {
        return Ok(hit);
    }
    let page = match list {
        Some(l) => ctx.graph.list_rooms_in(l).await?,
        None => ctx.graph.list_rooms().await?,
    };
    let items = page.items;
    with_cache(|c| put(&mut c.rooms, key, items.clone(), ROOMS_TTL));
    Ok(items)
}

/// Room lists (buildings), cached for ten minutes.
///
/// # Errors
/// Graph errors.
pub async fn room_lists(ctx: &Ctx) -> Result<Vec<Place>> {
    let key = "roomlists".to_owned();
    if let Some(hit) = with_cache(|c| get_fresh(&c.rooms, &key, ROOMS_TTL)) {
        return Ok(hit);
    }
    let items = ctx.graph.list_room_lists().await?.items;
    with_cache(|c| put(&mut c.rooms, key, items.clone(), ROOMS_TTL));
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_split_at_62_days() {
        let from: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        let to: Timestamp = "2026-05-01T00:00:00Z".parse().unwrap();
        let w = schedule_windows(from, to);
        assert_eq!(w.len(), 2);
        assert_eq!(w.first().map(|x| x.0), Some(from));
        assert_eq!(w.last().map(|x| x.1), Some(to));
        assert!(schedule_windows(to, from).is_empty());
    }
}
