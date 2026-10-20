//! App state and routing: current area, per-area selections, toasts, account, the command
//! list, and [`use_query`], which re-fetches when a `store_changed` event names its tables.
//!
//! [`AppState`] is a bundle of `Copy` signal handles provided as context by the shell; get it
//! anywhere with [`app_state`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::{CommandInfo, CoreEvent, Reply};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::components::avatar::Presence;
use crate::icons::IconName;
use crate::ipc;

/// The app areas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Area {
    /// Outlook-style mail.
    Mail,
    /// The merged calendar.
    Calendar,
    /// Teams chats.
    Chat,
    /// Teams and channels.
    Teams,
    /// OneDrive and SharePoint files.
    Files,
    /// People and presence.
    People,
    /// Universal search results.
    Search,
    /// Settings.
    Settings,
}

impl Area {
    /// Every area, rail order first.
    pub const ALL: [Area; 8] =
        [Self::Mail, Self::Calendar, Self::Chat, Self::Teams, Self::Files, Self::People, Self::Search, Self::Settings];

    /// The areas on the app rail (Search lives in the top bar, Settings at the rail's foot).
    pub const RAIL: [Area; 6] = [Self::Mail, Self::Calendar, Self::Chat, Self::Teams, Self::Files, Self::People];

    /// Stable id, used in command ids (`view.<id>`) and storage keys.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Calendar => "calendar",
            Self::Chat => "chat",
            Self::Teams => "teams",
            Self::Files => "files",
            Self::People => "people",
            Self::Search => "search",
            Self::Settings => "settings",
        }
    }

    /// Parses [`Area::id`].
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }

    /// Display name.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mail => "Mail",
            Self::Calendar => "Calendar",
            Self::Chat => "Chat",
            Self::Teams => "Teams",
            Self::Files => "Files",
            Self::People => "People",
            Self::Search => "Search",
            Self::Settings => "Settings",
        }
    }

    /// Rail icon.
    pub const fn icon(self) -> IconName {
        match self {
            Self::Mail => IconName::Mail,
            Self::Calendar => IconName::Calendar,
            Self::Chat => IconName::Chat,
            Self::Teams => IconName::Teams,
            Self::Files => IconName::Folder,
            Self::People => IconName::People,
            Self::Search => IconName::Search,
            Self::Settings => IconName::Settings,
        }
    }

    /// Engine command id prefixes that belong to this area (context for shared shortcuts).
    pub const fn command_prefixes(self) -> &'static [&'static str] {
        match self {
            Self::Mail => &["mail."],
            Self::Calendar => &["calendar."],
            Self::Chat => &["chat."],
            Self::Teams => &["channel.", "teams."],
            Self::Files => &["drive.", "files."],
            Self::People => &["people.", "presence."],
            Self::Search => &["search."],
            Self::Settings => &["settings."],
        }
    }

    /// The UI command that switches to this area.
    pub fn view_command(self) -> String {
        format!("view.{}", self.id())
    }
}

/// Toast severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    /// Neutral information.
    Info,
    /// Something worked.
    Success,
    /// Needs attention.
    Warning,
    /// Something failed.
    Error,
}

impl ToastLevel {
    /// Parses the `level` of [`CoreEvent::Toast`]; unknown levels are `Info`.
    pub fn parse(s: &str) -> Self {
        match s {
            "success" => Self::Success,
            "warning" => Self::Warning,
            "error" => Self::Error,
            _ => Self::Info,
        }
    }

    /// CSS modifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    fn lifetime(self) -> Duration {
        match self {
            Self::Error | Self::Warning => Duration::from_secs(8),
            Self::Info | Self::Success => Duration::from_secs(4),
        }
    }
}

/// One transient message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// Unique id.
    pub id: u64,
    /// Severity.
    pub level: ToastLevel,
    /// Text (never contains personal data from the core: messages are pre-redacted).
    pub message: String,
}

/// At most this many toasts are visible; older ones are dropped.
pub const MAX_TOASTS: usize = 4;

/// Account sign-in state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AccountState {
    /// Not known yet.
    #[default]
    Unknown,
    /// No account.
    SignedOut,
    /// Sign-in in progress.
    SigningIn,
    /// Signed in.
    SignedIn,
    /// The refresh token expired; the user must sign in again.
    NeedsReauth,
}

impl AccountState {
    /// Parses the `state` of [`CoreEvent::Account`].
    pub fn parse(s: &str) -> Self {
        match s {
            "signed_out" => Self::SignedOut,
            "signing_in" => Self::SigningIn,
            "signed_in" => Self::SignedIn,
            "needs_reauth" => Self::NeedsReauth,
            _ => Self::Unknown,
        }
    }
}

/// The signed-in account as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Account {
    /// Sign-in state.
    pub state: AccountState,
    /// Display name.
    pub name: Option<String>,
    /// Primary address.
    pub email: Option<String>,
    /// Own presence.
    pub presence: Presence,
}

/// Selection inside one area: the navigation column (folder, calendar, location) and the item.
#[derive(Debug, Clone, Copy)]
pub struct AreaSelection {
    /// Selected folder / calendar / location id.
    pub nav: RwSignal<Option<String>>,
    /// Selected list item id.
    pub item: RwSignal<Option<String>>,
}

impl AreaSelection {
    fn new() -> Self {
        Self { nav: RwSignal::new(None), item: RwSignal::new(None) }
    }
}

/// A list navigation request (from `j`/`k` and other commands) for the area's primary list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListNav {
    /// Next item.
    Next,
    /// Previous item.
    Previous,
}

/// Global UI state. `Copy`: every field is a signal handle.
#[derive(Debug, Clone, Copy)]
pub struct AppState {
    /// The area shown in the main region.
    pub area: RwSignal<Area>,
    selections: [AreaSelection; 8],
    /// Visible toasts, oldest first.
    pub toasts: RwSignal<Vec<Toast>>,
    next_toast: StoredValue<u64>,
    /// The account.
    pub account: RwSignal<Account>,
    /// Commands from the engine registry (`app.commands`).
    pub engine_commands: RwSignal<Vec<CommandInfo>>,
    /// Whether the command palette is open.
    pub palette_open: RwSignal<bool>,
    /// The palette's query text.
    pub palette_query: RwSignal<String>,
    /// Text in the top-bar search box.
    pub search_query: RwSignal<String>,
    /// Whether the folder/navigation column is shown.
    pub nav_pane_visible: RwSignal<bool>,
    /// Params of the latest `view.open` (extra keys such as `message` / `post` for areas to read).
    pub open_request: RwSignal<Option<Value>>,
    /// Latest list navigation request, with a counter so repeats are distinct.
    pub list_nav: RwSignal<(u64, Option<ListNav>)>,
    table_revs: RwSignal<HashMap<String, u64>>,
    /// Whether the UI runs on the mock backend (browser preview).
    pub preview: bool,
}

/// The wildcard table name: a `store_changed` naming it refreshes every query.
pub const ALL_TABLES: &str = "*";

impl AppState {
    /// Creates the state (call once, in the root component).
    pub fn new(preview: bool) -> Self {
        let nav_visible = crate::dom::storage_get("quorum.nav-pane").is_none_or(|v| v != "hidden");
        Self {
            area: RwSignal::new(Area::Mail),
            selections: [
                AreaSelection::new(),
                AreaSelection::new(),
                AreaSelection::new(),
                AreaSelection::new(),
                AreaSelection::new(),
                AreaSelection::new(),
                AreaSelection::new(),
                AreaSelection::new(),
            ],
            toasts: RwSignal::new(Vec::new()),
            next_toast: StoredValue::new(0),
            account: RwSignal::new(Account::default()),
            engine_commands: RwSignal::new(Vec::new()),
            palette_open: RwSignal::new(false),
            palette_query: RwSignal::new(String::new()),
            search_query: RwSignal::new(String::new()),
            nav_pane_visible: RwSignal::new(nav_visible),
            open_request: RwSignal::new(None),
            list_nav: RwSignal::new((0, None)),
            table_revs: RwSignal::new(HashMap::new()),
            preview,
        }
    }

    /// The selection of `area`.
    pub fn selection(&self, area: Area) -> AreaSelection {
        let i = Area::ALL.iter().position(|a| *a == area).unwrap_or(0);
        self.selections
            .get(i)
            .copied()
            .unwrap_or_else(|| AreaSelection { nav: RwSignal::new(None), item: RwSignal::new(None) })
    }

    /// Engine commands merged with the UI-only commands (UI ids win).
    pub fn all_commands(&self) -> Vec<CommandInfo> {
        self.engine_commands.with_untracked(|e| crate::commands::merged(e))
    }

    /// Like [`AppState::all_commands`] but tracked, for reactive views.
    pub fn all_commands_tracked(&self) -> Vec<CommandInfo> {
        self.engine_commands.with(|e| crate::commands::merged(e))
    }

    /// Shows a toast that dismisses itself.
    pub fn toast(&self, level: ToastLevel, message: impl Into<String>) {
        let id = self.next_toast.try_get_value().unwrap_or(0).wrapping_add(1);
        self.next_toast.set_value(id);
        let message = message.into();
        self.toasts.update(|t| {
            t.push(Toast { id, level, message });
            let excess = t.len().saturating_sub(MAX_TOASTS);
            t.drain(..excess);
        });
        let toasts = self.toasts;
        set_timeout(move || dismiss(toasts, id), level.lifetime());
    }

    /// Removes a toast.
    pub fn dismiss_toast(&self, id: u64) {
        dismiss(self.toasts, id);
    }

    /// Switches area and moves focus to its primary list. Going to the area already shown does
    /// nothing, so a core `view.open` for the open area neither remounts it nor steals focus.
    pub fn go(&self, area: Area) {
        if self.area.get_untracked() == area {
            return;
        }
        self.area.set(area);
        request_animation_frame(|| {
            let _ = crate::dom::focus_selector("[data-primary-list]");
        });
    }

    /// Marks `tables` changed so queries over them re-fetch.
    pub fn tables_changed(&self, tables: &[String]) {
        self.table_revs.update(|revs| {
            for t in tables {
                let r = revs.entry(t.clone()).or_insert(0);
                *r = r.wrapping_add(1);
            }
        });
    }

    /// Sum of the revisions of `tables` (plus the wildcard), tracked.
    fn revision(&self, tables: &[&str]) -> u64 {
        self.table_revs.with(|revs| table_revision(revs, tables))
    }

    /// Asks the area's primary list to move its selection.
    pub fn nav_list(&self, nav: ListNav) {
        self.list_nav.update(|(n, req)| {
            *n = n.wrapping_add(1);
            *req = Some(nav);
        });
    }

    /// Applies a core event.
    pub fn handle_event(&self, event: &CoreEvent) {
        match event {
            CoreEvent::StoreChanged { tables } => {
                self.tables_changed(tables);
                if tables.iter().any(|t| t == "commands" || t == ALL_TABLES) {
                    self.load_commands();
                }
            }
            CoreEvent::Toast { level, message } => self.toast(ToastLevel::parse(level), message.clone()),
            CoreEvent::Ui { command, params } => {
                // Menu bar and tray selections arrive with null params: give engine commands the
                // same context a shortcut would (the current selection).
                let params = if params.is_null() && !crate::commands::is_ui(command) {
                    crate::commands::context_params(*self, command)
                } else {
                    params.clone()
                };
                crate::commands::dispatch(*self, command, params);
            }
            CoreEvent::Account { state } => {
                self.account.update(|a| a.state = AccountState::parse(state));
                self.load_commands();
                self.load_account();
            }
        }
    }

    /// (Re)loads the engine command list (`app.commands`).
    pub fn load_commands(&self) {
        let commands = self.engine_commands;
        let state = *self;
        spawn_local(async move {
            match ipc::query("app.commands", Value::Null).await {
                Reply::Ok { value } => match serde_json::from_value::<Vec<CommandInfo>>(value) {
                    Ok(list) => {
                        commands.try_set(list);
                    }
                    Err(_) => state.toast(ToastLevel::Error, "The command list could not be read."),
                },
                Reply::Err { message, .. } => state.toast(ToastLevel::Error, message),
            }
        });
    }

    /// Loads the account summary (`account.current`); absent queries leave the defaults.
    pub fn load_account(&self) {
        let account = self.account;
        spawn_local(async move {
            if let Reply::Ok { value } = ipc::query("account.current", Value::Null).await {
                let text = |k: &str| value.get(k).and_then(Value::as_str).map(str::to_owned);
                account.try_update(|a| {
                    // The engine answers `display_name` / `username`; `name` / `email` are the
                    // older shape, kept as a fallback.
                    a.name = text("display_name").or_else(|| text("name"));
                    a.email = text("username").or_else(|| text("email"));
                    if let Some(p) = text("presence") {
                        a.presence = Presence::parse(&p);
                    }
                    if let Some(s) = text("state") {
                        a.state = AccountState::parse(&s);
                    }
                });
            }
        });
    }
}

fn dismiss(toasts: RwSignal<Vec<Toast>>, id: u64) {
    toasts.try_update(|t| t.retain(|x| x.id != id));
}

/// Sum of the revisions of `tables` and of the wildcard (pure, for tests).
pub fn table_revision(revs: &HashMap<String, u64>, tables: &[&str]) -> u64 {
    let wildcard = revs.get(ALL_TABLES).copied().unwrap_or(0);
    tables.iter().fold(wildcard, |acc, t| acc.wrapping_add(revs.get(*t).copied().unwrap_or(0)))
}

/// The area the main region renders. A memo, so setting [`AppState::area`] to the area already
/// shown doesn't rebuild it: a rebuilt Chat area reopened its chat, whose `view.open` set the area
/// again, and the window spun forever.
pub fn shown_area(area: RwSignal<Area>) -> Memo<Area> {
    Memo::new(move |_| area.get())
}

/// The app state from context.
pub fn app_state() -> AppState {
    use_context::<AppState>().unwrap_or_else(|| {
        ipc::warn("AppState missing from context; using a detached state");
        let s = AppState::new(ipc::is_preview());
        provide_context(s);
        s
    })
}

/// State of a query.
#[derive(Debug)]
pub enum QueryState<T> {
    /// First load in progress (or the params changed).
    Loading,
    /// Data (kept while a refresh is in flight, so lists don't flash).
    Ready(Arc<T>),
    /// The query failed; `message` is safe to show.
    Failed {
        /// Machine-readable kind.
        kind: String,
        /// Human-readable message.
        message: String,
    },
}

impl<T> Clone for QueryState<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Loading => Self::Loading,
            Self::Ready(v) => Self::Ready(Arc::clone(v)),
            Self::Failed { kind, message } => Self::Failed { kind: kind.clone(), message: message.clone() },
        }
    }
}

impl<T> QueryState<T> {
    /// The data, if ready.
    pub fn data(&self) -> Option<Arc<T>> {
        match self {
            Self::Ready(v) => Some(Arc::clone(v)),
            _ => None,
        }
    }
}

/// A store-backed query bound to the reactive graph. `Copy`.
#[derive(Debug)]
pub struct Query<T: Send + Sync + 'static> {
    /// Current state.
    pub state: RwSignal<QueryState<T>>,
    nonce: RwSignal<u64>,
}

impl<T: Send + Sync + 'static> Clone for Query<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for Query<T> {}

impl<T: Send + Sync + 'static> Query<T> {
    // Queries are often reached from timers and async replies that can outlive the component
    // that owns them, so every access is `try_*`: a disposed query is a no-op, not a panic.

    /// Re-fetches now (no-op once the owning component is gone).
    pub fn refetch(&self) {
        let _ = self.nonce.try_update(|n| *n = n.wrapping_add(1));
    }

    /// The data, if ready (tracked); `None` once the owning component is gone.
    pub fn data(&self) -> Option<Arc<T>> {
        self.state.try_with(QueryState::data).flatten()
    }

    /// Optimistic update: changes the local copy now; the next refresh from the store replaces
    /// it (with the real result, or the old data if the command failed).
    pub fn update_local(&self, f: impl FnOnce(&T) -> T) {
        let next = self.state.try_with_untracked(|s| s.data().map(|d| f(&d))).flatten();
        if let Some(next) = next {
            let _ = self.state.try_set(QueryState::Ready(Arc::new(next)));
        }
    }
}

/// Runs query `id` with `params()` and keeps the result fresh.
///
/// Re-fetches when `params()` changes (tracked), when a `store_changed` event names one of
/// `tables` (or `*`), and on [`Query::refetch`]. Stale replies are dropped. The result is
/// decoded into `T`; a shape mismatch shows as [`QueryState::Failed`] with kind `bug`.
pub fn use_query<T>(id: &'static str, params: impl Fn() -> Value + 'static, tables: &'static [&'static str]) -> Query<T>
where
    T: DeserializeOwned + Send + Sync + 'static,
{
    let app = app_state();
    let state = RwSignal::new(QueryState::Loading);
    let nonce = RwSignal::new(0u64);
    let generation = StoredValue::new(0u64);
    let last_params = StoredValue::new(None::<Value>);
    let revision = Memo::new(move |_| app.revision(tables));
    Effect::new(move |_| {
        let p = params();
        // `get`, not `track`: a memo only subscribes to its own inputs once it has computed.
        let _ = revision.get();
        nonce.track();
        let current = generation.try_get_value().unwrap_or(0).wrapping_add(1);
        generation.set_value(current);
        let changed = last_params.try_with_value(|l| l.as_ref() != Some(&p)).unwrap_or(true);
        if changed {
            state.set(QueryState::Loading);
            last_params.set_value(Some(p.clone()));
        }
        spawn_local(async move {
            let reply = ipc::query(id, p).await;
            if generation.try_get_value() != Some(current) {
                return; // a newer fetch started, or the owner was disposed
            }
            let next = match reply {
                Reply::Ok { value } => match serde_json::from_value::<T>(value) {
                    Ok(v) => QueryState::Ready(Arc::new(v)),
                    Err(e) => {
                        QueryState::Failed { kind: "bug".into(), message: format!("Unexpected data from {id}: {e}") }
                    }
                },
                Reply::Err { kind, message } => QueryState::Failed { kind, message },
            };
            state.try_set(next);
        });
    });
    Query { state, nonce }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reselecting_the_shown_area_does_not_rebuild_it() {
        let owner = Owner::new();
        owner.set();
        let area = RwSignal::new(Area::Mail);
        let shown = shown_area(area);
        let builds = StoredValue::new(0u32);
        let main = Memo::new(move |_| {
            builds.update_value(|b| *b += 1);
            shown.get()
        });
        assert_eq!(main.get(), Area::Mail);
        for _ in 0..5 {
            area.set(Area::Mail);
            assert_eq!(main.get(), Area::Mail);
        }
        assert_eq!(builds.get_value(), 1, "same area must not rebuild the main view");
        area.set(Area::Chat);
        assert_eq!(main.get(), Area::Chat);
        area.set(Area::Chat);
        assert_eq!(main.get(), Area::Chat);
        assert_eq!(builds.get_value(), 2);
    }

    #[test]
    fn area_ids_round_trip() {
        for a in Area::ALL {
            assert_eq!(Area::from_id(a.id()), Some(a));
            assert_eq!(a.view_command(), format!("view.{}", a.id()));
        }
        assert_eq!(Area::from_id("nope"), None);
    }

    #[test]
    fn revision_sums_tables_and_wildcard() {
        let mut revs = HashMap::new();
        assert_eq!(table_revision(&revs, &["messages"]), 0);
        revs.insert("messages".to_owned(), 2);
        revs.insert("chats".to_owned(), 5);
        assert_eq!(table_revision(&revs, &["messages"]), 2);
        assert_eq!(table_revision(&revs, &["events"]), 0);
        revs.insert(ALL_TABLES.to_owned(), 1);
        assert_eq!(table_revision(&revs, &["events"]), 1);
        revs.insert("messages".to_owned(), u64::MAX);
        assert_eq!(table_revision(&revs, &["messages"]), 0); // wraps, never panics
    }

    #[test]
    fn levels_and_states_parse_leniently() {
        assert_eq!(ToastLevel::parse("error"), ToastLevel::Error);
        assert_eq!(ToastLevel::parse("weird"), ToastLevel::Info);
        assert_eq!(AccountState::parse("needs_reauth"), AccountState::NeedsReauth);
        assert_eq!(AccountState::parse(""), AccountState::Unknown);
    }
}
