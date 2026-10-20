//! Live people: the presence hub, [`PersonAvatar`] (an [`Avatar`] whose presence dot and photo
//! stay current), and the person card popover.
//!
//! - **Presence hub.** Every mounted [`PersonAvatar`] registers its user id; the hub sends the
//!   visible set to `presence.watch` (debounced, and again every minute so the engine's TTL
//!   does not lapse) and reads `presence.of`, refreshing when the store announces `presence`.
//!   Photos come from `people.photo` (data URLs), cached per id and refreshed on `photo`.
//! - **Person card.** [`open_card`] (clicking any [`PersonAvatar`], or the engine's
//!   `people.open` command via the UI command `people.card.show`) shows a popover with photo,
//!   presence, title, department, Email / Chat / Meet buttons and an org snippet.
//!   Esc or a click outside closes it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::{CoreEvent, Reply};
use serde_json::{Value, json};

use super::avatar::{Avatar, Presence};
use super::icon::Icon;
use crate::icons::IconName;
use crate::ipc;
use crate::state::{Area, app_state, use_query};

/// Delay before a batch of newly visible ids is sent (debounce).
pub const WATCH_DEBOUNCE: Duration = Duration::from_millis(250);
/// How often visible ids are re-sent so the engine keeps watching them.
pub const WATCH_REFRESH: Duration = Duration::from_secs(60);
/// Most ids per `presence.watch` / `presence.of` call.
pub const MAX_IDS_PER_CALL: usize = 650;

/// Live presence of one person.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PresenceInfo {
    /// The dot.
    pub presence: Presence,
    /// Current status message, if any.
    pub status: Option<String>,
}

/// Where the card points.
#[derive(Debug, Clone, PartialEq)]
pub struct CardTarget {
    /// User id or `mail:<address>`.
    pub user: String,
    /// Anchor in viewport pixels (left, bottom of the clicked element), if any.
    pub anchor: Option<(f64, f64)>,
}

/// The shared presence/photo cache and the open card. `Copy`.
#[derive(Debug, Clone, Copy)]
pub struct Hub {
    presence: RwSignal<HashMap<String, PresenceInfo>>,
    photos: RwSignal<HashMap<String, Option<String>>>,
    visible: StoredValue<HashMap<String, u32>>,
    flush_pending: StoredValue<bool>,
    refresh_pending: StoredValue<bool>,
    /// The open person card.
    pub card: RwSignal<Option<CardTarget>>,
}

thread_local! {
    static HUB: RefCell<Option<Hub>> = const { RefCell::new(None) };
}

/// The person-card id for a mail address (`mail:<lower-cased address>`), or `None` when
/// there is no plausible address.
pub fn mail_user(address: &str) -> Option<String> {
    let a = address.trim();
    (!a.is_empty() && a.len() <= 320 && a.contains('@') && !a.chars().any(|c| c.is_control() || c.is_whitespace()))
        .then(|| format!("mail:{}", a.to_lowercase()))
}

/// Whether `id` is a directory user id worth watching (not a `mail:` placeholder).
pub fn is_watchable(id: &str) -> bool {
    !id.is_empty() && !id.starts_with("mail:") && id.len() <= 1024 && !id.chars().any(char::is_control)
}

/// Parses a `presence.of` reply into the hub's form (pure, for tests).
pub fn parse_presence_map(value: &Value) -> HashMap<String, PresenceInfo> {
    let mut out = HashMap::new();
    if let Some(map) = value.as_object() {
        for (id, v) in map.iter().take(MAX_IDS_PER_CALL) {
            let presence = Presence::parse(v.get("presence").and_then(Value::as_str).unwrap_or(""));
            let status = v.get("status_message").and_then(Value::as_str).map(str::to_owned).filter(|s| !s.is_empty());
            out.insert(id.clone(), PresenceInfo { presence, status });
        }
    }
    out
}

/// Creates the hub in the current (root) owner. Call once from the app root; later calls are
/// no-ops.
pub fn init() -> Hub {
    if let Some(h) = HUB.with(|h| h.try_borrow().ok().and_then(|h| *h)) {
        return h;
    }
    let hub = Hub {
        presence: RwSignal::new(HashMap::new()),
        photos: RwSignal::new(HashMap::new()),
        visible: StoredValue::new(HashMap::new()),
        flush_pending: StoredValue::new(false),
        refresh_pending: StoredValue::new(false),
        card: RwSignal::new(None),
    };
    HUB.with(|h| {
        if let Ok(mut slot) = h.try_borrow_mut() {
            *slot = Some(hub);
        }
    });
    ipc::subscribe(move |ev| {
        if let CoreEvent::StoreChanged { tables } = ev {
            let any = |name: &str| tables.iter().any(|t| t == name || t == "*");
            if any("presence") {
                hub.schedule_refresh();
            }
            if any("photo") || any("person") {
                hub.retry_missing_photos();
            }
        }
    });
    set_interval(move || hub.flush(), WATCH_REFRESH);
    hub
}

/// The hub (created on first use if the root did not).
pub fn hub() -> Hub {
    HUB.with(|h| h.try_borrow().ok().and_then(|h| *h)).unwrap_or_else(init)
}

/// Opens the person card for `user`, anchored at `anchor` (viewport px) if given.
pub fn open_card(user: &str, anchor: Option<(f64, f64)>) {
    let user = user.trim();
    if user.is_empty() || user.len() > 1024 {
        return;
    }
    hub().card.set(Some(CardTarget { user: user.to_owned(), anchor }));
}

/// Closes the person card.
pub fn close_card() {
    hub().card.set(None);
}

impl Hub {
    /// Live presence of `id` (tracked).
    pub fn presence_of(&self, id: &str) -> Option<PresenceInfo> {
        self.presence.with(|m| m.get(id).cloned())
    }

    /// The photo data URL of `id` (tracked); the first call starts loading it.
    pub fn photo_of(&self, id: &str) -> Option<String> {
        let known = self.photos.with_untracked(|m| m.contains_key(id));
        if !known && is_watchable(id) {
            self.photos.update_untracked(|m| {
                m.insert(id.to_owned(), None);
            });
            self.load_photo(id.to_owned());
        }
        self.photos.with(|m| m.get(id).cloned().flatten())
    }

    fn load_photo(&self, id: String) {
        let photos = self.photos;
        spawn_local(async move {
            if let Reply::Ok { value: Value::String(url) } = ipc::query("people.photo", json!({ "user": id })).await
                && url.starts_with("data:image/")
            {
                photos.try_update(|m| {
                    m.insert(id, Some(url));
                });
            }
        });
    }

    fn retry_missing_photos(&self) {
        let visible: HashSet<String> = self.visible.with_value(|v| v.keys().cloned().collect());
        let missing: Vec<String> = self.photos.with_untracked(|m| {
            m.iter().filter(|(k, v)| v.is_none() && visible.contains(*k)).map(|(k, _)| k.clone()).collect()
        });
        for id in missing {
            self.load_photo(id);
        }
    }

    /// Registers a visible person (ref-counted).
    pub fn watch(&self, id: &str) {
        if !is_watchable(id) {
            return;
        }
        let mut fresh = false;
        self.visible.update_value(|v| {
            let n = v.entry(id.to_owned()).or_insert(0);
            fresh = *n == 0;
            *n = n.saturating_add(1);
        });
        if fresh {
            self.schedule_flush();
        }
    }

    /// Unregisters a person that left the screen.
    pub fn unwatch(&self, id: &str) {
        let _ = self.visible.try_update_value(|v| {
            if let Some(n) = v.get_mut(id) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    v.remove(id);
                }
            }
        });
    }

    fn visible_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.visible.try_with_value(|v| v.keys().cloned().collect()).unwrap_or_default();
        ids.sort();
        ids
    }

    fn schedule_flush(&self) {
        if self.flush_pending.try_get_value().unwrap_or(true) {
            return;
        }
        self.flush_pending.set_value(true);
        let hub = *self;
        set_timeout(
            move || {
                hub.flush_pending.set_value(false);
                hub.flush();
            },
            WATCH_DEBOUNCE,
        );
    }

    fn schedule_refresh(&self) {
        if self.refresh_pending.try_get_value().unwrap_or(true) {
            return;
        }
        self.refresh_pending.set_value(true);
        let hub = *self;
        set_timeout(
            move || {
                hub.refresh_pending.set_value(false);
                hub.refresh();
            },
            WATCH_DEBOUNCE,
        );
    }

    /// Sends the visible ids to `presence.watch`, then reads their presence.
    fn flush(&self) {
        let ids = self.visible_ids();
        if ids.is_empty() {
            return;
        }
        let hub = *self;
        spawn_local(async move {
            for chunk in ids.chunks(MAX_IDS_PER_CALL) {
                let _ = ipc::run("presence.watch", json!({ "user_ids": chunk })).await;
            }
            hub.read(ids).await;
        });
    }

    fn refresh(&self) {
        let ids = self.visible_ids();
        if ids.is_empty() {
            return;
        }
        let hub = *self;
        spawn_local(async move { hub.read(ids).await });
    }

    async fn read(&self, ids: Vec<String>) {
        for chunk in ids.chunks(MAX_IDS_PER_CALL) {
            if let Reply::Ok { value } = ipc::query("presence.of", json!({ "user_ids": chunk })).await {
                let found = parse_presence_map(&value);
                self.presence.try_update(|m| m.extend(found));
            }
        }
    }
}

/// An [`Avatar`] for a person: live presence dot, photo when there is one, and a click that
/// opens the person card. Registers itself with the presence hub while mounted.
#[component]
pub fn PersonAvatar(
    /// User id (or `mail:<address>`).
    #[prop(into)]
    user: String,
    /// Display name.
    #[prop(into)]
    name: String,
    /// Size modifier: `sm`, `md` (default) or `lg`.
    #[prop(optional)]
    size: Option<&'static str>,
    /// Presence to show until the live value arrives (short id or Graph availability).
    #[prop(optional, into)]
    presence: Option<String>,
    /// Don't open the card on click (inside the card itself).
    #[prop(optional)]
    no_card: bool,
) -> impl IntoView {
    let hub = hub();
    hub.watch(&user);
    let cleanup_id = user.clone();
    on_cleanup(move || hub.unwatch(&cleanup_id));
    let hint = Presence::parse(presence.as_deref().unwrap_or(""));
    let id_p = user.clone();
    let live = Signal::derive(move || hub.presence_of(&id_p).map_or(hint, |p| p.presence));
    let id_photo = user.clone();
    let photo = Memo::new(move |_| hub.photo_of(&id_photo));
    let open_id = user.clone();
    let on_click = move |e: leptos::ev::MouseEvent| {
        if no_card {
            return;
        }
        e.stop_propagation();
        let anchor = anchor_of(&e);
        open_card(&open_id, anchor);
    };
    view! {
        <span class="person-avatar" class:clickable=!no_card on:mousedown=on_click>
            {move || {
                let name = name.clone();
                match photo.get() {
                    Some(src) => view! { <Avatar name=name photo=src size=size.unwrap_or("md") presence=live /> }.into_any(),
                    None => view! { <Avatar name=name size=size.unwrap_or("md") presence=live /> }.into_any(),
                }
            }}
        </span>
    }
}

/// The bottom-left corner of the event's target, in viewport pixels.
pub fn anchor_of(e: &leptos::ev::MouseEvent) -> Option<(f64, f64)> {
    use wasm_bindgen::JsCast;
    let el = e.current_target()?.dyn_into::<web_sys::Element>().ok()?;
    let r = el.get_bounding_client_rect();
    Some((r.left(), r.bottom()))
}

/// Clamps the card's top-left so a `w`×`h` card fits in a `vw`×`vh` viewport (pure).
pub fn place_card(anchor: Option<(f64, f64)>, w: f64, h: f64, vw: f64, vh: f64) -> (f64, f64) {
    let margin = 8.0;
    let (x, y) = anchor.unwrap_or(((vw - w) / 2.0, 72.0));
    let fin = |v: f64| if v.is_finite() { v } else { 0.0 };
    let (x, y) = (fin(x), fin(y) + 6.0);
    let max_x = (vw - w - margin).max(margin);
    let y = if y + h > vh - margin { (y - h - 40.0).max(margin) } else { y };
    (x.clamp(margin, max_x), y.max(margin))
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn viewport() -> (f64, f64) {
    let w = web_sys::window();
    let dim = |v: Option<Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>>| {
        v.and_then(Result::ok).and_then(|v| v.as_f64()).unwrap_or(1024.0)
    };
    (dim(w.as_ref().map(web_sys::Window::inner_width)), dim(w.as_ref().map(web_sys::Window::inner_height)))
}

/// Renders the open person card (mount once, in the app root).
#[component]
pub fn PersonCardHost() -> impl IntoView {
    let hub = init();
    let key = window_event_listener(leptos::ev::keydown, move |e| {
        if e.key() == "Escape" && hub.card.with_untracked(Option::is_some) {
            e.prevent_default();
            e.stop_propagation();
            close_card();
        }
    });
    on_cleanup(move || key.remove());
    move || {
        hub.card.get().map(|target| {
            view! { <PersonCard target=target /> }
        })
    }
}

/// Runs a person command for `user`, then closes the card.
fn act(state: crate::state::AppState, id: &'static str, user: String) {
    crate::commands::dispatch(state, id, json!({ "user": user }));
    close_card();
}

#[component]
fn PersonCard(target: CardTarget) -> impl IntoView {
    let hub = hub();
    let user = target.user.clone();
    let q_user = user.clone();
    let details =
        use_query::<Value>("people.person", move || json!({ "user": q_user }), &["person", "presence", "photo"]);
    let (vw, vh) = viewport();
    let (x, y) = place_card(target.anchor, 340.0, 300.0, vw, vh);
    let style = format!("left: {x:.0}px; top: {y:.0}px;");
    let state = app_state();
    let body = move || {
        let Some(d) = details.data() else {
            let failed = details.state.with(|s| matches!(s, crate::state::QueryState::Failed { .. }));
            return if failed {
                view! { <p class="person-card-empty">"This person couldn't be loaded."</p> }.into_any()
            } else {
                view! { <div class="person-card-loading"><crate::components::Spinner /></div> }.into_any()
            };
        };
        let p = d.get("person").cloned().unwrap_or(Value::Null);
        let id = text(&p, "id");
        let name = text(&p, "display_name");
        let presence = Presence::parse(&text(&p, "presence"));
        let status = text(&p, "status_message");
        let title = text(&p, "job_title");
        let dept = text(&p, "department");
        let email = text(&p, "email");
        let role = [title, dept].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
        let manager = d.get("managers").and_then(Value::as_array).and_then(|m| m.first()).cloned();
        let reports = d.get("reports").and_then(Value::as_array).map_or(0, Vec::len);
        let live_status = {
            let id = id.clone();
            move || hub.presence_of(&id).map_or(presence, |p| p.presence).label()
        };
        let (e_id, c_id, m_id, o_id) = (id.clone(), id.clone(), id.clone(), id.clone());
        view! {
            <div class="person-card-head">
                <PersonAvatar user=id.clone() name=name.clone() size="lg" presence=text(&p, "presence") no_card=true />
                <div class="person-card-id">
                    <h2 class="person-card-name" id="person-card-title">{name.clone()}</h2>
                    <p class="person-card-status">{live_status}</p>
                    {(!status.is_empty()).then(|| view! { <p class="person-card-message">{status}</p> })}
                    {(!role.is_empty()).then(|| view! { <p class="person-card-role">{role}</p> })}
                </div>
            </div>
            <div class="person-card-actions">
                <button class="pc-action" disabled=email.is_empty() on:click=move |_| act(state, "people.email", e_id.clone())>
                    <Icon name=IconName::Mail /><span>"Email"</span>
                </button>
                <button class="pc-action" on:click=move |_| act(state, "people.chat", c_id.clone())>
                    <Icon name=IconName::Chat /><span>"Chat"</span>
                </button>
                <button class="pc-action" disabled=email.is_empty() on:click=move |_| act(state, "people.schedule", m_id.clone())>
                    <Icon name=IconName::Calendar /><span>"Meet"</span>
                </button>
            </div>
            {(!email.is_empty()).then(|| view! {
                <dl class="person-card-facts"><dt>"Email"</dt><dd class="selectable">{email.clone()}</dd></dl>
            })}
            <div class="person-card-org">
                {manager.map(|m| {
                    let mid = text(&m, "id");
                    let mname = text(&m, "display_name");
                    view! {
                        <button class="pc-org-link" on:click=move |e| open_card(&mid, anchor_of(&e))>
                            <span class="pc-org-label">"Reports to"</span>
                            <span class="pc-org-name">{mname}</span>
                        </button>
                    }
                })}
                {(reports > 0).then(|| view! {
                    <span class="pc-org-reports">{format!("{reports} direct report{}", if reports == 1 { "" } else { "s" })}</span>
                })}
            </div>
            <button
                class="pc-open"
                on:click=move |_| {
                    state.go(Area::People);
                    state.selection(Area::People).item.set(Some(o_id.clone()));
                    close_card();
                }
            >
                "Open profile"
                <Icon name=IconName::ChevronRight />
            </button>
        }
        .into_any()
    };
    request_animation_frame(|| {
        let _ = crate::dom::focus_selector(".person-card .pc-action");
    });
    view! {
        <div class="person-card-backdrop" on:click=move |_| close_card()></div>
        <div
            class="person-card-popover"
            role="dialog"
            aria-labelledby="person-card-title"
            style=style
            on:click=|e| e.stop_propagation()
        >
            <div class="person-card">{body}</div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchable_ids() {
        assert!(is_watchable("00000000-0000-0000-0000-000000000001"));
        assert!(!is_watchable("mail:a@example.com"));
        assert!(!is_watchable(""));
        assert!(!is_watchable("bad\u{7}"));
    }

    #[test]
    fn presence_map_is_lenient() {
        let v = json!({ "a": { "presence": "busy", "status_message": "hi" }, "b": 5, "c": { "presence": "nope" } });
        let m = parse_presence_map(&v);
        assert_eq!(m.get("a").map(|p| p.presence), Some(Presence::Busy));
        assert_eq!(m.get("a").and_then(|p| p.status.clone()).as_deref(), Some("hi"));
        assert_eq!(m.get("b").map(|p| p.presence), Some(Presence::Unknown));
        assert!(parse_presence_map(&json!([1, 2])).is_empty());
    }

    #[test]
    fn card_stays_on_screen() {
        let (x, y) = place_card(Some((1000.0, 700.0)), 340.0, 300.0, 1024.0, 768.0);
        assert!(x + 340.0 <= 1024.0 && y >= 8.0 && y + 300.0 <= 768.0);
        let (x, _) = place_card(Some((f64::NAN, f64::INFINITY)), 340.0, 300.0, 1024.0, 768.0);
        assert!(x.is_finite());
        let (x, y) = place_card(None, 340.0, 300.0, 200.0, 100.0);
        assert!(x >= 8.0 && y >= 8.0);
    }
}
