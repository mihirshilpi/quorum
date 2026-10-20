//! The calendar side panel: event details (RSVP with message and proposed time, Join, edit,
//! delete, cancel, forward, reminders) and the new-event form with the scheduling assistant
//! (attendee and room rows over a free/busy grid, suggested times, room finder).

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use super::calendar::{Draft, TABLES, ViewModel, ui};
use super::calendar_date::{Ymd, hhmm, parse_hhmm};
use super::calendar_icons::{CalIcon, KIcon};
use crate::state::{AppState, Area, QueryState, ToastLevel, app_state, use_query};

/// A person on an event.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct PersonRef {
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Address.
    #[serde(default)]
    pub address: String,
}

/// A small avatar for a person known by address: live person card when the address is valid.
fn person_avatar(name: &str, address: &str) -> AnyView {
    let shown = if name.is_empty() { address.to_owned() } else { name.to_owned() };
    match crate::components::person_card::mail_user(address) {
        Some(user) => {
            view! { <crate::components::person_card::PersonAvatar user=user name=shown size="sm" /> }.into_any()
        }
        None => view! { <crate::components::Avatar name=shown size="sm" /> }.into_any(),
    }
}

/// An attendee row.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AttendeeRow {
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Address.
    #[serde(default)]
    pub address: String,
    /// `required`, `optional`, `resource`.
    #[serde(default, rename = "type")]
    pub kind: String,
    /// Graph response.
    #[serde(default)]
    pub response: String,
    /// Human label.
    #[serde(default)]
    pub response_label: String,
}

/// The layer an event is on.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct SourceRef {
    /// Id.
    #[serde(default)]
    pub id: String,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Colour.
    #[serde(default)]
    pub color: String,
}

/// `calendar.event`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Detail {
    /// Id.
    pub id: String,
    /// Subject.
    #[serde(default)]
    pub subject: String,
    /// When, formatted.
    #[serde(default)]
    pub when: String,
    /// Local start `YYYY-MM-DDTHH:MM`.
    #[serde(default)]
    pub start_local: Option<String>,
    /// Local end.
    #[serde(default)]
    pub end_local: Option<String>,
    /// All day.
    #[serde(default)]
    pub all_day: bool,
    /// Location.
    #[serde(default)]
    pub location: String,
    /// Organizer.
    #[serde(default)]
    pub organizer: Option<PersonRef>,
    /// Attendees.
    #[serde(default)]
    pub attendees: Vec<AttendeeRow>,
    /// The user's response.
    #[serde(default)]
    pub response: String,
    /// Its label.
    #[serde(default)]
    pub response_label: String,
    /// Organized by the user.
    #[serde(default)]
    pub is_organizer: bool,
    /// Can RSVP.
    #[serde(default)]
    pub can_respond: bool,
    /// Can edit/delete.
    #[serde(default)]
    pub can_edit: bool,
    /// Can cancel (organizer with attendees).
    #[serde(default)]
    pub can_cancel: bool,
    /// Online meeting.
    #[serde(default)]
    pub is_online: bool,
    /// Joinable.
    #[serde(default)]
    pub can_join: bool,
    /// Recurrence text.
    #[serde(default)]
    pub recurrence: Option<String>,
    /// Body preview.
    #[serde(default)]
    pub preview: String,
    /// Cancelled.
    #[serde(default)]
    pub is_cancelled: bool,
    /// Reminder lead time.
    #[serde(default)]
    pub reminder_minutes: Option<u32>,
    /// Not yet on the server.
    #[serde(default)]
    pub pending: bool,
    /// Layer.
    #[serde(default)]
    pub source: Option<SourceRef>,
}

impl Detail {
    /// A form draft to edit this event (pure, for tests).
    pub fn to_draft(&self) -> Draft {
        let split = |s: &Option<String>| {
            let s = s.clone().unwrap_or_default();
            let date = Ymd::parse(&s).map(Ymd::iso).unwrap_or_default();
            let time = s.get(11..16).and_then(parse_hhmm).map(hhmm).unwrap_or_else(|| "09:00".into());
            (date, time)
        };
        let (date, start) = split(&self.start_local);
        let (mut end_date, end) = split(&self.end_local);
        if self.all_day {
            // The engine's end is exclusive; the form shows the last day.
            end_date = Ymd::parse(&end_date).map(|d| d.add_days(-1).iso()).unwrap_or(end_date);
        }
        Draft {
            editing: Some(self.id.clone()),
            subject: self.subject.clone(),
            date,
            start,
            end_date,
            end,
            all_day: self.all_day,
            attendees: self
                .attendees
                .iter()
                .filter(|a| a.kind != "resource" && !a.address.is_empty())
                .map(|a| a.address.clone())
                .collect(),
            location: self.location.clone(),
            room: None,
            teams: self.is_online,
            body: String::new(),
            calendar: self.source.as_ref().map(|s| s.id.clone()),
            repeat: "none".into(),
        }
    }
}

fn response_icon(r: &str) -> CalIcon {
    match r {
        "accepted" | "organizer" => CalIcon::Check,
        "tentativelyAccepted" => CalIcon::Maybe,
        "declined" => CalIcon::Cross,
        _ => CalIcon::Time,
    }
}

fn close(state: AppState) {
    state.selection(Area::Calendar).item.set(None);
}

/// The details of event `id`.
#[component]
pub fn EventDetail(
    /// Event id.
    id: String,
) -> impl IntoView {
    let detail = use_query::<Detail>("calendar.event", move || json!({ "id": id }), TABLES);
    view! {
        <div class="cal-panel">
            {move || match detail.state.get() {
                QueryState::Loading => view! { <div class="list-loading"><crate::components::Spinner /></div> }.into_any(),
                QueryState::Failed { message, .. } => view! {
                    <div class="cal-panel-body">
                        <PanelHead title="Event" />
                        <p class="placeholder-note">{message}</p>
                    </div>
                }
                .into_any(),
                QueryState::Ready(d) => view! { <DetailBody d=(*d).clone() /> }.into_any(),
            }}
        </div>
    }
}

#[component]
fn PanelHead(#[prop(into)] title: String) -> impl IntoView {
    let state = app_state();
    view! {
        <header class="cal-panel-head">
            <span class="cal-panel-kicker">{title}</span>
            <button class="icon-button" aria-label="Close" title="Close (Esc)" on:click=move |_| {
                ui().form.set(None);
                close(state);
            }>
                <KIcon name=CalIcon::Cross />
            </button>
        </header>
    }
}

#[component]
fn DetailBody(d: Detail) -> impl IntoView {
    let state = app_state();
    let message = RwSignal::new(String::new());
    let proposing = RwSignal::new(false);
    let p_start = RwSignal::new(d.start_local.clone().unwrap_or_default());
    let p_end = RwSignal::new(d.end_local.clone().unwrap_or_default());
    let forward_to = RwSignal::new(None::<String>);
    let id = d.id.clone();
    let respond = {
        let id = id.clone();
        move |response: &'static str| {
            let mut p = json!({ "id": id, "response": response });
            let msg = message.get_untracked();
            if let Some(obj) = p.as_object_mut() {
                if !msg.trim().is_empty() {
                    obj.insert("message".into(), json!(msg.trim()));
                }
                if proposing.get_untracked() && response != "accept" {
                    obj.insert(
                        "propose".into(),
                        json!({ "start": p_start.get_untracked(), "end": p_end.get_untracked() }),
                    );
                }
            }
            crate::commands::dispatch(state, "calendar.respond", p);
            message.set(String::new());
            proposing.set(false);
        }
    };
    let color = d.source.as_ref().map(|s| s.color.clone()).unwrap_or_default();
    let draft = d.to_draft();
    let (id_join, id_del, id_cancel, id_fwd, id_snooze, id_dismiss) =
        (id.clone(), id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
    let r_acc = respond.clone();
    let r_ten = respond.clone();
    let r_dec = respond;
    let current = d.response.clone();
    let cur = move |r: &str| current == r;
    let (is_acc, is_ten, is_dec) = (cur("accepted"), cur("tentativelyAccepted"), cur("declined"));
    view! {
        <div class="cal-panel-body" style=format!("--ev: {color}")>
            <PanelHead title=d.source.as_ref().map(|s| s.name.clone()).unwrap_or_else(|| "Event".into()) />
            <h1 class="reading-title cal-detail-title" tabindex="-1" data-pane-focus="">
                <span class="cal-dot" aria-hidden="true"></span>
                {d.subject.clone()}
            </h1>
            {d.pending.then(|| view! { <p class="cal-pending">"Saving to your calendar…"</p> })}
            {d.is_cancelled.then(|| view! { <p class="cal-pending">"This meeting was cancelled."</p> })}
            <ul class="cal-facts">
                <li><KIcon name=CalIcon::Time /><span>{d.when.clone()}</span></li>
                {d.recurrence.clone().map(|r| view! { <li><KIcon name=CalIcon::Repeat /><span>{r}</span></li> })}
                {(!d.location.is_empty()).then(|| view! { <li><KIcon name=CalIcon::Place /><span>{d.location.clone()}</span></li> })}
                {d.reminder_minutes.map(|m| view! { <li><KIcon name=CalIcon::Bell /><span>{format!("Reminder {m} min before")}</span></li> })}
            </ul>
            {d.can_join.then(|| view! {
                <button class="cal-join cal-join-big" data-command="calendar.join" title="Join (⇧⌘J)"
                    on:click=move |_| crate::commands::dispatch(state, "calendar.join", json!({ "id": id_join }))>
                    <KIcon name=CalIcon::Video />
                    "Join online meeting"
                </button>
            })}
            {d.can_respond.then(|| view! {
                <section class="cal-rsvp" aria-label="Respond">
                    <div class="cal-rsvp-buttons" role="group" aria-label="Your response">
                        <button class="cal-rsvp-button" class:active=is_acc on:click=move |_| r_acc("accept")><KIcon name=CalIcon::Check />"Accept"</button>
                        <button class="cal-rsvp-button" class:active=is_ten on:click=move |_| r_ten("tentative")><KIcon name=CalIcon::Maybe />"Tentative"</button>
                        <button class="cal-rsvp-button" class:active=is_dec on:click=move |_| r_dec("decline")><KIcon name=CalIcon::Cross />"Decline"</button>
                    </div>
                    <input class="cal-input" type="text" placeholder="Add a message (optional)" aria-label="Message to the organizer"
                        prop:value=move || message.get() on:input=move |e| message.set(event_target_value(&e)) />
                    <label class="cal-check">
                        <input type="checkbox" prop:checked=move || proposing.get() on:change=move |e| proposing.set(event_target_checked(&e)) />
                        "Propose a new time (with Tentative or Decline)"
                    </label>
                    <Show when=move || proposing.get()>
                        <div class="cal-row">
                            <input class="cal-input" type="datetime-local" aria-label="Proposed start" prop:value=move || p_start.get() on:input=move |e| p_start.set(event_target_value(&e)) />
                            <input class="cal-input" type="datetime-local" aria-label="Proposed end" prop:value=move || p_end.get() on:input=move |e| p_end.set(event_target_value(&e)) />
                        </div>
                    </Show>
                </section>
            })}
            {d.organizer.clone().map(|o| view! {
                <section class="cal-people">
                    <h3 class="cal-section">"Organizer"</h3>
                    <div class="cal-person">{person_avatar(&o.name, &o.address)}<span>{o.name.clone()}</span></div>
                </section>
            })}
            {(!d.attendees.is_empty()).then(|| {
                let n = d.attendees.len();
                let accepted = d.attendees.iter().filter(|a| a.response == "accepted").count();
                view! {
                    <section class="cal-people">
                        <h3 class="cal-section"><KIcon name=CalIcon::Group />{format!("{n} attendees · {accepted} accepted")}</h3>
                        {d.attendees
                            .iter()
                            .take(200)
                            .map(|a| {
                                let name = if a.name.is_empty() { a.address.clone() } else { a.name.clone() };
                                view! {
                                    <div class="cal-person" title=a.address.clone()>
                                        {person_avatar(&name, &a.address)}
                                        <span class="cal-person-name">{name}</span>
                                        <span class=format!("cal-resp is-{}", a.response) title=a.response_label.clone()>
                                            <KIcon name=response_icon(&a.response) />
                                            <span class="sr-only">{a.response_label.clone()}</span>
                                        </span>
                                    </div>
                                }
                            })
                            .collect_view()}
                    </section>
                }
            })}
            {(!d.preview.trim().is_empty()).then(|| view! { <p class="cal-preview">{d.preview.clone()}</p> })}
            <footer class="cal-actions">
                {d.can_edit.then(|| {
                    let draft = draft.clone();
                    view! {
                        <button class="command-button" on:click=move |_| super::calendar::open_form(state, draft.clone())>
                            <crate::components::Icon name=crate::icons::IconName::Pencil />"Edit"
                        </button>
                    }
                })}
                {d.can_cancel.then(|| view! {
                    <button class="command-button danger" on:click=move |_| {
                        crate::commands::dispatch(state, "calendar.cancel", json!({ "id": id_cancel }));
                        close(state);
                    }>
                        <KIcon name=CalIcon::Cross />"Cancel meeting"
                    </button>
                })}
                {(d.can_edit && !d.can_cancel).then(|| view! {
                    <button class="command-button" title="Delete (⌘⌫)" on:click=move |_| {
                        crate::commands::dispatch(state, "calendar.delete_event", json!({ "id": id_del }));
                        close(state);
                    }>
                        <crate::components::Icon name=crate::icons::IconName::Trash />"Delete"
                    </button>
                })}
                <button class="command-button" on:click=move |_| forward_to.set(Some(String::new()))>
                    <crate::components::Icon name=crate::icons::IconName::Forward />"Forward"
                </button>
                {d.reminder_minutes.is_some().then(|| view! {
                    <button class="command-button" on:click=move |_| crate::commands::dispatch(state, "calendar.snooze", json!({ "id": id_snooze, "minutes": 5 }))>
                        <KIcon name=CalIcon::Bell />"Snooze 5 min"
                    </button>
                    <button class="command-button" on:click=move |_| crate::commands::dispatch(state, "calendar.dismiss", json!({ "id": id_dismiss }))>
                        "Dismiss reminder"
                    </button>
                })}
            </footer>
            {move || forward_to.get().map(|to| {
                let id = id_fwd.clone();
                view! {
                    <form class="cal-row" on:submit=move |e| {
                        e.prevent_default();
                        let list: Vec<String> = forward_to.get_untracked().unwrap_or_default().split([',', ';', ' ']).map(str::trim).filter(|s| s.contains('@')).map(str::to_owned).collect();
                        if list.is_empty() {
                            state.toast(ToastLevel::Warning, "Enter at least one address.");
                            return;
                        }
                        crate::commands::dispatch(state, "calendar.forward", json!({ "id": id, "to": list }));
                        forward_to.set(None);
                    }>
                        <input class="cal-input" type="text" placeholder="Forward to (addresses)" aria-label="Forward to" prop:value=to
                            on:input=move |e| forward_to.set(Some(event_target_value(&e))) />
                        <button class="command-button primary" type="submit"><crate::components::Icon name=crate::icons::IconName::Send />"Send"</button>
                    </form>
                }
            })}
        </div>
    }
}

/// `calendar.free_busy` → one schedule.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Schedule {
    /// Address.
    #[serde(default)]
    pub user: String,
    /// One status per interval.
    #[serde(default)]
    pub availability: Vec<String>,
    /// Why it is missing.
    #[serde(default)]
    pub error: Option<String>,
}

/// `calendar.free_busy`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct FreeBusy {
    /// Interval in minutes.
    #[serde(default)]
    pub interval: u32,
    /// Schedules.
    #[serde(default)]
    pub schedules: Vec<Schedule>,
}

/// A suggested time.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Suggestion {
    /// Local date.
    #[serde(default)]
    pub date: String,
    /// Start minute.
    #[serde(default)]
    pub start_min: u32,
    /// End minute.
    #[serde(default)]
    pub end_min: u32,
    /// Label.
    #[serde(default)]
    pub label: String,
    /// 0–100.
    #[serde(default)]
    pub confidence: f64,
    /// Rooms.
    #[serde(default)]
    pub locations: Vec<String>,
}

/// `calendar.find_times`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Suggestions {
    /// Suggestions.
    #[serde(default)]
    pub suggestions: Vec<Suggestion>,
    /// Why none.
    #[serde(default)]
    pub empty_reason: Option<String>,
}

/// A room.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Room {
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Address.
    #[serde(default)]
    pub address: Option<String>,
    /// Seats.
    #[serde(default)]
    pub capacity: Option<u32>,
    /// Building.
    #[serde(default)]
    pub building: Option<String>,
}

/// First and last hour of the scheduling grid.
const ASSIST_FROM: u32 = 7 * 60;
const ASSIST_TO: u32 = 20 * 60;
const ASSIST_STEP: u32 = 30;

/// The status of slot `minute` in an availability list starting at midnight (pure).
pub fn slot_status(availability: &[String], interval: u32, minute: u32) -> &str {
    if interval == 0 {
        return "unknown";
    }
    availability.get(usize::try_from(minute / interval).unwrap_or(usize::MAX)).map_or("unknown", String::as_str)
}

/// Parses comma/space separated addresses (pure).
pub fn parse_addresses(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in s.split([',', ';', ' ', '\n']).map(str::trim).filter(|a| a.contains('@') && a.len() <= 320) {
        if !out.iter().any(|x| x.eq_ignore_ascii_case(a)) {
            out.push(a.to_owned());
        }
    }
    out
}

/// The new / edit event form.
#[component]
pub fn EventForm(
    /// The view model.
    data: Memo<ViewModel>,
) -> impl IntoView {
    let state = app_state();
    let ui = ui();
    let init = ui.form.get_untracked().unwrap_or_default();
    let editing = init.editing.clone();
    let subject = RwSignal::new(init.subject.clone());
    let date = RwSignal::new(init.date.clone());
    let start = RwSignal::new(init.start.clone());
    let end_date = RwSignal::new(init.end_date.clone());
    let end = RwSignal::new(init.end.clone());
    let all_day = RwSignal::new(init.all_day);
    let attendees = RwSignal::new(init.attendees.clone());
    let attendee_text = RwSignal::new(String::new());
    let location = RwSignal::new(init.location.clone());
    let room = RwSignal::new(init.room.clone());
    let teams = RwSignal::new(init.teams);
    let body = RwSignal::new(init.body.clone());
    let calendar = RwSignal::new(init.calendar.clone());
    let repeat = RwSignal::new(if init.repeat.is_empty() { "none".to_owned() } else { init.repeat.clone() });
    let show_rooms = RwSignal::new(false);
    // Re-initialise when another open request arrives while the form is up.
    Effect::new(move |prev: Option<u64>| {
        let seq = ui.form_seq.get();
        if prev.is_some_and(|p| p != seq)
            && let Some(d) = ui.form.get_untracked()
        {
            subject.set(d.subject);
            date.set(d.date);
            start.set(d.start);
            end_date.set(d.end_date);
            end.set(d.end);
            all_day.set(d.all_day);
            attendees.set(d.attendees);
            location.set(d.location);
            room.set(d.room);
            teams.set(d.teams);
            body.set(d.body);
            calendar.set(d.calendar);
            repeat.set(d.repeat);
        }
        seq
    });

    let draft = move || Draft {
        editing: editing.clone(),
        subject: subject.get(),
        date: date.get(),
        start: start.get(),
        end_date: end_date.get(),
        end: end.get(),
        all_day: all_day.get(),
        attendees: {
            let mut a = attendees.get();
            for x in parse_addresses(&attendee_text.get()) {
                if !a.contains(&x) {
                    a.push(x);
                }
            }
            a
        },
        location: location.get(),
        room: room.get(),
        teams: teams.get(),
        body: body.get(),
        calendar: calendar.get(),
        repeat: repeat.get(),
    };
    let is_edit = init.editing.is_some();
    let draft2 = draft.clone();
    let save = move || {
        let d = draft2();
        let (s, e) = (parse_hhmm(&d.start), parse_hhmm(&d.end));
        if Ymd::parse(&d.date).is_none() || (!d.all_day && (s.is_none() || e.is_none())) {
            state.toast(ToastLevel::Warning, "Enter a valid date and time.");
            return;
        }
        let id = if d.editing.is_some() { "calendar.update_event" } else { "calendar.new_event" };
        crate::commands::dispatch(state, id, d.params());
        ui.form.set(None);
    };
    let save2 = save.clone();
    let editable: Vec<(String, String)> = data.with_untracked(|m| {
        m.sources.iter().filter(|s| s.can_edit && !s.free_busy_only).map(|s| (s.id.clone(), s.name.clone())).collect()
    });

    // Keep the end after the start when the start moves (same duration).
    let on_start = move |v: String| {
        let old_s = parse_hhmm(&start.get_untracked());
        let old_e = parse_hhmm(&end.get_untracked());
        if let (Some(os), Some(oe), Some(ns)) = (old_s, old_e, parse_hhmm(&v)) {
            let len = oe.saturating_sub(os).max(15);
            end.set(hhmm((ns + len).min(23 * 60 + 59)));
        }
        start.set(v);
    };
    let on_date = move |v: String| {
        if end_date.get_untracked() == date.get_untracked() || end_date.get_untracked().is_empty() {
            end_date.set(v.clone());
        }
        date.set(v);
    };

    view! {
        <div class="cal-panel">
            <div class="cal-panel-body">
                <PanelHead title=if is_edit { "Edit event" } else { "New event" } />
                <form class="cal-form" on:submit=move |e| {
                    e.prevent_default();
                    save();
                }>
                    <input id="cal-form-subject" class="cal-input cal-subject" type="text" placeholder="Add a title" aria-label="Title"
                        data-pane-focus="" prop:value=move || subject.get() on:input=move |e| subject.set(event_target_value(&e)) />
                    {(!is_edit && editable.len() > 1).then(|| view! {
                        <label class="cal-field">
                            <span>"Calendar"</span>
                            <select class="cal-input" on:change=move |e| { let v = event_target_value(&e); calendar.set((!v.is_empty()).then_some(v)); }>
                                {editable.iter().map(|(id, name)| {
                                    let id2 = id.clone();
                                    view! { <option value=id.clone() selected=move || calendar.get().as_deref() == Some(id2.as_str())>{name.clone()}</option> }
                                }).collect_view()}
                            </select>
                        </label>
                    })}
                    <div class="cal-row">
                        <input class="cal-input" type="date" aria-label="Date" prop:value=move || date.get() on:input=move |e| on_date(event_target_value(&e)) />
                        <Show when=move || !all_day.get()>
                            <input class="cal-input cal-time" type="time" step="900" aria-label="Start time" prop:value=move || start.get() on:input=move |e| on_start(event_target_value(&e)) />
                            <span class="cal-dash">"–"</span>
                            <input class="cal-input cal-time" type="time" step="900" aria-label="End time" prop:value=move || end.get() on:input=move |e| end.set(event_target_value(&e)) />
                        </Show>
                        <Show when=move || all_day.get()>
                            <span class="cal-dash">"to"</span>
                            <input class="cal-input" type="date" aria-label="Last day" prop:value=move || end_date.get() on:input=move |e| end_date.set(event_target_value(&e)) />
                        </Show>
                    </div>
                    <div class="cal-row">
                        <label class="cal-check">
                            <input type="checkbox" prop:checked=move || all_day.get() on:change=move |e| all_day.set(event_target_checked(&e)) />
                            "All day"
                        </label>
                        <label class="cal-check">
                            <input type="checkbox" prop:checked=move || teams.get() on:change=move |e| teams.set(event_target_checked(&e)) />
                            <KIcon name=CalIcon::Video />
                            "Teams meeting"
                        </label>
                        <label class="cal-field cal-inline">
                            <KIcon name=CalIcon::Repeat />
                            <select class="cal-input" aria-label="Repeat" on:change=move |e| repeat.set(event_target_value(&e))>
                                {[("none", "Doesn't repeat"), ("daily", "Every day"), ("weekly", "Every week"), ("monthly", "Every month"), ("yearly", "Every year")]
                                    .into_iter()
                                    .map(|(v, l)| view! { <option value=v selected=move || repeat.get() == v>{l}</option> })
                                    .collect_view()}
                            </select>
                        </label>
                    </div>
                    <div class="cal-field">
                        <span class="cal-label"><KIcon name=CalIcon::Group />"Attendees"</span>
                        <div class="cal-chips">
                            {move || attendees.get().into_iter().map(|a| {
                                let a2 = a.clone();
                                view! {
                                    <span class="cal-chip">
                                        {a.clone()}
                                        <button type="button" class="cal-chip-x" aria-label=format!("Remove {a}") on:click=move |_| attendees.update(|v| v.retain(|x| *x != a2))>
                                            <KIcon name=CalIcon::Cross />
                                        </button>
                                    </span>
                                }
                            }).collect_view()}
                            <input class="cal-input cal-chip-input" type="text" placeholder="Add people by email" aria-label="Add attendees"
                                prop:value=move || attendee_text.get()
                                on:input=move |e| attendee_text.set(event_target_value(&e))
                                on:keydown=move |e: web_sys::KeyboardEvent| {
                                    if matches!(e.key().as_str(), "Enter" | "," | "Tab") && !attendee_text.get_untracked().trim().is_empty() {
                                        let found = parse_addresses(&attendee_text.get_untracked());
                                        if !found.is_empty() {
                                            e.prevent_default();
                                            attendees.update(|v| for x in found { if !v.contains(&x) { v.push(x); } });
                                            attendee_text.set(String::new());
                                        }
                                    }
                                }
                            />
                        </div>
                    </div>
                    <div class="cal-row">
                        <input class="cal-input" type="text" placeholder="Add a location" aria-label="Location" prop:value=move || location.get() on:input=move |e| location.set(event_target_value(&e)) />
                        <button type="button" class="command-button" on:click=move |_| show_rooms.update(|v| *v = !*v)>
                            <KIcon name=CalIcon::Room />"Rooms"
                        </button>
                    </div>
                    {move || room.get().map(|(_, name)| view! {
                        <span class="cal-chip cal-room-chip"><KIcon name=CalIcon::Room />{name}
                            <button type="button" class="cal-chip-x" aria-label="Remove room" on:click=move |_| room.set(None)><KIcon name=CalIcon::Cross /></button>
                        </span>
                    })}
                    <Show when=move || show_rooms.get()>
                        <RoomFinder room=room show=show_rooms />
                    </Show>
                    <Assistant data=data date=date start=start end=end attendees=Signal::derive(move || draft().attendees) room=room all_day=all_day />
                    <textarea class="cal-input cal-body" rows="4" placeholder="Add a description" aria-label="Description"
                        prop:value=move || body.get() on:input=move |e| body.set(event_target_value(&e))></textarea>
                    <footer class="cal-actions">
                        <button type="submit" class="command-button primary" on:click=move |e| {
                            e.prevent_default();
                            save2();
                        }>
                            <crate::components::Icon name=crate::icons::IconName::Send />
                            {if is_edit { "Save" } else { "Save event" }}
                        </button>
                        <button type="button" class="command-button" on:click=move |_| ui.form.set(None)>"Discard"</button>
                    </footer>
                </form>
            </div>
        </div>
    }
}

#[component]
fn RoomFinder(room: RwSignal<Option<(String, String)>>, show: RwSignal<bool>) -> impl IntoView {
    let rooms = use_query::<Vec<Room>>("calendar.rooms", || Value::Null, &["calendar"]);
    view! {
        <div class="cal-rooms" role="listbox" aria-label="Rooms">
            {move || match rooms.state.get() {
                QueryState::Loading => view! { <crate::components::Spinner /> }.into_any(),
                QueryState::Failed { message, .. } => view! { <p class="placeholder-note">{message}</p> }.into_any(),
                QueryState::Ready(list) => list
                    .iter()
                    .filter(|r| r.address.is_some())
                    .map(|r| {
                        let pick = (r.address.clone().unwrap_or_default(), r.name.clone());
                        let meta = [r.building.clone().unwrap_or_default(), r.capacity.map(|c| format!("{c} seats")).unwrap_or_default()]
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(" · ");
                        view! {
                            <button type="button" class="cal-room" role="option" on:click=move |_| {
                                room.set(Some(pick.clone()));
                                show.set(false);
                            }>
                                <KIcon name=CalIcon::Room />
                                <span class="cal-room-name">{r.name.clone()}</span>
                                <span class="cal-ev-meta">{meta}</span>
                            </button>
                        }
                    })
                    .collect_view()
                    .into_any(),
            }}
        </div>
    }
}

/// The scheduling assistant: everyone's free/busy on the chosen day, and suggested times.
#[component]
fn Assistant(
    data: Memo<ViewModel>,
    date: RwSignal<String>,
    start: RwSignal<String>,
    end: RwSignal<String>,
    attendees: Signal<Vec<String>>,
    room: RwSignal<Option<(String, String)>>,
    all_day: RwSignal<bool>,
) -> impl IntoView {
    let people = Memo::new(move |_| {
        let mut v = attendees.get();
        if let Some((addr, _)) = room.get() {
            v.push(addr);
        }
        v.truncate(40);
        v
    });
    let range = move || {
        let d = Ymd::parse(&date.get()).unwrap_or_else(super::calendar_mock::today);
        (d.iso(), d.add_days(1).iso())
    };
    let fb = use_query::<FreeBusy>(
        "calendar.free_busy",
        move || {
            let (from, to) = range();
            json!({ "users": people.get(), "from": from, "to": to, "interval": ASSIST_STEP })
        },
        &["calendar"],
    );
    let duration = move || {
        let (s, e) = (parse_hhmm(&start.get()).unwrap_or(540), parse_hhmm(&end.get()).unwrap_or(570));
        e.saturating_sub(s).clamp(15, 480)
    };
    let sugg = use_query::<Suggestions>(
        "calendar.find_times",
        move || {
            let d = Ymd::parse(&date.get()).unwrap_or_else(super::calendar_mock::today);
            json!({
                "attendees": people.get(),
                "duration": duration(),
                "window": { "from": format!("{}T{}", d.iso(), hhmm(ASSIST_FROM)), "to": format!("{}T{}", d.add_days(4).iso(), hhmm(ASSIST_TO)) },
                "max": 5,
            })
        },
        &["calendar"],
    );
    let slots: Vec<u32> = (ASSIST_FROM..ASSIST_TO).step_by(ASSIST_STEP as usize).collect();
    let slots2 = slots.clone();
    // "You": the user's own events on that day, when it is in the current view.
    let mine = move |minute: u32| -> bool {
        let d = date.get();
        data.with(|m| {
            let Some(day) = m.days.iter().position(|x| x.date == d) else { return false };
            let day = day as i64;
            m.events.iter().any(|e| {
                !e.all_day
                    && e.show_as != "free"
                    && e.start_day <= day
                    && e.end_day >= day
                    && (if e.start_day == day { e.start_min } else { 0 }) < minute + ASSIST_STEP
                    && (if e.end_day == day { e.end_min } else { 1440 }) > minute
            })
        })
    };
    let selected = move |minute: u32| {
        let (s, e) = (parse_hhmm(&start.get()).unwrap_or(0), parse_hhmm(&end.get()).unwrap_or(0));
        !all_day.get() && minute + ASSIST_STEP > s && minute < e
    };
    let pick = move |minute: u32| {
        let len = duration();
        start.set(hhmm(minute));
        end.set(hhmm((minute + len).min(23 * 60 + 59)));
    };
    view! {
        <section class="cal-assist" aria-label="Scheduling assistant">
            <h3 class="cal-section"><KIcon name=CalIcon::Group />"Scheduling assistant"</h3>
            <div class="cal-assist-grid" style=format!("--slots: {}", slots.len())>
                <span class="cal-assist-name"></span>
                {slots.iter().map(|m| view! {
                    <span class="cal-assist-hour">{if m % 60 == 0 { format!("{}", m / 60) } else { String::new() }}</span>
                }).collect_view()}
                <span class="cal-assist-name">"You"</span>
                {slots.iter().map(|m| {
                    let m = *m;
                    view! {
                        <button type="button" class="cal-slot" class:is-busy=move || mine(m) class:sel=move || selected(m)
                            aria-label=format!("You at {}", hhmm(m)) on:click=move |_| pick(m)></button>
                    }
                }).collect_view()}
                {move || {
                    let info = fb.data();
                    people
                        .get()
                        .into_iter()
                        .map(|p| {
                            let sched = info.as_ref().and_then(|f| f.schedules.iter().find(|s| s.user.eq_ignore_ascii_case(&p)).cloned());
                            let interval = info.as_ref().map(|f| f.interval).unwrap_or(ASSIST_STEP);
                            let is_room = room.with(|r| r.as_ref().is_some_and(|(a, _)| *a == p));
                            let label = if is_room { room.with(|r| r.as_ref().map(|(_, n)| n.clone()).unwrap_or_default()) } else { p.clone() };
                            view! {
                                <span class="cal-assist-name" title=p.clone()>{label}</span>
                                {slots2.iter().map(|m| {
                                    let m = *m;
                                    let status = sched.as_ref().map(|s| slot_status(&s.availability, interval, m).to_owned()).unwrap_or_else(|| "unknown".into());
                                    view! {
                                        <button type="button" class=format!("cal-slot is-{status}") class:sel=move || selected(m)
                                            aria-label=format!("{} {} at {}", p, status, hhmm(m)) on:click=move |_| pick(m)></button>
                                    }
                                }).collect_view()}
                            }
                        })
                        .collect_view()
                }}
            </div>
            <div class="cal-legend" aria-hidden="true">
                <span class="cal-slot is-busy"></span>"Busy"
                <span class="cal-slot is-tentative"></span>"Tentative"
                <span class="cal-slot is-oof"></span>"Away"
                <span class="cal-slot is-unknown"></span>"No info"
            </div>
            <Show when=move || !people.get().is_empty()>
                <h3 class="cal-section"><KIcon name=CalIcon::Spark />"Suggested times"</h3>
                <div class="cal-suggestions">
                    {move || match sugg.state.get() {
                        QueryState::Loading => view! { <crate::components::Spinner /> }.into_any(),
                        QueryState::Failed { message, .. } => view! { <p class="placeholder-note">{message}</p> }.into_any(),
                        QueryState::Ready(s) => {
                            if s.suggestions.is_empty() {
                                let why = s.empty_reason.clone().unwrap_or_else(|| "No time works for everyone.".into());
                                view! { <p class="placeholder-note">{why}</p> }.into_any()
                            } else {
                                s.suggestions
                                    .iter()
                                    .map(|x| {
                                        let x2 = x.clone();
                                        view! {
                                            <button type="button" class="cal-suggestion" on:click=move |_| {
                                                date.set(x2.date.clone());
                                                start.set(hhmm(x2.start_min));
                                                end.set(hhmm(x2.end_min));
                                            }>
                                                <span>{x.label.clone()}</span>
                                                <span class="cal-ev-meta">{format!("{:.0}%{}", x.confidence, x.locations.first().map(|l| format!(" · {l}")).unwrap_or_default())}</span>
                                            </button>
                                        }
                                    })
                                    .collect_view()
                                    .into_any()
                            }
                        }
                    }}
                </div>
            </Show>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_becomes_an_edit_draft() {
        let d = Detail {
            id: "e1".into(),
            subject: "Plan".into(),
            start_local: Some("2026-10-20T09:00".into()),
            end_local: Some("2026-10-20T10:30".into()),
            attendees: vec![
                AttendeeRow { address: "a@example.com".into(), kind: "required".into(), ..AttendeeRow::default() },
                AttendeeRow { address: "room@example.com".into(), kind: "resource".into(), ..AttendeeRow::default() },
            ],
            ..Detail::default()
        };
        let draft = d.to_draft();
        assert_eq!(draft.editing.as_deref(), Some("e1"));
        assert_eq!((draft.start.as_str(), draft.end.as_str()), ("09:00", "10:30"));
        assert_eq!(draft.attendees, ["a@example.com"]);
        let all = Detail {
            all_day: true,
            start_local: Some("2026-10-20T00:00".into()),
            end_local: Some("2026-10-22T00:00".into()),
            ..d
        };
        assert_eq!(all.to_draft().end_date, "2026-10-21");
        assert_eq!(Detail::default().to_draft().start, "09:00");
    }

    #[test]
    fn slots_and_addresses() {
        let a: Vec<String> = ["free", "busy", "tentative"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(slot_status(&a, 30, 45), "busy");
        assert_eq!(slot_status(&a, 30, 9000), "unknown");
        assert_eq!(slot_status(&a, 0, 0), "unknown");
        assert_eq!(parse_addresses("a@x.com, b@x.com;A@x.com nope"), ["a@x.com", "b@x.com"]);
    }
}
