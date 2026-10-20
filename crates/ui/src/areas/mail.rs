//! Mail (Outlook-web layout, Apple polish): folders (favourites + tree, unread badges, drop
//! targets) | command bar, Focused/Other, message list (virtual, hover quick actions,
//! conversation grouping) | reading pane (`mail_read`) or inline compose (`mail_compose`).
//!
//! Every action is an engine command (`mail.*`). [`intercept`] adds the UI half of a few: after
//! `mail.compose`/`reply`/`reply_all`/`forward` it opens the returned draft in the compose pane,
//! `mail.send`/`save_draft`/`discard_draft` take the open draft's fields, and `mail.move` /
//! `mail.categorize` without a target open a picker. Programmatic calls (MCP, CLI) never reach
//! this module, so they never open anything.
//!
//! Keyboard (Outlook on the web, single-key set): `j`/`k` or arrows move, Enter opens, `R`
//! reply, `Shift+R` reply all, `Shift+F` forward, `Cmd+N` new, `Backspace`/`Delete` delete
//! (`Shift+Delete` permanently), `E` archive, `Q` read, `U` unread, `V` move, `C` categorize,
//! `Cmd+Shift+G` flag, `Cmd+Enter` send, `Cmd+S` save, Esc closes compose / returns to the list.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::Reply;
use serde::Deserialize;
use serde_json::{Value, json};

use super::common::{AreaLayout, nothing_selected};
use super::mail_icons::{MIcon, MailIcon};
use crate::components::person_card::PersonAvatar;
use crate::components::{Avatar, CommandBar, CommandButton, EmptyState, Icon, Spinner, VirtualList};
use crate::icons::IconName;
use crate::state::{AppState, Area, QueryState, ToastLevel, app_state, use_query};

/// Store tables whose changes refresh the message list.
pub const LIST_TABLES: &[&str] = &["message", "draft"];
/// Store tables whose changes refresh the folder pane.
pub const FOLDER_TABLES: &[&str] = &["mail_folder", "message"];
/// Rows requested per page of the list ("Show more" adds another page).
pub const PAGE: usize = 1000;
/// The largest list the UI asks for.
pub const MAX_ROWS: usize = 5000;

/// A message in the list (`mail.list`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MailRow {
    /// Message id.
    pub id: String,
    /// Sender name.
    #[serde(default)]
    pub from_name: String,
    /// Sender address.
    #[serde(default)]
    pub from_email: String,
    /// Recipients (for Sent and Drafts).
    #[serde(default)]
    pub to: String,
    /// Subject.
    #[serde(default)]
    pub subject: String,
    /// Body preview.
    #[serde(default)]
    pub preview: String,
    /// Received (RFC 3339).
    #[serde(default)]
    pub received: Option<String>,
    /// Unread.
    #[serde(default)]
    pub unread: bool,
    /// Flagged.
    #[serde(default)]
    pub flagged: bool,
    /// Has attachments.
    #[serde(default)]
    pub has_attachments: bool,
    /// `low` / `normal` / `high`.
    #[serde(default)]
    pub importance: String,
    /// Category names.
    #[serde(default)]
    pub categories: Vec<String>,
    /// A draft.
    #[serde(default)]
    pub is_draft: bool,
    /// Messages in the conversation (grouped view).
    #[serde(default = "one")]
    pub count: u32,
    /// Who took part in the conversation, newest first ("me" for you); grouped rows only.
    #[serde(default)]
    pub participants: Vec<String>,
    /// The local draft id, for local drafts listed in Drafts.
    #[serde(default)]
    pub draft_id: Option<String>,
}

fn one() -> u32 {
    1
}

impl super::common::Row for MailRow {
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// A mail folder (`mail.folders`), in tree order.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Folder {
    /// Folder id.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// `inbox`, `drafts`, `sent`, `deleted`, `archive`, `junk`, `outbox` or `folder`.
    #[serde(default)]
    pub kind: String,
    /// Parent folder.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Depth in the tree.
    #[serde(default)]
    pub depth: u32,
    /// Unread count.
    #[serde(default)]
    pub unread: u64,
    /// Total count.
    #[serde(default)]
    pub total: u64,
    /// Shown under Favourites.
    #[serde(default)]
    pub favourite: bool,
}

impl super::common::Row for Folder {
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// The list filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Everything.
    All,
    /// Unread only.
    Unread,
    /// Flagged only.
    Flagged,
}

impl Filter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Unread,
            Self::Unread => Self::Flagged,
            Self::Flagged => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Unread => "Unread",
            Self::Flagged => "Flagged",
        }
    }
}

/// A queued send that can still be undone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undo {
    /// The draft being sent.
    pub draft_id: String,
    /// Seconds until it goes out.
    pub seconds: u64,
    /// Distinguishes consecutive sends.
    pub token: u64,
}

/// What the picker popover chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// A destination folder for `mail.move`.
    Move,
    /// Categories for `mail.categorize`.
    Categorize,
}

/// An open picker and the messages it acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// Move or categorize.
    pub kind: PickerKind,
    /// Target message ids.
    pub ids: Vec<String>,
}

/// Mail UI state that outlives the area view (so commands from the keyboard or palette can
/// reach it). Reference-counted signals: never disposed with a view.
#[derive(Debug, Clone)]
pub struct MailUi {
    /// The draft shown in the compose pane.
    pub compose: ArcRwSignal<Option<String>>,
    /// The compose pane's current fields (`to`, `cc`, `bcc`, `subject`, `body_html`).
    pub fields: ArcRwSignal<Value>,
    /// Focused (`true`) or Other tab of the Inbox.
    pub focused_tab: ArcRwSignal<bool>,
    /// The list filter.
    pub filter: ArcRwSignal<Filter>,
    /// Group by conversation.
    pub group: ArcRwSignal<bool>,
    /// Messages whose remote images the user showed once.
    pub remote_once: ArcRwSignal<BTreeSet<String>>,
    /// The send that can be undone.
    pub undo: ArcRwSignal<Option<Undo>>,
    /// Rows requested.
    pub limit: ArcRwSignal<usize>,
    /// The open picker.
    pub picker: ArcRwSignal<Option<Picker>>,
    /// Collapsed folder ids.
    pub collapsed: ArcRwSignal<BTreeSet<String>>,
    /// Ids of the message list's rows, in order (to advance after a delete or move).
    pub order: ArcRwSignal<Vec<String>>,
    /// The last `view.thread.*` command for the open conversation (counter, command id).
    pub thread_cmd: ArcRwSignal<(u64, &'static str)>,
}

impl MailUi {
    fn new() -> Self {
        let group = crate::dom::storage_get("quorum.mail.group").is_none_or(|v| v != "0");
        let collapsed: BTreeSet<String> = crate::dom::storage_get("quorum.mail.collapsed")
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        Self {
            compose: ArcRwSignal::new(None),
            fields: ArcRwSignal::new(Value::Null),
            focused_tab: ArcRwSignal::new(true),
            filter: ArcRwSignal::new(Filter::All),
            group: ArcRwSignal::new(group),
            remote_once: ArcRwSignal::new(BTreeSet::new()),
            undo: ArcRwSignal::new(None),
            limit: ArcRwSignal::new(PAGE),
            picker: ArcRwSignal::new(None),
            collapsed: ArcRwSignal::new(collapsed),
            order: ArcRwSignal::new(Vec::new()),
            thread_cmd: ArcRwSignal::new((0, "")),
        }
    }
}

thread_local! {
    static UI: RefCell<Option<MailUi>> = const { RefCell::new(None) };
    static UNDO_TOKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The Mail UI state (created on first use).
pub fn mail_ui() -> MailUi {
    UI.with(|u| {
        if let Ok(mut slot) = u.try_borrow_mut() {
            return slot.get_or_insert_with(MailUi::new).clone();
        }
        MailUi::new()
    })
}

fn toast_err(state: AppState, reply: &Reply) {
    if let Reply::Err { message, .. } = reply {
        state.toast(ToastLevel::Error, message.clone());
    }
}

/// Opens `draft_id` in the compose pane (switching to Mail).
pub fn open_compose(state: AppState, draft_id: &str) {
    let ui = mail_ui();
    ui.fields.set(Value::Null);
    ui.compose.set(Some(draft_id.to_owned()));
    if state.area.get_untracked() != Area::Mail {
        state.area.set(Area::Mail);
    }
}

/// Closes the compose pane (the draft stays saved).
pub fn close_compose() {
    let ui = mail_ui();
    ui.compose.set(None);
    ui.fields.set(Value::Null);
    request_animation_frame(|| {
        let _ = crate::dom::focus_selector("[data-primary-list]");
    });
}

fn ids_of(params: &Value) -> Vec<String> {
    let mut out: Vec<String> = params.get("id").and_then(Value::as_str).map(str::to_owned).into_iter().collect();
    if let Some(ids) = params.get("ids").and_then(Value::as_array) {
        out.extend(ids.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    out
}

/// The row to select after `removed` leave the list while `selected` is open (pure): the
/// next remaining row below it, else the nearest one above (Outlook's behaviour).
pub fn next_after_removal(order: &[String], removed: &[String], selected: &str) -> Option<String> {
    let pos = order.iter().position(|id| id == selected)?;
    let keep = |id: &&String| !removed.contains(id) && id.as_str() != selected;
    order
        .get(pos.saturating_add(1)..)
        .and_then(|after| after.iter().find(keep))
        .or_else(|| order.get(..pos).and_then(|before| before.iter().rev().find(keep)))
        .cloned()
}

/// Before a delete, archive or move of the open message, selects the row that takes its place
/// so the reading pane advances instead of showing the moved message.
fn advance_selection(state: AppState, ui: &MailUi, params: &Value) {
    let sel = state.selection(Area::Mail);
    let Some(current) = sel.item.get_untracked() else { return };
    let removed = ids_of(params);
    if !removed.contains(&current) {
        return;
    }
    let next = ui.order.with_untracked(|order| next_after_removal(order, &removed, &current));
    sel.item.set(next);
}

fn compose_params(ui: &MailUi, params: &Value) -> Option<Value> {
    if params.get("draft_id").is_some() {
        return Some(params.clone());
    }
    let draft = ui.compose.get_untracked()?;
    let mut p = match ui.fields.get_untracked() {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    p.insert("draft_id".into(), json!(draft));
    Some(Value::Object(p))
}

/// The UI half of mail commands invoked from the UI (buttons, shortcuts, palette). Returns
/// `true` when handled here; `false` lets the dispatcher run the command normally.
pub fn intercept(state: AppState, id: &str, params: &Value) -> bool {
    let ui = mail_ui();
    match id {
        "mail.delete" | "mail.archive" => {
            advance_selection(state, &ui, params);
            false
        }
        "mail.move" if params.get("folder").is_some() => {
            advance_selection(state, &ui, params);
            false
        }
        "mail.compose" | "mail.reply" | "mail.reply_all" | "mail.forward" => {
            let (id, params) = (id.to_owned(), params.clone());
            spawn_local(async move {
                let reply = crate::ipc::run(&id, params).await;
                match &reply {
                    Reply::Ok { value } => {
                        if let Some(d) = value.get("draft_id").and_then(Value::as_str) {
                            open_compose(state, d);
                        }
                    }
                    Reply::Err { .. } => toast_err(state, &reply),
                }
            });
            true
        }
        "mail.send" | "mail.save_draft" | "mail.discard_draft" => {
            let Some(p) = compose_params(&ui, params) else {
                state.toast(ToastLevel::Info, "Open a draft first.");
                return true;
            };
            let id = id.to_owned();
            spawn_local(async move {
                let reply = crate::ipc::run(&id, p.clone()).await;
                let Reply::Ok { value } = &reply else { return toast_err(state, &reply) };
                match id.as_str() {
                    "mail.send" => {
                        close_compose();
                        let draft_id = p.get("draft_id").and_then(Value::as_str).unwrap_or_default().to_owned();
                        let seconds = value.get("delay_seconds").and_then(Value::as_u64).unwrap_or(5);
                        show_undo(draft_id, seconds);
                    }
                    "mail.discard_draft" => {
                        close_compose();
                        state.toast(ToastLevel::Info, "Draft discarded");
                    }
                    _ => state.toast(ToastLevel::Success, "Draft saved"),
                }
            });
            true
        }
        "mail.move" if params.get("folder").is_none() => {
            let ids = ids_of(params);
            if ids.is_empty() {
                state.toast(ToastLevel::Info, "Select a message first.");
            } else {
                ui.picker.set(Some(Picker { kind: PickerKind::Move, ids }));
            }
            true
        }
        "mail.categorize" if params.get("categories").is_none() => {
            let ids = ids_of(params);
            if ids.is_empty() {
                state.toast(ToastLevel::Info, "Select a message first.");
            } else {
                ui.picker.set(Some(Picker { kind: PickerKind::Categorize, ids }));
            }
            true
        }
        "view.open" if params.get("area").and_then(Value::as_str) == Some("mail") => {
            let sel = state.selection(Area::Mail);
            if let Some(f) = params.get("folder").and_then(Value::as_str) {
                sel.nav.set(Some(f.to_owned()));
            }
            if let Some(m) = params.get("id").and_then(Value::as_str) {
                ui.compose.set(None);
                if sel.item.with_untracked(|cur| cur.as_deref() != Some(m)) {
                    sel.item.set(Some(m.to_owned()));
                }
            }
            state.go(Area::Mail);
            true
        }
        _ => false,
    }
}

/// Shows the Undo bar for a message just sent.
pub fn show_undo(draft_id: String, seconds: u64) {
    let ui = mail_ui();
    let token = UNDO_TOKEN.with(|t| {
        let n = t.get().wrapping_add(1);
        t.set(n);
        n
    });
    ui.undo.set(Some(Undo { draft_id, seconds, token }));
    let undo = ui.undo.clone();
    set_timeout(
        move || {
            if undo.get_untracked().is_some_and(|u| u.token == token) {
                undo.set(None);
            }
        },
        Duration::from_secs(seconds.max(1)).saturating_add(Duration::from_millis(400)),
    );
}

fn undo_send(state: AppState) {
    let ui = mail_ui();
    let Some(u) = ui.undo.get_untracked() else { return };
    ui.undo.set(None);
    spawn_local(async move {
        let reply = crate::ipc::run("mail.undo_send", json!({ "draft_id": u.draft_id })).await;
        match &reply {
            Reply::Ok { .. } => open_compose(state, &u.draft_id),
            Reply::Err { .. } => toast_err(state, &reply),
        }
    });
}

const BUTTONS: &[CommandButton] = &[
    CommandButton::primary("mail.compose", "New mail", IconName::Plus),
    CommandButton::new("mail.delete", "Delete", IconName::Trash),
    CommandButton::new("mail.archive", "Archive", IconName::Archive),
    CommandButton::new("mail.move", "Move", IconName::Move),
    CommandButton::new("mail.reply", "Reply", IconName::Reply),
    CommandButton::new("mail.reply_all", "Reply all", IconName::Reply),
    CommandButton::new("mail.forward", "Forward", IconName::Forward),
    CommandButton::new("mail.flag", "Flag", IconName::Flag),
    CommandButton::new("mail.toggle_read", "Read / Unread", IconName::Mail),
    CommandButton::new("mail.categorize", "Categorize", IconName::Pencil),
];

/// The folder icon for a folder kind.
pub fn folder_icon(kind: &str) -> IconName {
    match kind {
        "inbox" => IconName::Inbox,
        "drafts" => IconName::Pencil,
        "sent" => IconName::Send,
        "archive" => IconName::Archive,
        "deleted" => IconName::Trash,
        "junk" => IconName::Blocked,
        "outbox" => IconName::Clock,
        _ => IconName::Folder,
    }
}

/// (minutes since the epoch now, local offset in minutes east of UTC).
pub fn now_and_offset() -> (i64, i64) {
    let now = js_sys::Date::now();
    let offset = js_sys::Date::new_0().get_timezone_offset();
    let to_i = |v: f64| if v.is_finite() { v as i64 } else { 0 };
    (to_i(now / 60_000.0), -to_i(offset))
}

/// Runs a core command in the background, ignoring the reply (reports, focus hints).
pub fn fire(id: &'static str, params: Value) {
    spawn_local(async move {
        let _ = crate::ipc::run(id, params).await;
    });
}

fn run_now(state: AppState, id: &'static str, params: Value) {
    crate::commands::dispatch(state, id, params);
}

fn drop_on_folder(state: AppState, folder: String, e: &web_sys::DragEvent) {
    e.prevent_default();
    let Some(dt) = e.data_transfer() else { return };
    let Ok(data) = dt.get_data("text/plain") else { return };
    let Some(id) = data.strip_prefix("quorum-mail:") else { return };
    run_now(state, "mail.move", json!({ "id": id, "folder": folder }));
}

#[component]
fn FolderButton(folder: Folder, selected: RwSignal<Option<String>>, tree: bool, kids: bool) -> impl IntoView {
    let state = app_state();
    let ui = mail_ui();
    let id = folder.id.clone();
    let (id_sel, id_click, id_drop, id_toggle, id_col) = (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
    let is_selected = move || selected.get().as_deref() == Some(id_sel.as_str());
    let count = match folder.kind.as_str() {
        "drafts" | "outbox" => folder.total,
        _ => folder.unread,
    };
    let has_children = tree && kids;
    let collapsed = ui.collapsed.clone();
    let collapsed_view = ui.collapsed.clone();
    let depth = if tree { folder.depth } else { 0 };
    view! {
        <button
            class="mail-folder"
            class:selected=is_selected
            data-folder=id.clone()
            style:padding-left=format!("calc(var(--space-3) + {}px)", if tree { 16 + depth.min(12) * 14 } else { 0 })
            on:click=move |_| {
                mail_ui().compose.set(None);
                state.selection(Area::Mail).item.set(None);
                selected.set(Some(id_click.clone()));
                mail_ui().limit.set(PAGE);
            }
            on:dragover=move |e: web_sys::DragEvent| e.prevent_default()
            on:drop=move |e: web_sys::DragEvent| drop_on_folder(state, id_drop.clone(), &e)
        >
            {has_children
                .then(|| {
                    let id_t = id_toggle.clone();
                    let col = collapsed_view.clone();
                    let id_c = id_col.clone();
                    view! {
                        <span
                            class="mail-folder-twisty"
                            class:open=move || !col.with(|c| c.contains(&id_c))
                            aria-hidden="true"
                            on:click=move |e: web_sys::MouseEvent| {
                                e.stop_propagation();
                                collapsed.update(|c| {
                                    if !c.remove(&id_t) {
                                        c.insert(id_t.clone());
                                    }
                                    if let Ok(s) = serde_json::to_string(c) {
                                        crate::dom::storage_set("quorum.mail.collapsed", &s);
                                    }
                                });
                            }
                        >
                            <Icon name=IconName::ChevronRight />
                        </span>
                    }
                })}
            <Icon name=folder_icon(&folder.kind) />
            <span class="nav-row-label">{folder.name.clone()}</span>
            {(count > 0).then(|| view! { <span class="mail-folder-count" class:total=folder.kind == "drafts">{count.to_string()}</span> })}
        </button>
    }
}

/// Folders visible in the tree: every folder whose ancestors are all expanded.
fn visible_tree(folders: &[Folder], collapsed: &BTreeSet<String>) -> Vec<Folder> {
    let mut hidden: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for f in folders {
        let parent_hidden = f.parent_id.as_ref().is_some_and(|p| hidden.contains(p) || collapsed.contains(p));
        if parent_hidden {
            hidden.insert(f.id.clone());
            continue;
        }
        out.push(f.clone());
    }
    out
}

fn has_children(folders: &[Folder], id: &str) -> bool {
    folders.iter().any(|f| f.parent_id.as_deref() == Some(id))
}

fn nav_keys(e: &web_sys::KeyboardEvent) {
    let step: isize = match e.key().as_str() {
        "ArrowDown" => 1,
        "ArrowUp" => -1,
        _ => return,
    };
    e.prevent_default();
    let buttons = crate::dom::query_all(".mail-nav .mail-folder");
    let active = crate::dom::active_element();
    let pos = active.as_ref().and_then(|a| buttons.iter().position(|b| b == a));
    let next = match pos {
        Some(p) => p.checked_add_signed(step).filter(|n| *n < buttons.len()).unwrap_or(p),
        None => 0,
    };
    if let Some(b) = buttons.get(next) {
        let _ = crate::dom::focus_element(b);
    }
}

#[component]
fn FolderPane(folders: crate::state::Query<Vec<Folder>>, selected: RwSignal<Option<String>>) -> impl IntoView {
    let ui = mail_ui();
    let body = move || {
        let collapsed = ui.collapsed.get();
        match folders.state.get() {
            QueryState::Loading => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
            QueryState::Failed { message, .. } => {
                view! { <EmptyState title="Couldn't load folders" detail=message error=true /> }.into_any()
            }
            QueryState::Ready(all) => {
                let favs: Vec<Folder> = all.iter().filter(|f| f.favourite).cloned().collect();
                let tree: Vec<(Folder, bool)> = visible_tree(&all, &collapsed)
                    .into_iter()
                    .map(|f| {
                        let kids = has_children(&all, &f.id);
                        (f, kids)
                    })
                    .collect();
                view! {
                    <div class="mail-nav-section" id="mail-favourites">"Favourites"</div>
                    <div role="group" aria-labelledby="mail-favourites">
                        {favs.into_iter().map(|f| view! { <FolderButton folder=f selected=selected tree=false kids=false /> }).collect_view()}
                    </div>
                    <div class="mail-nav-section" id="mail-all-folders">"Folders"</div>
                    <div role="group" aria-labelledby="mail-all-folders">
                        {tree
                            .into_iter()
                            .map(|(f, kids)| view! { <FolderButton folder=f selected=selected tree=true kids=kids /> }
                                .into_any())
                            .collect_view()}
                    </div>
                }
                .into_any()
            }
        }
    };
    view! { <div class="mail-nav" on:keydown=move |e: web_sys::KeyboardEvent| nav_keys(&e)>{body}</div> }
}

/// Renders one list row (with hover quick actions).
fn mail_row(state: AppState, m: MailRow, now: i64, offset: i64) -> AnyView {
    let time = m.received.as_deref().map(|r| super::mail_util::short_time(r, now, offset)).unwrap_or_default();
    let sender = if m.is_draft {
        if m.to.is_empty() { "(no recipients)".to_owned() } else { m.to.clone() }
    } else if m.count > 1 && m.participants.len() > 1 {
        m.participants.join(", ")
    } else if m.from_name.is_empty() {
        m.from_email.clone()
    } else {
        m.from_name.clone()
    };
    let subject = if m.subject.trim().is_empty() { "(no subject)".to_owned() } else { m.subject.clone() };
    let (id_drag, id_del, id_flag, id_read) = (m.id.clone(), m.id.clone(), m.id.clone(), m.id.clone());
    let quick = move |id: String, cmd: &'static str| {
        move |e: web_sys::MouseEvent| {
            e.stop_propagation();
            run_now(state, cmd, json!({ "id": id }));
        }
    };
    let stop = |e: web_sys::MouseEvent| e.stop_propagation();
    view! {
        <div
            class="item-row mail-row"
            class:unread=m.unread
            class:flagged=m.flagged
            draggable="true"
            on:dragstart=move |e: web_sys::DragEvent| {
                if let Some(dt) = e.data_transfer() {
                    let _ = dt.set_data("text/plain", &format!("quorum-mail:{id_drag}"));
                    dt.set_effect_allowed("move");
                }
            }
        >
            {match crate::components::person_card::mail_user(&m.from_email).filter(|_| !m.is_draft) {
                Some(user) => view! { <PersonAvatar user=user name=sender.clone() size="sm" /> }.into_any(),
                None => view! { <Avatar name=sender.clone() size="sm" /> }.into_any(),
            }}
            <div class="item-text">
                <div class="item-line">
                    <span class="item-primary">
                        {m.is_draft.then(|| view! { <span class="mail-draft-tag">"[Draft] "</span> })}
                        {sender}
                    </span>
                    {(m.count > 1).then(|| view! { <span class="mail-count">{m.count.to_string()}</span> })}
                    <span class="item-meta mail-time">{time}</span>
                </div>
                <div class="item-line">
                    <span class="item-secondary mail-subject">
                        {(m.importance == "high").then(|| view! { <span class="mail-important" title="High importance"><MIcon name=MailIcon::Important /></span> })}
                        {subject}
                    </span>
                    <span class="item-flags">
                        {m.has_attachments.then(|| view! { <span class="item-flag" title="Has attachments"><Icon name=IconName::Paperclip /></span> })}
                        {m.flagged.then(|| view! { <span class="item-flag flagged" title="Flagged"><Icon name=IconName::Flag /></span> })}
                    </span>
                </div>
                <div class="item-line">
                    <span class="item-tertiary mail-preview">{m.preview.clone()}</span>
                    {m.categories.iter().take(3).map(|c| view! { <span class=format!("mail-cat-dot {}", category_class(c)) title=c.clone()></span> }).collect_view()}
                </div>
            </div>
            <div class="mail-row-actions" on:mousedown=stop>
                <button class="mail-quick" title="Delete" aria-label="Delete" tabindex="-1" on:click=quick(id_del, "mail.delete")>
                    <Icon name=IconName::Trash />
                </button>
                <button class="mail-quick" class:on=m.flagged title="Flag" aria-label="Flag" tabindex="-1" on:click=quick(id_flag, "mail.flag")>
                    <Icon name=IconName::Flag />
                </button>
                <button
                    class="mail-quick"
                    title=if m.unread { "Mark as read" } else { "Mark as unread" }
                    aria-label=if m.unread { "Mark as read" } else { "Mark as unread" }
                    tabindex="-1"
                    on:click=quick(id_read, "mail.toggle_read")
                >
                    {if m.unread { view! { <MIcon name=MailIcon::EnvelopeOpen /> }.into_any() } else { view! { <Icon name=IconName::Mail /> }.into_any() }}
                </button>
            </div>
        </div>
    }
    .into_any()
}

/// A stable colour class for a category name.
pub fn category_class(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let known = ["red", "orange", "yellow", "green", "blue", "purple"];
    match known.iter().find(|k| lower.contains(*k)) {
        Some(k) => format!("cat-{k}"),
        None => format!("cat-{}", name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b))) % 6),
    }
}

#[component]
fn PickerPopover(folders: crate::state::Query<Vec<Folder>>) -> impl IntoView {
    let state = app_state();
    let ui = mail_ui();
    let picker = ui.picker.clone();
    let filter = RwSignal::new(String::new());
    let categories = use_query::<Vec<Value>>("mail.categories", || Value::Null, &["setting"]);
    let close = {
        let picker = picker.clone();
        move || {
            picker.set(None);
            request_animation_frame(|| {
                let _ = crate::dom::focus_selector("[data-primary-list]");
            });
        }
    };
    let input = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        if let Some(el) = input.get() {
            let _ = el.focus();
        }
    });
    let pick_view = move || {
        let Some(p) = picker.get() else { return ().into_any() };
        let q = filter.get().to_lowercase();
        let close_a = close.clone();
        let items: Vec<(String, String, String)> = match p.kind {
            PickerKind::Move => folders
                .data()
                .map(|f| {
                    f.iter()
                        .filter(|f| q.is_empty() || f.name.to_lowercase().contains(&q))
                        .map(|f| (f.id.clone(), f.name.clone(), f.kind.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            PickerKind::Categorize => categories
                .data()
                .map(|c| {
                    c.iter()
                        .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_owned))
                        .filter(|n| q.is_empty() || n.to_lowercase().contains(&q))
                        .map(|n| (n.clone(), n.clone(), "category".to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let title = match p.kind {
            PickerKind::Move => "Move to",
            PickerKind::Categorize => "Categorize",
        };
        let ids = p.ids.clone();
        let kind = p.kind;
        let first = items.first().map(|i| i.0.clone());
        let ids_enter = ids.clone();
        let close_enter = close_a.clone();
        let choose = move |value: String, ids: Vec<String>| match kind {
            PickerKind::Move => run_now(state, "mail.move", json!({ "ids": ids, "folder": value })),
            PickerKind::Categorize => run_now(state, "mail.categorize", json!({ "ids": ids, "categories": [value] })),
        };
        let choose_enter = choose;
        view! {
            <div class="mail-picker-backdrop" on:click={
                let c = close_a.clone();
                move |_| c()
            }></div>
            <div class="mail-picker" role="dialog" aria-label=title>
                <div class="mail-picker-title">{title}</div>
                <input
                    node_ref=input
                    class="mail-picker-input"
                    placeholder=if kind == PickerKind::Move { "Search folders" } else { "Search categories" }
                    prop:value=move || filter.get()
                    on:input=move |e| filter.set(event_target_value(&e))
                    on:keydown=move |e: web_sys::KeyboardEvent| match e.key().as_str() {
                        "Escape" => {
                            e.prevent_default();
                            e.stop_propagation();
                            close_enter();
                        }
                        "Enter" => {
                            e.prevent_default();
                            if let Some(v) = first.clone() {
                                choose_enter(v, ids_enter.clone());
                                close_enter();
                            }
                        }
                        _ => {}
                    }
                />
                <div class="mail-picker-list" role="listbox">
                    {items
                        .into_iter()
                        .map(|(value, label, kind_name)| {
                            let (ids, choose, close) = (ids.clone(), choose, close_a.clone());
                            view! {
                                <button class="mail-picker-item" role="option" on:click=move |_| {
                                    choose(value.clone(), ids.clone());
                                    close();
                                }>
                                    {if kind_name == "category" {
                                        view! { <span class=format!("mail-cat-dot {}", category_class(&label))></span> }.into_any()
                                    } else {
                                        view! { <Icon name=folder_icon(&kind_name) /> }.into_any()
                                    }}
                                    <span>{label.clone()}</span>
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
            </div>
        }
        .into_any()
    };
    view! { {pick_view} }
}

#[component]
fn UndoBar() -> impl IntoView {
    let state = app_state();
    let ui = mail_ui();
    let undo = ui.undo.clone();
    move || {
        undo.get().map(|u| {
            view! {
                <div class="mail-undo" role="status" style=format!("--undo-seconds: {}s", u.seconds.max(1))>
                    <Icon name=IconName::Send />
                    <span>"Sending…"</span>
                    <button class="mail-undo-button" on:click=move |_| undo_send(state)>"Undo"</button>
                    <span class="mail-undo-progress" aria-hidden="true"></span>
                </div>
            }
        })
    }
}

/// What the reading pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DetailPane {
    /// The compose pane for a draft.
    Compose(String),
    /// A message.
    Read(String),
    /// Nothing selected.
    Empty,
}

/// The Mail area.
#[component]
pub fn MailArea() -> impl IntoView {
    let state = app_state();
    let ui = mail_ui();
    let sel = state.selection(Area::Mail);
    if sel.nav.get_untracked().is_none() {
        sel.nav.set(Some("inbox".into()));
    }
    fire("sync.focus", json!({ "area": "mail" }));
    let folders = use_query::<Vec<Folder>>("mail.folders", || Value::Null, FOLDER_TABLES);
    // "inbox" is a placeholder until the folder list names the real id.
    Effect::new(move |_| {
        if let Some(all) = folders.data()
            && let Some(nav) = sel.nav.get_untracked()
            && !all.iter().any(|f| f.id == nav)
            && let Some(f) = all.iter().find(|f| f.kind == nav || (nav == "inbox" && f.kind == "inbox"))
        {
            sel.nav.set(Some(f.id.clone()));
        }
    });
    let current = Memo::new(move |_| {
        let id = sel.nav.get().unwrap_or_else(|| "inbox".into());
        folders.data().and_then(|f| f.iter().find(|f| f.id == id).cloned())
    });
    let is_inbox = Memo::new(move |_| {
        current.get().map_or_else(|| sel.nav.get().as_deref() == Some("inbox"), |f| f.kind == "inbox")
    });
    let (focused_tab, filter, group, limit) =
        (ui.focused_tab.clone(), ui.filter.clone(), ui.group.clone(), ui.limit.clone());
    let params = {
        let (focused_tab, filter, group, limit) = (focused_tab.clone(), filter.clone(), group.clone(), limit.clone());
        move || {
            let folder = sel.nav.get().unwrap_or_else(|| "inbox".into());
            let mut p = serde_json::Map::new();
            p.insert("folder".into(), json!(folder));
            p.insert("limit".into(), json!(limit.get().min(MAX_ROWS)));
            if is_inbox.get() {
                p.insert("focused".into(), json!(focused_tab.get()));
            }
            match filter.get() {
                Filter::Unread => {
                    p.insert("unread_only".into(), json!(true));
                }
                Filter::Flagged => {
                    p.insert("flagged_only".into(), json!(true));
                }
                Filter::All => {}
            }
            if group.get() {
                p.insert("group".into(), json!("conversation"));
            }
            Value::Object(p)
        }
    };
    let messages = use_query::<Vec<MailRow>>("mail.list", params, LIST_TABLES);
    let items: Signal<Arc<Vec<MailRow>>> = Signal::derive(move || messages.data().unwrap_or_default());
    {
        let order = ui.order.clone();
        Effect::new(move |_| {
            let ids: Vec<String> = items.with(|rows| rows.iter().map(|r| r.id.clone()).collect());
            order.set(ids);
        });
    }

    // Opening a local draft from Drafts goes to the compose pane.
    Effect::new(move |_| {
        let Some(id) = sel.item.get() else { return };
        let draft = messages
            .state
            .with_untracked(|s| s.data().and_then(|d| d.iter().find(|r| r.id == id).and_then(|r| r.draft_id.clone())));
        if let Some(d) = draft {
            open_compose(state, &d);
        }
    });
    // Mark as read when a message stays selected (Outlook's default).
    Effect::new(move |_| {
        let Some(id) = sel.item.get() else { return };
        let unread = messages
            .state
            .with_untracked(|s| s.data().is_some_and(|d| d.iter().any(|r| r.id == id && r.unread && !r.is_draft)));
        if unread {
            set_timeout(
                move || {
                    if sel.item.get_untracked().as_deref() == Some(id.as_str()) {
                        messages.update_local(|rows| {
                            rows.iter()
                                .map(|r| if r.id == id { MailRow { unread: false, ..r.clone() } } else { r.clone() })
                                .collect()
                        });
                        crate::commands::dispatch(state, "mail.mark_read", json!({ "id": id }));
                    }
                },
                Duration::from_millis(700),
            );
        }
    });
    // Tell agents what is on screen (debounced).
    let report_gen = StoredValue::new(0u64);
    {
        let compose = ui.compose.clone();
        Effect::new(move |_| {
            let report = json!({
                "area": "mail",
                "folder": sel.nav.get(),
                "selection": sel.item.get().into_iter().collect::<Vec<_>>(),
                "view": if compose.get().is_some() { "compose" } else if sel.item.get().is_some() { "read" } else { "list" },
                "draft_id": compose.get(),
            });
            let n = report_gen.get_value().wrapping_add(1);
            report_gen.set_value(n);
            set_timeout(
                move || {
                    if report_gen.try_get_value() == Some(n) {
                        fire("ui.report", report);
                    }
                },
                Duration::from_millis(250),
            );
        });
    }

    let title = Signal::derive(move || current.get().map(|f| f.name).unwrap_or_else(|| "Mail".into()));
    let list_keys = move |e: web_sys::KeyboardEvent| {
        let Some(id) = sel.item.get_untracked() else { return };
        if e.meta_key() || e.ctrl_key() || e.alt_key() {
            return;
        }
        if e.key() == "Delete" {
            e.prevent_default();
            if e.shift_key() {
                let ok = web_sys::window()
                    .and_then(|w| w.confirm_with_message("Permanently delete this message? This can't be undone.").ok())
                    .unwrap_or(false);
                if ok {
                    run_now(state, "mail.delete", json!({ "id": id, "permanent": true }));
                }
            } else {
                run_now(state, "mail.delete", json!({ "id": id }));
            }
        }
    };
    let on_activate = Callback::new(move |id: String| {
        let draft = messages
            .state
            .with_untracked(|s| s.data().and_then(|d| d.iter().find(|r| r.id == id).and_then(|r| r.draft_id.clone())));
        match draft {
            Some(d) => open_compose(state, &d),
            None => {
                let _ = crate::dom::focus_selector(".mail-reading [data-pane-focus], .mail-reading");
            }
        }
    });
    let render = Callback::new(move |m: MailRow| {
        let (now, offset) = now_and_offset();
        mail_row(state, m, now, offset)
    });

    // A memo, so a data refresh re-renders rows, not the whole list (keeps scroll and selection).
    let status = Memo::new(move |_| {
        messages.state.with(|s| match s {
            QueryState::Loading => 0u8,
            QueryState::Failed { .. } => 1,
            QueryState::Ready(d) if d.is_empty() => 2,
            QueryState::Ready(_) => 3,
        })
    });
    let list_body = move || match status.get() {
        0 => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
        1 => {
            let msg = messages.state.with(|s| match s {
                QueryState::Failed { message, .. } => message.clone(),
                _ => String::new(),
            });
            view! { <EmptyState title="Couldn't load this folder" detail=msg error=true /> }.into_any()
        }
        2 => view! { <EmptyState title="All caught up" detail="Nothing in this view." icon=IconName::Inbox /> }
            .into_any(),
        _ => view! {
            <div class="list-wrap mail-list" on:keydown=list_keys>
                <VirtualList
                    items=items
                    key=|r: &MailRow| r.id.clone()
                    render=render
                    selected=sel.item
                    label="Messages"
                    primary=true
                    on_activate=on_activate
                />
            </div>
        }
        .into_any(),
    };
    let more = {
        let limit = limit.clone();
        move || {
            let n = messages.data().map_or(0, |d| d.len());
            let lim = limit.get();
            (n >= lim && lim < MAX_ROWS).then(|| {
                let limit = limit.clone();
                view! {
                    <button class="mail-more" on:click=move |_| limit.update(|l| *l = (*l + PAGE).min(MAX_ROWS))>
                        "Show more messages"
                    </button>
                }
            })
        }
    };
    let tabs = {
        let focused_tab = focused_tab.clone();
        move || {
            is_inbox.get().then(|| {
                let (f1, f2, f3, f4) = (focused_tab.clone(), focused_tab.clone(), focused_tab.clone(), focused_tab.clone());
                view! {
                    <div class="mail-tabs" role="tablist" aria-label="Focused Inbox">
                        <button class="mail-tab" role="tab" class:selected=move || f1.get() aria-selected=move || f2.get().to_string()
                            on:click=move |_| f3.set(true)>"Focused"</button>
                        <button class="mail-tab" role="tab" class:selected={
                            let f = f4.clone();
                            move || !f.get()
                        } aria-selected={
                            let f = f4.clone();
                            move || (!f.get()).to_string()
                        } on:click=move |_| f4.set(false)>"Other"</button>
                    </div>
                }
            })
        }
    };
    let tools = {
        let (filter, group) = (filter.clone(), group.clone());
        move || {
            let (fl, fl2, g, g2) = (filter.clone(), filter.clone(), group.clone(), group.clone());
            view! {
                <div class="mail-list-tools">
                    <button class="mail-tool" class:on=move || fl.get() != Filter::All title="Filter"
                        on:click=move |_| fl2.update(|f| *f = f.next())>
                        <MIcon name=MailIcon::Filter />
                        <span>{
                            let f = filter.clone();
                            move || f.get().label()
                        }</span>
                    </button>
                    <button class="mail-tool" class:on=move || g.get() title="Group by conversation" aria-pressed={
                        let g = group.clone();
                        move || g.get().to_string()
                    }
                        on:click=move |_| {
                            g2.update(|v| *v = !*v);
                            crate::dom::storage_set("quorum.mail.group", if g2.get_untracked() { "1" } else { "0" });
                        }>
                        <MIcon name=MailIcon::Conversation />
                    </button>
                </div>
            }
        }
    };

    let compose = ui.compose.clone();
    view! {
        <div class="mail-area">
            <AreaLayout
                id="mail"
                nav_label="Folders"
                nav=move || view! { <FolderPane folders=folders selected=sel.nav /> }
                list_label="Messages"
                list=move || {
                    view! {
                        <div class="list-pane mail-list-pane">
                            <div class="mail-commands"><CommandBar buttons=BUTTONS label="Mail commands" /></div>
                            <header class="list-header mail-list-header">
                                <h2 class="list-title">{move || title.get()}</h2>
                                {tools.clone()}
                            </header>
                            {tabs.clone()}
                            <div class="list-body">{list_body}</div>
                            {more.clone()}
                            <PickerPopover folders=folders />
                        </div>
                    }
                }
                detail_label="Reading pane"
                detail=move || {
                    // Keyed, so a selection change under an open draft never rebuilds the editor.
                    let compose = compose.clone();
                    let pane = Memo::new(move |_| match (compose.get(), sel.item.get()) {
                        (Some(d), _) => DetailPane::Compose(d),
                        (None, Some(id)) => DetailPane::Read(id),
                        (None, None) => DetailPane::Empty,
                    });
                    move || match pane.get() {
                        DetailPane::Compose(d) => view! { <super::mail_compose::ComposePane draft_id=d /> }.into_any(),
                        DetailPane::Read(id) => view! { <super::mail_thread::ThreadPane id=id /> }.into_any(),
                        DetailPane::Empty => nothing_selected("Select a message to read it", IconName::Mail),
                    }
                }
            />
            <UndoBar />
        </div>
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn removal_advances_to_the_next_row() {
        let order: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| (*s).to_owned()).collect();
        let ids = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(super::next_after_removal(&order, &ids(&["b"]), "b").as_deref(), Some("c"));
        assert_eq!(super::next_after_removal(&order, &ids(&["b", "c"]), "b").as_deref(), Some("d"));
        assert_eq!(super::next_after_removal(&order, &ids(&["d"]), "d").as_deref(), Some("c"));
        assert_eq!(super::next_after_removal(&order, &ids(&["c", "d"]), "d").as_deref(), Some("b"));
        assert_eq!(super::next_after_removal(&order[..1], &ids(&["a"]), "a"), None);
        assert_eq!(super::next_after_removal(&order, &ids(&["a"]), "zz"), None);
    }

    use super::*;

    fn f(id: &str, parent: Option<&str>, depth: u32) -> Folder {
        Folder {
            id: id.into(),
            name: id.into(),
            kind: "folder".into(),
            parent_id: parent.map(Into::into),
            depth,
            unread: 0,
            total: 0,
            favourite: false,
        }
    }

    #[test]
    fn collapsed_folders_hide_their_whole_subtree() {
        let all = vec![f("a", None, 0), f("b", Some("a"), 1), f("c", Some("b"), 2), f("d", None, 0)];
        let none = visible_tree(&all, &BTreeSet::new());
        assert_eq!(none.len(), 4);
        let shut: BTreeSet<String> = ["a".to_owned()].into();
        let ids: Vec<String> = visible_tree(&all, &shut).into_iter().map(|f| f.id).collect();
        assert_eq!(ids, vec!["a", "d"]);
        assert!(has_children(&all, "a") && !has_children(&all, "d"));
    }

    #[test]
    fn rows_decode_engine_and_mock_shapes() {
        let row: MailRow = serde_json::from_value(json!({ "id": "x", "received": null })).unwrap();
        assert_eq!(row.count, 1);
        assert!(!row.unread);
        assert_eq!(category_class("Blue category"), "cat-blue");
        assert!(category_class("Synthetic").starts_with("cat-"));
        assert_eq!(Filter::All.next().next().next(), Filter::All);
    }
}
