//! Chat: chat list (search, filters, pinned first, unread bold, presence) | conversation
//! (grouped bubbles, day separators, reactions, hover actions, load older, "new messages" pill)
//! with the composer. New chat (Cmd+N) opens a people picker in the conversation pane.

use std::sync::Arc;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::Reply;
use serde::Deserialize;
use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use super::chat_icons::{CIcon, ChatIcon};
use super::chat_logic::{list_label, parse_time};
use super::chat_view::{Action, MessageRow, at_bottom, now_and_offset, render_messages, scroll_to_bottom};
use super::common::{AreaLayout, Row, nothing_selected};
use crate::components::person_card::PersonAvatar;
use crate::components::{Avatar, EmptyState, Presence, Spinner, VirtualList};
use crate::compose_chat::logic::Candidate;
use crate::compose_chat::{Composer, DraftSpec, Editing, Outgoing};
use crate::icons::IconName;
use crate::state::{Area, ToastLevel, app_state, use_query};

/// Store tables chat queries depend on.
pub const CHAT_TABLES: &[&str] = &["chat", "chat_member", "chat_message", "presence", "draft", "ui_prefs"];

/// A chat member.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MemberRow {
    /// User id.
    #[serde(default)]
    pub id: Option<String>,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// The signed-in user.
    #[serde(default)]
    pub me: bool,
}

/// A chat (`chat.list`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ChatRow {
    /// Chat id.
    pub id: String,
    /// `oneOnOne`, `group`, `meeting`.
    #[serde(default)]
    pub kind: String,
    /// Display title.
    #[serde(default)]
    pub title: String,
    /// Members.
    #[serde(default)]
    pub members: Vec<MemberRow>,
    /// Last message preview.
    #[serde(default)]
    pub preview: String,
    /// Last sender.
    #[serde(default)]
    pub from_name: String,
    /// Last message was yours.
    #[serde(default)]
    pub from_me: bool,
    /// Last activity (RFC 3339).
    #[serde(default)]
    pub time: Option<String>,
    /// Unread.
    #[serde(default)]
    pub unread: bool,
    /// Pinned to the top.
    #[serde(default)]
    pub pinned: bool,
    /// Being created.
    #[serde(default)]
    pub pending: bool,
    /// Has a saved draft.
    #[serde(default)]
    pub draft: bool,
    /// Presence of the other person (1:1).
    #[serde(default)]
    pub presence: Option<String>,
    /// User id the presence belongs to.
    #[serde(default)]
    pub presence_id: Option<String>,
}

impl Row for ChatRow {
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// A page of `chat.messages`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct MessagesPage {
    /// Messages, oldest first.
    #[serde(default)]
    pub items: Vec<MessageRow>,
    /// More cached or on the server.
    #[serde(default)]
    pub has_more: bool,
    /// More only on the server (`chat.load_older` fetches them).
    #[serde(default)]
    pub server_has_more: bool,
    /// The chat has been synced at least once.
    #[serde(default)]
    pub synced: bool,
}

/// Chat list filters.
const FILTERS: &[(&str, &str)] =
    &[("all", "All"), ("unread", "Unread"), ("pinned", "Pinned"), ("group", "Groups"), ("meeting", "Meetings")];

fn keep(c: &ChatRow, filter: &str, q: &str) -> bool {
    let f = match filter {
        "unread" => c.unread,
        "pinned" => c.pinned,
        "group" => c.kind == "group",
        "meeting" => c.kind == "meeting",
        _ => true,
    };
    f && (q.is_empty() || c.title.to_lowercase().contains(q) || c.preview.to_lowercase().contains(q))
}

/// Avatar for a chat: the other person with presence, or two stacked initials for groups.
fn chat_avatar(c: &ChatRow, size: &'static str) -> AnyView {
    if c.kind == "oneOnOne" {
        if let Some(user) = c.presence_id.clone().filter(|id| !id.is_empty()) {
            let hint = c.presence.clone().unwrap_or_default();
            return view! { <PersonAvatar user=user name=c.title.clone() size=size presence=hint /> }.into_any();
        }
        let p = Presence::parse(c.presence.as_deref().unwrap_or(""));
        return view! { <Avatar name=c.title.clone() size=size presence=Signal::stored(p) /> }.into_any();
    }
    let names =
        super::chat_logic::group_avatar_names(&c.members.iter().map(|m| (m.name.clone(), m.me)).collect::<Vec<_>>());
    match (names.first(), names.get(1)) {
        (Some(a), Some(b)) => view! {
            <span class=format!("group-avatar group-avatar-{size}") role="img" aria-label=c.title.clone()>
                <Avatar name=a.clone() size="sm" />
                <Avatar name=b.clone() size="sm" />
            </span>
        }
        .into_any(),
        _ => view! { <Avatar name=c.title.clone() size=size /> }.into_any(),
    }
}

fn chat_row(c: ChatRow) -> AnyView {
    let (now, off) = now_and_offset();
    let when = c.time.as_deref().and_then(parse_time).map(|t| list_label(t, now, off)).unwrap_or_default();
    let preview = if c.draft {
        view! { <span class="chat-draft">"Draft"</span> }.into_any()
    } else {
        let who = if c.from_me {
            "You: ".to_owned()
        } else if c.kind != "oneOnOne" && !c.from_name.is_empty() {
            format!("{}: ", c.from_name.split_whitespace().next().unwrap_or(""))
        } else {
            String::new()
        };
        view! { <span>{who}{c.preview.clone()}</span> }.into_any()
    };
    view! {
        <div class="item-row chat-row" class:unread=c.unread class:pending=c.pending>
            {chat_avatar(&c, "md")}
            <div class="item-text">
                <div class="item-line">
                    <span class="item-primary">{c.title.clone()}</span>
                    {c.pinned.then(|| view! { <span class="chat-pin" title="Pinned"><CIcon name=ChatIcon::Pin /></span> })}
                    <span class="item-meta">{when}</span>
                </div>
                <div class="item-secondary">
                    {preview}
                    {c.unread.then(|| view! { <span class="unread-dot" aria-label="Unread"></span> })}
                </div>
            </div>
        </div>
    }
    .into_any()
}

/// Renders a list row that follows later changes to the same chat (unread, preview, presence):
/// the virtual list keys rows by position and id, so it doesn't re-render changed data itself.
fn live_row(initial: ChatRow, chats: crate::state::Query<Vec<ChatRow>>) -> AnyView {
    let id = initial.id.clone();
    let row = Memo::new(move |_| {
        chats.data().and_then(|d| d.iter().find(|c| c.id == id).cloned()).unwrap_or_else(|| initial.clone())
    });
    (move || chat_row(row.get())).into_any()
}

/// Runs an engine command; failures toast.
pub fn run_cmd(id: &'static str, params: Value) {
    let state = app_state();
    spawn_local(async move {
        if let Reply::Err { message, .. } = crate::ipc::run(id, params).await {
            state.toast(ToastLevel::Error, message);
        }
    });
}

type KeyHandler = std::rc::Rc<dyn Fn(&web_sys::KeyboardEvent) -> bool>;

thread_local! {
    static AREA_KEYS: std::cell::RefCell<Vec<(Area, u64, KeyHandler)>> = const { std::cell::RefCell::new(Vec::new()) };
    static INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static NEXT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn install_area_listener() {
    if INSTALLED.with(std::cell::Cell::get) {
        return;
    }
    let Some(win) = web_sys::window() else { return };
    // Read the app state now, inside the component's owner: the listener runs outside any
    // reactive owner, where `use_context` would find nothing.
    let state = app_state();
    let cb = Closure::<dyn Fn(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
        if state.palette_open.get_untracked() || e.default_prevented() || e.is_composing() {
            return;
        }
        let area = state.area.get_untracked();
        let handlers: Vec<KeyHandler> =
            AREA_KEYS.with(|k| k.borrow().iter().filter(|(a, _, _)| *a == area).map(|(_, _, h)| h.clone()).collect());
        if handlers.iter().any(|h| h(&e)) {
            e.prevent_default();
            e.stop_propagation();
        }
    });
    if win.add_event_listener_with_callback_and_bool("keydown", cb.as_ref().unchecked_ref(), true).is_ok() {
        INSTALLED.with(|i| i.set(true));
        // One page-lifetime listener; handlers come and go through `AREA_KEYS`.
        cb.forget();
    }
}

/// Installs a capture-phase keydown handler (runs before the global keymap) while the calling
/// component is mounted and its area is current; the handler returns `true` when it handled
/// the key.
pub fn area_keys(area: Area, handler: impl Fn(&web_sys::KeyboardEvent) -> bool + 'static) {
    install_area_listener();
    let id = NEXT.with(|n| {
        let v = n.get().wrapping_add(1);
        n.set(v);
        v
    });
    AREA_KEYS.with(|k| k.borrow_mut().push((area, id, std::rc::Rc::new(handler))));
    on_cleanup(move || AREA_KEYS.with(|k| k.borrow_mut().retain(|(_, i, _)| *i != id)));
}

/// Whether the key event is Cmd+N (Ctrl+N off macOS) without other modifiers.
pub fn is_new_shortcut(e: &web_sys::KeyboardEvent) -> bool {
    let primary = if crate::dom::is_mac() { e.meta_key() } else { e.ctrl_key() };
    primary && !e.shift_key() && !e.alt_key() && e.code() == "KeyN"
}

/// The Chat area.
#[component]
pub fn ChatArea() -> impl IntoView {
    let state = app_state();
    let sel = state.selection(Area::Chat);
    let chats = use_query::<Vec<ChatRow>>("chat.list", || json!({ "limit": 500 }), CHAT_TABLES);
    let filter = RwSignal::new("all".to_owned());
    let search = RwSignal::new(String::new());
    let new_chat = RwSignal::new(false);
    let shown = Signal::derive(move || {
        let data = chats.data().unwrap_or_default();
        let f = filter.get();
        let q = search.get().trim().to_lowercase();
        Arc::new(data.iter().filter(|c| keep(c, &f, &q)).cloned().collect::<Vec<_>>())
    });
    // What the list body shows; a memo so typing in the search box doesn't remount the list.
    let list_state = Memo::new(move |_| {
        match chats.state.with(|s| match s {
            crate::state::QueryState::Loading => 0u8,
            crate::state::QueryState::Failed { .. } => 1,
            crate::state::QueryState::Ready(_) => 4,
        }) {
            4 if shown.with(|v| v.is_empty()) => {
                if search.with(String::is_empty) && filter.with(|f| f == "all") {
                    2
                } else {
                    3
                }
            }
            n => n,
        }
    });
    let current = Signal::derive(move || {
        let id = sel.item.get()?;
        chats.data().and_then(|d| d.iter().find(|c| c.id == id).cloned())
    });

    // Opening a chat: tell the engine (focus, fast polling, mark read) once per selection.
    let opened = StoredValue::new(None::<String>);
    Effect::new(move |_| {
        let id = sel.item.get();
        if id.is_some() && new_chat.get_untracked() {
            new_chat.set(false);
        }
        if id.is_some() && opened.get_value() != id {
            opened.set_value(id.clone());
            if let Some(id) = id.clone() {
                spawn_local(async move {
                    let _ = crate::ipc::run("chat.open", json!({ "id": id })).await;
                });
            }
        }
        super::people_ui::report(
            "chat",
            id.into_iter().collect(),
            if new_chat.get_untracked() { "new" } else { "conversation" },
        );
    });

    // Keep presence fresh for the people shown in the list.
    let watched = StoredValue::new(Vec::<String>::new());
    Effect::new(move |_| {
        let ids: Vec<String> = chats
            .data()
            .map(|d| d.iter().filter_map(|c| c.presence_id.clone()).take(100).collect())
            .unwrap_or_default();
        if !ids.is_empty() && watched.get_value() != ids {
            watched.set_value(ids.clone());
            spawn_local(async move {
                let _ = crate::ipc::run("presence.watch", json!({ "user_ids": ids })).await;
            });
        }
    });

    area_keys(Area::Chat, move |e| {
        if is_new_shortcut(e) {
            new_chat.set(true);
            request_animation_frame(|| {
                let _ = crate::dom::focus_selector(".new-chat-to input");
            });
            return true;
        }
        false
    });

    let list = move || {
        view! {
            <div class="list-pane chat-list-pane">
                <header class="chat-list-header">
                    <h2 class="list-title">"Chat"</h2>
                    <button
                        class="icon-button"
                        type="button"
                        title="New chat (⌘N)"
                        aria-label="New chat"
                        on:click=move |_| {
                            new_chat.set(true);
                            request_animation_frame(|| {
                                let _ = crate::dom::focus_selector(".new-chat-to input");
                            });
                        }
                    >
                        <CIcon name=ChatIcon::Compose />
                    </button>
                </header>
                <div class="chat-search">
                    <input
                        type="search"
                        placeholder="Search chats"
                        aria-label="Search chats"
                        prop:value=move || search.get()
                        on:input=move |e| search.set(event_target_value(&e))
                        on:keydown=move |e: web_sys::KeyboardEvent| {
                            if e.key() == "Escape" {
                                search.set(String::new());
                                let _ = crate::dom::focus_selector("[data-primary-list]");
                            } else if e.key() == "ArrowDown" {
                                e.prevent_default();
                                let _ = crate::dom::focus_selector("[data-primary-list]");
                            }
                        }
                    />
                </div>
                <div class="chip-row" role="radiogroup" aria-label="Filter chats">
                    {FILTERS
                        .iter()
                        .map(|(id, label)| {
                            let id: &'static str = id;
                            view! {
                                <button
                                    class="chip"
                                    type="button"
                                    role="radio"
                                    aria-checked=move || if filter.get() == id { "true" } else { "false" }
                                    class:selected=move || filter.get() == id
                                    on:click=move |_| filter.set(id.to_owned())
                                >
                                    {*label}
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
                <div class="list-body">
                    {move || match list_state.get() {
                        0 => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
                        1 => view! { <EmptyState title="Couldn't load your chats" error=true /> }.into_any(),
                        2 => view! { <EmptyState title="No chats yet" icon=IconName::Chat /> }.into_any(),
                        3 => view! { <EmptyState title="No matching chats" icon=IconName::Chat /> }.into_any(),
                        _ => view! {
                            <div class="list-wrap chat-list">
                                <VirtualList
                                    items=shown
                                    key=|c: &ChatRow| c.id.clone()
                                    render=Callback::new(move |c: ChatRow| live_row(c, chats))
                                    selected=sel.item
                                    label="Chats"
                                    primary=true
                                    on_activate=Callback::new(|_| {
                                        let _ = crate::dom::focus_selector(".conversation .composer-editor");
                                    })
                                />
                            </div>
                        }
                        .into_any(),
                    }}
                </div>
            </div>
        }
    };

    let current_id = Memo::new(move |_| current.with(|c| c.as_ref().map(|c| c.id.clone())));
    let detail = move || {
        move || {
            if new_chat.get() {
                return view! { <NewChat open=new_chat selected=sel.item /> }.into_any();
            }
            // Keyed on the id: list updates (previews, presence) refresh the header in place
            // instead of remounting the conversation and losing its scroll position.
            match current_id.get() {
                None => nothing_selected("Select a chat", IconName::Chat),
                Some(_) => match current.get_untracked() {
                    Some(c) => view! { <Conversation chat=c live=current /> }.into_any(),
                    None => nothing_selected("Select a chat", IconName::Chat),
                },
            }
        }
    };

    view! { <AreaLayout id="chat" list_label="Chats" list=list detail_label="Conversation" detail=detail /> }
}

/// The open conversation.
#[component]
fn Conversation(chat: ChatRow, live: Signal<Option<ChatRow>>) -> impl IntoView {
    let state = app_state();
    let id = chat.id.clone();
    let limit = RwSignal::new(50u32);
    let qid = id.clone();
    let page =
        use_query::<MessagesPage>("chat.messages", move || json!({ "chat": qid, "limit": limit.get() }), CHAT_TABLES);
    let scroller = NodeRef::<html::Div>::new();
    let stick = StoredValue::new(true);
    let new_pill = RwSignal::new(false);
    let last_id = StoredValue::new(None::<String>);
    let first_id = StoredValue::new(None::<String>);
    let prev_height = StoredValue::new(0i32);
    let loading_older = StoredValue::new(false);
    let editing = RwSignal::new(None::<Editing>);
    let quote = RwSignal::new(None::<MessageRow>);
    let first_render = StoredValue::new(true);

    // After each data change: stick to the bottom, keep position when older messages were
    // prepended, or show the "new messages" pill.
    Effect::new(move |_| {
        let Some(p) = page.data() else { return };
        let new_last = p.items.last().map(|m| m.id.clone());
        let new_first = p.items.first().map(|m| m.id.clone());
        let appended = new_last != last_id.get_value() && last_id.get_value().is_some();
        let prepended = new_first != first_id.get_value() && first_id.get_value().is_some() && !appended;
        let mine_last = p.items.last().is_some_and(|m| m.mine);
        last_id.set_value(new_last);
        first_id.set_value(new_first);
        let first = first_render.get_value();
        first_render.set_value(false);
        request_animation_frame(move || {
            let Some(el) = scroller.try_get_untracked().flatten() else { return };
            let el: web_sys::HtmlElement = el.into();
            if first {
                scroll_to_bottom(scroller, false);
                // Jump to a message named by `chat.open {message}` (search results).
                if let Some(target) = state
                    .open_request
                    .get_untracked()
                    .and_then(|r| r.get("message").and_then(Value::as_str).map(str::to_owned))
                    && let Ok(Some(m)) = el.query_selector(&format!("[data-message=\"{}\"]", target.replace('"', "")))
                {
                    m.scroll_into_view();
                    let _ = m.set_attribute("data-flash", "");
                }
            } else if prepended {
                let grown = el.scroll_height() - prev_height.get_value();
                el.set_scroll_top(el.scroll_top() + grown.max(0));
            } else if stick.get_value() || (appended && mine_last) {
                scroll_to_bottom(scroller, true);
            } else if appended {
                new_pill.set(true);
            }
            loading_older.set_value(false);
        });
    });

    let chat_id = id.clone();
    let on_scroll = move |_| {
        let Some(el) = scroller.try_get_untracked().flatten() else { return };
        let el: web_sys::HtmlElement = el.into();
        let bottom = at_bottom(&el);
        stick.set_value(bottom);
        if bottom {
            new_pill.set(false);
        }
        if el.scroll_top() < 120 {
            let Some(p) = page.data() else { return };
            if !p.has_more || loading_older.get_value() {
                return;
            }
            loading_older.set_value(true);
            prev_height.set_value(el.scroll_height());
            let cached = u32::try_from(p.items.len()).unwrap_or(u32::MAX);
            if cached < limit.get_untracked() && p.server_has_more {
                run_cmd("chat.load_older", json!({ "chat": chat_id.clone() }));
            }
            limit.update(|l| *l = l.saturating_add(50).min(1000));
        }
    };

    let cid = id.clone();
    let on_action = Callback::new(move |a: Action| {
        let chat = cid.clone();
        match a {
            Action::React { id, emoji, remove } => {
                let cmd = if remove { "chat.unreact" } else { "chat.react" };
                run_cmd(cmd, json!({ "chat": chat, "message": id, "reaction": emoji }));
            }
            Action::Reply(mid) => {
                let m = page.data().and_then(|p| p.items.iter().find(|m| m.id == mid).cloned());
                quote.set(m);
                let _ = crate::dom::focus_selector(".conversation .composer-editor");
            }
            Action::Edit(mid) => {
                if let Some(m) = page.data().and_then(|p| p.items.iter().find(|m| m.id == mid).cloned()) {
                    editing.set(Some(Editing { id: m.id, html: m.html }));
                }
            }
            Action::Delete(mid) => run_cmd("chat.delete", json!({ "chat": chat, "message": mid })),
            Action::OpenFile(url) => super::chat_view::open_url(url),
        }
    });

    let sid = id.clone();
    let on_send = Callback::new(move |out: Outgoing| {
        let mut params = out.into_params(json!({ "chat": sid.clone() }));
        if let (Some(q), Some(o)) = (quote.get_untracked(), params.as_object_mut()) {
            o.insert("quote".into(), json!(q.id));
        }
        quote.set(None);
        stick.set_value(true);
        run_cmd("chat.send", params);
    });
    let eid = id.clone();
    let on_edit = Callback::new(move |(ed, out): (Editing, Outgoing)| {
        let params = out.into_params(json!({ "chat": eid.clone(), "message": ed.id }));
        run_cmd("chat.edit", params);
    });
    let on_up = Callback::new(move |()| {
        let last = page.data().and_then(|p| p.items.iter().rev().find(|m| m.mine && !m.deleted && !m.pending).cloned());
        if let Some(m) = last {
            editing.set(Some(Editing { id: m.id, html: m.html }));
        }
    });

    let people: Vec<Candidate> = chat
        .members
        .iter()
        .filter(|m| !m.me)
        .filter_map(|m| Some(Candidate { id: m.id.clone()?, name: m.name.clone() }))
        .collect();
    let draft = DraftSpec { query: "chat.draft", save: "chat.save_draft", params: json!({ "chat": id.clone() }) };
    let placeholder =
        if chat.kind == "oneOnOne" { format!("Message {}", chat.title) } else { "Type a message".to_owned() };
    let row = Signal::derive({
        let initial = chat.clone();
        move || live.get().filter(|c| c.id == initial.id).unwrap_or_else(|| initial.clone())
    });
    let subtitle = move || {
        let c = row.get();
        match c.kind.as_str() {
            "oneOnOne" => Presence::parse(c.presence.as_deref().unwrap_or("")).label().to_owned(),
            _ => format!("{} people", c.members.len()),
        }
    };
    let pinned = move || row.with(|c| c.pinned);
    let unread_id = id.clone();
    let pin_id = id.clone();

    view! {
        <section class="conversation" aria-label=format!("Conversation: {}", chat.title)>
            <header class="conversation-header">
                {move || chat_avatar(&row.get(), "md")}
                <div class="conversation-title">
                    <h1>{move || row.with(|c| c.title.clone())}</h1>
                    <span class="conversation-subtitle">{subtitle}</span>
                </div>
                <span class="composer-spacer"></span>
                <button
                    class="icon-button"
                    type="button"
                    class:active=pinned
                    title=move || if pinned() { "Unpin" } else { "Pin" }
                    aria-label=move || if pinned() { "Unpin chat" } else { "Pin chat" }
                    aria-pressed=move || if pinned() { "true" } else { "false" }
                    on:click=move |_| {
                        run_cmd(if pinned() { "chat.unpin" } else { "chat.pin" }, json!({ "id": pin_id.clone() }))
                    }
                >
                    <CIcon name=ChatIcon::Pin />
                </button>
                <button
                    class="icon-button"
                    type="button"
                    title="Mark as unread (⇧⌘U)"
                    aria-label="Mark as unread"
                    on:click=move |_| run_cmd("chat.mark_unread", json!({ "id": unread_id.clone() }))
                >
                    <CIcon name=ChatIcon::More />
                </button>
            </header>
            <div class="messages" node_ref=scroller on:scroll=on_scroll tabindex="-1" aria-label="Messages" role="log">
                {move || match page.data() {
                    None => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
                    Some(p) if p.items.is_empty() => {
                        let title = if p.synced { "No messages yet. Say hello!" } else { "Loading messages…" };
                        view! { <div class="messages-empty"><EmptyState title=title icon=IconName::Chat /></div> }.into_any()
                    }
                    Some(p) => view! {
                        {p.has_more.then(|| view! { <div class="load-older">"Scroll up for older messages"</div> })}
                        {render_messages(&p.items, on_action, Some("Reply"))}
                    }
                    .into_any(),
                }}
            </div>
            {move || {
                new_pill
                    .get()
                    .then(|| {
                        view! {
                            <button
                                class="new-messages-pill"
                                type="button"
                                on:click=move |_| {
                                    new_pill.set(false);
                                    scroll_to_bottom(scroller, true);
                                }
                            >
                                <CIcon name=ChatIcon::ChevronDown />
                                "New messages"
                            </button>
                        }
                    })
            }}
            {move || {
                quote
                    .get()
                    .map(|q| {
                        view! {
                            <div class="composer-quote">
                                <CIcon name=ChatIcon::Quote />
                                <div class="composer-quote-text">
                                    <strong>{q.from_name.clone()}</strong>
                                    <span>{crate::areas::chat_view::strip_tags(&q.html)}</span>
                                </div>
                                <button class="icon-button small" type="button" aria-label="Remove quote" on:click=move |_| quote.set(None)>
                                    "×"
                                </button>
                            </div>
                        }
                    })
            }}
            <Composer
                draft=Signal::stored(Some(draft))
                placeholder=Signal::stored(placeholder)
                people=Signal::stored(people)
                on_send=on_send
                editing=editing
                on_edit=on_edit
                on_up=on_up
                label="Message"
            />
        </section>
    }
}

/// New chat: pick people (directory search), optional topic and first message.
#[component]
fn NewChat(open: RwSignal<bool>, selected: RwSignal<Option<String>>) -> impl IntoView {
    let state = app_state();
    let query = RwSignal::new(String::new());
    let results = RwSignal::new(Vec::<Candidate>::new());
    let picked = RwSignal::new(Vec::<Candidate>::new());
    let topic = RwSignal::new(String::new());
    let active = RwSignal::new(0usize);
    let search_gen = StoredValue::new(0u64);

    let search = move |q: String| {
        let n = search_gen.get_value().wrapping_add(1);
        search_gen.set_value(n);
        if q.trim().is_empty() {
            results.set(Vec::new());
            return;
        }
        spawn_local(async move {
            if let Reply::Ok { value: Value::Array(rows) } =
                crate::ipc::query("people.search", json!({ "query": q, "limit": 8 })).await
                && search_gen.try_get_value() == Some(n)
            {
                let found: Vec<Candidate> = rows
                    .iter()
                    .filter_map(|r| {
                        Some(Candidate {
                            id: r
                                .get("user_id")
                                .and_then(Value::as_str)
                                .or_else(|| r.get("email").and_then(Value::as_str))?
                                .to_owned(),
                            name: r.get("display_name").or_else(|| r.get("name")).and_then(Value::as_str)?.to_owned(),
                        })
                    })
                    .collect();
                active.set(0);
                results.try_set(found);
            }
        });
    };
    let add = move |c: Candidate| {
        picked.update(|p| {
            if !p.iter().any(|x| x.id == c.id) {
                p.push(c);
            }
        });
        query.set(String::new());
        results.set(Vec::new());
    };
    let on_send = Callback::new(move |out: Outgoing| {
        let members: Vec<String> = picked.get_untracked().into_iter().map(|c| c.id).collect();
        if members.is_empty() {
            state.toast(ToastLevel::Warning, "Add at least one person first.");
            return;
        }
        let t = topic.get_untracked();
        let mut params = json!({ "members": members, "message": out.html });
        if let (false, Some(o)) = (t.trim().is_empty(), params.as_object_mut()) {
            o.insert("topic".into(), json!(t));
        }
        spawn_local(async move {
            match crate::ipc::run("chat.new", params).await {
                Reply::Ok { value } => {
                    open.set(false);
                    if let Some(id) = value.get("id").and_then(Value::as_str) {
                        selected.set(Some(id.to_owned()));
                    }
                }
                Reply::Err { message, .. } => state.toast(ToastLevel::Error, message),
            }
        });
    });
    let editing = RwSignal::new(None::<Editing>);
    view! {
        <section class="conversation new-chat" aria-label="New chat">
            <header class="conversation-header">
                <div class="conversation-title"><h1>"New chat"</h1></div>
                <span class="composer-spacer"></span>
                <button class="icon-button" type="button" aria-label="Close" on:click=move |_| open.set(false)>"×"</button>
            </header>
            <div class="new-chat-to">
                <label>"To:"</label>
                {move || {
                    picked
                        .get()
                        .into_iter()
                        .map(|c| {
                            let rid = c.id.clone();
                            view! {
                                <span class="person-chip">
                                    {c.name.clone()}
                                    <button
                                        type="button"
                                        class="link-button"
                                        aria-label=format!("Remove {}", c.name)
                                        on:click=move |_| picked.update(|p| p.retain(|x| x.id != rid))
                                    >
                                        "×"
                                    </button>
                                </span>
                            }
                        })
                        .collect_view()
                }}
                <input
                    type="text"
                    placeholder="Enter a name or email"
                    aria-label="People to chat with"
                    autocomplete="off"
                    prop:value=move || query.get()
                    on:input=move |e| {
                        let v = event_target_value(&e);
                        query.set(v.clone());
                        search(v);
                    }
                    on:keydown=move |e: web_sys::KeyboardEvent| {
                        let n = results.with_untracked(Vec::len);
                        match e.key().as_str() {
                            "ArrowDown" if n > 0 => {
                                e.prevent_default();
                                active.update(|i| *i = (*i + 1) % n);
                            }
                            "ArrowUp" if n > 0 => {
                                e.prevent_default();
                                active.update(|i| *i = (*i + n - 1) % n);
                            }
                            "Enter" => {
                                e.prevent_default();
                                let i = active.get_untracked();
                                if let Some(c) = results.with_untracked(|r| r.get(i).cloned()) {
                                    add(c);
                                } else {
                                    let q = query.get_untracked();
                                    if q.contains('@') && !q.contains(' ') {
                                        add(Candidate { id: q.trim().to_owned(), name: q.trim().to_owned() });
                                    }
                                }
                            }
                            "Backspace" if query.with_untracked(String::is_empty) => {
                                picked.update(|p| {
                                    p.pop();
                                });
                            }
                            "Escape" => {
                                e.prevent_default();
                                open.set(false);
                            }
                            _ => {}
                        }
                    }
                />
            </div>
            {move || {
                let list = results.get();
                (!list.is_empty())
                    .then(|| {
                        view! {
                            <ul class="people-results" role="listbox" aria-label="People">
                                {list
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, c)| {
                                        let pick = c.clone();
                                        view! {
                                            <li
                                                role="option"
                                                class="mention-option"
                                                class:selected=move || active.get() == i
                                                aria-selected=move || if active.get() == i { "true" } else { "false" }
                                                on:mousedown=move |e| {
                                                    e.prevent_default();
                                                    add(pick.clone());
                                                }
                                            >
                                                <PersonAvatar user=c.id.clone() name=c.name.clone() size="sm" no_card=true />
                                                <span>{c.name.clone()}</span>
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                        }
                    })
            }}
            {move || {
                (picked.with(Vec::len) > 1)
                    .then(|| {
                        view! {
                            <input
                                class="composer-subject"
                                type="text"
                                placeholder="Group name (optional)"
                                aria-label="Group name"
                                maxlength="250"
                                prop:value=move || topic.get()
                                on:input=move |e| topic.set(event_target_value(&e))
                            />
                        }
                    })
            }}
            <div class="messages new-chat-empty">
                <EmptyState title="Start a new conversation" icon=IconName::Chat />
            </div>
            <Composer
                draft=Signal::stored(None)
                placeholder=Signal::stored("Type your first message".to_owned())
                people=Signal::derive(move || picked.get())
                on_send=on_send
                editing=editing
                label="First message"
            />
        </section>
    }
}
