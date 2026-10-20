//! Browser-preview backend for the calendar: synthetic layers, events around today, opened
//! people with free/busy, and the same query shapes as the engine (`calendar.view`,
//! `calendar.sources`, `calendar.event`, `calendar.free_busy`, `calendar.find_times`,
//! `calendar.rooms`, ...). Commands change the mock state and emit `store_changed`.
//! All names are made up; addresses are `example.com`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use quorum_ipc::{CoreEvent, Reply};
use serde_json::{Value, json};

use super::calendar_date::{Ymd, hhmm};

/// Queries answered here.
pub const QUERIES: &[&str] = &[
    "calendar.view",
    "calendar.sources",
    "calendar.events",
    "calendar.event",
    "calendar.upcoming",
    "calendar.free_busy",
    "calendar.find_times",
    "calendar.rooms",
    "calendar.room_lists",
];

const ME: (&str, &str) = ("Avery Example", "avery@example.com");

#[derive(Debug, Clone)]
struct MockEvent {
    id: String,
    subject: String,
    cal: &'static str,
    day: i64,
    start: u32,
    end_day: i64,
    end: u32,
    all_day: bool,
    location: String,
    organizer: (String, String),
    attendees: Vec<(String, String, &'static str)>,
    teams: bool,
    response: &'static str,
    recurring: bool,
    show_as: &'static str,
    preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Person {
    address: String,
    name: String,
    shared: bool,
}

#[derive(Debug, Default)]
struct State {
    view: String,
    anchor: Option<Ymd>,
    hidden: BTreeSet<String>,
    colors: BTreeMap<String, String>,
    people: Vec<Person>,
    created: Vec<MockEvent>,
    deleted: BTreeSet<String>,
    responses: BTreeMap<String, &'static str>,
    renamed: BTreeMap<String, String>,
    next: u32,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State { view: "week".into(), ..State::default() });
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().map(|mut s| f(&mut s)))
}

/// Today in the browser's zone (a fixed date off wasm, for tests).
pub fn today() -> Ymd {
    #[cfg(target_arch = "wasm32")]
    {
        let d = js_sys::Date::new_0();
        let (y, m, day) = (d.get_full_year(), d.get_month(), d.get_date());
        if let Some(t) = i32::try_from(y).ok().and_then(|y| Ymd::new(y, m + 1, day)) {
            return t;
        }
    }
    Ymd { y: 2026, m: 10, d: 17 }
}

fn now_min() -> u32 {
    #[cfg(target_arch = "wasm32")]
    {
        let d = js_sys::Date::new_0();
        d.get_hours() * 60 + d.get_minutes()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        10 * 60 + 25
    }
}

fn monday(d: Ymd) -> Ymd {
    d.add_days(-i64::from(d.weekday()))
}

fn person(name: &str) -> (String, String) {
    let addr = format!("{}@example.com", name.to_lowercase().replace(' ', "."));
    (name.to_owned(), addr)
}

#[expect(clippy::too_many_arguments, reason = "a flat synthetic-event builder")]
fn ev(
    id: &str,
    subject: &str,
    cal: &'static str,
    day: i64,
    start: u32,
    end: u32,
    location: &str,
    organizer: &str,
    teams: bool,
    response: &'static str,
) -> MockEvent {
    let org = if organizer.is_empty() { (ME.0.to_owned(), ME.1.to_owned()) } else { person(organizer) };
    let mine = organizer.is_empty();
    MockEvent {
        id: id.to_owned(),
        subject: subject.to_owned(),
        cal,
        day,
        start,
        end_day: day,
        end,
        all_day: false,
        location: location.to_owned(),
        attendees: vec![
            (ME.0.to_owned(), ME.1.to_owned(), if mine { "organizer" } else { response }),
            (person("Kai Novak").0, person("Kai Novak").1, "accepted"),
            (person("Riley Ito").0, person("Riley Ito").1, "tentativelyAccepted"),
            (person("Sage Moreau").0, person("Sage Moreau").1, "none"),
        ],
        organizer: org,
        teams,
        response: if mine { "organizer" } else { response },
        recurring: false,
        show_as: "busy",
        preview: "Agenda: status, open questions, next steps.".to_owned(),
    }
}

/// Events relative to the Monday of this week (day 0).
fn base_events() -> Vec<MockEvent> {
    let mut v = Vec::new();
    for w in -2..4 {
        let mut s =
            ev(&format!("m-standup-{w}"), "Team standup", "own", w * 7, 9 * 60, 9 * 60 + 15, "", "", true, "organizer");
        s.recurring = true;
        v.push(s);
    }
    v.extend([
        ev("m-plan", "Quarterly planning", "own", 1, 10 * 60, 11 * 60 + 30, "Room 4.01", "", false, "organizer"),
        ev("m-review", "Design review", "own", 1, 10 * 60 + 30, 11 * 60, "", "Kai Novak", true, "accepted"),
        ev("m-sync", "Roadmap sync", "own", 1, 10 * 60 + 45, 11 * 60 + 45, "Online meeting", "Riley Ito", true, "none"),
        ev("m-1on1", "1:1 with Kai", "own", 2, 14 * 60, 14 * 60 + 30, "", "Kai Novak", false, "accepted"),
        ev("m-lunch", "Team lunch", "own", 2, 12 * 60, 13 * 60, "Cafe", "", false, "organizer"),
        ev("m-retro", "Launch retro", "own", 3, 15 * 60, 16 * 60, "", "Sage Moreau", true, "tentativelyAccepted"),
        ev("m-hiring", "Hiring loop", "own", 4, 9 * 60 + 30, 11 * 60, "Room 2.12", "Riley Ito", false, "none"),
        ev("m-focus", "Focus time", "own", 3, 8 * 60, 10 * 60, "", "", false, "organizer"),
        ev("m-security", "Security review", "own", 4, 13 * 60, 14 * 60, "", "", true, "organizer"),
        ev("m-vendor", "Vendor contract", "own", 8, 11 * 60, 12 * 60, "", "Kai Novak", false, "declined"),
        ev("m-status", "Weekly status", "own", 9, 16 * 60, 16 * 60 + 30, "", "", true, "organizer"),
        ev(
            "t-feedback",
            "Customer feedback",
            "team",
            0,
            13 * 60,
            14 * 60,
            "Room 1.05",
            "Sage Moreau",
            false,
            "accepted",
        ),
        ev("t-budget", "Budget draft", "team", 3, 10 * 60, 11 * 60 + 30, "", "Sage Moreau", false, "accepted"),
        ev("g-launch", "Launch go/no-go", "group", 4, 16 * 60, 17 * 60, "", "Riley Ito", true, "none"),
    ]);
    let mut late = ev("m-release", "Release night", "own", 5, 20 * 60, 60, "", "", true, "organizer");
    late.end_day = 6;
    v.push(late);
    let mut offsite = ev("m-offsite", "Team offsite", "own", 7, 0, 1440, "Lakeside lodge", "", false, "organizer");
    offsite.all_day = true;
    offsite.end_day = 9;
    offsite.show_as = "oof";
    v.push(offsite);
    let mut bday = ev("b-sage", "Sage's birthday", "birthdays", 2, 0, 1440, "", "", false, "organizer");
    bday.all_day = true;
    bday.attendees.clear();
    bday.show_as = "free";
    v.push(bday);
    let mut ooo = ev("t-ooo", "Sage out", "team", 4, 0, 1440, "", "Sage Moreau", false, "accepted");
    ooo.all_day = true;
    ooo.end_day = 5;
    v.push(ooo);
    v
}

struct Src {
    id: String,
    name: String,
    kind: &'static str,
    group: &'static str,
    color: String,
    can_edit: bool,
    is_default: bool,
    person: Option<String>,
    mode: Option<&'static str>,
}

fn sources(s: &State) -> Vec<Src> {
    let color = |id: &str, def: &str| s.colors.get(id).cloned().unwrap_or_else(|| def.to_owned());
    let mut v = vec![
        Src {
            id: "own".into(),
            name: "Calendar".into(),
            kind: "own",
            group: "My calendars",
            color: color("own", "#3b82f6"),
            can_edit: true,
            is_default: true,
            person: None,
            mode: None,
        },
        Src {
            id: "birthdays".into(),
            name: "Birthdays".into(),
            kind: "own",
            group: "My calendars",
            color: color("birthdays", "#e46aa6"),
            can_edit: true,
            is_default: false,
            person: None,
            mode: None,
        },
        Src {
            id: "team".into(),
            name: "Sage's calendar".into(),
            kind: "shared",
            group: "Shared calendars",
            color: color("team", "#4caf7a"),
            can_edit: false,
            is_default: false,
            person: None,
            mode: None,
        },
        Src {
            id: "group".into(),
            name: "Launch team".into(),
            kind: "group",
            group: "Groups",
            color: color("group", "#8b5cf6"),
            can_edit: true,
            is_default: false,
            person: None,
            mode: None,
        },
    ];
    let palette = ["#f59e0b", "#14b8a6", "#ef4444", "#6366f1", "#a16207"];
    for (i, p) in s.people.iter().enumerate() {
        let id = format!("person:{}", p.address);
        let def = palette.get(i % palette.len()).copied().unwrap_or("#f59e0b");
        v.push(Src {
            color: color(&id, def),
            id,
            name: p.name.clone(),
            kind: "person",
            group: "People",
            can_edit: false,
            is_default: false,
            person: Some(p.address.clone()),
            mode: Some(if p.shared { "details" } else { "free_busy" }),
        });
    }
    v
}

fn source_json(s: &State, src: &Src) -> Value {
    json!({
        "id": src.id, "name": src.name, "kind": src.kind, "group": src.group, "color": src.color,
        "visible": !s.hidden.contains(&src.id), "can_edit": src.can_edit, "is_default": src.is_default,
        "owner": src.person.clone().unwrap_or_default(), "person": src.person, "mode": src.mode,
        "free_busy_only": src.mode == Some("free_busy"),
    })
}

/// Events of every layer, in days relative to this week's Monday.
fn all_events(s: &State) -> Vec<MockEvent> {
    let mut v = base_events();
    for p in s.people.iter().filter(|p| p.shared) {
        let cal: &'static str = "person";
        let mut a = ev(
            &format!("p-{}-1", p.address),
            &format!("{}: customer call", p.name),
            cal,
            1,
            14 * 60,
            15 * 60,
            "",
            &p.name,
            false,
            "accepted",
        );
        a.preview = p.address.clone();
        let mut b = ev(
            &format!("p-{}-2", p.address),
            &format!("{}: workshop", p.name),
            cal,
            3,
            9 * 60,
            11 * 60 + 30,
            "",
            &p.name,
            false,
            "accepted",
        );
        b.preview = p.address.clone();
        v.push(a);
        v.push(b);
    }
    v.extend(s.created.iter().cloned());
    v.retain(|e| !s.deleted.contains(&e.id));
    for e in &mut v {
        if let Some(r) = s.responses.get(&e.id) {
            e.response = r;
        }
        if let Some(n) = s.renamed.get(&e.id) {
            e.subject = n.clone();
        }
    }
    v
}

fn source_of(s: &State, e: &MockEvent) -> String {
    if e.cal == "person" {
        let addr = e.preview.clone();
        return format!("person:{addr}");
    }
    let _ = s;
    e.cal.to_owned()
}

fn view_days(view: &str, anchor: Ymd) -> (Ymd, i64) {
    match view {
        "day" => (anchor, 1),
        "work_week" => (monday(anchor), 5),
        "month" => (monday(Ymd { d: 1, ..anchor }), 42),
        "agenda" => (anchor, 14),
        _ => (monday(anchor), 7),
    }
}

fn title(view: &str, anchor: Ymd) -> String {
    let (first, n) = view_days(view, anchor);
    let last = first.add_days(n - 1);
    match view {
        "day" => format!("{}, {} {}, {}", full_weekday(anchor), anchor.month_name(), anchor.d, anchor.y),
        "month" => format!("{} {}", anchor.month_name(), anchor.y),
        _ if first.m == last.m => format!("{} {} – {}, {}", first.month_name(), first.d, last.d, last.y),
        _ => format!(
            "{} {} – {} {}, {}",
            first.month_name().get(..3).unwrap_or(""),
            first.d,
            last.month_name().get(..3).unwrap_or(""),
            last.d,
            last.y
        ),
    }
}

fn full_weekday(d: Ymd) -> &'static str {
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"]
        .get(usize::try_from(d.weekday()).unwrap_or(0))
        .copied()
        .unwrap_or("")
}

fn event_json(s: &State, e: &MockEvent, base: Ymd, first: Ymd, srcs: &[Src]) -> Value {
    let src_id = source_of(s, e);
    let src = srcs.iter().find(|x| x.id == src_id);
    let start_date = base.add_days(e.day);
    let end_date = base.add_days(e.end_day);
    let label = if e.all_day {
        "All day".to_owned()
    } else if e.day == e.end_day {
        format!("{} – {}", hhmm(e.start), hhmm(e.end))
    } else {
        format!("{} {} – {} {}", start_date.short(), hhmm(e.start), end_date.short(), hhmm(e.end))
    };
    json!({
        "id": e.id, "subject": e.subject, "location": e.location, "organizer": e.organizer.0,
        "source": src_id, "source_name": src.map(|x| x.name.clone()).unwrap_or_default(),
        "color": src.map(|x| x.color.clone()).unwrap_or_else(|| "#3b82f6".into()),
        "start": format!("{}T{}:00Z", start_date.iso(), hhmm(e.start)),
        "end": format!("{}T{}:00Z", end_date.iso(), hhmm(e.end)),
        "start_date": start_date.iso(), "end_date": end_date.iso(),
        "start_day": first.days_until(start_date), "end_day": first.days_until(end_date),
        "start_min": if e.all_day { 0 } else { e.start }, "end_min": if e.all_day { 1440 } else { e.end },
        "all_day": e.all_day, "time_label": label, "show_as": e.show_as, "response": e.response,
        "is_online": e.teams, "can_join": e.teams, "is_recurring": e.recurring, "is_cancelled": false,
        "is_organizer": e.response == "organizer", "pending": e.id.starts_with("local-"), "is_private": false,
    })
}

fn busy_for(address: &str) -> Vec<(i64, u32, u32, &'static str)> {
    let seed = address.bytes().map(u32::from).sum::<u32>();
    (0..12)
        .map(|i| {
            let day = i64::from((seed + i * 3) % 5);
            let start = 8 * 60 + ((seed + i * 97) % 16) * 30;
            let len = 30 + ((seed + i * 13) % 4) * 30;
            let status = match (seed + i) % 5 {
                0 => "tentative",
                1 => "oof",
                _ => "busy",
            };
            (day, start, (start + len).min(1440), status)
        })
        .collect()
}

fn view(s: &State, mini_offset: i32) -> Value {
    let today = today();
    let anchor = s.anchor.unwrap_or(today);
    let (first, n) = view_days(&s.view, anchor);
    let base = monday(today);
    let srcs = sources(s);
    let visible: BTreeSet<String> = srcs.iter().filter(|x| !s.hidden.contains(&x.id)).map(|x| x.id.clone()).collect();
    let mut events: Vec<(i64, u32, Value)> = all_events(s)
        .iter()
        .filter(|e| visible.contains(&source_of(s, e)))
        .filter_map(|e| {
            let sd = first.days_until(base.add_days(e.day));
            let ed = first.days_until(base.add_days(e.end_day));
            (ed >= 0 && sd < n)
                .then(|| (sd, if e.all_day { 0 } else { e.start + 1 }, event_json(s, e, base, first, &srcs)))
        })
        .collect();
    events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut busy = Vec::new();
    for src in srcs.iter().filter(|x| x.mode == Some("free_busy") && visible.contains(&x.id)) {
        let addr = src.person.clone().unwrap_or_default();
        for week in -2..4 {
            for (day, start, end, status) in busy_for(&addr) {
                let d = first.days_until(base.add_days(day + week * 7));
                if (0..n).contains(&d) {
                    busy.push(json!({
                        "source": src.id, "name": src.name, "color": src.color, "status": status,
                        "start_day": d, "end_day": d, "start_min": start, "end_min": end,
                        "time_label": format!("{} – {}", hhmm(start), hhmm(end)),
                    }));
                }
            }
        }
    }
    let days: Vec<Value> = (0..n)
        .map(|i| {
            let d = first.add_days(i);
            json!({
                "date": d.iso(), "day": d.d, "weekday": d.weekday_name(), "label": format!("{} {}", d.weekday_name(), d.d),
                "is_today": d == today, "in_month": d.m == anchor.m, "is_weekend": d.weekday() >= 5,
            })
        })
        .collect();
    json!({
        "view": s.view, "view_label": s.view, "date": anchor.iso(), "today": today.iso(), "now_min": now_min(),
        "time_zone": "Europe/Berlin", "title": title(&s.view, anchor), "days": days,
        "work_start": 480, "work_end": 1020, "work_days": [0, 1, 2, 3, 4],
        "events": events.into_iter().map(|e| e.2).collect::<Vec<_>>(), "busy": busy, "busy_error": null,
        "mini": mini(anchor, today, &s.view, mini_offset),
        "sources": srcs.iter().map(|x| source_json(s, x)).collect::<Vec<_>>(),
    })
}

fn mini(anchor: Ymd, today: Ymd, view: &str, offset: i32) -> Value {
    let months = i64::from(anchor.y) * 12 + i64::from(anchor.m) - 1 + i64::from(offset.clamp(-1200, 1200));
    let (y, m) =
        (i32::try_from(months.div_euclid(12)).unwrap_or(2026), u32::try_from(months.rem_euclid(12) + 1).unwrap_or(1));
    let month = Ymd::new(y, m, 1).unwrap_or(anchor);
    let (rf, rn) = view_days(view, anchor);
    let rl = rf.add_days(rn - 1);
    let first = monday(month);
    let weeks: Vec<Vec<Value>> = (0..6)
        .map(|w| {
            (0..7)
                .map(|i| {
                    let d = first.add_days(w * 7 + i);
                    json!({ "date": d.iso(), "day": d.d, "in_month": d.m == month.m, "is_today": d == today, "in_range": d >= rf && d <= rl })
                })
                .collect()
        })
        .collect();
    json!({ "title": format!("{} {}", month.month_name(), month.y), "weeks": weeks })
}

fn detail(s: &State, id: &str) -> Option<Value> {
    let today = today();
    let base = monday(today);
    let srcs = sources(s);
    let e = all_events(s).into_iter().find(|e| e.id == id)?;
    let src_id = source_of(s, &e);
    let src = srcs.iter().find(|x| x.id == src_id);
    let sd = base.add_days(e.day);
    let ed = base.add_days(e.end_day);
    let when = if e.all_day {
        if sd == ed {
            format!("{} · All day", sd.short())
        } else {
            format!("{} – {} · All day", sd.short(), ed.short())
        }
    } else if sd == ed {
        format!("{} · {} – {}", sd.short(), hhmm(e.start), hhmm(e.end))
    } else {
        format!("{} {} – {} {}", sd.short(), hhmm(e.start), ed.short(), hhmm(e.end))
    };
    let label = |r: &str| match r {
        "accepted" => "Accepted",
        "tentativelyAccepted" => "Tentative",
        "declined" => "Declined",
        "organizer" => "Organizer",
        _ => "No response",
    };
    let mine = e.response == "organizer";
    let can_edit = src.is_some_and(|x| x.can_edit) && (mine || e.attendees.is_empty());
    Some(json!({
        "id": e.id, "subject": e.subject, "when": when,
        "start_local": format!("{}T{}", sd.iso(), hhmm(e.start)),
        "end_local": if e.all_day { format!("{}T00:00", ed.add_days(1).iso()) } else { format!("{}T{}", ed.iso(), hhmm(e.end)) },
        "all_day": e.all_day, "time_zone": "Europe/Berlin", "location": e.location,
        "organizer": { "name": e.organizer.0, "address": e.organizer.1 },
        "attendees": e.attendees.iter().map(|(n, a, r)| json!({ "name": n, "address": a, "type": "required", "response": r, "response_label": label(r) })).collect::<Vec<_>>(),
        "response": e.response, "response_label": label(e.response), "is_organizer": mine,
        "can_respond": !mine && !e.attendees.is_empty(), "can_edit": can_edit, "can_cancel": mine && !e.attendees.is_empty(),
        "is_online": e.teams, "can_join": e.teams,
        "recurrence": if e.recurring { Some("Every week on Monday") } else { None }, "is_recurring": e.recurring,
        "preview": if e.cal == "person" { String::new() } else { e.preview.clone() }, "show_as": e.show_as, "sensitivity": "normal",
        "is_cancelled": false, "reminder_minutes": 15, "pending": e.id.starts_with("local-"),
        "source": src.map(|x| json!({ "id": x.id, "name": x.name, "color": x.color })),
    }))
}

fn free_busy(params: &Value) -> Value {
    let users: Vec<String> = params
        .get("users")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).take(100).map(str::to_owned).collect())
        .unwrap_or_default();
    let interval = params.get("interval").and_then(Value::as_u64).unwrap_or(30).clamp(5, 1440);
    let from = params.get("from").and_then(Value::as_str).and_then(Ymd::parse).unwrap_or_else(today);
    let to = params.get("to").and_then(Value::as_str).and_then(Ymd::parse).unwrap_or_else(|| from.add_days(1));
    let slots = u64::try_from(from.days_until(to).clamp(1, 14)).unwrap_or(1) * 1440 / interval;
    let base = monday(today());
    let rows: Vec<Value> = users
        .iter()
        .map(|u| {
            let blocks = busy_for(u);
            let availability: Vec<&str> = (0..slots)
                .map(|i| {
                    let minute = i * interval;
                    let day = from.add_days(i64::try_from(minute / 1440).unwrap_or(0));
                    let m = u32::try_from(minute % 1440).unwrap_or(0);
                    let rel = base.days_until(day).rem_euclid(7);
                    blocks.iter().find(|(d, s, e, _)| *d == rel && m >= *s && m < *e).map_or("free", |b| b.3)
                })
                .collect();
            json!({ "user": u, "availability": availability, "items": [], "error": null })
        })
        .collect();
    json!({ "from": format!("{}T00:00:00Z", from.iso()), "to": format!("{}T00:00:00Z", to.iso()), "interval": interval, "schedules": rows })
}

fn find_times(params: &Value) -> Value {
    let from = params
        .get("window")
        .and_then(|w| w.get("from"))
        .and_then(Value::as_str)
        .and_then(Ymd::parse)
        .unwrap_or_else(today);
    let dur = u32::try_from(params.get("duration").and_then(Value::as_u64).unwrap_or(30).clamp(5, 480)).unwrap_or(30);
    let slots = [(0, 9 * 60 + 30), (0, 14 * 60), (1, 11 * 60), (1, 15 * 60 + 30), (2, 10 * 60)];
    let suggestions: Vec<Value> = slots
        .iter()
        .enumerate()
        .map(|(i, (d, start))| {
            let day = from.add_days(*d);
            let end = start + dur;
            json!({
                "start": format!("{}T{}:00Z", day.iso(), hhmm(*start)), "end": format!("{}T{}:00Z", day.iso(), hhmm(end)),
                "date": day.iso(), "start_min": start, "end_min": end,
                "label": format!("{} · {} – {}", day.short(), hhmm(*start), hhmm(end)),
                "confidence": 100.0 - 10.0 * f64::from(u32::try_from(i).unwrap_or(0)),
                "organizer": "free", "attendees": [], "locations": if i == 0 { vec!["Room 4.01"] } else { vec![] },
                "reason": if i == 0 { "Everyone is available" } else { "Most attendees are available" },
            })
        })
        .collect();
    json!({ "suggestions": suggestions, "empty_reason": null })
}

fn rooms() -> Value {
    json!([
        { "id": "r1", "name": "Room 4.01", "address": "room401@example.com", "capacity": 8, "building": "North", "floor": 4, "accessible": true },
        { "id": "r2", "name": "Room 2.12", "address": "room212@example.com", "capacity": 4, "building": "North", "floor": 2, "accessible": false },
        { "id": "r3", "name": "Boardroom", "address": "boardroom@example.com", "capacity": 16, "building": "South", "floor": 1, "accessible": true },
    ])
}

/// Answers a calendar query.
pub fn query(id: &str, params: &Value) -> Reply {
    let offset = params.get("mini_offset").and_then(Value::as_i64).and_then(|o| i32::try_from(o).ok()).unwrap_or(0);
    let v = match id {
        "calendar.view" => with(|s| view(s, offset)),
        "calendar.sources" => with(|s| Value::Array(sources(s).iter().map(|x| source_json(s, x)).collect())),
        "calendar.events" => with(|s| view(s, 0).get("events").cloned().unwrap_or(Value::Null)),
        "calendar.upcoming" => Some(json!([])),
        "calendar.event" => {
            let id = params.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            match with(|s| detail(s, &id)).flatten() {
                Some(v) => Some(v),
                None => return Reply::Err { kind: "not_found".into(), message: "no such event".into() },
            }
        }
        "calendar.free_busy" => Some(free_busy(params)),
        "calendar.find_times" => Some(find_times(params)),
        "calendar.rooms" => Some(rooms()),
        "calendar.room_lists" => {
            Some(json!([{ "id": "l1", "name": "North building", "address": "north@example.com" }]))
        }
        _ => None,
    };
    match v {
        Some(value) => Reply::Ok { value },
        None => Reply::Err { kind: "unknown_command".into(), message: format!("no query `{id}`") },
    }
}

fn changed() -> CoreEvent {
    CoreEvent::StoreChanged { tables: vec!["event".into(), "calendar".into(), "setting".into(), "ui_prefs".into()] }
}

fn ok(v: Value, toast: Option<&str>) -> (Reply, Vec<CoreEvent>) {
    let mut events = vec![changed()];
    if let Some(t) = toast {
        events.push(CoreEvent::Toast { level: "success".into(), message: t.into() });
    }
    (Reply::Ok { value: v }, events)
}

fn bad(msg: &str) -> (Reply, Vec<CoreEvent>) {
    (Reply::Err { kind: "invalid_params".into(), message: msg.into() }, vec![])
}

fn text(p: &Value, k: &str) -> Option<String> {
    p.get(k).and_then(Value::as_str).map(|s| s.chars().filter(|c| !c.is_control()).take(512).collect())
}

/// Runs a calendar command in the preview; `None` for ids it does not know.
pub fn run(id: &str, p: &Value) -> Option<(Reply, Vec<CoreEvent>)> {
    let target = text(p, "id");
    let r = match id {
        "calendar.set_view"
        | "calendar.view.day"
        | "calendar.view.work_week"
        | "calendar.view.week"
        | "calendar.view.month"
        | "calendar.view.agenda" => {
            let v =
                id.strip_prefix("calendar.view.").map(str::to_owned).or_else(|| text(p, "view")).unwrap_or_default();
            if !["day", "work_week", "week", "month", "agenda"].contains(&v.as_str()) {
                return Some(bad("view must be day, work_week, week, month or agenda"));
            }
            with(|s| s.view = v);
            ok(Value::Null, None)
        }
        "calendar.go" => {
            let to = text(p, "to").or_else(|| text(p, "date")).unwrap_or_else(|| "today".into());
            let res = with(|s| {
                let anchor = s.anchor.unwrap_or_else(today);
                let step = |dir: i64| match s.view.as_str() {
                    "day" => anchor.add_days(dir),
                    "month" => {
                        let m = i64::from(anchor.y) * 12 + i64::from(anchor.m) - 1 + dir;
                        Ymd::new(
                            i32::try_from(m.div_euclid(12)).unwrap_or(2026),
                            u32::try_from(m.rem_euclid(12) + 1).unwrap_or(1),
                            1,
                        )
                        .unwrap_or(anchor)
                    }
                    "agenda" => anchor.add_days(14 * dir),
                    _ => anchor.add_days(7 * dir),
                };
                s.anchor = match to.as_str() {
                    "today" => None,
                    "next" => Some(step(1)),
                    "previous" => Some(step(-1)),
                    other => match Ymd::parse(other) {
                        Some(d) => Some(d),
                        None => return false,
                    },
                };
                true
            });
            if res != Some(true) {
                return Some(bad("`to` must be today, next, previous or YYYY-MM-DD"));
            }
            ok(Value::Null, None)
        }
        "calendar.toggle_source" => {
            let Some(t) = target else { return Some(bad("`id` is required")) };
            let visible = p.get("visible").and_then(Value::as_bool);
            with(|s| {
                let show = visible.unwrap_or_else(|| s.hidden.contains(&t));
                if show {
                    s.hidden.remove(&t);
                } else {
                    s.hidden.insert(t.clone());
                }
                if let Some(c) = text(p, "color") {
                    s.colors.insert(t, c);
                }
            });
            ok(Value::Null, None)
        }
        "calendar.open_person" => {
            let Some(user) =
                text(p, "user").map(|u| u.trim().to_lowercase()).filter(|u| u.contains('@') && u.len() > 3)
            else {
                return Some(bad("`user` must be an email address"));
            };
            let name = text(p, "name").filter(|n| !n.is_empty()).unwrap_or_else(|| {
                user.split('@')
                    .next()
                    .unwrap_or("")
                    .split('.')
                    .map(|w| {
                        let mut c = w.chars();
                        c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            // In the preview, people whose name starts with A–M share their calendar.
            let shared = name.chars().next().is_some_and(|c| c.to_ascii_uppercase() <= 'M');
            with(|s| {
                if !s.people.iter().any(|x| x.address == user) {
                    s.people.push(Person { address: user.clone(), name, shared });
                }
            });
            ok(json!({ "source": format!("person:{user}") }), None)
        }
        "calendar.close_person" => {
            let user = text(p, "user").unwrap_or_default().trim_start_matches("person:").to_lowercase();
            with(|s| s.people.retain(|x| x.address != user));
            ok(Value::Null, None)
        }
        "calendar.new_event" => {
            let subject = text(p, "subject").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "(No subject)".into());
            let start = text(p, "start").unwrap_or_default();
            let end = text(p, "end").unwrap_or_default();
            let Some(sd) = Ymd::parse(&start) else { return Some(bad("`start` is required")) };
            let all_day = p.get("all_day").and_then(Value::as_bool).unwrap_or(start.len() <= 10);
            let sm = start.get(11..16).and_then(super::calendar_date::parse_hhmm).unwrap_or(9 * 60);
            let ed = Ymd::parse(&end).unwrap_or(sd);
            let em = end.get(11..16).and_then(super::calendar_date::parse_hhmm).unwrap_or(sm + 30);
            let base = monday(today());
            let n = with(|s| {
                s.next = s.next.wrapping_add(1);
                s.next
            })
            .unwrap_or(0);
            let cal: &'static str = match text(p, "calendar").as_deref() {
                Some("birthdays") => "birthdays",
                Some("group") => "group",
                _ => "own",
            };
            let mut e = ev(
                &format!("local-{n}"),
                &subject,
                cal,
                base.days_until(sd),
                sm,
                em,
                &text(p, "location").unwrap_or_default(),
                "",
                p.get("online_meeting").and_then(Value::as_bool).unwrap_or(false),
                "organizer",
            );
            e.end_day = base.days_until(ed);
            e.all_day = all_day;
            e.recurring = p.get("recurrence").is_some_and(|r| r.as_str() != Some("none") && !r.is_null());
            e.attendees = p
                .get("attendees")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(|x| (x.to_owned(), x.to_owned(), "none")).collect())
                .unwrap_or_default();
            if let Some(b) = text(p, "body") {
                e.preview = b;
            }
            let id = e.id.clone();
            with(|s| s.created.push(e));
            ok(json!({ "id": id, "pending": true }), Some("Event saved"))
        }
        "calendar.update_event" => {
            let Some(t) = target else { return Some(bad("`id` is required")) };
            if let Some(subject) = text(p, "subject") {
                with(|s| s.renamed.insert(t.clone(), subject));
            }
            ok(json!({ "id": t }), Some("Event updated"))
        }
        "calendar.delete_event" | "calendar.cancel" => {
            let Some(t) = target else { return Some(bad("Select an event first.")) };
            with(|s| s.deleted.insert(t));
            ok(Value::Null, Some(if id == "calendar.cancel" { "Meeting cancelled" } else { "Event deleted" }))
        }
        "calendar.respond" => {
            let Some(t) = target else { return Some(bad("Select an event first.")) };
            let r: &'static str = match text(p, "response").as_deref() {
                Some("accept") => "accepted",
                Some("tentative") => "tentativelyAccepted",
                Some("decline") => "declined",
                _ => return Some(bad("response must be accept, tentative or decline")),
            };
            with(|s| s.responses.insert(t, r));
            ok(json!({ "response": r }), Some("Response sent"))
        }
        "calendar.forward" => ok(Value::Null, Some("Invitation forwarded")),
        "calendar.snooze" | "calendar.dismiss" => ok(Value::Null, None),
        "calendar.join" => {
            let Some(t) = target else { return Some(bad("Select an event first.")) };
            let url = format!("https://teams.example.com/l/meetup-join/{t}");
            (
                Reply::Ok { value: json!({ "url": url }) },
                vec![CoreEvent::Ui { command: "calendar.join".into(), params: json!({ "url": url, "id": t }) }],
            )
        }
        "calendar.compose" => (
            Reply::Ok { value: Value::Null },
            vec![CoreEvent::Ui { command: "calendar.compose".into(), params: p.clone() }],
        ),
        _ => return None,
    };
    Some(r)
}

/// The calendar commands the preview registry lists (id, label, menu, shortcut, destructive).
pub const COMMANDS: &[(&str, &str, Option<&str>, Option<&str>, bool)] = &[
    ("calendar.new_event", "New Event", Some("Calendar/New Event"), Some("Cmd+N"), false),
    ("calendar.update_event", "Edit Event", Some("Calendar/Edit Event"), None, false),
    ("calendar.delete_event", "Delete Event", Some("Calendar/Delete Event"), Some("Cmd+Backspace"), true),
    ("calendar.respond", "Respond to Invitation", Some("Calendar/Respond"), None, false),
    ("calendar.cancel", "Cancel Meeting", Some("Calendar/Cancel Meeting"), None, true),
    ("calendar.forward", "Forward Invitation", Some("Calendar/Forward"), None, true),
    ("calendar.snooze", "Snooze Reminder", Some("Calendar/Snooze Reminder"), None, false),
    ("calendar.dismiss", "Dismiss Reminder", Some("Calendar/Dismiss Reminder"), None, false),
    ("calendar.open_person", "Open Someone's Calendar", Some("Calendar/Open Calendar…"), None, false),
    ("calendar.close_person", "Close Person's Calendar", None, None, false),
    ("calendar.toggle_source", "Show or Hide Calendar", None, None, false),
    ("calendar.set_view", "Calendar View", None, None, false),
    ("calendar.view.day", "Day View", Some("View/Calendar/Day"), Some("Cmd+Alt+1"), false),
    ("calendar.view.work_week", "Work Week View", Some("View/Calendar/Work Week"), Some("Cmd+Alt+2"), false),
    ("calendar.view.week", "Week View", Some("View/Calendar/Week"), Some("Cmd+Alt+3"), false),
    ("calendar.view.month", "Month View", Some("View/Calendar/Month"), Some("Cmd+Alt+4"), false),
    ("calendar.view.agenda", "Agenda View", Some("View/Calendar/Agenda"), Some("Cmd+Alt+5"), false),
    ("calendar.go", "Go to Date", Some("Calendar/Go to Date"), None, false),
    ("calendar.join", "Join Online Meeting", Some("Calendar/Join"), Some("Cmd+Shift+J"), false),
    ("calendar.compose", "New Event With…", None, None, false),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn value(r: Reply) -> Value {
        match r {
            Reply::Ok { value } => value,
            Reply::Err { message, .. } => panic!("{message}"),
        }
    }

    #[test]
    fn view_has_the_engine_shape() {
        let v = value(query("calendar.view", &Value::Null));
        for k in
            ["view", "date", "today", "now_min", "title", "days", "events", "busy", "mini", "sources", "work_start"]
        {
            assert!(v.get(k).is_some(), "{k}");
        }
        let events = v["events"].as_array().unwrap();
        assert!(events.len() > 5);
        for e in events {
            for k in [
                "id",
                "subject",
                "color",
                "start_day",
                "end_day",
                "start_min",
                "end_min",
                "all_day",
                "time_label",
                "source",
            ] {
                assert!(e.get(k).is_some(), "{k}");
            }
        }
        assert!(matches!(query("calendar.event", &json!({ "id": "m-plan" })), Reply::Ok { .. }));
        assert!(matches!(query("calendar.event", &json!({ "id": "nope" })), Reply::Err { .. }));
    }

    #[test]
    fn commands_change_the_view() {
        run("calendar.set_view", &json!({ "view": "month" })).unwrap();
        assert_eq!(value(query("calendar.view", &Value::Null))["days"].as_array().map(Vec::len), Some(42));
        run("calendar.go", &json!({ "to": "next" })).unwrap();
        run("calendar.go", &json!({ "to": "today" })).unwrap();
        assert!(matches!(run("calendar.go", &json!({ "to": "someday" })).unwrap().0, Reply::Err { .. }));
        run("calendar.open_person", &json!({ "user": "riley.ito@example.com" })).unwrap();
        let v = value(query("calendar.view", &Value::Null));
        assert!(v["sources"].as_array().unwrap().iter().any(|s| s["id"] == "person:riley.ito@example.com"));
        assert!(!v["busy"].as_array().unwrap().is_empty());
        let (r, _) =
            run("calendar.new_event", &json!({ "subject": "X", "start": format!("{}T10:00", today().iso()) })).unwrap();
        assert!(matches!(r, Reply::Ok { .. }));
        assert!(run("calendar.nope", &Value::Null).is_none());
        run("calendar.set_view", &json!({ "view": "week" })).unwrap();
    }
}
