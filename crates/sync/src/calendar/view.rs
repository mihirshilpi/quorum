//! The merged calendar view model: which days a view shows, titles, the mini month, and each
//! event converted to the display time zone with day/minute layout hints.
//!
//! Everything here is pure (no I/O besides the time zone database lookup in [`resolve_tz`]), so
//! the range math, titles and recurrence text are tested natively.

use jiff::civil::{Date, Weekday};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};
use quorum_model::{
    DayOfWeek, Event, EventType, GraphEnum, RecurrencePatternType, RecurrenceRangeType, RecurrenceSummary, TimeZoneName,
};
use serde::Serialize;

use super::prefs::{LOCAL_ID_PREFIX, ViewKind, ViewState};
use super::sources::SourceInfo;

/// Days shown by the agenda view.
pub const AGENDA_DAYS: i32 = 14;
/// Longest range a merged query accepts, in days.
pub const MAX_RANGE_DAYS: i64 = 400;

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

/// Resolves the display time zone: the mailbox zone (Windows or IANA name) if it maps, else the
/// system zone, else UTC.
#[must_use]
pub fn resolve_tz(name: Option<&str>) -> TimeZone {
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        if name.eq_ignore_ascii_case("utc") {
            return TimeZone::UTC;
        }
        let tzn = TimeZoneName::new(name);
        if let Some(tz) = tzn.iana().and_then(|iana| TimeZone::get(iana).ok()) {
            return tz;
        }
    }
    TimeZone::try_system().unwrap_or(TimeZone::UTC)
}

/// The IANA (or fallback) name of a zone, for display.
#[must_use]
pub fn tz_name(tz: &TimeZone) -> String {
    tz.iana_name().unwrap_or("UTC").to_owned()
}

fn month_name(m: i8) -> &'static str {
    MONTHS.get(usize::try_from(m.saturating_sub(1)).unwrap_or(0)).copied().unwrap_or("")
}

fn short_month(m: i8) -> &'static str {
    month_name(m).get(..3).unwrap_or("")
}

fn weekday_name(w: Weekday) -> &'static str {
    WEEKDAYS.get(usize::try_from(w.to_monday_zero_offset()).unwrap_or(0)).copied().unwrap_or("")
}

fn short_weekday(w: Weekday) -> &'static str {
    weekday_name(w).get(..3).unwrap_or("")
}

/// Adds `days` to `d`, saturating at the calendar bounds.
#[must_use]
pub fn add_days(d: Date, days: i64) -> Date {
    d.checked_add(days.days()).unwrap_or(if days < 0 { Date::MIN } else { Date::MAX })
}

fn add_months(d: Date, months: i32) -> Date {
    d.checked_add(months.months()).unwrap_or(d)
}

/// Days from `a` to `b` (negative if `b` is earlier).
#[must_use]
pub fn days_between(a: Date, b: Date) -> i64 {
    a.until(b).map(|s| i64::from(s.get_days())).unwrap_or(0)
}

/// The Monday on or before `d`.
#[must_use]
pub fn week_start(d: Date) -> Date {
    add_days(d, -i64::from(d.weekday().to_monday_zero_offset()))
}

fn first_of_month(d: Date) -> Date {
    d.first_of_month()
}

/// The first day and the number of days a view shows around `anchor`.
#[must_use]
pub fn view_days(view: ViewKind, anchor: Date) -> (Date, i32) {
    match view {
        ViewKind::Day => (anchor, 1),
        ViewKind::WorkWeek => (week_start(anchor), 5),
        ViewKind::Week => (week_start(anchor), 7),
        ViewKind::Month => (week_start(first_of_month(anchor)), 42),
        ViewKind::Agenda => (anchor, AGENDA_DAYS),
    }
}

/// The anchor one period before (`dir < 0`) or after (`dir > 0`) `anchor`.
#[must_use]
pub fn step(view: ViewKind, anchor: Date, dir: i32) -> Date {
    let dir = dir.signum();
    match view {
        ViewKind::Day => add_days(anchor, i64::from(dir)),
        ViewKind::WorkWeek | ViewKind::Week => add_days(anchor, i64::from(dir) * 7),
        ViewKind::Month => add_months(first_of_month(anchor), dir),
        ViewKind::Agenda => add_days(anchor, i64::from(dir) * i64::from(AGENDA_DAYS)),
    }
}

/// `October 17, 2026`.
#[must_use]
pub fn long_date(d: Date) -> String {
    format!("{} {}, {}", month_name(d.month()), d.day(), d.year())
}

/// `Fri, Oct 17`.
#[must_use]
pub fn short_date(d: Date) -> String {
    format!("{}, {} {}", short_weekday(d.weekday()), short_month(d.month()), d.day())
}

/// The toolbar title of a view: `Friday, October 17, 2026`, `October 12 – 18, 2026`,
/// `Sep 28 – Oct 4, 2026`, `Dec 28, 2026 – Jan 3, 2027` or `October 2026`.
#[must_use]
pub fn title(view: ViewKind, anchor: Date) -> String {
    let (first, n) = view_days(view, anchor);
    match view {
        ViewKind::Day => format!("{}, {}", weekday_name(anchor.weekday()), long_date(anchor)),
        ViewKind::Month => format!("{} {}", month_name(anchor.month()), anchor.year()),
        _ => {
            let last = add_days(first, i64::from(n.saturating_sub(1)));
            if first.year() != last.year() {
                format!(
                    "{} {}, {} – {} {}, {}",
                    short_month(first.month()),
                    first.day(),
                    first.year(),
                    short_month(last.month()),
                    last.day(),
                    last.year()
                )
            } else if first.month() != last.month() {
                format!(
                    "{} {} – {} {}, {}",
                    short_month(first.month()),
                    first.day(),
                    short_month(last.month()),
                    last.day(),
                    last.year()
                )
            } else {
                format!("{} {} – {}, {}", month_name(first.month()), first.day(), last.day(), last.year())
            }
        }
    }
}

/// One day column / cell of a view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DayInfo {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// Day of month.
    pub day: i8,
    /// `Mon`.
    pub weekday: &'static str,
    /// `Mon 12`.
    pub label: String,
    /// Whether it is today.
    pub is_today: bool,
    /// Whether it is in the anchor's month (month view shading).
    pub in_month: bool,
    /// Saturday or Sunday.
    pub is_weekend: bool,
}

/// The days of a view.
#[must_use]
pub fn days(view: ViewKind, anchor: Date, today: Date) -> Vec<DayInfo> {
    let (first, n) = view_days(view, anchor);
    (0..n)
        .map(|i| {
            let d = add_days(first, i64::from(i));
            DayInfo {
                date: d.to_string(),
                day: d.day(),
                weekday: short_weekday(d.weekday()),
                label: format!("{} {}", short_weekday(d.weekday()), d.day()),
                is_today: d == today,
                in_month: d.month() == anchor.month() && d.year() == anchor.year(),
                is_weekend: matches!(d.weekday(), Weekday::Saturday | Weekday::Sunday),
            }
        })
        .collect()
}

/// A day of the mini month.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MiniDay {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// Day of month.
    pub day: i8,
    /// In the shown month.
    pub in_month: bool,
    /// Today.
    pub is_today: bool,
    /// Inside the main view's range.
    pub in_range: bool,
}

/// The mini month in the navigation column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MiniMonth {
    /// `October 2026`.
    pub title: String,
    /// Six weeks of seven days, Monday first.
    pub weeks: Vec<Vec<MiniDay>>,
}

/// The mini month `offset` months from the anchor's month.
#[must_use]
pub fn mini_month(state: &ViewState, today: Date, offset: i32) -> MiniMonth {
    let anchor = state.date.unwrap_or(today);
    let month = add_months(first_of_month(anchor), offset.clamp(-1200, 1200));
    let (range_first, n) = view_days(state.view, anchor);
    let range_last = add_days(range_first, i64::from(n.saturating_sub(1)));
    let first = week_start(month);
    let weeks = (0..6)
        .map(|w| {
            (0..7)
                .map(|i| {
                    let d = add_days(first, w * 7 + i);
                    MiniDay {
                        date: d.to_string(),
                        day: d.day(),
                        in_month: d.month() == month.month(),
                        is_today: d == today,
                        in_range: d >= range_first && d <= range_last,
                    }
                })
                .collect()
        })
        .collect();
    MiniMonth { title: format!("{} {}", month_name(month.month()), month.year()), weeks }
}

/// An event placed in a view: display-zone times plus layout hints.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventView {
    /// Event id.
    pub id: String,
    /// Subject (`(No subject)` when empty).
    pub subject: String,
    /// Location name.
    pub location: String,
    /// Organizer name or address.
    pub organizer: String,
    /// Source (layer) id.
    pub source: String,
    /// Source name.
    pub source_name: String,
    /// `#rrggbb`.
    pub color: String,
    /// Start instant (RFC 3339, UTC).
    pub start: String,
    /// End instant.
    pub end: String,
    /// Local start date.
    pub start_date: String,
    /// Local end date (inclusive).
    pub end_date: String,
    /// Index of the start day in the view (may be negative: started earlier).
    pub start_day: i64,
    /// Index of the last day it touches (inclusive; may be past the view).
    pub end_day: i64,
    /// Minutes after local midnight on the start day.
    pub start_min: u16,
    /// Minutes after local midnight on the end day (1440 = midnight).
    pub end_min: u16,
    /// Shown in the all-day row (all-day, or 24 hours or longer).
    pub all_day: bool,
    /// `09:00 – 09:30` or `All day`.
    pub time_label: String,
    /// Graph `showAs`.
    pub show_as: String,
    /// The user's response.
    pub response: String,
    /// Has an online meeting.
    pub is_online: bool,
    /// Can be joined (has an https join URL).
    pub can_join: bool,
    /// Part of a series.
    pub is_recurring: bool,
    /// Cancelled by the organizer.
    pub is_cancelled: bool,
    /// The user organizes it.
    pub is_organizer: bool,
    /// Created or changed locally and not yet on the server.
    pub pending: bool,
    /// Private / confidential.
    pub is_private: bool,
}

/// `09:05`.
#[must_use]
pub fn hhmm(minutes: u16) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

fn minutes_of(z: &Zoned) -> u16 {
    let m = i32::from(z.hour()) * 60 + i32::from(z.minute());
    u16::try_from(m.clamp(0, 1440)).unwrap_or(0)
}

/// Whether a join URL may be opened: `https` only, no control characters, bounded length.
#[must_use]
pub fn safe_join_url(url: Option<&str>) -> Option<&str> {
    let url = url?.trim();
    let ok = url.len() <= 4096
        && url.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        && url.len() > 8
        && !url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '"' || c == '<' || c == '>');
    ok.then_some(url)
}

/// Local start and end of an event: `(start date, start minute, end date inclusive, end minute,
/// all-day row)`. All-day events are floating dates (no zone conversion).
#[must_use]
pub fn local_span(e: &Event, tz: &TimeZone) -> Option<(Date, u16, Date, u16, bool)> {
    local_span_of(&e.start, &e.end, e.is_all_day, tz)
}

/// [`local_span`] for a bare start/end pair.
#[must_use]
pub fn local_span_of(
    start: &quorum_model::DateTimeZone,
    end: &quorum_model::DateTimeZone,
    all_day: bool,
    tz: &TimeZone,
) -> Option<(Date, u16, Date, u16, bool)> {
    if all_day {
        let start = start.date_time.date();
        let end_excl = end.date_time.date();
        let end = if end_excl > start { add_days(end_excl, -1) } else { start };
        return Some((start, 0, end, 1440, true));
    }
    let s = start.to_timestamp().ok()?.to_zoned(tz.clone());
    let en = end.to_timestamp().ok()?.to_zoned(tz.clone());
    let en = if en < s { s.clone() } else { en };
    let long = s.timestamp().duration_until(en.timestamp()) >= jiff::SignedDuration::from_hours(24);
    let (mut end_date, mut end_min) = (en.date(), minutes_of(&en));
    if end_min == 0 && en.timestamp() > s.timestamp() {
        end_date = add_days(end_date, -1);
        end_min = 1440;
    }
    Some((s.date(), minutes_of(&s), end_date, end_min, long))
}

/// Places `e` (from `source`) in a view starting at `first` with `n` days, or `None` when it
/// does not touch the view.
#[must_use]
pub fn event_view(e: &Event, source: &SourceInfo, tz: &TimeZone, first: Date, n: i32) -> Option<EventView> {
    let (sd, sm, ed, em, all_day) = local_span(e, tz)?;
    let start_day = days_between(first, sd);
    let end_day = days_between(first, ed);
    if end_day < 0 || start_day >= i64::from(n) {
        return None;
    }
    let ts = |d: &quorum_model::DateTimeZone| d.to_timestamp().map(|t| t.to_string()).unwrap_or_default();
    let subject = if e.subject.as_str().trim().is_empty() { "(No subject)".to_owned() } else { e.subject().to_owned() };
    let time_label = if e.is_all_day {
        "All day".to_owned()
    } else if sd == ed {
        format!("{} – {}", hhmm(sm), hhmm(em))
    } else {
        format!("{} {} – {} {}", short_date(sd), hhmm(sm), short_date(ed), hhmm(em))
    };
    Some(EventView {
        id: e.id.as_str().to_owned(),
        subject,
        location: e.location.as_ref().map(|l| l.display_name.as_str().to_owned()).unwrap_or_default(),
        organizer: e
            .organizer
            .as_ref()
            .map(|o| {
                if o.name.is_empty() {
                    o.address.as_ref().map(|a| a.as_str().to_owned()).unwrap_or_default()
                } else {
                    o.name.as_str().to_owned()
                }
            })
            .unwrap_or_default(),
        source: source.id.clone(),
        source_name: source.name.clone(),
        color: source.color.clone(),
        start: ts(&e.start),
        end: ts(&e.end),
        start_date: sd.to_string(),
        end_date: ed.to_string(),
        start_day,
        end_day,
        start_min: sm,
        end_min: em,
        all_day,
        time_label,
        show_as: e.show_as.as_graph().to_owned(),
        response: e.response_status.response.as_graph().to_owned(),
        is_online: e.is_online_meeting,
        can_join: safe_join_url(e.online_meeting_join_url.as_deref()).is_some(),
        is_recurring: e.is_recurring(),
        is_cancelled: e.is_cancelled,
        is_organizer: e.is_organizer,
        pending: e.id.as_str().starts_with(LOCAL_ID_PREFIX),
        is_private: matches!(
            e.sensitivity,
            quorum_model::Sensitivity::Private | quorum_model::Sensitivity::Confidential
        ),
    })
}

/// Sorts placed events: all-day first, then by start, longer first, then id (stable layout).
pub fn sort_views(v: &mut [EventView]) {
    v.sort_by(|a, b| {
        b.all_day
            .cmp(&a.all_day)
            .then(a.start_day.cmp(&b.start_day))
            .then(a.start_min.cmp(&b.start_min))
            .then(b.end_day.cmp(&a.end_day))
            .then(b.end_min.cmp(&a.end_min))
            .then(a.id.cmp(&b.id))
    });
}

/// The UTC instants bounding local days `[first, first + n)` in `tz`, widened by a day on each
/// side so floating all-day events in any zone are found.
#[must_use]
pub fn query_bounds(first: Date, n: i32, tz: &TimeZone) -> (Timestamp, Timestamp) {
    let start = add_days(first, -1);
    let end = add_days(first, i64::from(n).saturating_add(1));
    let at = |d: Date| d.to_zoned(tz.clone()).map(|z| z.timestamp()).unwrap_or(Timestamp::UNIX_EPOCH);
    (at(start), at(end))
}

fn day_list(days: &[DayOfWeek]) -> String {
    let names: Vec<&str> = days
        .iter()
        .filter_map(|d| match d {
            DayOfWeek::Monday => Some("Monday"),
            DayOfWeek::Tuesday => Some("Tuesday"),
            DayOfWeek::Wednesday => Some("Wednesday"),
            DayOfWeek::Thursday => Some("Thursday"),
            DayOfWeek::Friday => Some("Friday"),
            DayOfWeek::Saturday => Some("Saturday"),
            DayOfWeek::Sunday => Some("Sunday"),
            _ => None,
        })
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn every(interval: u32, one: &str, many: &str) -> String {
    if interval <= 1 { format!("Every {one}") } else { format!("Every {interval} {many}") }
}

/// A human summary of a recurrence: `Every 2 weeks on Monday and Wednesday, until
/// December 1, 2026`.
#[must_use]
pub fn recurrence_text(r: &RecurrenceSummary) -> String {
    let days = day_list(&r.days_of_week);
    let on_days = if days.is_empty() { String::new() } else { format!(" on {days}") };
    let mut text = match &r.pattern {
        RecurrencePatternType::Daily => every(r.interval, "day", "days"),
        RecurrencePatternType::Weekly => format!("{}{on_days}", every(r.interval, "week", "weeks")),
        RecurrencePatternType::AbsoluteMonthly => match r.day_of_month {
            Some(d) => format!("{} on day {d}", every(r.interval, "month", "months")),
            None => every(r.interval, "month", "months"),
        },
        RecurrencePatternType::RelativeMonthly => format!("{}{on_days}", every(r.interval, "month", "months")),
        RecurrencePatternType::AbsoluteYearly => match (r.start_date, r.day_of_month) {
            (Some(s), _) => format!("{} on {} {}", every(r.interval, "year", "years"), month_name(s.month()), s.day()),
            (None, Some(d)) => format!("{} on day {d}", every(r.interval, "year", "years")),
            (None, None) => every(r.interval, "year", "years"),
        },
        RecurrencePatternType::RelativeYearly => format!("{}{on_days}", every(r.interval, "year", "years")),
        _ => "Repeats".to_owned(),
    };
    match (&r.range_type, r.end_date, r.number_of_occurrences) {
        (Some(RecurrenceRangeType::EndDate), Some(end), _) => text.push_str(&format!(", until {}", long_date(end))),
        (Some(RecurrenceRangeType::Numbered), _, Some(n)) => text.push_str(&format!(", {n} times")),
        _ => {}
    }
    text
}

/// Whether the event is an occurrence/exception whose series text must come from its master.
#[must_use]
pub fn needs_master(e: &Event) -> bool {
    e.recurrence.is_none() && matches!(e.event_type, EventType::Occurrence | EventType::Exception)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    #[test]
    fn view_ranges_and_titles() {
        let fri = date(2026, 10, 16);
        assert_eq!(view_days(ViewKind::Day, fri), (fri, 1));
        assert_eq!(view_days(ViewKind::Week, fri), (date(2026, 10, 12), 7));
        assert_eq!(view_days(ViewKind::WorkWeek, fri), (date(2026, 10, 12), 5));
        assert_eq!(view_days(ViewKind::Month, fri), (date(2026, 9, 28), 42));
        assert_eq!(title(ViewKind::Week, fri), "October 12 – 18, 2026");
        assert_eq!(title(ViewKind::Week, date(2026, 10, 1)), "Sep 28 – Oct 4, 2026");
        assert_eq!(title(ViewKind::Week, date(2026, 12, 30)), "Dec 28, 2026 – Jan 3, 2027");
        assert_eq!(title(ViewKind::Month, fri), "October 2026");
        assert_eq!(title(ViewKind::Day, fri), "Friday, October 16, 2026");
        assert_eq!(step(ViewKind::Month, date(2026, 1, 31), 1), date(2026, 2, 1));
        assert_eq!(step(ViewKind::Week, fri, -1), date(2026, 10, 9));
        assert_eq!(step(ViewKind::Day, Date::MAX, 1), Date::MAX);
    }

    #[test]
    fn mini_month_marks_range_and_today() {
        let state = ViewState { view: ViewKind::Week, date: Some(date(2026, 10, 16)) };
        let m = mini_month(&state, date(2026, 10, 17), 0);
        assert_eq!(m.title, "October 2026");
        assert_eq!(m.weeks.len(), 6);
        let flat: Vec<&MiniDay> = m.weeks.iter().flatten().collect();
        assert_eq!(flat.iter().filter(|d| d.in_range).count(), 7);
        assert_eq!(flat.iter().filter(|d| d.is_today).count(), 1);
        assert_eq!(mini_month(&state, date(2026, 10, 17), 3).title, "January 2027");
    }

    #[test]
    fn recurrence_texts() {
        let mut r = RecurrenceSummary {
            pattern: RecurrencePatternType::Weekly,
            interval: 2,
            days_of_week: vec![DayOfWeek::Monday, DayOfWeek::Wednesday],
            day_of_month: None,
            range_type: Some(RecurrenceRangeType::EndDate),
            start_date: Some(date(2026, 1, 5)),
            end_date: Some(date(2026, 12, 1)),
            number_of_occurrences: None,
        };
        assert_eq!(recurrence_text(&r), "Every 2 weeks on Monday and Wednesday, until December 1, 2026");
        r.pattern = RecurrencePatternType::Daily;
        r.interval = 1;
        r.range_type = Some(RecurrenceRangeType::Numbered);
        r.number_of_occurrences = Some(10);
        assert_eq!(recurrence_text(&r), "Every day, 10 times");
        r.pattern = RecurrencePatternType::AbsoluteMonthly;
        r.day_of_month = Some(15);
        r.range_type = Some(RecurrenceRangeType::NoEnd);
        assert_eq!(recurrence_text(&r), "Every month on day 15");
        r.pattern = RecurrencePatternType::AbsoluteYearly;
        assert_eq!(recurrence_text(&r), "Every year on January 5");
        r.pattern = RecurrencePatternType::Unknown("x".into());
        assert_eq!(recurrence_text(&r), "Repeats");
    }

    #[test]
    fn join_urls_must_be_https() {
        assert!(safe_join_url(Some("https://teams.example/l/meetup-join/1")).is_some());
        assert!(safe_join_url(Some("http://teams.example")).is_none());
        assert!(safe_join_url(Some("javascript:alert(1)")).is_none());
        assert!(safe_join_url(Some("https://a b")).is_none());
        assert!(safe_join_url(Some("https://")).is_none());
        assert!(safe_join_url(None).is_none());
    }
}
