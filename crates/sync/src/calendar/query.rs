//! The merged range query: every visible layer's events from the store, tagged with their
//! source and colour and placed in the display time zone, plus free/busy blocks for people
//! whose calendars are not shared.

use std::collections::BTreeMap;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use quorum_model::{Calendar, CalendarId, Event, FreeBusyStatus, GraphEnum, MailAddress};
use quorum_store::Store;
use serde::Serialize;

use super::prefs::{
    GROUPS_KEY, ME_KEY, OpenedPerson, PEOPLE_KEY, SOURCES_KEY, SourcePrefs, TIME_ZONE_KEY, VIEW_PREF, ViewState,
    WORK_HOURS_KEY, WorkingHours,
};
use super::sources::{SourceInfo, build_sources};
use super::view::{EventView, add_days, days_between, event_view, local_span_of, query_bounds, resolve_tz, sort_views};
use crate::{Ctx, Result};

/// Everything a calendar query needs from settings, read in one go.
#[derive(Debug, Clone)]
pub struct CalendarContext {
    /// Cached calendars.
    pub calendars: Vec<Calendar>,
    /// Layer visibility and colours.
    pub prefs: SourcePrefs,
    /// Opened people.
    pub people: Vec<OpenedPerson>,
    /// Calendar id → calendar group name.
    pub groups: BTreeMap<String, String>,
    /// Display time zone.
    pub tz: TimeZone,
    /// Working hours.
    pub work: WorkingHours,
    /// The signed-in address, if known.
    pub me: Option<String>,
    /// The current view.
    pub view: ViewState,
}

impl CalendarContext {
    /// Every layer.
    #[must_use]
    pub fn sources(&self) -> Vec<SourceInfo> {
        build_sources(&self.calendars, &self.groups, &self.prefs, &self.people)
    }

    /// Today in the display zone.
    #[must_use]
    pub fn today(&self) -> Date {
        jiff::Timestamp::now().to_zoned(self.tz.clone()).date()
    }
}

/// Reads the calendar context from the store.
///
/// # Errors
/// As the store (corrupt settings fall back to defaults).
pub async fn load(store: &Store) -> Result<CalendarContext> {
    let ctx = store
        .read(|r| {
            let tz_name: Option<String> = r.setting(TIME_ZONE_KEY).ok().flatten();
            Ok(CalendarContext {
                calendars: r.calendars()?.items,
                prefs: r.setting(SOURCES_KEY).ok().flatten().unwrap_or_default(),
                people: r.setting(PEOPLE_KEY).ok().flatten().unwrap_or_default(),
                groups: r.setting(GROUPS_KEY).ok().flatten().unwrap_or_default(),
                tz: resolve_tz(tz_name.as_deref()),
                work: r.setting(WORK_HOURS_KEY).ok().flatten().unwrap_or_default(),
                me: r.setting(ME_KEY).ok().flatten(),
                view: r.ui_pref(VIEW_PREF).ok().flatten().unwrap_or_default(),
            })
        })
        .await?;
    Ok(ctx)
}

/// Which layers a query covers: the given ids (visible or not), or every visible layer.
#[must_use]
pub fn pick_sources<'a>(all: &'a [SourceInfo], only: Option<&[String]>) -> Vec<&'a SourceInfo> {
    match only {
        Some(ids) => all.iter().filter(|s| ids.iter().any(|i| i == &s.id)).collect(),
        None => all.iter().filter(|s| s.visible).collect(),
    }
}

/// Events of `sources` touching local days `[first, first + n)`, merged and sorted.
///
/// # Errors
/// As the store.
pub async fn merged_events(
    store: &Store,
    cx: &CalendarContext,
    sources: &[&SourceInfo],
    first: Date,
    n: i32,
) -> Result<Vec<EventView>> {
    let ids: Vec<CalendarId> = sources
        .iter()
        .filter(|s| !s.free_busy_only)
        .filter_map(|s| CalendarId::new(s.id.clone()).ok())
        .take(quorum_store::MAX_RANGE_CALENDARS)
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let (from, to) = query_bounds(first, n, &cx.tz);
    let events = store.events_in_range(ids, from, to).await?.items;
    Ok(place(&events, sources, &cx.tz, first, n))
}

/// Places stored events in a view (pure: the merge step of [`merged_events`]).
#[must_use]
pub fn place(events: &[Event], sources: &[&SourceInfo], tz: &TimeZone, first: Date, n: i32) -> Vec<EventView> {
    let mut out: Vec<EventView> = events
        .iter()
        .filter_map(|e| {
            let cal = e.calendar_id.as_ref()?;
            let src = sources.iter().find(|s| s.id == cal.as_str())?;
            event_view(e, src, tz, first, n)
        })
        .collect();
    sort_views(&mut out);
    out
}

/// A free/busy block of a person whose calendar is not shared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BusyBlock {
    /// The person's source id.
    pub source: String,
    /// The person's name.
    pub name: String,
    /// `#rrggbb`.
    pub color: String,
    /// `busy`, `tentative`, `oof`, `workingElsewhere`.
    pub status: String,
    /// Day index in the view.
    pub start_day: i64,
    /// Last day index (inclusive).
    pub end_day: i64,
    /// Minutes after midnight on the start day.
    pub start_min: u16,
    /// Minutes after midnight on the end day (1440 = midnight).
    pub end_min: u16,
    /// `09:00 – 10:00`.
    pub time_label: String,
}

/// Free/busy blocks for the free/busy-only people among `sources` (network, cached briefly).
/// Failures are soft: the error is returned next to whatever was found.
pub async fn busy_blocks(
    ctx: &Ctx,
    cx: &CalendarContext,
    sources: &[&SourceInfo],
    first: Date,
    n: i32,
) -> (Vec<BusyBlock>, Option<String>) {
    let people: Vec<(&SourceInfo, MailAddress)> = sources
        .iter()
        .filter(|s| s.free_busy_only)
        .filter_map(|s| Some((*s, MailAddress::new(s.person.clone()?).ok()?)))
        .collect();
    if people.is_empty() {
        return (Vec::new(), None);
    }
    let (from, _) = query_bounds(add_days(first, 1), 0, &cx.tz);
    let (_, to) = query_bounds(add_days(first, -1), n, &cx.tz);
    let addrs: Vec<MailAddress> = people.iter().map(|(_, a)| a.clone()).collect();
    match super::lookup::free_busy(ctx, &addrs, from, to, 30).await {
        Ok(schedules) => {
            let mut out = Vec::new();
            for (src, addr) in &people {
                let Some(s) = schedules.iter().find(|s| s.schedule_id.as_str().eq_ignore_ascii_case(addr.as_str()))
                else {
                    continue;
                };
                for item in s.schedule_items.iter().take(2000) {
                    if item.status == FreeBusyStatus::Free {
                        continue;
                    }
                    let Some((sd, sm, ed, em, _)) = local_span_of(&item.start, &item.end, false, &cx.tz) else {
                        continue;
                    };
                    let (start_day, end_day) = (days_between(first, sd), days_between(first, ed));
                    if end_day < 0 || start_day >= i64::from(n) {
                        continue;
                    }
                    out.push(BusyBlock {
                        source: src.id.clone(),
                        name: src.name.clone(),
                        color: src.color.clone(),
                        status: item.status.as_graph().to_owned(),
                        start_day,
                        end_day,
                        start_min: sm,
                        end_min: em,
                        time_label: format!("{} – {}", super::view::hhmm(sm), super::view::hhmm(em)),
                    });
                }
            }
            (out, None)
        }
        Err(e) => {
            tracing::warn!(name: "calendar.free_busy_failed", kind = ?e.kind(), "free/busy lookup failed");
            (Vec::new(), Some(e.message().to_owned()))
        }
    }
}
