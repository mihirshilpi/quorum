//! The top-bar account button and its status menu: pick a presence (with an optional
//! duration), set a status message with an expiry, and toggle "Keep me available while Quorum
//! is open". Every choice is an engine command (`presence.set`, `presence.set_status_message`,
//! `presence.clear_status_message`, `presence.keepalive`), so the keyboard, the palette and
//! agents can do the same.

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::Reply;
use serde_json::{Value, json};
use wasm_bindgen::JsCast;

use super::avatar::{Avatar, Presence};
use crate::state::{AccountState, ToastLevel, app_state, use_query};

/// The status choices: (command value, label, dot).
pub const CHOICES: &[(&str, &str, Presence)] = &[
    ("available", "Available", Presence::Available),
    ("busy", "Busy", Presence::Busy),
    ("dnd", "Do not disturb", Presence::DoNotDisturb),
    ("brb", "Be right back", Presence::BeRightBack),
    ("away", "Appear away", Presence::Away),
    ("offline", "Appear offline", Presence::Offline),
];

/// Presence durations: (label, minutes; `None` = until changed).
pub const DURATIONS: &[(&str, Option<u64>)] = &[
    ("Until I change it", None),
    ("30 minutes", Some(30)),
    ("1 hour", Some(60)),
    ("2 hours", Some(120)),
    ("4 hours", Some(240)),
    ("1 day", Some(24 * 60)),
    ("1 week", Some(7 * 24 * 60)),
];

/// Status message expiries: (label, minutes; `None` = never).
pub const EXPIRIES: &[(&str, Option<u64>)] =
    &[("Never", None), ("1 hour", Some(60)), ("4 hours", Some(240)), ("Today", Some(0)), ("1 week", Some(7 * 24 * 60))];

/// Longest status message (Teams' limit).
pub const MAX_MESSAGE: usize = 280;

/// RFC 3339 instant `minutes` from now; `Some(0)` means the end of today (local time).
fn expiry_iso(minutes: Option<u64>) -> Option<String> {
    let minutes = minutes?;
    let now = js_sys::Date::new_0();
    let ms = if minutes == 0 {
        let end = js_sys::Date::new_0();
        end.set_hours(23);
        end.set_minutes(59);
        end.set_seconds(0);
        end.get_time()
    } else {
        now.get_time() + (minutes as f64) * 60_000.0
    };
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms));
    d.to_iso_string().as_string()
}

fn input_value(e: &leptos::ev::Event) -> String {
    e.target()
        .and_then(|t| {
            t.clone()
                .dyn_into::<web_sys::HtmlInputElement>()
                .map(|i| i.value())
                .ok()
                .or_else(|| t.dyn_into::<web_sys::HtmlTextAreaElement>().ok().map(|i| i.value()))
        })
        .unwrap_or_default()
}

fn select_index(e: &leptos::ev::Event) -> usize {
    e.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlSelectElement>().ok())
        .and_then(|s| usize::try_from(s.selected_index()).ok())
        .unwrap_or(0)
}

async fn run(id: &str, params: Value) -> bool {
    let state = app_state();
    match crate::ipc::run(id, params).await {
        Reply::Ok { .. } => true,
        Reply::Err { message, .. } => {
            state.toast(ToastLevel::Error, message);
            false
        }
    }
}

/// Moves focus among the menu's items with ↑/↓ (Home/End too).
fn menu_keys(menu: &web_sys::Element, e: &web_sys::KeyboardEvent) {
    let step: isize = match e.key().as_str() {
        "ArrowDown" => 1,
        "ArrowUp" => -1,
        "Home" => isize::MIN,
        "End" => isize::MAX,
        _ => return,
    };
    let Ok(list) = menu.query_selector_all("[role=menuitemradio], [role=menuitemcheckbox], [data-menu-item]") else {
        return;
    };
    let items: Vec<web_sys::HtmlElement> = (0..list.length())
        .filter_map(|i| list.item(i))
        .filter_map(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
        .collect();
    if items.is_empty() {
        return;
    }
    e.prevent_default();
    let active = crate::dom::active_element();
    let current = items.iter().position(|i| active.as_ref().is_some_and(|a| a == i.as_ref()));
    let len = items.len();
    let next = match (step, current) {
        (isize::MIN, _) => 0,
        (isize::MAX, _) => len - 1,
        (s, None) if s < 0 => len - 1,
        (_, None) => 0,
        (s, Some(c)) => crate::commands::next_pane(Some(c), len, s),
    };
    if let Some(el) = items.get(next) {
        let _ = el.focus();
    }
}

/// The account button with the status menu.
#[component]
pub fn PresencePicker() -> impl IntoView {
    let state = app_state();
    let open = RwSignal::new(false);
    let me = use_query::<Value>("presence.me", || Value::Null, &["presence"]);
    let keepalive = use_query::<Value>("presence.keepalive", || Value::Null, &["setting", "settings"]);
    let duration = RwSignal::new(0usize);
    let expiry = RwSignal::new(0usize);
    let draft = RwSignal::new(String::new());
    let optimistic = RwSignal::new(None::<Presence>);
    let menu_ref = NodeRef::<html::Div>::new();
    let button_ref = NodeRef::<html::Button>::new();

    let presence = Signal::derive(move || {
        optimistic.get().unwrap_or_else(|| {
            me.data()
                .and_then(|v| v.get("presence").and_then(Value::as_str).map(Presence::parse))
                .filter(|p| *p != Presence::Unknown)
                .unwrap_or_else(|| state.account.with(|a| a.presence))
        })
    });
    let message = Signal::derive(move || {
        me.data()
            .and_then(|v| v.get("status_message").and_then(Value::as_str).map(str::to_owned))
            .filter(|s| !s.is_empty())
    });
    let keep_on = Signal::derive(move || {
        keepalive.data().and_then(|v| v.get("enabled").and_then(Value::as_bool)).unwrap_or(false)
    });
    let account_name = move || state.account.with(|a| a.name.clone().unwrap_or_else(|| "Account".into()));
    let account_email = move || state.account.with(|a| a.email.clone().unwrap_or_default());
    let label = move || {
        state.account.with(|a| match a.state {
            AccountState::NeedsReauth => "Account: sign in again".to_owned(),
            AccountState::SignedOut => "Account: signed out".to_owned(),
            _ => format!("Account and status: {}, {}", a.name.as_deref().unwrap_or("unknown"), presence.get().label()),
        })
    };

    let close = move |refocus: bool| {
        open.set(false);
        if refocus && let Some(b) = button_ref.get_untracked() {
            let _ = b.focus();
        }
    };
    let toggle = move |_| {
        let now = !open.get_untracked();
        open.set(now);
        if now {
            draft.set(message.get_untracked().unwrap_or_default());
            request_animation_frame(move || {
                if let Some(m) = menu_ref.get_untracked()
                    && let Ok(Some(el)) = m.query_selector("[aria-checked=true], [role=menuitemradio]")
                    && let Ok(el) = el.dyn_into::<web_sys::HtmlElement>()
                {
                    let _ = el.focus();
                }
            });
        }
    };
    let choose = move |value: &'static str, p: Presence| {
        let minutes = DURATIONS.get(duration.get_untracked()).and_then(|d| d.1);
        let mut params = json!({ "availability": value });
        if let (Some(m), Some(obj)) = (minutes, params.as_object_mut()) {
            obj.insert("duration_minutes".into(), json!(m));
        }
        optimistic.set((value != "reset").then_some(p));
        if value != "reset" {
            state.account.update(|a| a.presence = p);
        }
        close(true);
        spawn_local(async move {
            if !run("presence.set", params).await {
                optimistic.set(None);
            }
            me.refetch();
        });
    };
    // Drop the optimistic value once the store reports a real one.
    Effect::new(move |_| {
        if me.data().is_some() {
            optimistic.set(None);
        }
    });
    let app = app_state();
    let save_message = move || {
        let text: String = draft.get_untracked().trim().chars().take(MAX_MESSAGE).collect();
        let exp = EXPIRIES.get(expiry.get_untracked()).and_then(|e| expiry_iso(e.1));
        spawn_local(async move {
            let ok = if text.is_empty() {
                run("presence.clear_status_message", Value::Null).await
            } else {
                run("presence.set_status_message", json!({ "message": text, "expires": exp })).await
            };
            if ok {
                app.toast(ToastLevel::Success, "Status message updated");
            }
            me.refetch();
        });
        close(true);
    };
    let clear_message = move || {
        draft.set(String::new());
        spawn_local(async move {
            run("presence.clear_status_message", Value::Null).await;
            me.refetch();
        });
    };
    let set_keepalive = move |on: bool| {
        spawn_local(async move {
            run("presence.keepalive", json!({ "enabled": on })).await;
            keepalive.refetch();
        });
    };
    let outside = window_event_listener(leptos::ev::pointerdown, move |e| {
        if !open.get_untracked() {
            return;
        }
        let target = e.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
        let inside = |el: Option<web_sys::Element>| el.zip(target.as_ref()).is_some_and(|(el, t)| el.contains(Some(t)));
        let in_menu = inside(menu_ref.get_untracked().map(Into::into));
        let in_button = inside(button_ref.get_untracked().map(Into::into));
        if !in_menu && !in_button {
            open.set(false);
        }
    });
    on_cleanup(move || outside.remove());

    let option = move |(value, text, p): (&'static str, &'static str, Presence)| {
        let checked = move || presence.get() == p;
        view! {
            <button
                class="pp-option"
                role="menuitemradio"
                aria-checked=move || if checked() { "true" } else { "false" }
                on:click=move |_| choose(value, p)
            >
                <span class=format!("pp-dot presence-{}", p.id()) aria-hidden="true"></span>
                <span class="pp-option-label">{text}</span>
                <span class="pp-check" aria-hidden="true">{move || if checked() { "✓" } else { "" }}</span>
            </button>
        }
    };

    view! {
        <div class="presence-picker">
            <button
                class="account-button"
                node_ref=button_ref
                aria-label=label
                title=label
                aria-haspopup="menu"
                aria-expanded=move || if open.get() { "true" } else { "false" }
                on:click=toggle
            >
                {move || view! { <Avatar name=account_name() size="sm" presence=presence /> }}
            </button>
            <Show when=move || open.get()>
                <div
                    class="pp-menu"
                    role="menu"
                    aria-label="Status"
                    node_ref=menu_ref
                    on:keydown=move |e: web_sys::KeyboardEvent| {
                        if e.key() == "Escape" {
                            e.prevent_default();
                            e.stop_propagation();
                            close(true);
                        } else if let Some(m) = menu_ref.get_untracked() {
                            let el: web_sys::Element = m.into();
                            if !crate::dom::is_editable(e.target().as_ref()) {
                                menu_keys(&el, &e);
                            }
                        }
                    }
                >
                    <div class="pp-head">
                        <Avatar name=account_name() size="md" presence=presence />
                        <div class="pp-head-text">
                            <span class="pp-name">{account_name}</span>
                            <span class="pp-email">{account_email}</span>
                            <span class="pp-current">{move || presence.get().label()}</span>
                        </div>
                    </div>
                    <div class="pp-section" role="group" aria-label="Presence">
                        {CHOICES.iter().copied().map(option).collect_view()}
                        <label class="pp-row">
                            <span class="pp-row-label">"For"</span>
                            <select
                                class="pp-select"
                                aria-label="Presence duration"
                                on:change=move |e| duration.set(select_index(&e))
                            >
                                {DURATIONS
                                    .iter()
                                    .enumerate()
                                    .map(|(i, (l, _))| view! { <option value=i.to_string() selected=move || duration.get() == i>{*l}</option> })
                                    .collect_view()}
                            </select>
                        </label>
                        <button class="pp-option pp-reset" data-menu-item="" on:click=move |_| choose("reset", Presence::Unknown)>
                            <span class="pp-dot presence-unknown" aria-hidden="true"></span>
                            <span class="pp-option-label">"Reset status"</span>
                        </button>
                    </div>
                    <div class="pp-section" role="group" aria-label="Status message">
                        <textarea
                            class="pp-message"
                            rows="2"
                            maxlength=MAX_MESSAGE.to_string()
                            placeholder="Set a status message"
                            aria-label="Status message"
                            prop:value=move || draft.get()
                            on:input=move |e| draft.set(input_value(&e))
                            on:keydown=move |e: web_sys::KeyboardEvent| {
                                if e.key() == "Enter" && !e.shift_key() {
                                    e.prevent_default();
                                    save_message();
                                }
                            }
                        ></textarea>
                        <div class="pp-row">
                            <label class="pp-row-label" for="pp-expiry">"Clear after"</label>
                            <select id="pp-expiry" class="pp-select" on:change=move |e| expiry.set(select_index(&e))>
                                {EXPIRIES
                                    .iter()
                                    .enumerate()
                                    .map(|(i, (l, _))| view! { <option value=i.to_string() selected=move || expiry.get() == i>{*l}</option> })
                                    .collect_view()}
                            </select>
                            <span class="pp-count">{move || format!("{}/{MAX_MESSAGE}", draft.with(|d| d.chars().count()))}</span>
                        </div>
                        <div class="pp-buttons">
                            <button class="pp-button" data-menu-item="" disabled=move || message.get().is_none() && draft.with(String::is_empty) on:click=move |_| clear_message()>
                                "Clear"
                            </button>
                            <button class="pp-button primary" data-menu-item="" on:click=move |_| save_message()>"Save"</button>
                        </div>
                    </div>
                    <div class="pp-section">
                        <button
                            class="pp-toggle"
                            role="menuitemcheckbox"
                            aria-checked=move || if keep_on.get() { "true" } else { "false" }
                            on:click=move |_| set_keepalive(!keep_on.get_untracked())
                        >
                            <span class="pp-toggle-text">"Keep me available while Quorum is open"</span>
                            <span class="pp-switch" class:on=move || keep_on.get() aria-hidden="true"><span class="pp-knob"></span></span>
                        </button>
                    </div>
                </div>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_match_engine_values() {
        let values: Vec<&str> = CHOICES.iter().map(|c| c.0).collect();
        assert_eq!(values, ["available", "busy", "dnd", "brb", "away", "offline"]);
        assert!(DURATIONS.iter().all(|d| d.1.is_none_or(|m| (1..=10_080).contains(&m))));
    }
}
