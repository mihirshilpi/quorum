//! People: relevant people (`people.list`) | person profile with org chart (`people.person`):
//! manager chain above, direct reports below, recent mail and chats with them.

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use super::common::{AreaLayout, ListPane, QueryList, Row, nothing_selected};
use crate::components::person_card::{PersonAvatar, hub};
use crate::components::{CommandButton, EmptyState, Icon, Presence, Spinner};
use crate::icons::IconName;
use crate::state::{Area, QueryState, app_state, use_query};

/// A person row (`people.list`, `people.search`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PersonRow {
    /// User id, or `mail:<address>`.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Address.
    #[serde(default)]
    pub email: Option<String>,
    /// Job title.
    #[serde(default)]
    pub job_title: Option<String>,
    /// Department.
    #[serde(default)]
    pub department: Option<String>,
    /// Short presence id.
    #[serde(default)]
    pub presence: Option<String>,
}

impl PersonRow {
    /// The name to show.
    pub fn name(&self) -> String {
        self.display_name.clone().filter(|n| !n.is_empty()).or_else(|| self.email.clone()).unwrap_or_default()
    }

    /// Title and department joined.
    pub fn role(&self) -> String {
        [self.job_title.clone(), self.department.clone()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

impl Row for PersonRow {
    fn key(&self) -> String {
        self.id.clone()
    }
}

const BUTTONS: &[CommandButton] = &[
    CommandButton::primary("people.chat", "Chat", IconName::Chat),
    CommandButton::new("people.email", "Email", IconName::Mail),
    CommandButton::new("people.schedule", "Meet", IconName::Calendar),
    CommandButton::new("people.open", "Card", IconName::People),
];

fn person_row(p: PersonRow) -> AnyView {
    let name = p.name();
    let role = p.role();
    view! {
        <div class="item-row person-row">
            <PersonAvatar user=p.id.clone() name=name.clone() size="sm" presence=p.presence.clone().unwrap_or_default() />
            <div class="item-text">
                <span class="item-primary">{name}</span>
                <span class="item-secondary">{role}</span>
            </div>
        </div>
    }
    .into_any()
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// One node in the org chart.
fn org_node(p: &Value, current: bool) -> AnyView {
    let state = app_state();
    let id = text(p, "id");
    let name = text(p, "display_name");
    let title = text(p, "job_title");
    let select = id.clone();
    view! {
        <button
            class="org-node"
            class:current=current
            aria-current=current.then_some("true")
            on:click=move |_| state.selection(Area::People).item.set(Some(select.clone()))
        >
            <PersonAvatar user=id name=name.clone() size="sm" presence=text(p, "presence") no_card=true />
            <span class="org-node-text">
                <span class="org-node-name">{name}</span>
                <span class="org-node-title">{title}</span>
            </span>
        </button>
    }
    .into_any()
}

fn open_item(item: &Value) {
    let state = app_state();
    let Some(open) = item.get("open") else { return };
    let cmd = text(open, "command");
    if cmd.is_empty() {
        return;
    }
    crate::commands::dispatch(state, &cmd, open.get("params").cloned().unwrap_or(Value::Null));
}

fn activity_list(title: &'static str, items: Vec<Value>, icon: IconName, primary: &'static str) -> AnyView {
    if items.is_empty() {
        return ().into_any();
    }
    view! {
        <section class="profile-section">
            <h3 class="section-title">{title}</h3>
            <ul class="activity-list">
                {items
                    .into_iter()
                    .map(|it| {
                        let label = it
                            .get(primary)
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("(untitled)")
                            .to_owned();
                        let detail = it
                            .pointer("/snippet/text")
                            .or_else(|| it.get("preview"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let target = it.clone();
                        view! {
                            <li>
                                <button class="activity-item" on:click=move |_| open_item(&target)>
                                    <Icon name=icon />
                                    <span class="activity-text">
                                        <span class="activity-title">{label}</span>
                                        <span class="activity-detail">{detail}</span>
                                    </span>
                                </button>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        </section>
    }
    .into_any()
}

fn profile(d: &Value) -> AnyView {
    let p = d.get("person").cloned().unwrap_or(Value::Null);
    let id = text(&p, "id");
    let name = text(&p, "display_name");
    let presence = Presence::parse(&text(&p, "presence"));
    let status = text(&p, "status_message");
    let role = [text(&p, "job_title"), text(&p, "department")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let facts: Vec<(&'static str, String)> =
        [("Email", text(&p, "email")), ("Office", text(&p, "office_location")), ("Phone", text(&p, "phone"))]
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .collect();
    let mut managers: Vec<Value> = d.get("managers").and_then(Value::as_array).cloned().unwrap_or_default();
    managers.reverse();
    let reports: Vec<Value> = d.get("reports").and_then(Value::as_array).cloned().unwrap_or_default();
    let recent_mail = d.get("recent_mail").and_then(Value::as_array).cloned().unwrap_or_default();
    let recent_chats = d.get("recent_chats").and_then(Value::as_array).cloned().unwrap_or_default();
    let state = app_state();
    let action =
        move |cmd: &'static str, user: String| move |_| crate::commands::dispatch(state, cmd, json!({ "user": user }));
    let live_id = id.clone();
    let live = move || hub().presence_of(&live_id).map_or(presence, |p| p.presence).label();
    let has_org = !managers.is_empty() || !reports.is_empty();
    view! {
        <article class="reading profile">
            <header class="profile-head">
                <PersonAvatar user=id.clone() name=name.clone() size="lg" presence=text(&p, "presence") no_card=true />
                <div class="profile-id">
                    <h1 class="reading-title">{name.clone()}</h1>
                    <p class="profile-status">{live}</p>
                    {(!status.is_empty()).then(|| view! { <p class="profile-message">{status}</p> })}
                    {(!role.is_empty()).then(|| view! { <p class="item-secondary">{role}</p> })}
                </div>
            </header>
            <div class="profile-actions">
                <button class="command-button primary" on:click=action("people.chat", id.clone())>
                    <Icon name=IconName::Chat /><span>"Chat"</span>
                </button>
                <button class="command-button" on:click=action("people.email", id.clone())>
                    <Icon name=IconName::Mail /><span>"Email"</span>
                </button>
                <button class="command-button" on:click=action("people.schedule", id.clone())>
                    <Icon name=IconName::Calendar /><span>"Schedule"</span>
                </button>
            </div>
            {(!facts.is_empty()).then(|| view! {
                <dl class="facts">
                    {facts.into_iter().map(|(k, v)| view! { <dt>{k}</dt><dd class="selectable">{v}</dd> }).collect_view()}
                </dl>
            })}
            {has_org.then(|| view! {
                <section class="profile-section">
                    <h3 class="section-title">"Organization"</h3>
                    <div class="org-chart" role="tree" aria-label="Organization">
                        {managers.iter().map(|m| view! { <div class="org-level">{org_node(m, false)}</div> }).collect_view()}
                        <div class="org-level">{org_node(&p, true)}</div>
                        {(!reports.is_empty()).then(|| view! {
                            <div class="org-level"><div class="org-reports">{reports.iter().map(|r| org_node(r, false)).collect_view()}</div></div>
                        })}
                    </div>
                </section>
            })}
            {activity_list("Recent mail", recent_mail, IconName::Mail, "title")}
            {activity_list("Recent chats", recent_chats, IconName::Chat, "topic")}
        </article>
    }
    .into_any()
}

/// The People area.
#[component]
pub fn PeopleArea() -> impl IntoView {
    let state = app_state();
    let sel = state.selection(Area::People);
    let people = use_query::<Vec<PersonRow>>("people.list", || Value::Null, &["person", "setting"]);
    Effect::new(move |_| {
        let id = sel.item.get();
        super::people_ui::report("people", id.into_iter().collect(), "list");
    });
    view! {
        <AreaLayout
            id="people"
            list_label="People"
            list=move || {
                view! {
                    <ListPane title=Signal::stored("People".to_owned()) buttons=BUTTONS>
                        <QueryList
                            query=people
                            selected=sel.item
                            label="People"
                            render=person_row
                            empty="No people yet"
                            empty_icon=IconName::People
                            primary=true
                        />
                    </ListPane>
                }
            }
            detail_label="Person"
            detail=move || {
                move || match sel.item.get() {
                    None => nothing_selected("Select a person", IconName::People),
                    Some(id) => view! { <PersonDetail user=id /> }.into_any(),
                }
            }
        />
    }
}

#[component]
fn PersonDetail(user: String) -> impl IntoView {
    let details =
        use_query::<Value>("people.person", move || json!({ "user": user }), &["person", "presence", "setting"]);
    move || match details.state.get() {
        QueryState::Loading => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
        QueryState::Failed { message, .. } => {
            view! { <EmptyState title="Couldn't load this person" detail=message error=true /> }.into_any()
        }
        QueryState::Ready(d) => profile(&d),
    }
}
