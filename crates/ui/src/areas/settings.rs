//! Settings: a section list | the section's controls (macOS System Settings layout).
//!
//! Every control reads `settings.get` / `settings.schema` and writes with
//! `settings.set {key, value}`, so agents can change the same things over MCP. The Account
//! section uses `account.current`, `account.permissions`, `account.configure`,
//! `account.sign_in` and `account.sign_out`; Keyboard lists the registry's shortcuts; About
//! shows `app.about` (version and contributors compiled into the core).

use std::cell::Cell;

use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::Reply;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use wasm_bindgen::JsCast;

use crate::components::{EmptyState, Icon, Spinner};
use crate::icons::IconName;
use crate::state::{AppState, ToastLevel, app_state, use_query};

/// The settings sections, in list order: (id, label, icon).
pub const SECTIONS: &[(&str, &str, IconName)] = &[
    ("account", "Account", IconName::People),
    ("general", "General", IconName::Settings),
    ("notifications", "Notifications", IconName::Inbox),
    ("mail", "Mail", IconName::Mail),
    ("presence", "Presence", IconName::Chat),
    ("appearance", "Appearance", IconName::Pencil),
    ("keyboard", "Keyboard", IconName::Command),
    ("agents", "Agents", IconName::Teams),
    ("about", "About", IconName::Search),
];

/// Which schema `section` each settings section shows.
fn schema_section(id: &str) -> Option<&'static str> {
    match id {
        "general" => Some("app"),
        "notifications" => Some("notifications"),
        "mail" => Some("mail"),
        "presence" => Some("presence"),
        "appearance" => Some("appearance"),
        "agents" => Some("agents"),
        _ => None,
    }
}

thread_local! {
    static SECTION: Cell<Option<RwSignal<String>>> = const { Cell::new(None) };
}

/// The selected section, shared by the area and core events (`view.settings {"section"}`).
pub fn section_signal() -> RwSignal<String> {
    // A signal created under a component that has since unmounted is disposed: make a new one.
    SECTION.with(|s| match s.get().filter(|sig| sig.try_get_untracked().is_some()) {
        Some(sig) => sig,
        None => {
            let sig = RwSignal::new("account".to_owned());
            s.set(Some(sig));
            sig
        }
    })
}

/// Selects a section if `id` names one.
pub fn select_section(id: &str) {
    if SECTIONS.iter().any(|(s, _, _)| *s == id) {
        section_signal().set(id.to_owned());
    }
}

/// One setting definition (`settings.schema`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SettingDef {
    /// Key, e.g. `mail.undo_send_seconds`.
    pub key: String,
    /// Label.
    pub label: String,
    /// Schema section.
    pub section: String,
    /// `bool`, `int` or `choice`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Minimum (int).
    #[serde(default)]
    pub min: Option<i64>,
    /// Maximum (int).
    #[serde(default)]
    pub max: Option<i64>,
    /// Choices (choice).
    #[serde(default)]
    pub choices: Vec<String>,
}

/// Runs a command; shows its error as a toast. Returns the value on success.
pub async fn run_or_toast(state: AppState, id: &str, params: Value) -> Option<Value> {
    match crate::ipc::run(id, params).await {
        Reply::Ok { value } => Some(value),
        Reply::Err { message, .. } => {
            state.toast(ToastLevel::Error, message);
            None
        }
    }
}

fn set_setting(state: AppState, key: String, value: Value) {
    spawn_local(async move {
        let _ = run_or_toast(state, "settings.set", json!({ "key": key, "value": value })).await;
    });
}

fn input_value(e: &web_sys::Event) -> String {
    e.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()).map(|i| i.value()).unwrap_or_default()
}

fn input_checked(e: &web_sys::Event) -> bool {
    e.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()).is_some_and(|i| i.checked())
}

/// A human label for a choice value.
fn choice_label(v: &str) -> String {
    match v {
        "confirm" => "Ask first".into(),
        "deny" => "Never".into(),
        "allow" => "Allow without asking".into(),
        other => {
            let mut c = other.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
    }
}

/// One setting row: label and its control.
fn setting_row(state: AppState, def: SettingDef, value: Option<Value>) -> AnyView {
    let id = format!("setting-{}", def.key.replace('.', "-"));
    let key = def.key.clone();
    let control = match def.kind.as_str() {
        "bool" => {
            let on = value.as_ref().and_then(Value::as_bool).unwrap_or(false);
            let k = key.clone();
            view! {
                <input
                    id=id.clone()
                    class="switch"
                    type="checkbox"
                    role="switch"
                    prop:checked=on
                    aria-checked=if on { "true" } else { "false" }
                    on:change=move |e| set_setting(state, k.clone(), json!(input_checked(&e)))
                />
            }
            .into_any()
        }
        "int" => {
            let n = value.as_ref().and_then(Value::as_i64).unwrap_or(0);
            let (min, max) = (def.min.unwrap_or(0), def.max.unwrap_or(i64::from(i32::MAX)));
            let k = key.clone();
            let on_change = move |e: web_sys::Event| match input_value(&e).trim().parse::<i64>() {
                Ok(v) if (min..=max).contains(&v) => set_setting(state, k.clone(), json!(v)),
                _ => state.toast(ToastLevel::Warning, format!("Enter a number from {min} to {max}.")),
            };
            view! {
                <input
                    id=id.clone()
                    class="settings-number"
                    type="number"
                    min=min.to_string()
                    max=max.to_string()
                    prop:value=n.to_string()
                    on:change=on_change
                />
            }
            .into_any()
        }
        _ => {
            let current = value.as_ref().and_then(Value::as_str).unwrap_or_default().to_owned();
            view! {
                <div class="segmented" role="radiogroup" id=id.clone() aria-label=def.label.clone()>
                    {def
                        .choices
                        .iter()
                        .map(|c| {
                            let selected = *c == current;
                            let (k, v) = (key.clone(), c.clone());
                            view! {
                                <button
                                    type="button"
                                    role="radio"
                                    class="segment"
                                    class:selected=selected
                                    aria-checked=if selected { "true" } else { "false" }
                                    on:click=move |_| set_setting(state, k.clone(), json!(v.clone()))
                                >
                                    {choice_label(c)}
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
            }
            .into_any()
        }
    };
    view! {
        <div class="settings-row">
            <label class="settings-label" for=id>{def.label}</label>
            <div class="settings-control">{control}</div>
        </div>
    }
    .into_any()
}

/// The Settings area.
#[component]
pub fn SettingsArea() -> impl IntoView {
    let state = app_state();
    let section = section_signal();
    let schema = use_query::<Vec<SettingDef>>("settings.schema", || Value::Null, &[]);
    let values = use_query::<Map<String, Value>>("settings.get", || json!({}), &["settings", "setting"]);
    Effect::new(move |_| {
        let view = section.get();
        spawn_local(async move {
            let _ = crate::ipc::run("ui.report", json!({ "area": "settings", "selection": [], "view": view })).await;
        });
    });
    let on_key = move |e: web_sys::KeyboardEvent| {
        let step: isize = match e.key().as_str() {
            "ArrowDown" | "j" => 1,
            "ArrowUp" | "k" => -1,
            _ => return,
        };
        e.prevent_default();
        let cur = SECTIONS.iter().position(|(s, _, _)| *s == section.get_untracked()).unwrap_or(0);
        let len = isize::try_from(SECTIONS.len()).unwrap_or(1);
        let next = usize::try_from((isize::try_from(cur).unwrap_or(0) + step).rem_euclid(len)).unwrap_or(0);
        if let Some((id, _, _)) = SECTIONS.get(next) {
            section.set((*id).to_owned());
            let _ = crate::dom::focus_selector(&format!("#settings-tab-{id}"));
        }
    };
    let generic = move |sec: &'static str| {
        let defs: Vec<SettingDef> =
            schema.data().map(|d| d.iter().filter(|d| d.section == sec).cloned().collect()).unwrap_or_default();
        let vals = values.data();
        if defs.is_empty() {
            return view! { <Spinner /> }.into_any();
        }
        view! {
            <div class="settings-group">
                {defs
                    .into_iter()
                    .map(|d| {
                        let v = vals.as_ref().and_then(|m| m.get(&d.key).cloned());
                        setting_row(state, d, v)
                    })
                    .collect_view()}
            </div>
        }
        .into_any()
    };
    let content = move || {
        let sec = section.get();
        let title = SECTIONS.iter().find(|(s, _, _)| *s == sec).map(|(_, l, _)| *l).unwrap_or("Settings");
        let body = match sec.as_str() {
            "account" => view! { <AccountSection /> }.into_any(),
            "keyboard" => view! { <KeyboardSection /> }.into_any(),
            "about" => view! { <AboutSection /> }.into_any(),
            other => match schema_section(other) {
                Some(s) => {
                    let extra = match other {
                        "agents" => view! { <AgentsHelp /> }.into_any(),
                        "general" => view! {
                            <p class="settings-note">
                                "On macOS, choose Quorum as your default email app in Mail ▸ Settings ▸ General after turning on email links."
                            </p>
                        }
                        .into_any(),
                        "presence" => view! {
                            <p class="settings-note">"Keep-alive renews your Teams presence while Quorum is open, using the documented presence API."</p>
                        }
                        .into_any(),
                        _ => ().into_any(),
                    };
                    view! { {generic(s)} {extra} }.into_any()
                }
                None => view! { <EmptyState title="Unknown section" /> }.into_any(),
            },
        };
        view! {
            <h1 class="settings-title" id="settings-section-title">{title}</h1>
            {body}
        }
    };
    view! {
        <section class="pane pane-full settings-area" aria-label="Settings">
            <nav class="pane settings-nav" data-pane="" tabindex="-1" aria-label="Settings sections">
                <div class="settings-tabs" role="tablist" aria-orientation="vertical" on:keydown=on_key>
                    {SECTIONS
                        .iter()
                        .map(|(id, label, icon)| {
                            let id = *id;
                            let selected = move || section.get() == id;
                            view! {
                                <button
                                    id=format!("settings-tab-{id}")
                                    class="settings-tab"
                                    role="tab"
                                    class:selected=selected
                                    aria-selected=move || if selected() { "true" } else { "false" }
                                    tabindex=move || if selected() { "0" } else { "-1" }
                                    data-pane-focus=move || selected().then_some("")
                                    data-primary-list=move || selected().then_some("")
                                    on:click=move |_| section.set(id.to_owned())
                                >
                                    <Icon name=*icon />
                                    <span>{*label}</span>
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
            </nav>
            <div
                class="pane settings-content"
                data-pane=""
                tabindex="-1"
                role="tabpanel"
                aria-labelledby="settings-section-title"
            >
                {content}
            </div>
        </section>
    }
}

/// `account.current` (both the engine's and the preview's field names).
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
struct AccountInfo {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    mock: bool,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
struct Permissions {
    #[serde(default)]
    granted: Vec<String>,
    #[serde(default)]
    missing: Vec<String>,
}

/// Tenant / client id fields and the Configure button (shared with onboarding).
#[component]
pub fn TenantForm(
    /// Label of the submit button.
    #[prop(into)]
    submit: String,
    /// Also sign in after saving.
    #[prop(optional)]
    sign_in: bool,
    /// Ids to start with (from a `quorum://setup` link or the saved configuration).
    #[prop(optional)]
    initial: Option<(String, String)>,
) -> impl IntoView {
    let state = app_state();
    let (t0, c0) = initial.unwrap_or_default();
    let tenant = RwSignal::new(t0);
    let client = RwSignal::new(c0);
    let busy = RwSignal::new(false);
    let save = move |e: web_sys::SubmitEvent| {
        e.prevent_default();
        let (t, c) = (tenant.get_untracked().trim().to_owned(), client.get_untracked().trim().to_owned());
        if t.is_empty() || c.is_empty() {
            state.toast(ToastLevel::Warning, "Enter both the tenant ID and the client ID.");
            return;
        }
        busy.set(true);
        spawn_local(async move {
            if run_or_toast(state, "account.configure", json!({ "tenant": t, "client_id": c })).await.is_some() {
                state.toast(ToastLevel::Success, "Tenant saved.");
                if sign_in {
                    let _ = run_or_toast(state, "account.sign_in", json!({})).await;
                }
            }
            busy.try_set(false);
        });
    };
    view! {
        <form class="tenant-form" on:submit=save>
            <label class="field">
                <span>"Directory (tenant) ID"</span>
                <input
                    type="text"
                    spellcheck="false"
                    autocomplete="off"
                    placeholder="00000000-0000-0000-0000-000000000000"
                    prop:value=move || tenant.get()
                    on:input=move |e| tenant.set(input_value(&e))
                />
            </label>
            <label class="field">
                <span>"Application (client) ID"</span>
                <input
                    type="text"
                    spellcheck="false"
                    autocomplete="off"
                    placeholder="00000000-0000-0000-0000-000000000000"
                    prop:value=move || client.get()
                    on:input=move |e| client.set(input_value(&e))
                />
            </label>
            <button class="button primary" type="submit" disabled=move || busy.get()>{submit}</button>
        </form>
    }
}

/// `account.setup`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
struct SetupLinks {
    #[serde(default)]
    tenant: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    admin_consent_url: Option<String>,
    #[serde(default)]
    setup_link: Option<String>,
}

#[component]
fn AccountSection() -> impl IntoView {
    let state = app_state();
    let account = use_query::<AccountInfo>("account.current", || json!({}), &["account"]);
    let perms = use_query::<Permissions>("account.permissions", || json!({}), &["account"]);
    let links = use_query::<SetupLinks>("account.setup", || json!({}), &["account"]);
    let rev = RwSignal::new(0u32);
    let act = move |id: &'static str| {
        spawn_local(async move {
            let _ = run_or_toast(state, id, json!({})).await;
            account.refetch();
            perms.refetch();
            rev.update(|r| *r = r.wrapping_add(1));
        });
    };
    let summary = move || {
        let a = account.data().map(|a| (*a).clone()).unwrap_or_default();
        let signed_in = a.state.as_deref() == Some("signed_in");
        let name = a.display_name.clone().or(a.name.clone()).unwrap_or_else(|| "Not signed in".into());
        let address = a.username.clone().or(a.email.clone()).unwrap_or_default();
        let status = match a.state.as_deref() {
            Some("signed_in") if a.mock => "Signed in (sample data)",
            Some("signed_in") => "Signed in",
            Some("signed_out") => "Signed out",
            Some("not_configured") => "Not set up yet",
            _ => "Unknown",
        };
        view! {
            <div class="settings-group account-card">
                <div class="account-line">
                    <crate::components::Avatar name=name.clone() size="lg" />
                    <div class="item-text">
                        <span class="item-primary">{name}</span>
                        <span class="item-secondary">{address}</span>
                        <span class="item-tertiary">{status}</span>
                    </div>
                    {if signed_in {
                        view! { <button class="button" on:click=move |_| act("account.sign_out")>"Sign out"</button> }.into_any()
                    } else {
                        view! { <button class="button primary" on:click=move |_| act("account.sign_in")>"Sign in"</button> }.into_any()
                    }}
                </div>
                {a.tenant_id.map(|t| view! { <p class="settings-note">"Tenant: " <code>{t}</code></p> })}
            </div>
        }
    };
    let permissions = move || match perms.data() {
        None => view! { <Spinner /> }.into_any(),
        Some(p) => {
            let missing = p.missing.clone();
            let granted = p.granted.clone();
            view! {
                <div class="settings-group">
                    <p class="settings-note">
                        {if missing.is_empty() {
                            "Every permission Quorum uses is granted.".to_owned()
                        } else {
                            match missing.len() {
                                1 => "1 permission is missing; features that need it are turned off. Ask your admin to grant consent.".to_owned(),
                                n => format!("{n} permissions are missing; features that need them are turned off. Ask your admin to grant consent."),
                            }
                        }}
                    </p>
                    <ul class="scope-list" aria-label="Permissions">
                        {missing.into_iter().map(|s| view! { <li class="scope missing" title="Missing">{s}</li> }).collect_view()}
                        {granted.into_iter().map(|s| view! { <li class="scope granted" title="Granted">{s}</li> }).collect_view()}
                    </ul>
                </div>
            }
            .into_any()
        }
    };
    view! {
        {summary}
        <h2 class="section-title">"Organisation"</h2>
        <div class="settings-group">
            <p class="settings-note">
                "Your IT admin registers Quorum once in Microsoft Entra ID (the setup script does it in a minute) and gives you two IDs, or a setup link that fills them in. See docs/setup-tenant.md."
            </p>
            {move || {
                let initial = links.data().and_then(|l| Some((l.tenant.clone()?, l.client_id.clone()?)));
                view! { <TenantForm submit="Save" initial=initial.unwrap_or_default() /> }
            }}
            {move || links.data().and_then(|l| {
                let consent = l.admin_consent_url.clone()?;
                let setup = l.setup_link.clone().unwrap_or_default();
                Some(view! {
                    <div class="setup-links">
                        <div class="setup-link">
                            <span class="setup-link-label">"Admin consent link"</span>
                            <span class="settings-note">"Send it to an admin to approve Quorum for everyone in your organisation."</span>
                            <button class="button" on:click=move |_| crate::areas::files::copy_to_clipboard(state, consent.clone())>"Copy link"</button>
                        </div>
                        <div class="setup-link">
                            <span class="setup-link-label">"Setup link for colleagues"</span>
                            <span class="settings-note">"Opens Quorum with your organisation's IDs filled in."</span>
                            <button class="button" on:click=move |_| crate::areas::files::copy_to_clipboard(state, setup.clone())>"Copy link"</button>
                        </div>
                    </div>
                })
            })}
        </div>
        <h2 class="section-title">"Permissions"</h2>
        {permissions}
    }
}

#[component]
fn KeyboardSection() -> impl IntoView {
    let state = app_state();
    let mac = crate::dom::is_mac();
    view! {
        <p class="settings-note">"Every command is also in the menu bar and the command palette."</p>
        <table class="shortcuts settings-group">
            <thead>
                <tr><th scope="col">"Command"</th><th scope="col">"ID"</th><th scope="col">"Shortcut"</th></tr>
            </thead>
            <tbody>
                {move || {
                    state
                        .all_commands_tracked()
                        .into_iter()
                        .filter_map(|c| c.shortcut.clone().map(|s| (c, s)))
                        .map(|(c, s)| {
                            view! {
                                <tr>
                                    <td>{c.label}</td>
                                    <td class="item-secondary"><code>{c.id}</code></td>
                                    <td><kbd>{crate::keys::display_shortcut(&s, mac)}</kbd></td>
                                </tr>
                            }
                        })
                        .collect_view()
                }}
            </tbody>
        </table>
    }
}

#[component]
fn AgentsHelp() -> impl IntoView {
    let values = use_query::<Map<String, Value>>("settings.get", || json!({}), &["settings", "setting"]);
    let port =
        move || values.data().and_then(|m| m.get("agents.control_port").and_then(Value::as_i64)).unwrap_or(47900);
    view! {
        <h2 class="section-title">"Connect an agent (MCP)"</h2>
        <div class="settings-group">
            <p class="settings-note">
                "With the control channel on, an MCP client can drive the Quorum window you see. Add this server to your agent:"
            </p>
            <pre class="code-block">{move || format!("quorum-cli mcp --attach {}", port())}</pre>
            <p class="settings-note">
                "Without the app running, " <code>"quorum-cli mcp"</code>
                " serves the same commands headless. Agents must pass \"confirm\": true for send, post, delete and share unless you allow it above."
            </p>
        </div>
    }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
struct About {
    #[serde(default)]
    version: String,
    #[serde(default)]
    contributors: Vec<Contributor>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
struct Contributor {
    #[serde(default)]
    name: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    commits: u64,
}

#[component]
fn AboutSection() -> impl IntoView {
    let about = use_query::<About>("app.about", || json!({}), &[]);
    view! {
        <div class="settings-group about">
            <p class="about-name">"Quorum"</p>
            <p class="item-secondary">{move || about.data().map(|a| format!("Version {}", a.version)).unwrap_or_default()}</p>
            <p class="settings-note">
                "Mail, calendar, Teams chat and files for a Microsoft 365 work account, through documented Microsoft Graph APIs. Not affiliated with Microsoft."
            </p>
        </div>
        <h2 class="section-title">"Contributors"</h2>
        <div class="settings-group">
            {move || {
                let list = about.data().map(|a| a.contributors.clone()).unwrap_or_default();
                if list.is_empty() {
                    view! { <p class="settings-note">"Contributors appear here once the first release is cut."</p> }.into_any()
                } else {
                    view! {
                        <ul class="contributors">
                            {list
                                .into_iter()
                                .map(|c| {
                                    let handle = c.username.map(|u| format!("@{u}")).unwrap_or_default();
                                    view! {
                                        <li>
                                            <span class="item-primary">{c.name}</span>
                                            " "
                                            <span class="item-secondary">{handle}</span>
                                            <span class="item-tertiary">{format!("{} commits", c.commits)}</span>
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                    }
                    .into_any()
                }
            }}
        </div>
        <h2 class="section-title">"Icons"</h2>
        <div class="icon-gallery" aria-label="Icon set">
            {IconName::ALL.iter().map(|i| view! { <span class="icon-tile" title=format!("{i:?}")><Icon name=*i /></span> }).collect_view()}
        </div>
    }
}

// --- Browser preview data -------------------------------------------------------------------

thread_local! {
    static MOCK: std::cell::RefCell<Map<String, Value>> = std::cell::RefCell::new(Map::new());
}

/// (key, label, section, kind, default, min/max or choices) for the preview schema.
fn mock_schema() -> Value {
    json!([
        { "key": "app.autostart", "label": "Open Quorum at login", "section": "app", "type": "bool", "default": false },
        { "key": "app.tray", "label": "Show Quorum in the menu bar", "section": "app", "type": "bool", "default": true },
        { "key": "app.dock_badge", "label": "Show the unread count on the Dock icon", "section": "app", "type": "bool", "default": true },
        { "key": "app.default_mail_app", "label": "Use Quorum for email links (mailto:)", "section": "app", "type": "bool", "default": false },
        { "key": "notifications.mail", "label": "New mail", "section": "notifications", "type": "bool", "default": true },
        { "key": "notifications.chat", "label": "New chat messages", "section": "notifications", "type": "bool", "default": true },
        { "key": "notifications.meetings", "label": "Meeting reminders", "section": "notifications", "type": "bool", "default": true },
        { "key": "notifications.reminder_minutes", "label": "Remind me before meetings (minutes)", "section": "notifications", "type": "int", "min": 0, "max": 120, "default": 5 },
        { "key": "notifications.quiet_when_dnd", "label": "Quiet while Do not disturb", "section": "notifications", "type": "bool", "default": true },
        { "key": "mail.undo_send_seconds", "label": "Undo send delay (seconds)", "section": "mail", "type": "int", "min": 0, "max": 30, "default": 5 },
        { "key": "mail.remote_images", "label": "Load remote images by default", "section": "mail", "type": "bool", "default": false },
        { "key": "mail.body_retention_days", "label": "Keep message bodies offline (days)", "section": "mail", "type": "int", "min": 1, "max": 3650, "default": 90 },
        { "key": "presence.keep_alive", "label": "Keep my presence available while Quorum is open", "section": "presence", "type": "bool", "default": true },
        { "key": "presence.idle_minutes", "label": "Show me as away after idle (minutes)", "section": "presence", "type": "int", "min": 1, "max": 120, "default": 5 },
        { "key": "appearance.theme", "label": "Theme", "section": "appearance", "type": "choice", "choices": ["system", "light", "dark"], "default": "system" },
        { "key": "appearance.density", "label": "Density", "section": "appearance", "type": "choice", "choices": ["comfortable", "compact"], "default": "comfortable" },
        { "key": "agents.control_enabled", "label": "Allow agents to control Quorum (local control channel)", "section": "agents", "type": "bool", "default": false },
        { "key": "agents.control_port", "label": "Control channel port", "section": "agents", "type": "int", "min": 1024, "max": 65535, "default": 47900 },
        { "key": "agents.destructive_policy", "label": "Send, post, delete and share by agents", "section": "agents", "type": "choice", "choices": ["confirm", "deny", "allow"], "default": "confirm" }
    ])
}

fn mock_values() -> Map<String, Value> {
    let mut out = Map::new();
    if let Value::Array(defs) = mock_schema() {
        for d in defs {
            if let (Some(k), Some(v)) = (d.get("key").and_then(Value::as_str), d.get("default")) {
                out.insert(k.to_owned(), v.clone());
            }
        }
    }
    MOCK.with(|m| {
        if let Ok(m) = m.try_borrow() {
            for (k, v) in m.iter() {
                out.insert(k.clone(), v.clone());
            }
        }
    });
    out
}

/// Preview answers for this area's queries.
pub fn mock_query(id: &str, params: &Value) -> Reply {
    let ok = |value: Value| Reply::Ok { value };
    match id {
        "settings.schema" => ok(mock_schema()),
        "settings.get" => match params.get("key").and_then(Value::as_str) {
            Some(k) => ok(json!({ "key": k, "value": mock_values().get(k).cloned().unwrap_or(Value::Null) })),
            None => ok(Value::Object(mock_values())),
        },
        "account.permissions" => ok(json!({
            "granted": ["User.Read", "Mail.ReadWrite", "Mail.Send", "Calendars.ReadWrite", "Chat.ReadWrite", "Presence.ReadWrite", "Files.ReadWrite.All"],
            "missing": ["ChannelMessage.Read.All"]
        })),
        "app.about" => ok(json!({
            "name": "Quorum",
            "version": "preview",
            "contributors": [{ "name": "Sample Contributor", "username": "sample", "commits": 12 }]
        })),
        _ => Reply::Err { kind: "unknown_command".into(), message: format!("no query `{id}`") },
    }
}

/// Preview behaviour of this area's commands.
pub fn mock_run(id: &str, params: &Value) -> (Reply, Vec<quorum_ipc::CoreEvent>) {
    let changed = |t: &str| quorum_ipc::CoreEvent::StoreChanged { tables: vec![t.to_owned()] };
    match id {
        "settings.set" => {
            let (Some(k), Some(v)) = (params.get("key").and_then(Value::as_str), params.get("value")) else {
                return (Reply::Err { kind: "invalid_params".into(), message: "missing key or value".into() }, vec![]);
            };
            MOCK.with(|m| {
                if let Ok(mut m) = m.try_borrow_mut() {
                    m.insert(k.to_owned(), v.clone());
                }
            });
            (Reply::Ok { value: json!({ "key": k, "value": v }) }, vec![changed("settings")])
        }
        "link.confirm" => {
            let url = params.get("url").and_then(Value::as_str).unwrap_or_default();
            (
                Reply::Ok {
                    value: json!({ "url": url, "allowed": url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:") }),
                },
                vec![],
            )
        }
        "app.open_url" => (
            Reply::Ok { value: json!({ "opened": false }) },
            vec![quorum_ipc::CoreEvent::Toast {
                level: "info".into(),
                message: "Links open in the system browser in the desktop app.".into(),
            }],
        ),
        "account.configure" | "account.sign_in" | "account.sign_out" => (
            Reply::Err {
                kind: "not_available".into(),
                message: "Sign-in isn't available in the browser preview.".into(),
            },
            vec![],
        ),
        _ => (Reply::Err { kind: "unknown_command".into(), message: format!("no command `{id}`") }, vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_section_has_a_settings_section() {
        if let Value::Array(defs) = mock_schema() {
            for d in defs {
                let s = d.get("section").and_then(Value::as_str).unwrap_or_default();
                assert!(SECTIONS.iter().any(|(id, _, _)| schema_section(id) == Some(s)), "{s}");
            }
        }
    }

    #[test]
    fn mock_settings_round_trip() {
        let (r, ev) = mock_run("settings.set", &json!({ "key": "appearance.theme", "value": "dark" }));
        assert!(matches!(r, Reply::Ok { .. }));
        assert_eq!(ev.len(), 1);
        let v = match mock_query("settings.get", &json!({ "key": "appearance.theme" })) {
            Reply::Ok { value } => value,
            Reply::Err { .. } => Value::Null,
        };
        assert_eq!(v.get("value"), Some(&json!("dark")));
        assert_eq!(choice_label("confirm"), "Ask first");
        assert_eq!(choice_label("dark"), "Dark");
        assert_eq!(choice_label(""), "");
    }
}
