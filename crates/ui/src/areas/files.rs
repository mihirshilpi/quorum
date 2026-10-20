//! Files: OneDrive and SharePoint, laid out like OneDrive on the web with Finder polish.
//!
//! Locations (`files.locations`: My files, Recent, Shared with me, followed sites with their
//! libraries and lists) | breadcrumb, command bar and the folder (`files.children`) as a
//! sortable list or a thumbnail grid | details (`files.item`: preview, facts, sharing,
//! versions). SharePoint lists show as a typed table (`lists.items`, [`super::files_lists`]).
//!
//! Every action is an engine command (`files.*`, `lists.*`). When invoked from the UI without
//! the parameters a dialog collects (a new folder's name, a move target, share options), the
//! command opens that dialog first ([`run_ui`]); programmatic calls never get here.
//!
//! Keyboard: `j`/`k` and arrows move, Enter opens (folders navigate), Backspace goes up, F2
//! renames, Delete deletes (with confirmation), ⇧⌘N new folder, ⌘U upload, ⇧⌘C copy link,
//! ⇧⌘D download. Files dropped from Finder upload into the open folder.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::Reply;
use serde::Deserialize;
use serde_json::{Value, json};

use super::common::{AreaLayout, QueryList, Row};
use super::files_details::Details;
use super::files_dialogs::{Dialog, DialogHost};
use super::files_icons::{FileIcon, SiteIcon};
use super::files_lists::{ListDetails, ListTable};
use crate::components::{EmptyState, Icon, Spinner, VirtualList};
use crate::icons::IconName;
use crate::state::{AppState, Area, QueryState, ToastLevel, app_state, use_query};

/// Store tables the files views depend on.
pub const TABLES: &[&str] = &["drive", "drive_item", "site", "list", "setting"];

/// A drive item row (`files.children`, `files.search`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FileRow {
    /// `"<drive>|<item>"`.
    pub id: String,
    /// Drive id.
    #[serde(default)]
    pub drive: String,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Folder?
    #[serde(default)]
    pub folder: bool,
    /// Children of a folder.
    #[serde(default)]
    pub child_count: u32,
    /// Kind for the icon.
    #[serde(default)]
    pub kind: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Modified, as a short label.
    #[serde(default)]
    pub modified_label: String,
    /// Who modified it.
    #[serde(default)]
    pub modified_by: String,
    /// Shared with others.
    #[serde(default)]
    pub shared: bool,
    /// Not yet on the server.
    #[serde(default)]
    pub pending: bool,
    /// Parent row id.
    #[serde(default)]
    pub parent: Option<String>,
}

impl Row for FileRow {
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// A breadcrumb entry.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Crumb {
    /// A location (`mine`, `drive:<id>`) or a folder row id.
    pub id: String,
    /// Name.
    #[serde(default)]
    pub name: String,
}

/// `files.children`.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Children {
    /// The location shown.
    #[serde(default)]
    pub location: String,
    /// The folder's row id (the root's, for a location).
    #[serde(default)]
    pub folder: Option<String>,
    /// Path to the folder.
    #[serde(default)]
    pub breadcrumb: Vec<Crumb>,
    /// The rows.
    #[serde(default)]
    pub items: Vec<FileRow>,
    /// First sync still running.
    #[serde(default)]
    pub syncing: bool,
}

/// A location (`files.locations`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LocationRow {
    /// `mine`, `recent`, `shared`, `sites`, `site:<id>`, `drive:<id>`, `list:<site>|<list>`.
    pub id: String,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// `mine`, `recent`, `shared`, `header`, `site`, `library`, `list`.
    #[serde(default)]
    pub kind: String,
    /// Indent level.
    #[serde(default)]
    pub depth: u32,
    /// Drive (libraries, My files).
    #[serde(default)]
    pub drive: Option<String>,
}

impl Row for LocationRow {
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// Upload progress (`files.uploads`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct UploadRow {
    /// Operation id.
    pub op: String,
    /// File name.
    #[serde(default)]
    pub name: String,
    /// Bytes accepted.
    #[serde(default)]
    pub sent: u64,
    /// Total bytes.
    #[serde(default)]
    pub total: u64,
    /// `queued`, `uploading`, `done`, `failed`.
    #[serde(default)]
    pub state: String,
}

/// Sort column and direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    /// `name`, `modified`, `size`, `modified_by`.
    pub column: &'static str,
    /// Descending.
    pub desc: bool,
}

/// The Files area's view state; `Copy` signal handles, reachable from [`run_ui`].
#[derive(Debug, Clone, Copy)]
pub struct FilesUi {
    /// Selected location id (the area's nav selection).
    pub location: RwSignal<Option<String>>,
    /// Open folder row id (`None`: the location's root).
    pub folder: RwSignal<Option<String>>,
    /// Selected row id.
    pub selected: RwSignal<Option<String>>,
    /// Sort.
    pub sort: RwSignal<Sort>,
    /// Thumbnail grid instead of the list.
    pub grid: RwSignal<bool>,
    /// Open dialog.
    pub dialog: RwSignal<Option<Dialog>>,
    /// The current folder listing.
    pub children: Query<Children>,
    /// Bumped to open the upload file picker.
    pub pick: RwSignal<u64>,
}

use crate::state::Query;

thread_local! {
    static UI: Cell<Option<FilesUi>> = const { Cell::new(None) };
    static PENDING_SHOW: RefCell<Option<(String, bool)>> = const { RefCell::new(None) };
    static REPORT_GEN: Cell<u64> = const { Cell::new(0) };
}

fn ui() -> Option<FilesUi> {
    UI.with(Cell::get).filter(|u| u.location.try_get_untracked().is_some())
}

/// The folder target for new items: the open folder, else the location (engine resolves its
/// root).
pub fn current_target(u: &FilesUi) -> Option<String> {
    u.folder
        .get_untracked()
        .or_else(|| u.children.data().and_then(|c| c.folder.clone()))
        .or_else(|| u.location.get_untracked().filter(|l| l == "mine" || l.starts_with("drive:")))
}

/// Human-readable size, e.g. `12.5 KB` (pure).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    let u = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 { format!("{bytes} B") } else { format!("{v:.1} {u}") }
}

/// Base64 (standard alphabet, padded) for inline uploads (pure).
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(A.get(((n >> shift) & 63) as usize).copied().unwrap_or(b'A')));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Runs an engine command from the UI (no dialog interception); errors toast. `then` gets the
/// result value.
pub fn exec(state: AppState, id: &'static str, params: Value, then: impl FnOnce(Value) + 'static) {
    spawn_local(async move {
        match crate::ipc::run(id, params).await {
            Reply::Ok { value } => then(value),
            Reply::Err { message, .. } => state.toast(ToastLevel::Error, message),
        }
    });
}

fn param(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn target_of(u: &FilesUi, params: &Value) -> Option<String> {
    param(params, "item").or_else(|| param(params, "id")).or_else(|| u.selected.get_untracked())
}

fn row_of(u: &FilesUi, id: &str) -> Option<FileRow> {
    u.children.data().and_then(|c| c.items.iter().find(|r| r.id == id).cloned())
}

/// Copies `url` to the clipboard and confirms with a toast.
pub fn copy_to_clipboard(state: AppState, url: String) {
    let Some(w) = web_sys::window() else { return };
    let promise = w.navigator().clipboard().write_text(&url);
    spawn_local(async move {
        match wasm_bindgen_futures::JsFuture::from(promise).await {
            Ok(_) => state.toast(ToastLevel::Success, "Link copied"),
            Err(_) => state.toast(ToastLevel::Warning, "Couldn't reach the clipboard."),
        }
    });
}

/// Handles UI events from the engine (`files.launch`, `files.show`, `files.dropped`) and opens
/// the dialog of a `files.*` command invoked from the UI without its dialog's parameters.
/// Returns `false` to let the command run in the engine as is.
pub fn run_ui(state: AppState, id: &str, params: &Value) -> bool {
    match id {
        "files.launch" => {
            let url = param(params, "url").unwrap_or_default();
            let opened = params.get("opened").and_then(Value::as_bool).unwrap_or(false);
            let desktop = param(params, "target").as_deref() == Some("desktop");
            if !opened
                && url.starts_with("https://")
                && state.preview
                && let Some(w) = web_sys::window()
            {
                let _ = w.open_with_url_and_target(&url, "_blank");
            }
            let msg = match (desktop, opened || state.preview) {
                (true, true) => "Opening in the desktop app…",
                (false, true) => "Opening in your browser…",
                _ => "Open the file from the desktop app.",
            };
            state.toast(ToastLevel::Info, msg);
            return true;
        }
        "files.show" => {
            let Some(target) = param(params, "id") else { return true };
            let folder = params.get("folder").and_then(Value::as_bool).unwrap_or(false);
            PENDING_SHOW.with(|p| *p.borrow_mut() = Some((target, folder)));
            if state.area.get_untracked() != Area::Files {
                state.go(Area::Files);
            }
            if let Some(u) = ui() {
                take_pending(&u);
            }
            return true;
        }
        "files.dropped" => {
            let paths: Vec<String> = params
                .get("paths")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            let folder = ui().and_then(|u| current_target(&u));
            if paths.is_empty() || folder.is_none() {
                state.toast(ToastLevel::Warning, "Open a folder in My files or a library to upload.");
                return true;
            }
            exec(state, "files.upload", json!({ "paths": paths, "folder": folder }), move |_| {});
            return true;
        }
        _ => {}
    }
    // Dialog interception only while the Files area is on screen.
    let Some(u) = ui() else { return false };
    if state.area.get_untracked() != Area::Files {
        return false;
    }
    let has = |k: &str| params.get(k).is_some_and(|v| !v.is_null());
    let dialog =
        match id {
            "files.new_folder" if !has("name") => current_target(&u).map(|folder| Dialog::NewFolder { folder }),
            "files.rename" if !has("name") => {
                target_of(&u, params).and_then(|t| row_of(&u, &t)).map(|r| Dialog::Rename { id: r.id, name: r.name })
            }
            "files.move" | "files.copy" if !has("folder") => {
                target_of(&u, params).and_then(|t| row_of(&u, &t)).map(|r| Dialog::Move {
                    id: r.id,
                    name: r.name,
                    copy: id == "files.copy",
                    start: u.location.get_untracked().unwrap_or_else(|| "mine".into()),
                })
            }
            "files.share" if !has("type") && !has("recipients") => {
                target_of(&u, params).and_then(|t| row_of(&u, &t)).map(|r| Dialog::Share { id: r.id, name: r.name })
            }
            "files.delete" if !has("items") => target_of(&u, params)
                .and_then(|t| row_of(&u, &t))
                .map(|r| Dialog::Delete { id: r.id, name: r.name, folder: r.folder }),
            "files.upload" if !has("paths") && !has("files") => {
                u.pick.update(|n| *n = n.wrapping_add(1));
                return true;
            }
            "files.copy_link" => {
                let Some(t) = target_of(&u, params) else {
                    state.toast(ToastLevel::Warning, "Select a file first.");
                    return true;
                };
                let mut p = params.clone();
                if let Some(o) = p.as_object_mut() {
                    o.insert("item".into(), json!(t));
                } else {
                    p = json!({ "item": t });
                }
                exec(state, "files.copy_link", p, move |v| {
                    if let Some(url) = v.get("url").and_then(Value::as_str) {
                        copy_to_clipboard(state, url.to_owned());
                    }
                });
                return true;
            }
            _ => return false,
        };
    match dialog {
        Some(d) => u.dialog.set(Some(d)),
        None => state.toast(ToastLevel::Warning, "Select a file first."),
    }
    true
}

fn take_pending(u: &FilesUi) {
    let Some((target, folder)) = PENDING_SHOW.with(|p| p.borrow_mut().take()) else { return };
    if folder {
        let drive = target.split_once('|').map(|(d, _)| d.to_owned()).unwrap_or_default();
        // Keep the location when the folder is in it; else jump to the drive's location.
        let loc = u.location.get_untracked().unwrap_or_default();
        let same = u
            .children
            .data()
            .is_some_and(|c| c.items.iter().any(|r| r.id == target) || c.folder.as_deref() == Some(&target));
        if !same && loc != format!("drive:{drive}") && loc != "mine" {
            u.location.set(Some("mine".into()));
        }
        u.folder.set(Some(target));
        u.selected.set(None);
    } else {
        u.selected.set(Some(target));
    }
}

/// Opens a row: folders navigate in place, files go to `files.open`.
pub fn activate(state: AppState, u: &FilesUi, id: &str) {
    match row_of(u, id) {
        Some(r) if r.folder => {
            u.folder.set(Some(r.id));
            u.selected.set(None);
        }
        Some(r) if r.pending => state.toast(ToastLevel::Info, "Still uploading…"),
        Some(r) => crate::commands::dispatch(state, "files.open", json!({ "item": r.id })),
        None => {}
    }
}

/// Goes to the parent folder (Backspace).
fn go_up(u: &FilesUi) {
    let crumbs = u.children.data().map(|c| c.breadcrumb.clone()).unwrap_or_default();
    if crumbs.len() < 2 {
        return;
    }
    let parent = crumbs.get(crumbs.len().saturating_sub(2)).map(|c| c.id.clone());
    let came_from = u.folder.get_untracked();
    match parent {
        Some(p) if p.contains('|') => u.folder.set(Some(p)),
        _ => u.folder.set(None),
    }
    u.selected.set(came_from);
}

fn report(u: &FilesUi) {
    let generation = REPORT_GEN.with(|g| {
        let n = g.get().wrapping_add(1);
        g.set(n);
        n
    });
    let payload = json!({
        "area": "files",
        "location": u.location.get_untracked(),
        "folder": u.folder.get_untracked(),
        "selection": u.selected.get_untracked().into_iter().collect::<Vec<_>>(),
        "view": if u.grid.get_untracked() { "grid" } else { "list" },
        "sort": u.sort.get_untracked().column,
    });
    set_timeout(
        move || {
            if REPORT_GEN.with(Cell::get) == generation {
                spawn_local(async move {
                    let _ = crate::ipc::run("ui.report", payload).await;
                });
            }
        },
        Duration::from_millis(300),
    );
}

fn location_icon(kind: &str) -> IconName {
    match kind {
        "recent" => IconName::Clock,
        "shared" => IconName::People,
        "list" => IconName::Inbox,
        "library" => IconName::Archive,
        _ => IconName::Folder,
    }
}

fn location_row(l: LocationRow) -> AnyView {
    let depth = l.depth.min(4);
    match l.kind.as_str() {
        "header" => view! { <div class="files-nav-header">{l.name}</div> }.into_any(),
        "site" => view! {
            <div class="nav-row files-nav-site">
                <SiteIcon name=l.name.clone() />
                <span class="nav-row-label">{l.name}</span>
            </div>
        }
        .into_any(),
        kind => view! {
            <div class="nav-row" style=format!("padding-left: calc(var(--space-3) + {}px)", depth.saturating_sub(1) * 14)>
                <Icon name=location_icon(kind) />
                <span class="nav-row-label">{l.name}</span>
            </div>
        }
        .into_any(),
    }
}

fn sharing_label(r: &FileRow) -> &'static str {
    if r.shared { "Shared" } else { "Private" }
}

fn file_row(r: FileRow) -> AnyView {
    let size = if r.folder {
        if r.child_count == 1 { "1 item".to_owned() } else { format!("{} items", r.child_count) }
    } else {
        format_size(r.size)
    };
    let sharing = sharing_label(&r);
    view! {
        <div class="files-row" class:pending=r.pending>
            <span class="files-cell files-name">
                <FileIcon kind=r.kind.clone() />
                <span class="files-name-text">{r.name.clone()}</span>
                {r.pending.then(|| view! { <span class="files-pending" title="Uploading">"…"</span> })}
            </span>
            <span class="files-cell files-modified">{r.modified_label.clone()}</span>
            <span class="files-cell files-by">{r.modified_by.clone()}</span>
            <span class="files-cell files-size">{size}</span>
            <span class="files-cell files-sharing" class:shared=r.shared>
                {r.shared.then(|| view! { <Icon name=IconName::People /> })}
                {sharing}
            </span>
        </div>
    }
    .into_any()
}

/// Column header with sort toggles.
#[component]
fn ColumnHeader(sort: RwSignal<Sort>) -> impl IntoView {
    let col = move |column: &'static str, label: &'static str, class: &'static str| {
        let active = move || sort.get().column == column;
        let arrow = move || {
            if !active() {
                ""
            } else if sort.get().desc {
                " ↓"
            } else {
                " ↑"
            }
        };
        let aria = move || {
            if !active() {
                "none"
            } else if sort.get().desc {
                "descending"
            } else {
                "ascending"
            }
        };
        view! {
            <button
                class=format!("files-col {class}")
                class:active=active
                role="columnheader"
                aria-sort=aria
                on:click=move |_| sort.update(|s| {
                    if s.column == column { s.desc = !s.desc } else { *s = Sort { column, desc: column == "modified" } }
                })
            >
                {label}{arrow}
            </button>
        }
    };
    view! {
        <div class="files-header" role="row">
            {col("name", "Name", "files-name")}
            {col("modified", "Modified", "files-modified")}
            {col("modified_by", "Modified by", "files-by")}
            {col("size", "Size", "files-size")}
            <span class="files-col files-sharing">"Sharing"</span>
        </div>
    }
}

/// The thumbnail grid.
#[component]
fn Grid(items: Signal<Arc<Vec<FileRow>>>, u: FilesUi) -> impl IntoView {
    let state = app_state();
    let on_key = move |e: web_sys::KeyboardEvent| {
        let step: isize = match e.key().as_str() {
            "ArrowRight" | "ArrowDown" => 1,
            "ArrowLeft" | "ArrowUp" => -1,
            "Enter" => {
                if let Some(id) = u.selected.get_untracked() {
                    e.prevent_default();
                    activate(state, &u, &id);
                }
                return;
            }
            _ => return,
        };
        e.prevent_default();
        move_selection(&u, &items, step);
    };
    // j / k move in the grid too.
    Effect::new(move |prev: Option<u64>| {
        let (n, req) = state.list_nav.get();
        if prev.is_some_and(|p| p != n)
            && state.area.get_untracked() == Area::Files
            && u.grid.get_untracked()
            && let Some(req) = req
        {
            move_selection(&u, &items, if req == crate::state::ListNav::Next { 1 } else { -1 });
        }
        n
    });
    view! {
        <div class="files-grid" role="listbox" tabindex="0" aria-label="Files" data-primary-list="" on:keydown=on_key>
            <For
                each={move || items.get().iter().take(2_000).cloned().collect::<Vec<_>>()}
                key={|r: &FileRow| (r.id.clone(), r.name.clone(), r.pending)}
                children={move |r: FileRow| {
                    let id = r.id.clone();
                    let id_sel = id.clone();
                    let id_act = id.clone();
                    let selected = move || u.selected.with(|s| s.as_deref() == Some(id_sel.as_str()));
                    view! {
                        <div
                            class="files-tile"
                            class:selected=selected
                            class:pending=r.pending
                            role="option"
                            title=r.name.clone()
                            on:mousedown=move |_| u.selected.set(Some(id.clone()))
                            on:dblclick=move |_| activate(state, &u, &id_act)
                        >
                            <FileIcon kind=r.kind.clone() class="large" />
                            <span class="files-tile-name">{r.name.clone()}</span>
                            <span class="files-tile-meta">{if r.folder { format!("{} items", r.child_count) } else { r.modified_label.clone() }}</span>
                        </div>
                    }
                }}
            />
        </div>
    }
}

fn move_selection(u: &FilesUi, items: &Signal<Arc<Vec<FileRow>>>, step: isize) {
    let list = items.get_untracked();
    if list.is_empty() {
        return;
    }
    let cur = u.selected.get_untracked().and_then(|s| list.iter().position(|r| r.id == s));
    let next = match cur {
        None => 0,
        Some(c) => (c as isize + step).clamp(0, list.len() as isize - 1) as usize,
    };
    if let Some(r) = list.get(next) {
        u.selected.set(Some(r.id.clone()));
    }
}

/// The breadcrumb.
#[component]
fn Breadcrumb(u: FilesUi) -> impl IntoView {
    let crumbs = move || u.children.data().map(|c| c.breadcrumb.clone()).unwrap_or_default();
    view! {
        <nav class="files-crumbs" aria-label="Folder path">
            {move || {
                let list = crumbs();
                if list.len() < 2 {
                    return None;
                }
                let last = list.len().saturating_sub(1);
                Some(list.into_iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let id = c.id.clone();
                        let current = i == last;
                        view! {
                            {(i > 0).then(|| view! { <span class="files-crumb-sep" aria-hidden="true"><Icon name=IconName::ChevronRight /></span> })}
                            <button
                                class="files-crumb"
                                class:current=current
                                aria-current=current.then_some("page")
                                on:click=move |_| {
                                    if id.contains('|') { u.folder.set(Some(id.clone())) } else { u.folder.set(None) }
                                    u.selected.set(None);
                                }
                            >
                                {c.name}
                            </button>
                        }
                    })
                    .collect_view())
            }}
        </nav>
    }
}

/// The command bar: every button is a command id.
#[component]
fn FilesCommandBar(u: FilesUi) -> impl IntoView {
    let state = app_state();
    let has_sel = move || u.selected.get().is_some();
    let in_drive = move || u.location.get().is_some_and(|l| l == "mine" || l.starts_with("drive:"));
    let button = move |id: &'static str, label: &'static str, icon: IconName, primary: bool, needs_sel: bool| {
        let info = move || state.engine_commands.with(|c| c.iter().find(|c| c.id == id).cloned());
        let title = move || {
            let mac = crate::dom::is_mac();
            let i = info();
            let full = i.as_ref().map(|i| i.label.clone()).unwrap_or_else(|| label.to_owned());
            match i.and_then(|i| i.shortcut) {
                Some(s) => format!("{full} ({})", crate::keys::display_shortcut(&s, mac)),
                None => full,
            }
        };
        let disabled = move || (needs_sel && !has_sel()) || (!needs_sel && !in_drive());
        view! {
            <button
                class="command-button"
                class:primary=primary
                data-command=id
                title=title
                disabled=disabled
                on:click=move |_| crate::commands::dispatch(state, id, crate::commands::context_params(state, id))
            >
                <Icon name=icon />
                <span class="command-label">{label}</span>
            </button>
        }
    };
    view! {
        <div class="command-bar files-commands" role="toolbar" aria-label="File commands">
            {button("files.new_folder", "New folder", IconName::Plus, true, false)}
            {button("files.upload", "Upload", IconName::Paperclip, false, false)}
            <span class="files-bar-sep" aria-hidden="true"></span>
            {button("files.share", "Share", IconName::Send, false, true)}
            {button("files.copy_link", "Copy link", IconName::Command, false, true)}
            {button("files.download", "Download", IconName::Inbox, false, true)}
            {button("files.rename", "Rename", IconName::Pencil, false, true)}
            {button("files.move", "Move", IconName::Move, false, true)}
            {button("files.delete", "Delete", IconName::Trash, false, true)}
        </div>
    }
}

/// List / thumbnails switch.
#[component]
fn ViewToggle(u: FilesUi) -> impl IntoView {
    view! {
        <div class="files-view-toggle" role="group" aria-label="View">
            <button class="files-toggle" class:active=move || !u.grid.get() aria-pressed=move || (!u.grid.get()).to_string() title="List" on:click=move |_| u.grid.set(false)>
                <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" aria-hidden="true"><path d="M4 6.5h16M4 12h16M4 17.5h16" /></svg>
            </button>
            <button class="files-toggle" class:active=move || u.grid.get() aria-pressed=move || u.grid.get().to_string() title="Thumbnails" on:click=move |_| u.grid.set(true)>
                <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" aria-hidden="true"><path d="M4.5 4.5h6v6h-6zM13.5 4.5h6v6h-6zM4.5 13.5h6v6h-6zM13.5 13.5h6v6h-6z" /></svg>
            </button>
        </div>
    }
}

/// Reads `files` (from the picker or a browser drop) and uploads them inline.
fn upload_files(state: AppState, u: FilesUi, files: web_sys::FileList) {
    const MAX_INLINE: f64 = 64.0 * 1024.0 * 1024.0;
    let Some(folder) = current_target(&u) else {
        state.toast(ToastLevel::Warning, "Open a folder in My files or a library to upload.");
        return;
    };
    let list: Vec<web_sys::File> = (0..files.length()).filter_map(|i| files.get(i)).collect();
    if list.is_empty() {
        return;
    }
    spawn_local(async move {
        let mut payload = Vec::new();
        for f in list.into_iter().take(100) {
            if f.size() > MAX_INLINE {
                state.toast(ToastLevel::Warning, format!("{} is over 64 MB: drag it from Finder instead.", f.name()));
                continue;
            }
            match wasm_bindgen_futures::JsFuture::from(f.array_buffer()).await {
                Ok(buf) => {
                    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                    payload.push(json!({ "name": f.name(), "data": base64(&bytes) }));
                }
                Err(_) => state.toast(ToastLevel::Error, format!("Couldn't read {}.", f.name())),
            }
        }
        if payload.is_empty() {
            return;
        }
        let n = payload.len();
        match crate::ipc::run("files.upload", json!({ "files": payload, "folder": folder })).await {
            Reply::Ok { .. } => state.toast(
                ToastLevel::Info,
                if n == 1 { "Uploading 1 file".to_owned() } else { format!("Uploading {n} files") },
            ),
            Reply::Err { message, .. } => state.toast(ToastLevel::Error, message),
        }
    });
}

/// Upload progress, bottom of the list pane.
#[component]
fn UploadTray() -> impl IntoView {
    let uploads = use_query::<Vec<UploadRow>>("files.uploads", || Value::Null, &["setting", "drive_item"]);
    let active = Memo::new(move |_| {
        uploads
            .data()
            .map(|v| v.iter().filter(|r| r.state == "queued" || r.state == "uploading").cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    });
    // Poll while something is in flight (progress lands in the store as chunks go out).
    Effect::new(move |_| {
        if !active.get().is_empty() {
            set_timeout(move || uploads.refetch(), Duration::from_millis(900));
        }
    });
    move || {
        let rows = active.get();
        (!rows.is_empty()).then(|| {
            view! {
                <div class="files-uploads" role="status" aria-live="polite">
                    {rows
                        .into_iter()
                        .map(|r| {
                            let pct = r.sent.saturating_mul(100).checked_div(r.total).unwrap_or(0).min(100);
                            view! {
                                <div class="files-upload">
                                    <span class="files-upload-name">{r.name}</span>
                                    <span class="files-upload-pct">{format!("{pct}%")}</span>
                                    <div class="files-upload-bar"><div class="files-upload-fill" style=format!("width: {pct}%")></div></div>
                                </div>
                            }
                        })
                        .collect_view()}
                </div>
            }
        })
    }
}

/// The folder pane: breadcrumb, command bar, list or grid, drop target, upload tray.
#[component]
fn FolderPane(u: FilesUi) -> impl IntoView {
    let state = app_state();
    let items = Memo::new(move |_| Arc::new(u.children.data().map(|c| c.items.clone()).unwrap_or_default()));
    let items: Signal<Arc<Vec<FileRow>>> = items.into();
    let status = Memo::new(move |_| {
        u.children.state.with(|s| match s {
            QueryState::Loading => 0u8,
            QueryState::Failed { .. } => 1,
            QueryState::Ready(c) if c.items.is_empty() && c.syncing => 3,
            QueryState::Ready(_) => 2,
        })
    });
    let dragging = RwSignal::new(false);
    let picker = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |prev: Option<u64>| {
        let n = u.pick.get();
        if prev.is_some_and(|p| p != n)
            && let Some(input) = picker.get_untracked()
        {
            input.click();
        }
        n
    });
    let on_pick = move |_| {
        if let Some(input) = picker.get_untracked() {
            if let Some(files) = input.files() {
                upload_files(state, u, files);
            }
            input.set_value("");
        }
    };
    let on_key = move |e: web_sys::KeyboardEvent| {
        if e.meta_key() || e.ctrl_key() || e.alt_key() || crate::dom::is_editable(e.target().as_ref()) {
            return;
        }
        match e.key().as_str() {
            "Backspace" => {
                e.prevent_default();
                go_up(&u);
            }
            "F2" => {
                e.prevent_default();
                run_ui(state, "files.rename", &Value::Null);
            }
            "Delete" => {
                e.prevent_default();
                run_ui(state, "files.delete", &Value::Null);
            }
            _ => {}
        }
    };
    let activate_cb = Callback::new(move |id: String| activate(state, &u, &id));
    let key: fn(&FileRow) -> String = <FileRow as Row>::key;
    let render = Callback::new(file_row);
    let title = move || {
        u.children.data().and_then(|c| c.breadcrumb.last().map(|b| b.name.clone())).unwrap_or_else(|| "Files".into())
    };
    view! {
        <div
            class="list-pane files-pane"
            class:dragging=move || dragging.get()
            on:keydown=on_key
            on:dragover=move |e: web_sys::DragEvent| {
                e.prevent_default();
                dragging.set(true);
            }
            on:dragleave=move |_| dragging.set(false)
            on:drop=move |e: web_sys::DragEvent| {
                e.prevent_default();
                dragging.set(false);
                if let Some(files) = e.data_transfer().and_then(|d| d.files()) {
                    upload_files(state, u, files);
                }
            }
        >
            <FilesCommandBar u=u />
            <header class="list-header files-list-header">
                <div class="files-title-row">
                    <h2 class="list-title">{title}</h2>
                    <ViewToggle u=u />
                </div>
                <Breadcrumb u=u />
            </header>
            <input node_ref=picker class="files-picker" type="file" multiple=true tabindex="-1" aria-hidden="true" on:change=on_pick />
            <div class="list-body files-body">
                {move || match status.get() {
                    0 => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
                    1 => {
                        let msg = u.children.state.with(|s| match s {
                            QueryState::Failed { message, .. } => message.clone(),
                            _ => String::new(),
                        });
                        view! { <EmptyState title="Couldn't load this folder" detail=msg error=true /> }.into_any()
                    }
                    3 => view! { <div class="list-loading files-syncing"><Spinner /><span>"Syncing your files…"</span></div> }.into_any(),
                    _ if items.with(|v| v.is_empty()) => view! {
                        <EmptyState title="This folder is empty" detail="Drop files here or use Upload." icon=IconName::Folder />
                    }
                    .into_any(),
                    _ if u.grid.get() => view! { <Grid items=items u=u /> }.into_any(),
                    _ => view! {
                        <div class="files-table" role="grid" aria-label="Files">
                            <ColumnHeader sort=u.sort />
                            <div class="list-wrap files-rows">
                                <VirtualList items=items key=key render=render selected=u.selected label="Files" primary=true on_activate=activate_cb />
                            </div>
                        </div>
                    }
                    .into_any(),
                }}
                <div class="files-drop-hint" aria-hidden="true">
                    <Icon name=IconName::Paperclip />
                    <span>"Drop to upload"</span>
                </div>
            </div>
            <UploadTray />
        </div>
    }
}

/// The Files area.
#[component]
pub fn FilesArea() -> impl IntoView {
    let state = app_state();
    let sel = state.selection(Area::Files);
    if sel.nav.get_untracked().is_none() {
        sel.nav.set(Some("mine".into()));
    }
    let folder = RwSignal::new(None::<String>);
    let sort = RwSignal::new(Sort { column: "name", desc: false });
    let grid = RwSignal::new(crate::dom::storage_get("quorum.files.view").is_some_and(|v| v == "grid"));
    let dialog = RwSignal::new(None::<Dialog>);
    let pick = RwSignal::new(0u64);
    let location = sel.nav;
    let children = use_query::<Children>(
        "files.children",
        move || {
            let s = sort.get();
            let loc = location.get().unwrap_or_else(|| "mine".into());
            match folder.get() {
                Some(f) => json!({ "folder": f, "sort": s.column, "desc": s.desc }),
                None if loc.starts_with("list:") || loc.starts_with("site:") || loc == "sites" => {
                    json!({ "location": "mine" })
                }
                None => json!({ "location": loc, "sort": s.column, "desc": s.desc }),
            }
        },
        TABLES,
    );
    let u = FilesUi { location, folder, selected: sel.item, sort, grid, dialog, children, pick };
    UI.with(|c| c.set(Some(u)));
    on_cleanup(|| UI.with(|c| c.set(None)));
    take_pending(&u);

    let locations = use_query::<Vec<LocationRow>>("files.locations", || Value::Null, TABLES);
    // Changing location resets the folder; headers and sites jump to their first library.
    Effect::new(move |prev: Option<Option<String>>| {
        let loc = location.get();
        if let Some(l) = loc.as_deref()
            && (l == "sites" || l.starts_with("site:"))
            && let Some(rows) = locations.data()
        {
            let start = rows.iter().position(|r| r.id == l).unwrap_or(0);
            if let Some(next) = rows.iter().skip(start + 1).find(|r| r.kind == "library" || r.kind == "list") {
                location.set(Some(next.id.clone()));
                return loc;
            }
        }
        if prev.is_some_and(|p| p != loc) {
            folder.set(None);
            sel.item.set(None);
        }
        loc
    });
    Effect::new(move |_| {
        crate::dom::storage_set("quorum.files.view", if grid.get() { "grid" } else { "list" });
    });
    Effect::new(move |_| {
        let _ = (location.get(), folder.get(), sel.item.get(), grid.get(), sort.get());
        report(&u);
    });
    let is_list = move || location.get().is_some_and(|l| l.starts_with("list:"));
    let list_ref = move || {
        let l = location.get().unwrap_or_default();
        let rest = l.strip_prefix("list:").unwrap_or_default().to_owned();
        let (site, list) = rest.split_once('|').map(|(a, b)| (a.to_owned(), b.to_owned())).unwrap_or_default();
        (site, list)
    };
    let list_name = move || {
        let id = location.get();
        locations
            .data()
            .and_then(|l| l.iter().find(|x| Some(&x.id) == id.as_ref()).map(|x| x.name.clone()))
            .unwrap_or_default()
    };
    view! {
        <AreaLayout
            id="files"
            nav_label="Locations"
            nav=move || {
                view! {
                    <QueryList query=locations selected=sel.nav label="Locations" render=location_row empty="No locations" class="nav-list files-nav" />
                }
            }
            list_label="Files"
            list=move || {
                // Re-rendered only when the pane kind (or the open list) changes.
                let open_list = Memo::new(move |_| is_list().then(list_ref));
                move || match open_list.get() {
                    Some((site, list)) => {
                        view! { <ListTable site=site list=list name=untrack(list_name) selected=sel.item /> }.into_any()
                    }
                    None => view! { <FolderPane u=u /> }.into_any(),
                }
            }
            detail_label="Details"
            detail=move || {
                move || {
                    if is_list() {
                        let (site, list) = list_ref();
                        return match sel.item.get() {
                            Some(item) => view! { <ListDetails site=site list=list item=item /> }.into_any(),
                            None => view! { <EmptyState title="Select an item" icon=IconName::Inbox /> }.into_any(),
                        };
                    }
                    match sel.item.get() {
                        Some(id) => view! { <Details id=id /> }.into_any(),
                        None => {
                            let summary = u.children.data().map(|c| {
                                let n = c.items.len();
                                if n == 1 { "1 item".to_owned() } else { format!("{n} items") }
                            });
                            view! { <EmptyState title="Select a file to see its details" detail=summary.unwrap_or_default() icon=IconName::File /> }.into_any()
                        }
                    }
                }
            }
        />
        <DialogHost dialog=dialog />
    }
}

#[cfg(test)]
mod tests {
    use super::{base64, format_size};

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert!(format_size(u64::MAX).ends_with("TB"));
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }
}
