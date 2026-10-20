//! SharePoint lists: a table with typed columns (`lists.items`) and an item editor in the
//! details pane (`lists.update_item`, `lists.create_item`, `lists.delete_item`).

use std::sync::Arc;

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::files::exec;
use crate::components::{EmptyState, Spinner};
use crate::icons::IconName;
use crate::state::{QueryState, ToastLevel, app_state, use_query};

const TABLES: &[&str] = &["list_item", "list", "setting"];

/// A column (`lists.items`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Column {
    /// Internal name.
    pub name: String,
    /// Header.
    #[serde(default)]
    pub display_name: String,
    /// `text`, `number`, `choice`, `dateTime`, `boolean`, `personOrGroup`, `currency`, ...
    #[serde(default)]
    pub kind: String,
    /// Choices.
    #[serde(default)]
    pub choices: Vec<String>,
    /// Not editable.
    #[serde(default)]
    pub read_only: bool,
}

/// An item (`lists.items`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ListRow {
    /// Item id.
    pub id: String,
    /// Column values.
    #[serde(default)]
    pub fields: Map<String, Value>,
    /// Modified label.
    #[serde(default)]
    pub modified_label: String,
    /// Not yet on the server.
    #[serde(default)]
    pub pending: bool,
}

/// `lists.items`.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct ListData {
    /// Columns in view order.
    #[serde(default)]
    pub columns: Vec<Column>,
    /// Items.
    #[serde(default)]
    pub items: Vec<ListRow>,
    /// Why the first fetch failed, if it did.
    #[serde(default)]
    pub error: Option<String>,
}

/// A cell value as text for its column kind (pure).
pub fn cell_text(kind: &str, v: Option<&Value>) -> String {
    let Some(v) = v else { return String::new() };
    match (kind, v) {
        (_, Value::Null) => String::new(),
        ("boolean", Value::Bool(b)) => if *b { "Yes" } else { "No" }.to_owned(),
        ("dateTime", Value::String(s)) => s.get(..10).unwrap_or(s).to_owned(),
        ("currency", Value::Number(n)) => n.as_f64().map(|f| format!("{f:.2}")).unwrap_or_default(),
        (_, Value::String(s)) => s.chars().take(500).collect(),
        (_, Value::Number(n)) => n.to_string(),
        (_, Value::Bool(b)) => if *b { "Yes" } else { "No" }.to_owned(),
        (_, Value::Array(a)) => {
            a.iter().map(|x| cell_text("text", Some(x))).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ")
        }
        (_, Value::Object(o)) => o
            .get("LookupValue")
            .or_else(|| o.get("Label"))
            .or_else(|| o.get("DisplayName"))
            .or_else(|| o.get("Email"))
            .map(|x| cell_text("text", Some(x)))
            .unwrap_or_default(),
    }
}

fn cell(c: &Column, row: &ListRow) -> AnyView {
    let v = row.fields.get(&c.name);
    let text = cell_text(&c.kind, v);
    match c.kind.as_str() {
        "boolean" => {
            let on = v.and_then(Value::as_bool).unwrap_or(false);
            view! { <span class="lists-cell lists-bool" class:on=on aria-label=text>{if on { "✓" } else { "" }}</span> }
                .into_any()
        }
        "choice" if !text.is_empty() => {
            view! { <span class="lists-cell"><span class="lists-pill">{text}</span></span> }.into_any()
        }
        "number" | "currency" => view! { <span class="lists-cell lists-num">{text}</span> }.into_any(),
        _ => view! { <span class="lists-cell">{text}</span> }.into_any(),
    }
}

/// The list table.
#[component]
pub fn ListTable(
    /// Site id.
    site: String,
    /// List id.
    list: String,
    /// Title.
    name: String,
    /// Selected item id.
    selected: RwSignal<Option<String>>,
) -> impl IntoView {
    let state = app_state();
    let (s2, l2) = (site.clone(), list.clone());
    let data = use_query::<ListData>("lists.items", move || json!({ "site": s2, "list": l2 }), TABLES);
    let (s3, l3) = (site.clone(), list.clone());
    let new_item = move |_| {
        exec(
            state,
            "lists.create_item",
            json!({ "site": s3, "list": l3, "fields": { "Title": "New item" } }),
            move |v| {
                if let Some(id) = v.get("id").and_then(Value::as_str) {
                    selected.set(Some(id.to_owned()));
                }
            },
        );
    };
    let (s4, l4) = (site.clone(), list.clone());
    let delete = move |_| {
        let Some(item) = selected.get_untracked() else { return };
        exec(state, "lists.delete_item", json!({ "site": s4, "list": l4, "item": item }), move |_| {
            state.toast(ToastLevel::Success, "Item deleted");
        });
        selected.set(None);
    };
    let on_key = move |e: web_sys::KeyboardEvent| {
        let rows = data.data().map(|d| d.items.clone()).unwrap_or_default();
        let cur = selected.get_untracked().and_then(|s| rows.iter().position(|r| r.id == s));
        let next = match e.key().as_str() {
            "ArrowDown" | "j" => cur.map_or(0, |c| (c + 1).min(rows.len().saturating_sub(1))),
            "ArrowUp" | "k" => cur.map_or(0, |c| c.saturating_sub(1)),
            _ => return,
        };
        e.prevent_default();
        if let Some(r) = rows.get(next) {
            selected.set(Some(r.id.clone()));
        }
    };
    view! {
        <div class="list-pane lists-pane">
            <div class="command-bar" role="toolbar" aria-label="List commands">
                <button class="command-button primary" data-command="lists.create_item" on:click=new_item>
                    <crate::components::Icon name=IconName::Plus />
                    <span class="command-label">"New item"</span>
                </button>
                <button class="command-button" data-command="lists.delete_item" disabled=move || selected.get().is_none() on:click=delete>
                    <crate::components::Icon name=IconName::Trash />
                    <span class="command-label">"Delete"</span>
                </button>
            </div>
            <header class="list-header"><h2 class="list-title">{name}</h2></header>
            <div class="list-body">
                {move || match data.state.get() {
                    QueryState::Loading => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
                    QueryState::Failed { message, .. } => view! { <EmptyState title="Couldn't load this list" detail=message error=true /> }.into_any(),
                    QueryState::Ready(d) if d.items.is_empty() => {
                        let detail = d.error.clone().unwrap_or_default();
                        view! { <EmptyState title="This list is empty" detail=detail icon=IconName::Inbox /> }.into_any()
                    }
                    QueryState::Ready(d) => {
                        let d: Arc<ListData> = d;
                        let cols = d.columns.clone();
                        let n = cols.len().max(1);
                        let template = format!("grid-template-columns: repeat({n}, minmax(120px, 1fr)); min-width: {}px", n.saturating_mul(132));
                        view! {
                            <div class="lists-table" role="grid" aria-label="List items" tabindex="0" data-primary-list="" on:keydown=on_key>
                                <div class="lists-row lists-head" role="row" style=template.clone()>
                                    {cols.iter().map(|c| view! { <span class="lists-cell" role="columnheader">{c.display_name.clone()}</span> }).collect_view()}
                                </div>
                                {d.items.iter().map(|r| {
                                    let id = r.id.clone();
                                    let id2 = id.clone();
                                    let cells = cols.iter().map(|c| cell(c, r)).collect_view();
                                    view! {
                                        <div class="lists-row" role="row" style=template.clone() class:pending=r.pending
                                            class:selected=move || selected.with(|s| s.as_deref() == Some(id2.as_str()))
                                            on:mousedown=move |_| selected.set(Some(id.clone()))>
                                            {cells}
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        }
                        .into_any()
                    }
                }}
            </div>
        </div>
    }
}

/// Editable fields of the selected list item.
#[component]
pub fn ListDetails(
    /// Site id.
    site: String,
    /// List id.
    list: String,
    /// Item id.
    item: String,
) -> impl IntoView {
    let state = app_state();
    let (s2, l2) = (site.clone(), list.clone());
    let data = use_query::<ListData>("lists.items", move || json!({ "site": s2, "list": l2 }), TABLES);
    let edits = RwSignal::new(Map::new());
    let key = item.clone();
    move || {
        let d = data.data()?;
        let row = d.items.iter().find(|r| r.id == key)?.clone();
        let cols = d.columns.clone();
        let title = cell_text("text", row.fields.get("Title"));
        let (site, list, item) = (site.clone(), list.clone(), item.clone());
        let save = move |_| {
            let fields = edits.get_untracked();
            if fields.is_empty() {
                return;
            }
            exec(
                state,
                "lists.update_item",
                json!({ "site": site, "list": list, "item": item, "fields": fields }),
                move |_| {
                    state.toast(ToastLevel::Success, "Saved");
                },
            );
            edits.set(Map::new());
        };
        Some(view! {
            <article class="reading files-details">
                <h1 class="reading-title">{if title.is_empty() { "Item".to_owned() } else { title }}</h1>
                <div class="lists-form">
                    {cols.into_iter().map(|c| {
                        let current = cell_text(&c.kind, row.fields.get(&c.name));
                        let name = c.name.clone();
                        let editable = !c.read_only && matches!(c.kind.as_str(), "text" | "number" | "choice" | "boolean" | "dateTime" | "unknown");
                        let input = match c.kind.as_str() {
                            _ if !editable => view! { <span class="lists-readonly">{current}</span> }.into_any(),
                            "boolean" => {
                                let on = row.fields.get(&c.name).and_then(Value::as_bool).unwrap_or(false);
                                view! { <input type="checkbox" prop:checked=on on:change=move |e| {
                                    let v = event_target_checked(&e);
                                    edits.update(|m| { m.insert(name.clone(), Value::Bool(v)); });
                                } /> }.into_any()
                            }
                            "choice" => view! {
                                <select class="files-input" on:change=move |e| {
                                    let v = event_target_value(&e);
                                    edits.update(|m| { m.insert(name.clone(), Value::String(v)); });
                                }>
                                    {c.choices.iter().map(|ch| view! { <option value=ch.clone() selected=*ch == current>{ch.clone()}</option> }).collect_view()}
                                </select>
                            }.into_any(),
                            kind => {
                                let number = kind == "number";
                                view! { <input class="files-input" type=if number { "number" } else { "text" } prop:value=current on:input=move |e| {
                                    let raw = event_target_value(&e);
                                    let v = if number { raw.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number).unwrap_or(Value::Null) } else { Value::String(raw) };
                                    edits.update(|m| { m.insert(name.clone(), v); });
                                } /> }.into_any()
                            }
                        };
                        view! {
                            <label class="files-field">
                                <span>{c.display_name.clone()}</span>
                                {input}
                            </label>
                        }
                    }).collect_view()}
                </div>
                <div class="files-dialog-actions">
                    <button class="files-button primary" disabled=move || edits.with(Map::is_empty) on:click=save>"Save"</button>
                </div>
                <p class="placeholder-note">{format!("Modified {}", row.modified_label)}</p>
            </article>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::cell_text;
    use serde_json::json;

    #[test]
    fn cells_render_by_kind() {
        assert_eq!(cell_text("boolean", Some(&json!(true))), "Yes");
        assert_eq!(cell_text("dateTime", Some(&json!("2026-10-24T00:00:00Z"))), "2026-10-24");
        assert_eq!(cell_text("number", Some(&json!(3))), "3");
        assert_eq!(cell_text("currency", Some(&json!(2.5))), "2.50");
        assert_eq!(cell_text("text", Some(&json!(["a", "b"]))), "a, b");
        assert_eq!(cell_text("lookup", Some(&json!({ "LookupValue": "X" }))), "X");
        assert_eq!(cell_text("text", None), "");
        assert_eq!(cell_text("dateTime", Some(&json!("é"))), "é");
    }
}
