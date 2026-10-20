//! The calendar's sync resources.
//!
//! - `calendar.list`: calendars, calendar groups, Microsoft 365 group calendars, the signed-in
//!   address and the mailbox time zone / working hours.
//! - `calendar.default`: `/me/calendarView/delta` over a rolling window (−30 d … +180 d),
//!   re-anchored daily (a new window starts a fresh round and prunes events outside it).
//! - `calendar.others`: `calendarView` polling for every other layer (own non-default,
//!   shared, group and opened people's calendars): the near window each pass, the whole window
//!   hourly.
//! - `calendar.people`: checks whether each opened person shares their calendar (details) or
//!   not (free/busy only).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicI64, Ordering};

use futures::future::BoxFuture;
use jiff::{Timestamp, ToSpan};
use quorum_graph::api::calendar::{
    CalendarRef, SourceContext, calendar_delta_link_request, calendar_view_delta_request,
};
use quorum_graph::api::mail::{Change, Mailbox};
use quorum_graph::{DeltaOutcome, ErrorKind as G, Request, ResourceFamily};
use quorum_model::{Calendar, CalendarId, CalendarSource, Event, EventId, MailAddress, Sensitive};
use quorum_store::DeltaState;
use serde_json::Value;

use super::prefs::{
    GROUPS_KEY, LOCAL_ID_PREFIX, ME_KEY, OpenedPerson, PEOPLE_KEY, PersonMode, TIME_ZONE_KEY, WORK_HOURS_KEY,
    WorkingHours,
};
use crate::{Cadence, Ctx, Error, ErrorKind, Outcome, Resource, Result};

/// Days before today the synced window starts.
pub const WINDOW_PAST_DAYS: i64 = 30;
/// Days after today the synced window ends.
pub const WINDOW_FUTURE_DAYS: i64 = 180;
/// Pages one pass follows before yielding (`More`).
pub const MAX_PAGES_PER_PASS: usize = 20;
/// Days before today the frequent poll of non-delta calendars covers.
pub const NEAR_PAST_DAYS: i64 = 7;
/// Days after today the frequent poll covers.
pub const NEAR_FUTURE_DAYS: i64 = 42;
/// How often non-delta calendars are re-read over the whole window, in seconds.
pub const FULL_POLL_SECS: i64 = 3600;
/// Prefix of synthetic ids for Microsoft 365 group calendars.
pub const GROUP_CAL_PREFIX: &str = "group:";

/// The synced window for a UTC day: `[day − 30 d, day + 180 d)` at UTC midnight.
#[must_use]
pub fn sync_window(today: jiff::civil::Date) -> (Timestamp, Timestamp) {
    let at = |d: jiff::civil::Date| {
        d.to_zoned(jiff::tz::TimeZone::UTC).map(|z| z.timestamp()).unwrap_or(Timestamp::UNIX_EPOCH)
    };
    (at(super::view::add_days(today, -WINDOW_PAST_DAYS)), at(super::view::add_days(today, WINDOW_FUTURE_DAYS)))
}

fn utc_today() -> jiff::civil::Date {
    Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date()
}

fn bug(msg: &str) -> Error {
    Error::new(ErrorKind::Invalid, msg.to_owned())
}

async fn setting<T: serde::de::DeserializeOwned + Send + 'static>(ctx: &Ctx, key: &str) -> Option<T> {
    ctx.store.setting::<T>(key.to_owned()).await.ok().flatten()
}

/// Writes a setting only when it changed (keeps the change feed quiet).
async fn set_if_changed<T>(ctx: &Ctx, key: &str, value: T) -> Result<()>
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + Send + 'static,
{
    let current: Option<T> = setting(ctx, key).await;
    if current.as_ref() != Some(&value) {
        ctx.store.set_setting(key.to_owned(), value).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------

/// Lists calendars, groups and settings.
#[derive(Debug, Default)]
pub struct CalendarList;

impl CalendarList {
    async fn pass(&self, ctx: &Ctx) -> Result<Outcome> {
        let g = &ctx.graph;
        // Who am I (decides own vs shared)?
        let me: Option<String> = match setting::<String>(ctx, ME_KEY).await {
            Some(m) => Some(m),
            None => match g.me().await {
                Ok(p) => {
                    let addr = p.email.map(|a| a.as_str().to_ascii_lowercase());
                    if let Some(a) = &addr {
                        ctx.store.set_setting(ME_KEY.to_owned(), a.clone()).await?;
                    }
                    addr
                }
                Err(e) => {
                    tracing::warn!(name: "calendar.me_failed", kind = ?e.kind(), "could not read the signed-in user");
                    None
                }
            },
        };
        // Time zone and working hours (soft: a missing scope must not stop calendar sync).
        match g.get_mailbox_settings(&Mailbox::Me).await {
            Ok(s) => {
                if let Some(tz) = s.time_zone.filter(|t| !t.trim().is_empty() && t.len() <= 128) {
                    set_if_changed(ctx, TIME_ZONE_KEY, tz).await?;
                }
                if let Some(wh) =
                    s.working_hours.and_then(|w| WorkingHours::from_graph(&w.start_time, &w.end_time, &w.days_of_week))
                {
                    set_if_changed(ctx, WORK_HOURS_KEY, wh).await?;
                }
            }
            Err(e) => tracing::warn!(name: "calendar.mailbox_settings_failed", kind = ?e.kind(), "no mailbox settings"),
        }
        let sctx = SourceContext { me: me.and_then(|m| MailAddress::new(m).ok()) };
        let mut found: BTreeMap<String, Calendar> = BTreeMap::new();
        let mut page = g.list_calendars(&Mailbox::Me, &sctx).await?;
        for _ in 0..MAX_PAGES_PER_PASS {
            for c in page.items.drain(..) {
                found.insert(c.id.as_str().to_owned(), c);
            }
            let Some(next) = page.next_link.take() else { break };
            page = g.calendars_page(Request::link(next).family(ResourceFamily::Calendar), &Mailbox::Me, &sctx).await?;
        }
        // Calendar groups: names for the sources list, plus calendars only listed there.
        let mut groups: BTreeMap<String, String> = BTreeMap::new();
        if let Ok(gp) = g.list_calendar_groups(&Mailbox::Me).await {
            for grp in gp.items.iter().take(50) {
                if let Ok(cals) = g.list_group_calendars(&Mailbox::Me, &grp.id, &sctx).await {
                    for c in cals.items {
                        if c.source == CalendarSource::Own {
                            groups.insert(c.id.as_str().to_owned(), grp.name.as_str().to_owned());
                        }
                        found.entry(c.id.as_str().to_owned()).or_insert(c);
                    }
                }
            }
        }
        // Microsoft 365 group calendars (needs Group.Read.All; skipped softly without it).
        for c in m365_group_calendars(ctx).await {
            found.insert(c.id.as_str().to_owned(), c);
        }
        let found: Vec<Calendar> = found.into_values().collect();
        set_if_changed(ctx, GROUPS_KEY, groups).await?;
        ctx.store
            .write(move |tx| {
                let existing = tx.read().calendars()?.items;
                let keep: BTreeSet<&str> = found.iter().map(|c| c.id.as_str()).collect();
                let gone: Vec<CalendarId> = existing
                    .into_iter()
                    .filter(|c| c.source != CalendarSource::OpenedUser && !keep.contains(c.id.as_str()))
                    .map(|c| c.id)
                    .collect();
                tx.delete_calendars(&gone)?;
                // Only write calendars that changed.
                let current = tx.read().calendars()?.items;
                let changed: Vec<Calendar> = found.iter().filter(|c| !current.contains(c)).cloned().collect();
                tx.upsert_calendars(&changed)
            })
            .await?;
        Ok(Outcome::CaughtUp)
    }
}

async fn m365_group_calendars(ctx: &Ctx) -> Vec<Calendar> {
    let req = Request::get("/me/memberOf/microsoft.graph.group")
        .family(ResourceFamily::Calendar)
        .query("$select", "id,displayName,groupTypes")
        .query("$top", "100");
    let page = match ctx.graph.page::<Value>(req).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(name: "calendar.groups_skipped", kind = ?e.kind(), "group calendars unavailable");
            return Vec::new();
        }
    };
    page.items.iter().take(200).filter_map(group_calendar_from_json).collect()
}

/// Maps a `memberOf` group to a synthetic calendar layer (`group:<id>`), for unified groups only.
#[must_use]
pub fn group_calendar_from_json(v: &Value) -> Option<Calendar> {
    let unified = v
        .get("groupTypes")
        .and_then(Value::as_array)
        .is_some_and(|t| t.iter().any(|x| x.as_str().is_some_and(|s| s.eq_ignore_ascii_case("unified"))));
    if !unified {
        return None;
    }
    let id = v.get("id").and_then(Value::as_str)?;
    if !valid_group_id(id) {
        return None;
    }
    let name: String = v.get("displayName").and_then(Value::as_str).unwrap_or("Group").chars().take(256).collect();
    Some(Calendar {
        id: CalendarId::new(format!("{GROUP_CAL_PREFIX}{id}")).ok()?,
        name: Sensitive::new(name),
        color: quorum_model::CalendarColor::Auto,
        hex_color: None,
        owner: None,
        can_edit: true,
        can_share: false,
        is_default_calendar: false,
        source: CalendarSource::Group,
    })
}

fn valid_group_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

impl Resource for CalendarList {
    fn key(&self) -> String {
        "calendar.list".to_owned()
    }
    fn area(&self) -> &'static str {
        "calendar"
    }
    fn cadence(&self) -> Cadence {
        Cadence::secs(600, 900, 1800)
    }
    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(self.pass(ctx))
    }
}

// ---------------------------------------------------------------------------------------------

/// The default calendar's calendar-view delta.
#[derive(Debug, Default)]
pub struct DefaultDelta;

/// The delta state key of the default calendar.
pub const DEFAULT_DELTA_KEY: &str = "calendar.default";

async fn default_calendar(ctx: &Ctx) -> Result<Option<CalendarId>> {
    let cals = ctx.store.calendars().await?.items;
    Ok(cals.into_iter().find(|c| c.is_default_calendar && c.source == CalendarSource::Own).map(|c| c.id))
}

impl DefaultDelta {
    async fn pass(&self, ctx: &Ctx) -> Result<Outcome> {
        let Some(cal) = default_calendar(ctx).await? else {
            // `calendar.list` has not run yet.
            return Ok(Outcome::More);
        };
        let today = utc_today();
        let anchor = today.to_string();
        let mut state = ctx
            .store
            .delta_state(DEFAULT_DELTA_KEY.to_owned())
            .await?
            .unwrap_or_else(|| DeltaState::new(DEFAULT_DELTA_KEY));
        let (from, to) = sync_window(today);
        if state.backfill_cursor.as_deref() != Some(anchor.as_str()) {
            // New day (or first run): a fresh round over the re-anchored window.
            state.delta_link = None;
            state.next_link = None;
            state.backfill_cursor = Some(anchor.clone());
            state.backfill_complete = false;
        }
        let mut req = match (state.next_link.clone(), state.delta_link.clone()) {
            (Some(link), _) | (None, Some(link)) => calendar_delta_link_request(&Mailbox::Me, link, None),
            (None, None) => calendar_view_delta_request(&Mailbox::Me, from, to, None).map_err(Error::from)?,
        };
        for _ in 0..MAX_PAGES_PER_PASS {
            let page = match ctx.graph.calendar_view_delta_page(req).await {
                Ok(p) => p,
                Err(e) if e.kind() == G::DeltaExpired => {
                    tracing::info!(name: "calendar.delta_expired", "calendar delta expired; resyncing");
                    ctx.store.write(|tx| tx.clear_delta_state(DEFAULT_DELTA_KEY)).await?;
                    return Ok(Outcome::More);
                }
                Err(e) => return Err(e.into()),
            };
            let mut upserts: Vec<Event> = Vec::new();
            let mut deletes: Vec<EventId> = Vec::new();
            for c in page.items {
                match c {
                    Change::Upsert(e) => upserts.push(e),
                    Change::Removed(id) => deletes.push(id),
                }
            }
            let (done, next) = match page.outcome {
                DeltaOutcome::Next(n) => (false, n),
                DeltaOutcome::Done(d) => (true, d),
            };
            if done {
                state.next_link = None;
                state.delta_link = Some(next.clone());
            } else {
                state.next_link = Some(next.clone());
            }
            let first_round_done = done && !state.backfill_complete;
            if done {
                state.backfill_complete = true;
            }
            let st = state.clone();
            let cal2 = cal.clone();
            ctx.store
                .write(move |tx| {
                    tx.upsert_events(&cal2, &upserts)?;
                    tx.delete_events(&deletes)?;
                    if first_round_done {
                        // Prune what fell out of the re-anchored window.
                        let r = tx.read();
                        let before = r.events_in_range(std::slice::from_ref(&cal2), Timestamp::MIN, from)?.items;
                        let after = r.events_in_range(std::slice::from_ref(&cal2), to, Timestamp::MAX)?.items;
                        let stale: Vec<EventId> = before
                            .into_iter()
                            .filter(|e| e.end.to_timestamp().is_ok_and(|t| t <= from))
                            .chain(after.into_iter().filter(|e| e.start.to_timestamp().is_ok_and(|t| t >= to)))
                            .filter(|e| !e.id.as_str().starts_with(LOCAL_ID_PREFIX))
                            .map(|e| e.id)
                            .collect();
                        tx.delete_events(&stale)?;
                    }
                    tx.set_delta_state(&st)
                })
                .await?;
            if done {
                return Ok(Outcome::CaughtUp);
            }
            req = calendar_delta_link_request(&Mailbox::Me, next, None);
        }
        Ok(Outcome::More)
    }
}

impl Resource for DefaultDelta {
    fn key(&self) -> String {
        DEFAULT_DELTA_KEY.to_owned()
    }
    fn area(&self) -> &'static str {
        "calendar"
    }
    fn cadence(&self) -> Cadence {
        Cadence::secs(60, 180, 600)
    }
    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(self.pass(ctx))
    }
}

// ---------------------------------------------------------------------------------------------

/// Where a non-delta calendar's view is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewTarget {
    /// `{mailbox}/calendars/{id}/calendarView`.
    Mailbox(Mailbox, CalendarId),
    /// `/groups/{id}/calendarView`.
    Group(String),
}

/// How the view of `cal` is read; `None` for the default calendar (delta-tracked).
#[must_use]
pub fn view_target(cal: &Calendar) -> Option<ViewTarget> {
    match &cal.source {
        CalendarSource::Own if cal.is_default_calendar => None,
        CalendarSource::Group => {
            let gid = cal.id.as_str().strip_prefix(GROUP_CAL_PREFIX)?;
            valid_group_id(gid).then(|| ViewTarget::Group(gid.to_owned()))
        }
        CalendarSource::OpenedUser => {
            let owner = cal.owner.as_ref()?.address.as_ref()?;
            Some(ViewTarget::Mailbox(Mailbox::user(owner.as_str()), cal.id.clone()))
        }
        _ => Some(ViewTarget::Mailbox(Mailbox::Me, cal.id.clone())),
    }
}

/// Builds the first calendar-view request of a target.
///
/// # Errors
/// [`ErrorKind::Invalid`] for an invalid id or range.
pub fn view_request(target: &ViewTarget, from: Timestamp, to: Timestamp) -> Result<Request> {
    match target {
        ViewTarget::Mailbox(mb, id) => {
            Ok(quorum_graph::api::calendar::calendar_view_request(mb, &CalendarRef::Id(id.clone()), from, to, None)?)
        }
        ViewTarget::Group(gid) => {
            if !valid_group_id(gid) || to <= from {
                return Err(bug("invalid group calendar view"));
            }
            Ok(Request::get(format!("/groups/{gid}/calendarView"))
                .family(ResourceFamily::Calendar)
                .bucket(format!("group:{gid}"))
                .prefer(format!(
                    "{}, {}",
                    quorum_graph::api::mail::PREFER_IMMUTABLE_ID,
                    quorum_graph::api::calendar::PREFER_UTC
                ))
                .query("startDateTime", from.to_string())
                .query("endDateTime", to.to_string())
                .query("$select", quorum_graph::api::calendar::EVENT_SELECT)
                .query("$top", "100"))
        }
    }
}

/// Reads a whole calendar view (all pages, capped).
///
/// # Errors
/// As the Graph calls.
pub async fn read_view(
    ctx: &Ctx,
    target: &ViewTarget,
    cal: &CalendarId,
    from: Timestamp,
    to: Timestamp,
) -> Result<Vec<Event>> {
    let mut out = Vec::new();
    let mut page = ctx.graph.events_page(view_request(target, from, to)?, Some(cal)).await?;
    for _ in 0..MAX_PAGES_PER_PASS {
        out.append(&mut page.items);
        let Some(next) = page.next_link.take() else { return Ok(out) };
        let req = Request::link(next).family(ResourceFamily::Calendar).prefer(format!(
            "{}, {}",
            quorum_graph::api::mail::PREFER_IMMUTABLE_ID,
            quorum_graph::api::calendar::PREFER_UTC
        ));
        page = ctx.graph.events_page(req, Some(cal)).await?;
    }
    Ok(out)
}

/// Replaces the cached events of `cal` in `[from, to)` with `events`, keeping optimistic ones.
///
/// # Errors
/// As the store.
pub async fn replace_range(
    ctx: &Ctx,
    cal: CalendarId,
    from: Timestamp,
    to: Timestamp,
    events: Vec<Event>,
) -> Result<()> {
    ctx.store
        .write(move |tx| {
            let local: Vec<Event> = tx
                .read()
                .events_in_range(std::slice::from_ref(&cal), from, to)?
                .items
                .into_iter()
                .filter(|e| e.id.as_str().starts_with(LOCAL_ID_PREFIX))
                .collect();
            tx.clear_event_range(&cal, from, to)?;
            tx.upsert_events(&cal, &events)?;
            tx.upsert_events(&cal, &local)?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Polls every non-delta calendar.
#[derive(Debug, Default)]
pub struct OtherCalendars {
    last_full: AtomicI64,
}

impl OtherCalendars {
    async fn pass(&self, ctx: &Ctx) -> Result<Outcome> {
        let now = Timestamp::now();
        let full = now.as_second().saturating_sub(self.last_full.load(Ordering::Relaxed)) >= FULL_POLL_SECS;
        let today = utc_today();
        let (from, to) = if full {
            sync_window(today)
        } else {
            let near = |d: i64| {
                today
                    .checked_add(d.days())
                    .ok()
                    .and_then(|d| d.to_zoned(jiff::tz::TimeZone::UTC).ok())
                    .map(|z| z.timestamp())
                    .unwrap_or(now)
            };
            (near(-NEAR_PAST_DAYS), near(NEAR_FUTURE_DAYS))
        };
        let cals = ctx.store.calendars().await?.items;
        let mut failures = 0usize;
        for cal in cals.iter().take(quorum_store::MAX_RANGE_CALENDARS) {
            let Some(target) = view_target(cal) else { continue };
            match read_view(ctx, &target, &cal.id, from, to).await {
                Ok(events) => replace_range(ctx, cal.id.clone(), from, to, events).await?,
                Err(e) if matches!(e.kind(), ErrorKind::Permanent) => {
                    // No longer shared with us: skip this layer, keep the others going.
                    failures = failures.saturating_add(1);
                    tracing::info!(name: "calendar.view_unavailable", "a calendar layer could not be read");
                }
                Err(e) => return Err(e),
            }
        }
        if full {
            self.last_full.store(now.as_second(), Ordering::Relaxed);
        }
        if failures > 0 {
            tracing::debug!(name: "calendar.views_skipped", failures, "some calendar layers were skipped");
        }
        Ok(Outcome::CaughtUp)
    }
}

impl Resource for OtherCalendars {
    fn key(&self) -> String {
        "calendar.others".to_owned()
    }
    fn area(&self) -> &'static str {
        "calendar"
    }
    fn cadence(&self) -> Cadence {
        Cadence::secs(120, 300, 900)
    }
    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(self.pass(ctx))
    }
}

// ---------------------------------------------------------------------------------------------

/// Decides details vs free/busy for opened people.
#[derive(Debug, Default)]
pub struct People;

impl People {
    async fn pass(&self, ctx: &Ctx) -> Result<Outcome> {
        let mut people: Vec<OpenedPerson> = setting(ctx, PEOPLE_KEY).await.unwrap_or_default();
        if people.is_empty() {
            return Ok(Outcome::CaughtUp);
        }
        let mut changed = false;
        for p in people.iter_mut().filter(|p| p.mode == PersonMode::Pending) {
            let mb = Mailbox::user(p.address.clone());
            match ctx.graph.list_calendars(&mb, &SourceContext::default()).await {
                Ok(page) if !page.items.is_empty() => {
                    if p.name.is_empty()
                        && let Some(owner) = page.items.iter().find_map(|c| c.owner.clone())
                    {
                        p.name = owner.name.as_str().to_owned();
                    }
                    let addr = p.address.to_ascii_lowercase();
                    let cals: Vec<Calendar> = page
                        .items
                        .into_iter()
                        .map(|mut c| {
                            // Calendars must name their owner so their view can be read.
                            if c.owner.as_ref().and_then(|o| o.address.as_ref()).is_none() {
                                c.owner = quorum_model::EmailAddress::new(p.name.clone(), addr.clone()).ok();
                            }
                            c
                        })
                        .collect();
                    ctx.store.write(move |tx| tx.upsert_calendars(&cals)).await?;
                    p.mode = PersonMode::Details;
                    changed = true;
                }
                Ok(_) => {
                    p.mode = PersonMode::FreeBusy;
                    changed = true;
                }
                Err(e) if matches!(e.kind(), G::Forbidden | G::NotFound | G::BadRequest) => {
                    p.mode = PersonMode::FreeBusy;
                    changed = true;
                }
                Err(e) => return Err(e.into()),
            }
        }
        if changed {
            ctx.store.set_setting(PEOPLE_KEY.to_owned(), people).await?;
        }
        Ok(Outcome::CaughtUp)
    }
}

impl Resource for People {
    fn key(&self) -> String {
        "calendar.people".to_owned()
    }
    fn area(&self) -> &'static str {
        "calendar"
    }
    fn cadence(&self) -> Cadence {
        Cadence::secs(30, 300, 900)
    }
    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(self.pass(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn window_is_rolling() {
        let (from, to) = sync_window(jiff::civil::date(2026, 10, 17));
        assert_eq!(from.to_string(), "2026-09-17T00:00:00Z");
        assert_eq!(to.to_string(), "2027-04-15T00:00:00Z");
    }

    #[test]
    fn group_calendars_only_for_unified_groups() {
        assert!(
            group_calendar_from_json(&json!({"id": "g-1", "displayName": "Team", "groupTypes": ["Unified"]})).is_some()
        );
        assert!(group_calendar_from_json(&json!({"id": "g-1", "groupTypes": []})).is_none());
        assert!(group_calendar_from_json(&json!({"id": "../x", "groupTypes": ["Unified"]})).is_none());
        assert!(group_calendar_from_json(&json!({"groupTypes": ["Unified"]})).is_none());
        assert!(group_calendar_from_json(&json!(null)).is_none());
    }
}
