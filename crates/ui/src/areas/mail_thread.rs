//! The Mail conversation view: every message of the selected message's conversation as a card
//! (your replies from Sent Items too), Outlook-on-the-web style with Apple Mail polish.
//!
//! - Read older messages collapse to one line (sender, preview, time); the selected message,
//!   unread ones and the newest open expanded. Click or Enter toggles a card.
//! - Each reply shows only its new text; the quoted history the core split off
//!   (`body_new_html` vs `body_html`) is one click away ("Show quoted text").
//! - Bodies render in sandboxed, script-free `srcdoc` frames sized to their content (ADR 0006
//!   addendum: `allow-same-origin` without `allow-scripts`, so the UI can measure them). In dark
//!   mode plain mail is colour-inverted to match; designed mail stays on a white page.
//! - Per-message actions (reply, reply all, forward, flag, delete) and a quick reply at the
//!   end whose text autosaves as a server draft on every pause.
//! - Keyboard: `N`/`P` next/previous message, `Shift+E`/`Shift+C` expand/collapse all, ↑/↓
//!   between cards, Enter to toggle. All are `view.thread.*` commands (palette, MCP).

use std::cell::Cell;
use std::collections::{BTreeSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::Reply;
use serde::Deserialize;
use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use super::mail::{category_class, mail_ui, now_and_offset, open_compose, show_undo};
use super::mail_icons::{MIcon, MailIcon};
use super::mail_read::{Address, Attachment, AttachmentChip, RemoteImagesBar};
use super::mail_util::{
    conversation_topic, display_name, initially_expanded, long_time, plain_to_html, short_time, thread_frame_document,
};
use crate::components::person_card::PersonAvatar;
use crate::components::{Avatar, EmptyState, Icon, Spinner};
use crate::icons::IconName;
use crate::state::{AppState, QueryState, ToastLevel, app_state, use_query};

/// Tables whose changes refresh the conversation.
pub const THREAD_TABLES: &[&str] = &["message", "message_body", "attachment", "mail_folder", "setting"];
/// How many times a missing body is polled for before giving up.
const MAX_BODY_POLLS: u32 = 25;
/// Quick-reply autosave delay after typing pauses.
const AUTOSAVE: Duration = Duration::from_millis(1000);
/// Local storage key of the conversation order.
const ORDER_KEY: &str = "quorum.mail.thread-order";

/// A message of the conversation (`mail.conversation`).
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct ThreadMessage {
    /// Message id.
    pub id: String,
    /// Subject.
    #[serde(default)]
    pub subject: String,
    /// Sender.
    #[serde(default)]
    pub from_address: Option<Address>,
    /// Sender name (row fallback).
    #[serde(default)]
    pub from_name: String,
    /// Sender address (row fallback).
    #[serde(default)]
    pub from_email: String,
    /// To.
    #[serde(default)]
    pub to_recipients: Vec<Address>,
    /// Cc.
    #[serde(default)]
    pub cc_recipients: Vec<Address>,
    /// Received (RFC 3339).
    #[serde(default)]
    pub received: Option<String>,
    /// Preview text.
    #[serde(default)]
    pub preview: String,
    /// Unread.
    #[serde(default)]
    pub unread: bool,
    /// Flagged.
    #[serde(default)]
    pub flagged: bool,
    /// A draft.
    #[serde(default)]
    pub is_draft: bool,
    /// Has attachments.
    #[serde(default)]
    pub has_attachments: bool,
    /// `high` importance.
    #[serde(default)]
    pub importance: String,
    /// Categories.
    #[serde(default)]
    pub categories: Vec<String>,
    /// The full sanitised body; absent until downloaded (or for very old messages).
    #[serde(default)]
    pub body_html: Option<String>,
    /// The body without its quoted history, when it has some.
    #[serde(default)]
    pub body_new_html: Option<String>,
    /// There is quoted history to show.
    #[serde(default)]
    pub has_quoted: bool,
    /// The mail paints its own backgrounds (keep it on a white page in dark mode).
    #[serde(default)]
    pub own_background: bool,
    /// The body is being downloaded.
    #[serde(default)]
    pub body_loading: bool,
    /// Remote images were blocked.
    #[serde(default)]
    pub remote_images_blocked: bool,
    /// Attachments (not inline images).
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// `inbox`, `sent`, `drafts`, `deleted`, `archive`, `junk`, `outbox` or `folder`.
    #[serde(default)]
    pub folder_kind: String,
    /// You sent it (or it is your draft).
    #[serde(default)]
    pub from_me: bool,
}

impl ThreadMessage {
    /// (display name, address) of the sender.
    fn sender(&self) -> (String, String) {
        let from = self.from_address.clone().unwrap_or_default();
        let addr = from.address.clone().filter(|a| !a.is_empty()).unwrap_or_else(|| self.from_email.clone());
        let name = from.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| self.from_name.clone());
        (display_name(&name, &addr), addr)
    }

    /// Changes whenever what the card shows changes (the card's key).
    fn fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        (
            &self.id,
            self.flagged,
            self.unread,
            self.body_html.as_ref().map(String::len),
            self.body_new_html.as_ref().map(String::len),
            self.remote_images_blocked,
            self.attachments.len(),
            &self.categories,
            &self.folder_kind,
        )
            .hash(&mut h);
        h.finish()
    }
}

/// Handles a `view.thread.*` command: the open conversation picks it up.
pub fn run_ui(id: &str) -> bool {
    let cmd: &'static str = match id {
        "view.thread.next" => "next",
        "view.thread.previous" => "previous",
        "view.thread.expand_all" => "expand_all",
        "view.thread.collapse_all" => "collapse_all",
        "view.thread.quoted" => "quoted",
        "view.thread.order" => "order",
        "view.thread.reply" => "reply",
        _ => return false,
    };
    mail_ui().thread_cmd.update(|(n, c)| {
        *n = n.wrapping_add(1);
        *c = cmd;
    });
    true
}

fn recipients_line(list: &[Address]) -> String {
    list.iter()
        .map(|a| display_name(a.name.as_deref().unwrap_or_default(), a.address.as_deref().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("; ")
}

fn toggle(set: RwSignal<BTreeSet<String>>, id: &str) {
    set.update(|s| {
        if !s.remove(id) {
            s.insert(id.to_owned());
        }
    });
}

/// The card heads in document order and the index of the focused one.
fn heads() -> (Vec<web_sys::Element>, Option<usize>) {
    let all = crate::dom::query_all(".thread .tcard-head");
    let active = crate::dom::active_element();
    let at = active.and_then(|a| all.iter().position(|h| *h == a));
    (all, at)
}

fn focus_head(el: &web_sys::Element) {
    let _ = crate::dom::focus_element(el);
    let opts = web_sys::ScrollIntoViewOptions::new();
    opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
    el.scroll_into_view_with_scroll_into_view_options(&opts);
}

/// The conversation of message `id`.
#[component]
pub fn ThreadPane(
    /// The message selected in the list.
    id: String,
) -> impl IntoView {
    let state = app_state();
    let ui = mail_ui();
    let tauri = crate::ipc::backend() == crate::ipc::Backend::Tauri;
    let remote = ui.remote_once.clone();
    let qid = id.clone();
    let thread = use_query::<Vec<ThreadMessage>>(
        "mail.conversation",
        move || {
            let ids: Vec<String> = remote.with(|s| s.iter().cloned().collect());
            json!({ "message": qid.clone(), "allow_remote_ids": ids, "webview": tauri })
        },
        THREAD_TABLES,
    );

    // Bodies download in the background: poll briefly until they arrive.
    let polls = StoredValue::new(0u32);
    Effect::new(move |_| {
        let loading = thread.state.with(|s| s.data().is_some_and(|d| d.iter().any(|m| m.body_loading)));
        if loading && polls.get_value() < MAX_BODY_POLLS {
            polls.update_value(|n| *n = n.saturating_add(1));
            set_timeout(move || thread.refetch(), Duration::from_millis(1200));
        }
    });

    let newest_first = RwSignal::new(crate::dom::storage_get(ORDER_KEY).is_none_or(|v| v != "oldest"));
    let expanded = RwSignal::new(BTreeSet::<String>::new());
    let quoted = RwSignal::new(BTreeSet::<String>::new());
    let light = RwSignal::new(BTreeSet::<String>::new());
    let reply_mode = RwSignal::new(None::<&'static str>);
    let dark = crate::dom::is_dark();

    // Which cards open: on first load the selected, unread and newest; later, new unread mail.
    let seen = StoredValue::new(None::<BTreeSet<String>>);
    let selected = id.clone();
    Effect::new(move |_| {
        let Some(data) = thread.data() else { return };
        let ids: Vec<(String, bool)> = data.iter().map(|m| (m.id.clone(), m.unread)).collect();
        let first = seen.with_value(Option::is_none);
        let open: Vec<String> = if first {
            initially_expanded(&ids, &selected)
        } else {
            let known = seen.with_value(|s| s.clone().unwrap_or_default());
            ids.iter().filter(|(id, unread)| *unread && !known.contains(id)).map(|(id, _)| id.clone()).collect()
        };
        if !open.is_empty() {
            expanded.update(|s| s.extend(open));
        }
        seen.set_value(Some(ids.into_iter().map(|(id, _)| id).collect()));
        if first {
            // Bring the selected card into view when it isn't the first one.
            let sel = selected.clone();
            request_animation_frame(move || {
                let cards = crate::dom::query_all(".thread .tcard");
                if let Some(card) = cards.iter().find(|c| c.get_attribute("data-selected").is_some())
                    && cards.first() != Some(card)
                {
                    let opts = web_sys::ScrollIntoViewOptions::new();
                    opts.set_block(web_sys::ScrollLogicalPosition::Start);
                    card.scroll_into_view_with_scroll_into_view_options(&opts);
                }
                let _ = sel;
            });
        }
    });

    // Opening an unread card marks it read (the list already marks the selected message).
    let marked = StoredValue::new(BTreeSet::<String>::new());
    let selected_for_read = id.clone();
    Effect::new(move |_| {
        let open = expanded.get();
        let Some(data) = thread.data() else { return };
        for m in data.iter() {
            if !m.unread || m.is_draft || m.id == selected_for_read || !open.contains(&m.id) {
                continue;
            }
            if marked.with_value(|s| s.contains(&m.id)) {
                continue;
            }
            marked.update_value(|s| {
                s.insert(m.id.clone());
            });
            let mid = m.id.clone();
            set_timeout(
                move || {
                    if expanded.try_with_untracked(|s| s.contains(&mid)).unwrap_or(false) {
                        crate::commands::dispatch(state, "mail.mark_read", json!({ "id": mid }));
                    }
                },
                Duration::from_millis(900),
            );
        }
    });

    // `view.thread.*` commands (keys, palette, MCP).
    let last_cmd = StoredValue::new(ui.thread_cmd.get_untracked().0);
    let cmd = ui.thread_cmd.clone();
    Effect::new(move |_| {
        let (n, c) = cmd.get();
        if last_cmd.get_value() == n {
            return;
        }
        last_cmd.set_value(n);
        let Some(data) = thread.data() else { return };
        match c {
            "expand_all" => expanded.set(data.iter().map(|m| m.id.clone()).collect()),
            "collapse_all" => expanded.set(BTreeSet::new()),
            "order" => flip_order(newest_first),
            "reply" => {
                reply_mode.set(Some("reply"));
                request_animation_frame(|| {
                    let _ = crate::dom::focus_selector(".quick-reply-text");
                    if let Some(el) = crate::dom::query_all(".quick-reply").first() {
                        let opts = web_sys::ScrollIntoViewOptions::new();
                        opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
                        el.scroll_into_view_with_scroll_into_view_options(&opts);
                    }
                });
            }
            "quoted" => {
                let open = expanded.get_untracked();
                let targets: Vec<String> =
                    data.iter().filter(|m| m.has_quoted && open.contains(&m.id)).map(|m| m.id.clone()).collect();
                let all_shown = quoted.with_untracked(|q| targets.iter().all(|t| q.contains(t)));
                quoted.update(|q| {
                    for t in &targets {
                        if all_shown {
                            q.remove(t);
                        } else {
                            q.insert(t.clone());
                        }
                    }
                });
            }
            "next" | "previous" => {
                let (all, at) = heads();
                let next = match (c, at) {
                    ("next", Some(i)) => i.saturating_add(1).min(all.len().saturating_sub(1)),
                    ("previous", Some(i)) => i.saturating_sub(1),
                    _ => 0,
                };
                if let Some(h) = all.get(next) {
                    if let Some(mid) = h.get_attribute("data-mid") {
                        expanded.update(|s| {
                            s.insert(mid);
                        });
                    }
                    focus_head(h);
                }
            }
            _ => {}
        }
    });

    let ordered = Memo::new(move |_| {
        let mut v: Vec<ThreadMessage> = thread.data().map(|d| (*d).clone()).unwrap_or_default();
        if newest_first.get() {
            v.reverse();
        }
        v
    });
    let status = Memo::new(move |_| match thread.state.get() {
        QueryState::Loading => 0u8,
        QueryState::Failed { .. } => 1,
        QueryState::Ready(_) => 2,
    });
    let error = move || match thread.state.get() {
        QueryState::Failed { message, .. } => message,
        _ => String::new(),
    };

    let card_keys = move |e: web_sys::KeyboardEvent| {
        let key = e.key();
        if !matches!(key.as_str(), "ArrowDown" | "ArrowUp" | "Home" | "End") {
            return;
        }
        let on_head = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|t| t.class_list().contains("tcard-head"));
        if !on_head {
            return;
        }
        let (all, at) = heads();
        let Some(i) = at else { return };
        let next = match key.as_str() {
            "ArrowDown" => i.saturating_add(1).min(all.len().saturating_sub(1)),
            "ArrowUp" => i.saturating_sub(1),
            "Home" => 0,
            _ => all.len().saturating_sub(1),
        };
        e.prevent_default();
        e.stop_propagation();
        if let Some(h) = all.get(next) {
            focus_head(h);
        }
    };

    let selected_id = id.clone();
    move || match status.get() {
        0 => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
        1 => view! { <EmptyState title="Couldn't open this conversation" detail=error() error=true /> }.into_any(),
        _ => {
            let data = thread.data().map(|d| (*d).clone()).unwrap_or_default();
            if data.is_empty() {
                return view! { <EmptyState title="This message is no longer available" detail=String::new() /> }
                    .into_any();
            }
            let newest = data.last().cloned().unwrap_or_default();
            let topic = conversation_topic(&newest.subject);
            let count = data.len();
            let mut people: Vec<String> = Vec::new();
            for m in data.iter().rev() {
                let who = if m.from_me { "You".to_owned() } else { m.sender().0 };
                if !who.is_empty() && !people.contains(&who) {
                    people.push(who);
                }
            }
            let shown_people = people.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
            let more_people = people.len().saturating_sub(5);
            let categories: BTreeSet<String> = data.iter().flat_map(|m| m.categories.iter().cloned()).collect();
            let important = data.iter().any(|m| m.importance == "high");
            // Quick reply goes to the newest message that isn't a draft.
            let target = data.iter().rev().find(|m| !m.is_draft).cloned();
            let all_ids: Vec<String> = data.iter().map(|m| m.id.clone()).collect();
            let all_open = {
                let all_ids = all_ids.clone();
                Signal::derive(move || expanded.with(|s| all_ids.iter().all(|i| s.contains(i))))
            };
            let sel = selected_id.clone();
            view! {
                <article class="thread" tabindex="-1" data-pane-focus="" aria-label=topic.clone()>
                    <header class="thread-header">
                        <div class="thread-title-row">
                            <h1 class="thread-subject">
                                {important.then(|| view! { <span class="mail-important" title="High importance"><MIcon name=MailIcon::Important /></span> })}
                                {topic.clone()}
                            </h1>
                            <div class="thread-tools">
                                {(count > 1).then(|| {
                                    let all_ids = all_ids.clone();
                                    view! {
                                        <button
                                            class="thread-tool"
                                            title=move || if all_open.get() { "Collapse all (Shift+C)" } else { "Expand all (Shift+E)" }
                                            aria-label=move || if all_open.get() { "Collapse all messages" } else { "Expand all messages" }
                                            on:click=move |_| {
                                                if all_open.get_untracked() {
                                                    expanded.set(BTreeSet::new());
                                                } else {
                                                    expanded.set(all_ids.iter().cloned().collect());
                                                }
                                            }
                                        >
                                            {move || if all_open.get() { view! { <MIcon name=MailIcon::CollapseAll /> }.into_any() } else { view! { <MIcon name=MailIcon::ExpandAll /> }.into_any() }}
                                        </button>
                                        <button
                                            class="thread-tool"
                                            title=move || if newest_first.get() { "Newest on top" } else { "Oldest on top" }
                                            aria-label=move || if newest_first.get() { "Newest on top; show oldest first" } else { "Oldest on top; show newest first" }
                                            on:click=move |_| flip_order(newest_first)
                                        >
                                            <MIcon name=MailIcon::Sort />
                                        </button>
                                    }
                                })}
                            </div>
                        </div>
                        {(count > 1).then(|| view! {
                            <div class="thread-meta">
                                <span class="thread-count"><MIcon name=MailIcon::Conversation />{format!("{count} messages")}</span>
                                <span class="thread-people">
                                    {shown_people.clone()}
                                    {(more_people > 0).then(|| format!(" and {more_people} more"))}
                                </span>
                            </div>
                        })}
                        {(!categories.is_empty()).then(|| view! {
                            <div class="mail-categories">
                                {categories.iter().map(|c| view! { <span class=format!("mail-category {}", category_class(c))>{c.clone()}</span> }).collect_view()}
                            </div>
                        })}
                    </header>
                    <div class="thread-cards" role="list" aria-label="Messages in this conversation" on:keydown=card_keys>
                        <For
                            each=move || ordered.get()
                            key=|m| m.fingerprint()
                            children=move |m: ThreadMessage| {
                                let is_sel = m.id == sel;
                                view! {
                                    <ThreadCard
                                        m=m
                                        selected=is_sel
                                        expanded=expanded
                                        quoted=quoted
                                        light=light
                                        dark=dark
                                    />
                                }
                            }
                        />
                    </div>
                    {target.map(|t| view! { <QuickReply target=t mode=reply_mode /> })}
                </article>
            }
            .into_any()
        }
    }
}

fn flip_order(newest_first: RwSignal<bool>) {
    newest_first.update(|v| *v = !*v);
    crate::dom::storage_set(ORDER_KEY, if newest_first.get_untracked() { "newest" } else { "oldest" });
}

/// One message of the conversation.
#[component]
fn ThreadCard(
    m: ThreadMessage,
    selected: bool,
    expanded: RwSignal<BTreeSet<String>>,
    quoted: RwSignal<BTreeSet<String>>,
    light: RwSignal<BTreeSet<String>>,
    dark: bool,
) -> impl IntoView {
    let state = app_state();
    let id = m.id.clone();
    let is_open = {
        let id = id.clone();
        Signal::derive(move || expanded.with(|s| s.contains(&id)))
    };
    let (name, addr) = m.sender();
    let shown_name = if m.from_me && !m.is_draft { format!("{name} (you)") } else { name.clone() };
    let (now, offset) = now_and_offset();
    let short = m.received.as_deref().map(|t| short_time(t, now, offset)).unwrap_or_default();
    let long = m.received.as_deref().map(|t| long_time(t, offset)).unwrap_or_default();
    let chip: Option<(&'static str, &'static str)> = match (m.is_draft, m.folder_kind.as_str()) {
        (true, _) | (_, "drafts") => Some(("Draft", "draft")),
        (_, "sent") => Some(("Sent", "sent")),
        (_, "deleted") => Some(("Deleted", "deleted")),
        (_, "junk") => Some(("Junk", "deleted")),
        (_, "outbox") => Some(("Outbox", "sent")),
        _ => None,
    };
    let to = recipients_line(&m.to_recipients);
    let cc = recipients_line(&m.cc_recipients);
    let preview = if m.preview.trim().is_empty() { "(no preview)".to_owned() } else { m.preview.clone() };
    let toggle_id = id.clone();
    let on_toggle = move |_| toggle(expanded, &toggle_id);
    let avatar = match crate::components::person_card::mail_user(&addr).filter(|_| !m.from_me) {
        Some(user) => view! { <PersonAvatar user=user name=name.clone() size="sm" /> }.into_any(),
        None => view! { <Avatar name=name.clone() size="sm" /> }.into_any(),
    };
    let head_label = format!("{shown_name}, {short}");
    let m2 = m.clone();
    let (unread, mine) = (m.unread, m.from_me);
    let long_title = long.clone();
    view! {
        <div
            class="tcard"
            class:expanded=move || is_open.get()
            class:unread=unread
            class:mine=mine
            role="listitem"
            data-selected=selected.then_some("")
        >
            <div class="tcard-top">
                <button
                    class="tcard-head"
                    data-mid=id.clone()
                    aria-expanded=move || is_open.get().to_string()
                    aria-label=head_label
                    on:click=on_toggle
                >
                    {avatar}
                    <span class="tcard-who">
                        <span class="tcard-line">
                            <span class="tcard-name">{shown_name.clone()}</span>
                            {chip.map(|(label, class)| view! { <span class=format!("tcard-chip {class}")>{label}</span> })}
                            {move || is_open.get().then(|| (!addr.is_empty() && addr != name).then(|| view! { <span class="tcard-address">{addr.clone()}</span> }))}
                        </span>
                        {move || if is_open.get() {
                            view! {
                                <span class="tcard-recipients">
                                    {(!to.is_empty()).then(|| view! { <span><span class="tcard-label">"To "</span>{to.clone()}</span> })}
                                    {(!cc.is_empty()).then(|| view! { <span><span class="tcard-label">" Cc "</span>{cc.clone()}</span> })}
                                </span>
                            }.into_any()
                        } else {
                            view! { <span class="tcard-preview">{preview.clone()}</span> }.into_any()
                        }}
                    </span>
                    <span class="tcard-icons">
                        {m.has_attachments.then(|| view! { <span class="item-flag" title="Has attachments"><Icon name=IconName::Paperclip /></span> })}
                        {m.flagged.then(|| view! { <span class="item-flag flagged" title="Flagged"><Icon name=IconName::Flag /></span> })}
                    </span>
                    <time class="tcard-time" title=long_title>{move || if is_open.get() { long.clone() } else { short.clone() }}</time>
                </button>
                {move || is_open.get().then(|| view! { <CardActions m=m2.clone() light=light dark=dark state=state /> })}
            </div>
            {move || is_open.get().then(|| view! { <CardBody m=m.clone() quoted=quoted light=light dark=dark /> })}
        </div>
    }
}

/// Reply, reply all, forward, flag, delete (and the light-page toggle in dark mode).
#[component]
fn CardActions(m: ThreadMessage, light: RwSignal<BTreeSet<String>>, dark: bool, state: AppState) -> impl IntoView {
    let act = move |cmd: &'static str, id: String| {
        move |_| crate::commands::dispatch(state, cmd, json!({ "id": id.clone() }))
    };
    let id = m.id.clone();
    let adapts = dark && !m.own_background;
    let light_id = id.clone();
    let is_light = {
        let id = id.clone();
        Signal::derive(move || light.with(|s| s.contains(&id)))
    };
    view! {
        <div class="tcard-actions">
            {(!m.is_draft).then(|| view! {
                <button class="tcard-action" title="Reply (R)" aria-label="Reply" on:click=act("mail.reply", id.clone())>
                    <Icon name=IconName::Reply />
                </button>
                <button class="tcard-action" title="Reply all (Shift+R)" aria-label="Reply all" on:click=act("mail.reply_all", id.clone())>
                    <MIcon name=MailIcon::ReplyAll />
                </button>
                <button class="tcard-action" title="Forward (Shift+F)" aria-label="Forward" on:click=act("mail.forward", id.clone())>
                    <Icon name=IconName::Forward />
                </button>
            })}
            <button class="tcard-action" class:on=m.flagged title=if m.flagged { "Unflag" } else { "Flag" } aria-label=if m.flagged { "Unflag" } else { "Flag" } on:click=act("mail.flag", id.clone())>
                <Icon name=IconName::Flag />
            </button>
            <button class="tcard-action" title="Delete" aria-label="Delete this message" on:click=act("mail.delete", id.clone())>
                <Icon name=IconName::Trash />
            </button>
            {adapts.then(|| view! {
                <button
                    class="tcard-action"
                    class:on=move || is_light.get()
                    title=move || if is_light.get() { "Show in dark mode" } else { "Show on a light background" }
                    aria-label="Light background"
                    aria-pressed=move || is_light.get().to_string()
                    on:click=move |_| toggle(light, &light_id)
                >
                    <MIcon name=MailIcon::Sun />
                </button>
            })}
        </div>
    }
}

/// Banners, attachments, the body frame and the quoted-text toggle of an open card.
#[component]
fn CardBody(
    m: ThreadMessage,
    quoted: RwSignal<BTreeSet<String>>,
    light: RwSignal<BTreeSet<String>>,
    dark: bool,
) -> impl IntoView {
    let id = m.id.clone();
    let (_, addr) = m.sender();
    let show_quoted = {
        let id = id.clone();
        Memo::new(move |_| quoted.with(|s| s.contains(&id)))
    };
    let is_light = {
        let id = id.clone();
        Memo::new(move |_| light.with(|s| s.contains(&id)))
    };
    let body = {
        let m = m.clone();
        move || {
            let full = m.body_html.clone().filter(|h| !h.is_empty());
            let html = if show_quoted.get() { full.clone() } else { m.body_new_html.clone().or_else(|| full.clone()) };
            match (html, m.body_loading) {
                (Some(html), _) => {
                    let adapt = dark && !m.own_background && !is_light.get();
                    let paper = dark && !adapt;
                    view! { <ThreadFrame html=html adapt=adapt paper=paper /> }.into_any()
                }
                (None, true) => {
                    view! { <div class="tcard-loading"><Spinner label="Downloading message" /></div> }.into_any()
                }
                (None, false) => view! { <div class="tcard-empty">{m.preview.clone()}</div> }.into_any(),
            }
        }
    };
    let quoted_id = id.clone();
    view! {
        <div class="tcard-body">
            {m.remote_images_blocked.then(|| view! { <RemoteImagesBar id=id.clone() sender=addr.clone() /> })}
            {(!m.attachments.is_empty()).then(|| {
                let id = id.clone();
                view! {
                    <div class="mail-attachments tcard-attachments" aria-label="Attachments">
                        {m.attachments.iter().cloned().map(|a| view! { <AttachmentChip message=id.clone() att=a /> }).collect_view()}
                    </div>
                }
            })}
            <div class="tcard-content">{body}</div>
            {m.has_quoted.then(|| view! {
                <button
                    class="tcard-quoted"
                    title=move || if show_quoted.get() { "Hide quoted text" } else { "Show quoted text" }
                    aria-label=move || if show_quoted.get() { "Hide quoted text" } else { "Show quoted text" }
                    aria-pressed=move || show_quoted.get().to_string()
                    on:click=move |_| toggle(quoted, &quoted_id)
                >
                    <MIcon name=MailIcon::Ellipsis />
                </button>
            })}
        </div>
    }
}

type ResizeHandler = Closure<dyn FnMut(js_sys::Array, web_sys::ResizeObserver)>;
type KeyHandler = Closure<dyn FnMut(web_sys::KeyboardEvent)>;

/// A script-free body frame sized to its content.
#[component]
fn ThreadFrame(html: String, adapt: bool, paper: bool) -> impl IntoView {
    let node = NodeRef::<leptos::html::Iframe>::new();
    let observer = StoredValue::new_local(None::<(web_sys::ResizeObserver, ResizeHandler)>);
    let keys = StoredValue::new_local(None::<(web_sys::Window, KeyHandler)>);
    let doc = thread_frame_document(&html, adapt, paper);
    let on_load = move |_| {
        let Some(frame) = node.get_untracked() else { return };
        fit(&frame);
        forward_keys(&frame, keys);
        if observer.try_with_value(Option::is_none).unwrap_or(false) {
            let width = Rc::new(Cell::new(frame.client_width()));
            let watched = frame.clone();
            let handler: ResizeHandler = Closure::new(move |_: js_sys::Array, _: web_sys::ResizeObserver| {
                let w = watched.client_width();
                if w != width.get() {
                    width.set(w);
                    fit(&watched);
                }
            });
            if let Ok(obs) = web_sys::ResizeObserver::new(handler.as_ref().unchecked_ref()) {
                obs.observe(&frame);
                observer.set_value(Some((obs, handler)));
            }
        }
    };
    on_cleanup(move || {
        let _ = observer.try_with_value(|o| {
            if let Some((obs, _)) = o {
                obs.disconnect();
            }
        });
        let _ = keys.try_with_value(|k| {
            if let Some((win, handler)) = k {
                let _ = win.remove_event_listener_with_callback("keydown", handler.as_ref().unchecked_ref());
            }
        });
    });
    view! {
        <iframe
            node_ref=node
            class="tcard-frame"
            class:paper=paper
            sandbox="allow-same-origin"
            referrerpolicy="no-referrer"
            title="Message body"
            srcdoc=doc
            on:load=on_load
        ></iframe>
    }
}

/// Sets the frame's height to its document's (shrinking first so narrower content can shrink).
fn fit(frame: &web_sys::HtmlIFrameElement) {
    let Some(root) = frame.content_document().and_then(|d| d.document_element()) else { return };
    let style = web_sys::HtmlElement::style(frame);
    let _ = style.set_property("height", "1px");
    let h = root.scroll_height().clamp(16, 200_000);
    let _ = style.set_property("height", &format!("{h}px"));
}

/// Keys pressed while the (script-free) frame has focus are re-sent to the app window, so
/// shortcuts keep working while reading. Copy and select-all stay with the frame.
fn forward_keys(
    frame: &web_sys::HtmlIFrameElement,
    slot: StoredValue<Option<(web_sys::Window, KeyHandler)>, LocalStorage>,
) {
    let Some(win) = frame.content_window() else { return };
    let handler: KeyHandler = Closure::new(move |e: web_sys::KeyboardEvent| {
        let primary = e.meta_key() || e.ctrl_key();
        let key = e.key().to_ascii_lowercase();
        if primary && matches!(key.as_str(), "c" | "a" | "x") {
            return;
        }
        let init = web_sys::KeyboardEventInit::new();
        init.set_key(&e.key());
        init.set_code(&e.code());
        init.set_meta_key(e.meta_key());
        init.set_ctrl_key(e.ctrl_key());
        init.set_alt_key(e.alt_key());
        init.set_shift_key(e.shift_key());
        init.set_bubbles(true);
        init.set_cancelable(true);
        let Some(app) = web_sys::window() else { return };
        if let Ok(copy) = web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init)
            && let Ok(false) = app.dispatch_event(&copy)
        {
            e.prevent_default();
        }
    });
    if win.add_event_listener_with_callback("keydown", handler.as_ref().unchecked_ref()).is_ok() {
        // Replace (and detach) any listener from a previous load of this frame.
        let _ = slot.try_with_value(|old| {
            if let Some((w, h)) = old {
                let _ = w.remove_event_listener_with_callback("keydown", h.as_ref().unchecked_ref());
            }
        });
        slot.set_value(Some((win, handler)));
    }
}

/// Reply / Reply all / Forward at the end of the conversation, expanding into an inline reply
/// box whose text is saved as a server draft on every pause.
#[component]
fn QuickReply(target: ThreadMessage, mode: RwSignal<Option<&'static str>>) -> impl IntoView {
    let state = app_state();
    let text = RwSignal::new(String::new());
    let draft = RwSignal::new(None::<String>);
    let status = RwSignal::new("");
    let generation = StoredValue::new(0u64);
    let busy = RwSignal::new(false);
    let target_id = target.id.clone();
    let (name, _) = target.sender();
    let to_label = if target.from_me { "Reply to your message".to_owned() } else { format!("Reply to {name}") };

    let start = move |m: &'static str| {
        mode.set(Some(m));
        request_animation_frame(|| {
            let _ = crate::dom::focus_selector(".quick-reply-text");
            if let Some(el) = crate::dom::query_all(".quick-reply").first() {
                let opts = web_sys::ScrollIntoViewOptions::new();
                opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
                el.scroll_into_view_with_scroll_into_view_options(&opts);
            }
        });
    };
    let fwd_id = target_id.clone();

    // Creates the reply draft on first save, then keeps it current.
    let save = {
        let target_id = target_id.clone();
        move || -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>>>> {
            let target_id = target_id.clone();
            Box::pin(async move {
                let body = plain_to_html(&text.get_untracked());
                let kind = mode.get_untracked().unwrap_or("reply");
                match draft.get_untracked() {
                    Some(d) => {
                        let reply =
                            crate::ipc::run("mail.save_draft", json!({ "draft_id": d, "body_html": body })).await;
                        match reply {
                            Reply::Ok { .. } => {
                                status.try_set("Draft saved");
                                Some(d)
                            }
                            Reply::Err { message, .. } => {
                                state.toast(ToastLevel::Error, message);
                                None
                            }
                        }
                    }
                    None => {
                        let cmd = if kind == "reply_all" { "mail.reply_all" } else { "mail.reply" };
                        match crate::ipc::run(cmd, json!({ "id": target_id, "body_html": body })).await {
                            Reply::Ok { value } => {
                                let d = value.get("draft_id").and_then(Value::as_str).map(str::to_owned);
                                draft.try_set(d.clone());
                                status.try_set("Draft saved");
                                d
                            }
                            Reply::Err { message, .. } => {
                                state.toast(ToastLevel::Error, message);
                                None
                            }
                        }
                    }
                }
            })
        }
    };
    let save_later = {
        let save = save.clone();
        move || {
            let n = generation.get_value().wrapping_add(1);
            generation.set_value(n);
            status.set("");
            let save = save.clone();
            set_timeout(
                move || {
                    if generation.try_get_value() == Some(n) && !text.get_untracked().trim().is_empty() {
                        spawn_local(async move {
                            let _ = save().await;
                        });
                    }
                },
                AUTOSAVE,
            );
        }
    };
    let send = {
        let save = save.clone();
        move || {
            if busy.get_untracked() || text.get_untracked().trim().is_empty() {
                return;
            }
            busy.set(true);
            generation.update_value(|g| *g = g.wrapping_add(1));
            let save = save.clone();
            spawn_local(async move {
                let Some(d) = save().await else {
                    busy.try_set(false);
                    return;
                };
                let body = plain_to_html(&text.get_untracked());
                match crate::ipc::run("mail.send", json!({ "draft_id": d, "body_html": body })).await {
                    Reply::Ok { value } => {
                        let seconds = value.get("delay_seconds").and_then(Value::as_u64).unwrap_or(5);
                        show_undo(d, seconds);
                        text.try_set(String::new());
                        draft.try_set(None);
                        mode.try_set(None);
                        status.try_set("");
                    }
                    Reply::Err { message, .. } => state.toast(ToastLevel::Error, message),
                }
                busy.try_set(false);
            });
        }
    };
    let pop_out = {
        let save = save.clone();
        let target_id = target_id.clone();
        move || {
            generation.update_value(|g| *g = g.wrapping_add(1));
            if text.get_untracked().trim().is_empty() && draft.get_untracked().is_none() {
                let cmd = if mode.get_untracked() == Some("reply_all") { "mail.reply_all" } else { "mail.reply" };
                crate::commands::dispatch(state, cmd, json!({ "id": target_id.clone() }));
                return;
            }
            let save = save.clone();
            spawn_local(async move {
                if let Some(d) = save().await {
                    open_compose(state, &d);
                }
            });
        }
    };
    let discard = move || {
        generation.update_value(|g| *g = g.wrapping_add(1));
        let d = draft.get_untracked();
        text.set(String::new());
        draft.set(None);
        mode.set(None);
        status.set("");
        if let Some(d) = d {
            spawn_local(async move {
                let _ = crate::ipc::run("mail.discard_draft", json!({ "draft_id": d })).await;
            });
        }
    };

    let send_key = send.clone();
    let discard_key = discard;
    let on_keydown = move |e: web_sys::KeyboardEvent| {
        if e.key() == "Enter" && (e.meta_key() || e.ctrl_key()) {
            e.prevent_default();
            e.stop_propagation();
            send_key();
        } else if e.key() == "Escape" && text.get_untracked().trim().is_empty() {
            e.prevent_default();
            e.stop_propagation();
            discard_key();
        }
    };
    let to_label2 = to_label.clone();
    view! {
        <footer class="quick-reply-area">
            {move || match mode.get() {
                None => {
                    let fwd_id = fwd_id.clone();
                    view! {
                        <div class="quick-reply-bar">
                            <button class="mail-button" on:click=move |_| start("reply")><Icon name=IconName::Reply />"Reply"</button>
                            {(!target.from_me).then(|| view! {
                                <button class="mail-button" on:click=move |_| start("reply_all")><MIcon name=MailIcon::ReplyAll />"Reply all"</button>
                            })}
                            <button class="mail-button" on:click=move |_| crate::commands::dispatch(state, "mail.forward", json!({ "id": fwd_id.clone() }))>
                                <Icon name=IconName::Forward />"Forward"
                            </button>
                        </div>
                    }.into_any()
                }
                Some(kind) => {
                    let (send, pop_out) = (send.clone(), pop_out.clone());
                    let save_later = save_later.clone();
                    let label = if kind == "reply_all" { to_label2.replace("Reply to", "Reply all to") } else { to_label2.clone() };
                    view! {
                        <div class="quick-reply">
                            <div class="quick-reply-to">{label}</div>
                            <textarea
                                class="quick-reply-text"
                                rows="4"
                                placeholder="Write a reply… (⌘↩ to send)"
                                aria-label="Reply"
                                prop:value=move || text.get()
                                on:input=move |e| {
                                    text.set(event_target_value(&e));
                                    save_later();
                                }
                                on:keydown=on_keydown.clone()
                            ></textarea>
                            <div class="quick-reply-actions">
                                <button class="mail-button primary" disabled=move || busy.get() || text.with(|t| t.trim().is_empty()) on:click=move |_| send()>
                                    <Icon name=IconName::Send />"Send"
                                </button>
                                <button class="mail-button" title="Open in the full editor" on:click=move |_| pop_out()>"Open in editor"</button>
                                <button class="mail-button" on:click=move |_| discard()>"Discard"</button>
                                <span class="quick-reply-status">{move || status.get()}</span>
                            </div>
                        </div>
                    }.into_any()
                }
            }}
        </footer>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_messages_decode_rows_and_full_messages() {
        let row: ThreadMessage =
            serde_json::from_value(json!({ "id": "a", "from_name": "A", "from_email": "a@example.com" })).unwrap();
        assert_eq!(row.sender(), ("A".to_owned(), "a@example.com".to_owned()));
        assert!(row.body_html.is_none() && !row.has_quoted);
        let full: ThreadMessage = serde_json::from_value(json!({
            "id": "b", "from_address": { "name": null, "address": "b@example.com" }, "body_html": "<p>x</p>",
            "body_new_html": null, "has_quoted": false, "folder_kind": "sent", "from_me": true, "unknown": 1,
        }))
        .unwrap();
        assert_eq!(full.sender().0, "b@example.com");
        assert!(full.from_me);
        assert_ne!(row.fingerprint(), full.fingerprint());
    }

    #[test]
    fn fingerprint_follows_what_the_card_shows() {
        let a = ThreadMessage { id: "a".into(), ..ThreadMessage::default() };
        let flagged = ThreadMessage { flagged: true, ..a.clone() };
        assert_eq!(a.fingerprint(), a.clone().fingerprint());
        assert_ne!(a.fingerprint(), flagged.fingerprint());
    }

    #[test]
    fn hostile_conversation_json_never_panics() {
        for v in [json!(null), json!([{"id": 5}]), json!([{"id": "x", "to_recipients": "nope"}]), json!({"id": "x"})] {
            let _ = serde_json::from_value::<Vec<ThreadMessage>>(v);
        }
    }
}
