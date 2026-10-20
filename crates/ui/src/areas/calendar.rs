//! Calendar: one merged calendar. Navigation column (mini month, layers with colour toggles,
//! "Add person…") | toolbar and day/work week/week/month/agenda view | side panel (event
//! details with RSVP and Join, or the new-event form with the scheduling assistant).
//!
//! Everything the calendar shows comes from one query, `calendar.view`; navigation and layer
//! toggles are engine commands (`calendar.go`, `calendar.set_view`, `calendar.toggle_source`),
//! so the keyboard, the palette and MCP drive the same view. Keys inside the calendar: `T`
//! today, `←`/`→` previous/next, `J`/`K` (or `↓`/`↑`) next/previous event, `1`–`5` views, `N` new
//! event, `Enter` open, `Esc` close the panel.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use serde_json::{Value, json};

use super::calendar_date::{Ymd, hhmm};
use super::calendar_grid::{AgendaView, MonthView, TimeGrid};
use super::calendar_icons::{CalIcon, KIcon};
use super::calendar_side::{EventDetail, EventForm};
use crate::state::{AppState, Area, ListNav, Query, ToastLevel, app_state, use_query};

/// Store tables the calendar view depends on.
pub const TABLES: &[&str] = &["event", "calendar", "setting", "ui_prefs", "outbox"];

/// One day of the view (`calendar.view` → `days`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Day {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// Day of month.
    #[serde(default)]
    pub day: i32,
    /// `Mon`.
    #[serde(default)]
    pub weekday: String,
    /// `Mon 12`.
    #[serde(default)]
    pub label: String,
    /// Today.
    #[serde(default)]
    pub is_today: bool,
    /// In the anchor month.
    #[serde(default)]
    pub in_month: bool,
    /// Weekend.
    #[serde(default)]
    pub is_weekend: bool,
}

/// A placed event (`calendar.view` → `events`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Ev {
    /// Id.
    pub id: String,
    /// Subject.
    #[serde(default)]
    pub subject: String,
    /// Location.
    #[serde(default)]
    pub location: String,
    /// Organizer.
    #[serde(default)]
    pub organizer: String,
    /// Layer id.
    #[serde(default)]
    pub source: String,
    /// Layer name.
    #[serde(default)]
    pub source_name: String,
    /// `#rrggbb`.
    #[serde(default)]
    pub color: String,
    /// First day index.
    #[serde(default)]
    pub start_day: i64,
    /// Last day index.
    #[serde(default)]
    pub end_day: i64,
    /// Start minute.
    #[serde(default)]
    pub start_min: u32,
    /// End minute.
    #[serde(default)]
    pub end_min: u32,
    /// In the all-day row.
    #[serde(default)]
    pub all_day: bool,
    /// `09:00 – 09:30`.
    #[serde(default)]
    pub time_label: String,
    /// `busy`, `free`, ...
    #[serde(default)]
    pub show_as: String,
    /// The user's response.
    #[serde(default)]
    pub response: String,
    /// Online meeting.
    #[serde(default)]
    pub is_online: bool,
    /// Joinable.
    #[serde(default)]
    pub can_join: bool,
    /// Recurring.
    #[serde(default)]
    pub is_recurring: bool,
    /// Cancelled.
    #[serde(default)]
    pub is_cancelled: bool,
    /// Organized by the user.
    #[serde(default)]
    pub is_organizer: bool,
    /// Not yet on the server.
    #[serde(default)]
    pub pending: bool,
    /// Private.
    #[serde(default)]
    pub is_private: bool,
}

impl Ev {
    /// CSS modifier classes for response and status.
    pub fn status_class(&self) -> String {
        let mut c = String::from("cal-ev");
        match self.response.as_str() {
            "tentativelyAccepted" => c.push_str(" is-tentative"),
            "declined" => c.push_str(" is-declined"),
            "none" | "notResponded" => c.push_str(" is-unanswered"),
            _ => {}
        }
        if self.show_as == "free" {
            c.push_str(" is-free");
        }
        if self.show_as == "oof" {
            c.push_str(" is-oof");
        }
        if self.is_cancelled {
            c.push_str(" is-cancelled");
        }
        if self.pending {
            c.push_str(" is-pending");
        }
        c
    }

    /// Accessible description.
    pub fn aria(&self) -> String {
        let mut s = format!("{}, {}", self.subject, self.time_label);
        if !self.location.is_empty() {
            s.push_str(&format!(", {}", self.location));
        }
        if !self.source_name.is_empty() {
            s.push_str(&format!(", {}", self.source_name));
        }
        s
    }
}

/// A free/busy block of an opened person (`calendar.view` → `busy`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Busy {
    /// Layer id.
    #[serde(default)]
    pub source: String,
    /// Person.
    #[serde(default)]
    pub name: String,
    /// `#rrggbb`.
    #[serde(default)]
    pub color: String,
    /// `busy`, `tentative`, `oof`, ...
    #[serde(default)]
    pub status: String,
    /// Start day.
    #[serde(default)]
    pub start_day: i64,
    /// End day.
    #[serde(default)]
    pub end_day: i64,
    /// Start minute.
    #[serde(default)]
    pub start_min: u32,
    /// End minute.
    #[serde(default)]
    pub end_min: u32,
    /// `09:00 – 10:00`.
    #[serde(default)]
    pub time_label: String,
}

/// A mini-month day.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct MiniDay {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// Day of month.
    #[serde(default)]
    pub day: i32,
    /// In the shown month.
    #[serde(default)]
    pub in_month: bool,
    /// Today.
    #[serde(default)]
    pub is_today: bool,
    /// In the main view's range.
    #[serde(default)]
    pub in_range: bool,
}

/// The mini month.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Mini {
    /// `October 2026`.
    #[serde(default)]
    pub title: String,
    /// Six weeks.
    #[serde(default)]
    pub weeks: Vec<Vec<MiniDay>>,
}

/// A layer (`calendar.view` → `sources`, `calendar.sources`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Source {
    /// Id.
    pub id: String,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// `own`, `shared`, `group`, `person`.
    #[serde(default)]
    pub kind: String,
    /// Heading.
    #[serde(default)]
    pub group: String,
    /// `#rrggbb`.
    #[serde(default)]
    pub color: String,
    /// Shown.
    #[serde(default)]
    pub visible: bool,
    /// Writable.
    #[serde(default)]
    pub can_edit: bool,
    /// Default calendar.
    #[serde(default)]
    pub is_default: bool,
    /// The person's address (people layers).
    #[serde(default)]
    pub person: Option<String>,
    /// `pending`, `details`, `free_busy`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Free/busy only.
    #[serde(default)]
    pub free_busy_only: bool,
}

/// Everything the calendar shows (`calendar.view`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ViewModel {
    /// `day`, `work_week`, `week`, `month`, `agenda`.
    #[serde(default)]
    pub view: String,
    /// Anchor date.
    #[serde(default)]
    pub date: String,
    /// Today.
    #[serde(default)]
    pub today: String,
    /// Minutes after midnight now.
    #[serde(default)]
    pub now_min: i64,
    /// Display zone.
    #[serde(default)]
    pub time_zone: String,
    /// Toolbar title.
    #[serde(default)]
    pub title: String,
    /// Days.
    #[serde(default)]
    pub days: Vec<Day>,
    /// Working day start.
    #[serde(default)]
    pub work_start: u32,
    /// Working day end.
    #[serde(default)]
    pub work_end: u32,
    /// Events.
    #[serde(default)]
    pub events: Vec<Ev>,
    /// Free/busy blocks.
    #[serde(default)]
    pub busy: Vec<Busy>,
    /// Why free/busy is missing.
    #[serde(default)]
    pub busy_error: Option<String>,
    /// Mini month.
    #[serde(default)]
    pub mini: Mini,
    /// Layers.
    #[serde(default)]
    pub sources: Vec<Source>,
}

/// A new or edited event in the form.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    /// Event being edited (`None`: new).
    pub editing: Option<String>,
    /// Subject.
    pub subject: String,
    /// Start date.
    pub date: String,
    /// Start time `HH:MM`.
    pub start: String,
    /// End date.
    pub end_date: String,
    /// End time.
    pub end: String,
    /// All day.
    pub all_day: bool,
    /// Attendee addresses.
    pub attendees: Vec<String>,
    /// Location text.
    pub location: String,
    /// Room (address, name).
    pub room: Option<(String, String)>,
    /// Teams meeting.
    pub teams: bool,
    /// Description.
    pub body: String,
    /// Target calendar.
    pub calendar: Option<String>,
    /// `none`, `daily`, `weekly`, `monthly`, `yearly`.
    pub repeat: String,
}

impl Draft {
    /// A new event at `date` `start_min` lasting `len` minutes.
    pub fn at(date: Ymd, start_min: u32, len: u32) -> Self {
        let end = start_min.saturating_add(len.max(15));
        let end_date = if end >= 1440 { date.add_days(1) } else { date };
        Self {
            date: date.iso(),
            start: hhmm(start_min),
            end_date: end_date.iso(),
            end: hhmm(end),
            repeat: "none".into(),
            ..Self::default()
        }
    }

    /// The `calendar.new_event` / `calendar.update_event` params (pure, for tests).
    pub fn params(&self) -> Value {
        let mut attendees: Vec<Value> = self.attendees.iter().map(|a| Value::String(a.clone())).collect();
        if let Some((addr, _)) = &self.room {
            attendees.push(json!({ "address": addr, "type": "resource" }));
        }
        let (start, end) = if self.all_day {
            (self.date.clone(), if self.end_date.is_empty() { self.date.clone() } else { self.end_date.clone() })
        } else {
            (format!("{}T{}", self.date, self.start), format!("{}T{}", self.end_date_or_start(), self.end))
        };
        let location = match (&self.room, self.location.trim()) {
            (Some((addr, name)), "") => json!({ "name": name, "address": addr }),
            (_, loc) => json!(loc),
        };
        let mut p = json!({
            "subject": self.subject.trim(),
            "start": start,
            "end": end,
            "all_day": self.all_day,
            "attendees": attendees,
            "location": location,
            "online_meeting": self.teams,
            "body": self.body,
        });
        if let Some(obj) = p.as_object_mut() {
            if self.repeat != "none" && !self.repeat.is_empty() {
                obj.insert("recurrence".into(), json!(self.repeat));
            }
            match &self.editing {
                Some(id) => {
                    obj.insert("id".into(), json!(id));
                }
                None => {
                    if let Some(c) = &self.calendar {
                        obj.insert("calendar".into(), json!(c));
                    }
                }
            }
        }
        p
    }

    fn end_date_or_start(&self) -> String {
        if self.end_date.is_empty() { self.date.clone() } else { self.end_date.clone() }
    }
}

/// Calendar UI state shared with the command handlers (`calendar.compose` can arrive while
/// another area is on screen).
#[derive(Debug, Clone, Copy)]
pub struct CalendarUi {
    /// Mini month offset from the anchor month.
    pub mini_offset: RwSignal<i32>,
    /// The open form, if any.
    pub form: RwSignal<Option<Draft>>,
    /// Bumped on every open so the form re-initialises.
    pub form_seq: RwSignal<u64>,
}

thread_local! {
    static UI: std::cell::RefCell<Option<(CalendarUi, Owner)>> = const { std::cell::RefCell::new(None) };
}

/// The calendar UI state (created on first use in its own root owner, so it outlives areas).
pub fn ui() -> CalendarUi {
    if let Some(u) = UI.with(|u| u.try_borrow().ok().and_then(|u| u.as_ref().map(|(ui, _)| *ui))) {
        return u;
    }
    // `new_root` makes itself the current owner; put the caller's owner back afterwards so the
    // component that asked keeps its context (AppState etc.).
    let previous = Owner::current();
    let owner = Owner::new_root(None);
    if let Some(p) = previous {
        p.set();
    }
    let ui = owner.with(|| CalendarUi {
        mini_offset: RwSignal::new(0),
        form: RwSignal::new(None),
        form_seq: RwSignal::new(0),
    });
    UI.with(|u| {
        if let Ok(mut slot) = u.try_borrow_mut() {
            *slot = Some((ui, owner));
        }
    });
    ui
}

/// Opens the new-event form (switching to the calendar).
pub fn open_form(state: AppState, draft: Draft) {
    let ui = ui();
    ui.form.set(Some(draft));
    ui.form_seq.update(|n| *n = n.wrapping_add(1));
    if state.area.get_untracked() != Area::Calendar {
        state.area.set(Area::Calendar);
    }
    request_animation_frame(|| {
        let _ = crate::dom::focus_selector("#cal-form-subject");
    });
}

/// UI-side command ids handled (or pre-empted) here.
pub const UI_IDS: &[&str] = &["calendar.new_event", "calendar.compose", "calendar.join"];

fn default_draft() -> Draft {
    let today = super::calendar_mock::today();
    // Next full hour today (local).
    let now = now_minutes();
    let start = ((now / 60) + 1).min(23) * 60;
    Draft::at(today, start, 30)
}

fn now_minutes() -> u32 {
    #[cfg(target_arch = "wasm32")]
    {
        let d = js_sys::Date::new_0();
        d.get_hours() * 60 + d.get_minutes()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        9 * 60
    }
}

fn prefill(params: &Value) -> Draft {
    let mut d = default_draft();
    let text = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .map(|s| s.chars().filter(|c| !c.is_control()).take(1024).collect::<String>())
    };
    if let Some(s) = text("subject") {
        d.subject = s;
    }
    if let Some(a) = params.get("attendees").and_then(Value::as_array) {
        d.attendees = a
            .iter()
            .filter_map(|v| v.as_str().or_else(|| v.get("address").and_then(Value::as_str)))
            .filter(|a| a.contains('@') && a.len() <= 320)
            .take(100)
            .map(str::to_owned)
            .collect();
    }
    if let Some(start) = text("start")
        && let Some(date) = Ymd::parse(&start)
    {
        d.date = date.iso();
        d.end_date = date.iso();
        if let Some(t) = start.get(11..16).and_then(super::calendar_date::parse_hhmm) {
            d.start = hhmm(t);
            d.end = hhmm(t.saturating_add(30));
        }
    }
    if let Some(end) = text("end")
        && let Some(date) = Ymd::parse(&end)
    {
        d.end_date = date.iso();
        if let Some(t) = end.get(11..16).and_then(super::calendar_date::parse_hhmm) {
            d.end = hhmm(t);
        }
    }
    d
}

/// Handles calendar commands that need the UI first; returns `false` to let the engine run
/// them (`calendar.new_event` with explicit times saves directly; `calendar.join` without a
/// URL asks the engine for the link).
pub fn run_ui(state: AppState, id: &str, params: &Value) -> bool {
    match id {
        "calendar.new_event" => {
            let explicit = params.get("start").is_some() || params.get("subject").is_some();
            if explicit {
                return false;
            }
            open_form(state, prefill(params));
            true
        }
        "calendar.compose" => {
            open_form(state, prefill(params));
            true
        }
        "calendar.join" => {
            let Some(url) = params.get("url").and_then(Value::as_str) else { return false };
            open_url(state, url);
            true
        }
        _ => false,
    }
}

/// Opens a meeting link in the system browser (desktop) or a new tab (preview); https only.
fn open_url(state: AppState, url: &str) {
    let url = url.trim();
    let ok = url.len() <= 4096
        && url.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        && !url.chars().any(|c| c.is_control() || c.is_whitespace());
    if !ok {
        state.toast(ToastLevel::Warning, "That meeting link can't be opened.");
        return;
    }
    if state.preview {
        if let Some(w) = web_sys::window() {
            let _ = w.open_with_url_and_target(url, "_blank");
        }
        return;
    }
    let url = url.to_owned();
    spawn_local(async move {
        if let quorum_ipc::Reply::Err { message, .. } = crate::ipc::run("app.open_url", json!({ "url": url })).await {
            state.toast(ToastLevel::Error, message);
        }
    });
}

fn go(state: AppState, to: &str) {
    crate::commands::dispatch(state, "calendar.go", json!({ "to": to }));
}

fn set_view(state: AppState, view: &str) {
    crate::commands::dispatch(state, "calendar.set_view", json!({ "view": view }));
}

thread_local! {
    static REPORT_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Reports the calendar's state to the engine (`ui.report`), debounced.
fn report(payload: Value) {
    let generation = REPORT_GEN.with(|g| {
        let n = g.get().wrapping_add(1);
        g.set(n);
        n
    });
    set_timeout(
        move || {
            if REPORT_GEN.with(std::cell::Cell::get) != generation {
                return;
            }
            spawn_local(async move {
                let _ = crate::ipc::run("ui.report", payload).await;
            });
        },
        Duration::from_millis(300),
    );
}

/// Events in keyboard order: by day, all-day first, then start time.
pub fn keyboard_order(events: &[Ev]) -> Vec<String> {
    let mut v: Vec<&Ev> = events.iter().collect();
    v.sort_by(|a, b| {
        a.start_day
            .max(0)
            .cmp(&b.start_day.max(0))
            .then(b.all_day.cmp(&a.all_day))
            .then(a.start_min.cmp(&b.start_min))
            .then(a.id.cmp(&b.id))
    });
    v.into_iter().map(|e| e.id.clone()).collect()
}

/// The id after (`step > 0`) or before `current` in `order`, wrapping at neither end.
pub fn step_selection(order: &[String], current: Option<&str>, step: i32) -> Option<String> {
    if order.is_empty() {
        return None;
    }
    let pos = current.and_then(|c| order.iter().position(|x| x == c));
    let next = match (pos, step >= 0) {
        (None, true) => 0,
        (None, false) => order.len() - 1,
        (Some(p), true) => (p + 1).min(order.len() - 1),
        (Some(p), false) => p.saturating_sub(1),
    };
    order.get(next).cloned()
}

/// The Calendar area.
#[component]
pub fn CalendarArea() -> impl IntoView {
    let state = app_state();
    let ui = ui();
    let sel = state.selection(Area::Calendar);
    let model = use_query::<ViewModel>("calendar.view", move || json!({ "mini_offset": ui.mini_offset.get() }), TABLES);
    // The current-time line and "today" move on their own.
    let ticker = set_interval_with_handle(move || model.refetch(), Duration::from_secs(60));
    on_cleanup(move || {
        if let Ok(h) = ticker {
            h.clear();
        }
    });
    let data = Memo::new(move |_| model.data().map(|m| (*m).clone()).unwrap_or_default());

    // j/k (global list navigation) step through events while the calendar is on screen.
    let last_nav = StoredValue::new(state.list_nav.get_untracked().0);
    Effect::new(move |_| {
        let (n, req) = state.list_nav.get();
        if last_nav.get_value() == n || state.area.get_untracked() != Area::Calendar {
            last_nav.set_value(n);
            return;
        }
        last_nav.set_value(n);
        let step = match req {
            Some(ListNav::Next) => 1,
            Some(ListNav::Previous) => -1,
            None => return,
        };
        select_step(state, model, step);
    });

    // Tell agents what is on screen.
    Effect::new(move |_| {
        let m = data.get();
        let selected = sel.item.get();
        let form = ui.form.with(Option::is_some);
        report(json!({
            "area": "calendar",
            "view": m.view,
            "date": m.date,
            "title": m.title,
            "selection": selected.into_iter().collect::<Vec<_>>(),
            "panel": if form { "form" } else if sel.item.get_untracked().is_some() { "event" } else { "none" },
            "events": m.events.len(),
        }));
    });

    let on_key = move |e: web_sys::KeyboardEvent| on_key(state, model, e);
    view! {
        <div class="cal" on:keydown=on_key>
            <Show when=move || state.nav_pane_visible.get()>
                <nav class="pane pane-nav cal-nav" data-pane="" tabindex="-1" aria-label="Calendars">
                    <MiniMonth data=data />
                    <AddPerson />
                    <SourceList data=data />
                </nav>
            </Show>
            <section class="pane cal-main" data-pane="" tabindex="-1" aria-label="Calendar">
                <Toolbar data=data />
                {move || match model.state.with(|s| matches!(s, crate::state::QueryState::Failed { .. })) {
                    true => {
                        let msg = model.state.with(|s| match s {
                            crate::state::QueryState::Failed { message, .. } => message.clone(),
                            _ => String::new(),
                        });
                        view! { <div class="cal-error" role="alert">{msg}</div> }.into_any()
                    }
                    false => ().into_any(),
                }}
                <div class="cal-body">
                    {move || {
                        let view = data.with(|m| m.view.clone());
                        match view.as_str() {
                            "month" => view! { <MonthView data=data /> }.into_any(),
                            "agenda" => view! { <AgendaView data=data /> }.into_any(),
                            _ => view! { <TimeGrid data=data /> }.into_any(),
                        }
                    }}
                </div>
            </section>
            {move || {
                let form = ui.form.get().is_some();
                let item = sel.item.get();
                if form {
                    view! {
                        <aside class="pane cal-side" data-pane="" tabindex="-1" aria-label="New event">
                            <EventForm data=data />
                        </aside>
                    }
                    .into_any()
                } else if let Some(id) = item {
                    view! {
                        <aside class="pane cal-side" data-pane="" tabindex="-1" aria-label="Event details">
                            <EventDetail id=id />
                        </aside>
                    }
                    .into_any()
                } else {
                    ().into_any()
                }
            }}
        </div>
    }
}

fn select_step(state: AppState, model: Query<ViewModel>, step: i32) {
    let sel = state.selection(Area::Calendar);
    let Some(m) = model.state.with_untracked(crate::state::QueryState::data) else { return };
    let order = keyboard_order(&m.events);
    let current = sel.item.get_untracked();
    if let Some(next) = step_selection(&order, current.as_deref(), step) {
        let selector = format!("[data-event-id=\"{}\"]", css_escape(&next));
        sel.item.set(Some(next));
        request_animation_frame(move || {
            if let Some(el) = crate::dom::query_all(&selector).into_iter().next() {
                el.scroll_into_view_with_bool(false);
                let _ = crate::dom::focus_element(&el);
            }
        });
    }
}

/// Escapes a value for a CSS attribute selector in double quotes.
pub fn css_escape(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .flat_map(|c| if c == '"' || c == '\\' { vec!['\\', c] } else { vec![c] })
        .collect()
}

fn on_key(state: AppState, model: Query<ViewModel>, e: web_sys::KeyboardEvent) {
    if e.default_prevented() || e.is_composing() || e.meta_key() || e.ctrl_key() || e.alt_key() {
        return;
    }
    let ui = ui();
    let sel = state.selection(Area::Calendar);
    let key = e.key();
    let target = e.target();
    if key == "Escape" {
        if ui.form.get_untracked().is_some() {
            ui.form.set(None);
        } else if sel.item.get_untracked().is_some() {
            sel.item.set(None);
        } else {
            return;
        }
        e.prevent_default();
        e.stop_propagation();
        request_animation_frame(|| {
            let _ = crate::dom::focus_selector(".cal [data-primary-list]");
        });
        return;
    }
    if crate::dom::is_editable(target.as_ref()) {
        return;
    }
    let handled = match key.as_str() {
        "t" | "T" => {
            go(state, "today");
            true
        }
        "ArrowLeft" => {
            go(state, "previous");
            true
        }
        "ArrowRight" => {
            go(state, "next");
            true
        }
        "ArrowDown" => {
            select_step(state, model, 1);
            true
        }
        "ArrowUp" => {
            select_step(state, model, -1);
            true
        }
        "n" | "N" => {
            open_form(state, default_draft());
            true
        }
        "Enter" => {
            if sel.item.get_untracked().is_some() {
                request_animation_frame(|| {
                    let _ = crate::dom::focus_selector(".cal-side [data-pane-focus]");
                });
                true
            } else {
                false
            }
        }
        "1" => {
            set_view(state, "day");
            true
        }
        "2" => {
            set_view(state, "work_week");
            true
        }
        "3" => {
            set_view(state, "week");
            true
        }
        "4" => {
            set_view(state, "month");
            true
        }
        "5" => {
            set_view(state, "agenda");
            true
        }
        _ => false,
    };
    if handled {
        e.prevent_default();
        e.stop_propagation();
    }
}

#[component]
fn Toolbar(data: Memo<ViewModel>) -> impl IntoView {
    let state = app_state();
    let views = [
        ("day", "Day", "1"),
        ("work_week", "Work week", "2"),
        ("week", "Week", "3"),
        ("month", "Month", "4"),
        ("agenda", "Agenda", "5"),
    ];
    view! {
        <header class="cal-toolbar" role="toolbar" aria-label="Calendar navigation">
            <button class="command-button primary" data-command="calendar.new_event" title="New event (N)"
                on:click=move |_| open_form(state, default_draft())>
                <KIcon name=CalIcon::Add />
                <span class="command-label">"New event"</span>
            </button>
            <button class="cal-today" data-command="calendar.go" title="Today (T)" on:click=move |_| go(state, "today")>"Today"</button>
            <div class="cal-stepper">
                <button class="icon-button" aria-label="Previous" title="Previous (←)" on:click=move |_| go(state, "previous")>
                    <KIcon name=CalIcon::Prev />
                </button>
                <button class="icon-button" aria-label="Next" title="Next (→)" on:click=move |_| go(state, "next")>
                    <KIcon name=CalIcon::Next />
                </button>
            </div>
            <h2 class="cal-title" aria-live="polite">{move || data.with(|m| m.title.clone())}</h2>
            <span class="cal-tz" title="Times are shown in this time zone">{move || data.with(|m| m.time_zone.clone())}</span>
            <div class="cal-views" role="radiogroup" aria-label="View">
                {views
                    .into_iter()
                    .map(|(id, label, key)| {
                        let active = move || data.with(|m| m.view == id);
                        view! {
                            <button
                                role="radio"
                                class="cal-view-button"
                                class:active=active
                                aria-checked=move || if active() { "true" } else { "false" }
                                title=format!("{label} ({key})")
                                on:click=move |_| set_view(state, id)
                            >
                                {label}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
        </header>
    }
}

#[component]
fn MiniMonth(data: Memo<ViewModel>) -> impl IntoView {
    let state = app_state();
    let ui = ui();
    view! {
        <div class="cal-mini" aria-label="Month">
            <div class="cal-mini-head">
                <span class="cal-mini-title">{move || data.with(|m| m.mini.title.clone())}</span>
                <button class="icon-button" aria-label="Previous month" on:click=move |_| ui.mini_offset.update(|o| *o -= 1)>
                    <KIcon name=CalIcon::Prev />
                </button>
                <button class="icon-button" aria-label="Next month" on:click=move |_| ui.mini_offset.update(|o| *o += 1)>
                    <KIcon name=CalIcon::Next />
                </button>
            </div>
            <div class="cal-mini-grid" role="grid">
                {["M", "T", "W", "T", "F", "S", "S"].into_iter().map(|d| view! { <span class="cal-mini-wd" aria-hidden="true">{d}</span> }).collect_view()}
                {move || {
                    data.with(|m| {
                        m.mini
                            .weeks
                            .iter()
                            .flatten()
                            .map(|d| {
                                let date = d.date.clone();
                                let label = Ymd::parse(&d.date).map(Ymd::short).unwrap_or_default();
                                view! {
                                    <button
                                        class="cal-mini-day"
                                        class:out=!d.in_month
                                        class:today=d.is_today
                                        class:in-range=d.in_range
                                        aria-label=label
                                        on:click=move |_| {
                                            ui.mini_offset.set(0);
                                            go(state, &date);
                                        }
                                    >
                                        {d.day}
                                    </button>
                                }
                            })
                            .collect_view()
                    })
                }}
            </div>
        </div>
    }
}

#[component]
fn AddPerson() -> impl IntoView {
    let state = app_state();
    let text = RwSignal::new(String::new());
    let submit = move || {
        let user = text.get_untracked().trim().to_owned();
        if user.is_empty() {
            return;
        }
        if !user.contains('@') {
            state.toast(ToastLevel::Warning, "Enter an email address to open their calendar.");
            return;
        }
        text.set(String::new());
        crate::commands::dispatch(state, "calendar.open_person", json!({ "user": user }));
    };
    view! {
        <form class="cal-add-person" on:submit=move |e| {
            e.prevent_default();
            submit();
        }>
            <label class="sr-only" for="cal-add-person">"Add person"</label>
            <input
                id="cal-add-person"
                type="email"
                placeholder="Add person…"
                autocomplete="off"
                spellcheck="false"
                prop:value=move || text.get()
                on:input=move |e| text.set(event_target_value(&e))
            />
        </form>
    }
}

#[component]
fn SourceList(data: Memo<ViewModel>) -> impl IntoView {
    let state = app_state();
    view! {
        <div class="cal-sources" role="list" aria-label="Calendar layers">
            {move || {
                let sources = data.with(|m| m.sources.clone());
                let mut groups: Vec<(String, Vec<Source>)> = Vec::new();
                for s in sources {
                    match groups.iter_mut().find(|(g, _)| *g == s.group) {
                        Some((_, list)) => list.push(s),
                        None => groups.push((s.group.clone(), vec![s])),
                    }
                }
                groups
                    .into_iter()
                    .map(|(group, list)| {
                        view! {
                            <div class="cal-source-group">
                                <h3 class="cal-source-heading">{group}</h3>
                                {list
                                    .into_iter()
                                    .map(|s| {
                                        let id = s.id.clone();
                                        let id2 = s.id.clone();
                                        let person = s.person.clone();
                                        let fb = s.free_busy_only;
                                        let pending = s.mode.as_deref() == Some("pending");
                                        view! {
                                            <div class="cal-source" role="listitem" style=format!("--ev: {}", s.color)>
                                                <label class="cal-source-label">
                                                    <input
                                                        type="checkbox"
                                                        class="cal-source-check"
                                                        prop:checked=s.visible
                                                        on:change=move |_| crate::commands::dispatch(state, "calendar.toggle_source", json!({ "id": id }))
                                                    />
                                                    <span class="cal-source-name">{s.name.clone()}</span>
                                                    {fb.then(|| view! { <span class="cal-source-tag" title="Free/busy only">"Free/busy"</span> })}
                                                    {pending.then(|| view! { <span class="cal-source-tag">"Checking…"</span> })}
                                                </label>
                                                {person.map(|p| {
                                                    view! {
                                                        <button class="icon-button cal-source-remove" aria-label="Close calendar" title="Close calendar"
                                                            on:click=move |_| crate::commands::dispatch(state, "calendar.close_person", json!({ "user": p.clone() }))>
                                                            <KIcon name=CalIcon::Cross />
                                                        </button>
                                                    }
                                                })}
                                                <span class="sr-only">{id2}</span>
                                            </div>
                                        }
                                    })
                                    .collect_view()}
                            </div>
                        }
                    })
                    .collect_view()
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, day: i64, start: u32, all_day: bool) -> Ev {
        Ev { id: id.into(), start_day: day, start_min: start, all_day, ..Ev::default() }
    }

    #[test]
    fn keyboard_order_and_stepping() {
        let events = vec![ev("b", 1, 600, false), ev("a", 0, 900, false), ev("c", 1, 0, true), ev("x", -1, 0, true)];
        let order = keyboard_order(&events);
        assert_eq!(order, ["x", "a", "c", "b"]);
        assert_eq!(step_selection(&order, None, 1).as_deref(), Some("x"));
        assert_eq!(step_selection(&order, None, -1).as_deref(), Some("b"));
        assert_eq!(step_selection(&order, Some("a"), 1).as_deref(), Some("c"));
        assert_eq!(step_selection(&order, Some("b"), 1).as_deref(), Some("b"));
        assert_eq!(step_selection(&order, Some("gone"), 1).as_deref(), Some("x"));
        assert_eq!(step_selection(&[], None, 1), None);
    }

    #[test]
    fn draft_params_match_the_engine() {
        let mut d = Draft::at(Ymd::new(2026, 10, 20).unwrap(), 23 * 60 + 45, 30);
        assert_eq!((d.end_date.as_str(), d.end.as_str()), ("2026-10-21", "00:15"));
        d.subject = " Plan ".into();
        d.attendees = vec!["a@example.com".into()];
        d.room = Some(("room@example.com".into(), "Room 1".into()));
        d.teams = true;
        d.repeat = "weekly".into();
        let p = d.params();
        assert_eq!(p["subject"], "Plan");
        assert_eq!(p["start"], "2026-10-20T23:45");
        assert_eq!(p["end"], "2026-10-21T00:15");
        assert_eq!(p["attendees"][1]["type"], "resource");
        assert_eq!(p["location"]["address"], "room@example.com");
        assert_eq!(p["recurrence"], "weekly");
        assert!(p.get("id").is_none());
        d.editing = Some("e1".into());
        d.all_day = true;
        let p = d.params();
        assert_eq!(p["id"], "e1");
        assert_eq!(p["start"], "2026-10-20");
    }

    #[test]
    fn prefill_reads_compose_params_safely() {
        let d = prefill(
            &json!({ "attendees": ["a@example.com", "nope", { "address": "b@example.com" }], "subject": "Hi\u{0}", "start": "2026-10-20T10:00" }),
        );
        assert_eq!(d.attendees, ["a@example.com", "b@example.com"]);
        assert_eq!(d.subject, "Hi");
        assert_eq!((d.date.as_str(), d.start.as_str(), d.end.as_str()), ("2026-10-20", "10:00", "10:30"));
        let d = prefill(&json!({ "start": 5, "attendees": "x" }));
        assert!(d.attendees.is_empty());
        assert_eq!(css_escape("a\"b\\c\u{1}"), "a\\\"b\\\\c");
    }
}
