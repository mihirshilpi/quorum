//! Shared area scaffolding: the Office-style three-column layout (navigation column, list
//! pane with command bar, reading pane) and a query-backed list.

use std::sync::Arc;

use leptos::prelude::*;
use serde::de::DeserializeOwned;

use crate::components::{CommandBar, CommandButton, EmptyState, Spinner, SplitPane, VirtualList};
use crate::icons::IconName;
use crate::state::{Query, QueryState, app_state};

/// A row of a list query.
pub trait Row: Clone + Send + Sync + DeserializeOwned + 'static {
    /// Stable id (what selections hold).
    fn key(&self) -> String;
}

/// The area layout: optional navigation column | list pane | reading pane, each a focusable
/// `[data-pane]` region (F6 cycles them). The nav column follows `view.sidebar.toggle`.
#[component]
pub fn AreaLayout(
    /// Area id, for persisted split widths.
    id: &'static str,
    /// Navigation column (folders, calendars, locations).
    #[prop(optional, into)]
    nav: Option<ViewFn>,
    /// Accessible name of the navigation column.
    #[prop(optional)]
    nav_label: Option<&'static str>,
    /// List pane.
    #[prop(into)]
    list: ViewFn,
    /// Accessible name of the list pane.
    list_label: &'static str,
    /// Reading / detail pane.
    #[prop(into)]
    detail: ViewFn,
    /// Accessible name of the detail pane.
    detail_label: &'static str,
) -> impl IntoView {
    let state = app_state();
    let inner = move || {
        let (list, detail) = (list.clone(), detail.clone());
        view! {
            <SplitPane
                id=format!("{id}.list")
                initial=400.0
                min=260.0
                max=760.0
                label="Resize list"
                class="split-list"
                start=move || {
                    view! { <section class="pane pane-list" data-pane="" tabindex="-1" aria-label=list_label>{list.run()}</section> }
                }
                end=move || {
                    view! {
                        <section class="pane pane-detail" data-pane="" tabindex="-1" aria-label=detail_label>{detail.run()}</section>
                    }
                }
            />
        }
    };
    move || match (&nav, state.nav_pane_visible.get()) {
        (Some(nav), true) => {
            let nav = nav.clone();
            let inner = inner.clone();
            view! {
                <SplitPane
                    id=format!("{id}.nav")
                    initial=220.0
                    min=160.0
                    max=380.0
                    label="Resize navigation"
                    class="split-nav"
                    start=move || {
                        view! {
                            <nav class="pane pane-nav" data-pane="" tabindex="-1" aria-label=nav_label.unwrap_or("Folders")>
                                {nav.run()}
                            </nav>
                        }
                    }
                    end=inner.clone()
                />
            }
            .into_any()
        }
        _ => inner().into_any(),
    }
}

/// The list pane chrome: a header line and the command bar above the list.
#[component]
pub fn ListPane(
    /// Title (folder name etc.).
    #[prop(into)]
    title: Signal<String>,
    /// Command bar buttons.
    buttons: &'static [CommandButton],
    /// The list.
    children: Children,
) -> impl IntoView {
    view! {
        <div class="list-pane">
            <CommandBar buttons=buttons label="Commands" />
            <header class="list-header">
                <h2 class="list-title">{move || title.get()}</h2>
            </header>
            <div class="list-body">{children()}</div>
        </div>
    }
}

/// A [`VirtualList`] over a query, with loading, error and empty states.
#[component]
pub fn QueryList<T: Row>(
    /// The query.
    query: Query<Vec<T>>,
    /// Rows to show instead of the raw query data (e.g. filtered by the nav selection).
    #[prop(optional, into)]
    items: Option<Signal<Arc<Vec<T>>>>,
    /// Selected row id.
    selected: RwSignal<Option<String>>,
    /// Accessible name.
    label: &'static str,
    /// Row renderer.
    render: fn(T) -> AnyView,
    /// Text when there are no rows.
    empty: &'static str,
    /// Empty-state icon.
    #[prop(optional)]
    empty_icon: Option<IconName>,
    /// Target of `j`/`k` and area focus.
    #[prop(optional)]
    primary: bool,
    /// CSS class for density variants (e.g. `nav-list`).
    #[prop(optional)]
    class: Option<&'static str>,
) -> impl IntoView {
    let items = items.unwrap_or_else(|| Signal::derive(move || query.data().unwrap_or_default()));
    let status = Memo::new(move |_| {
        query.state.with(|s| match s {
            QueryState::Loading => 0u8,
            QueryState::Failed { .. } => 1,
            QueryState::Ready(_) => 2,
        })
    });
    // Memos, so a data refresh re-renders rows, not the whole list (keeps scroll and selection).
    let is_empty = Memo::new(move |_| items.with(|v| v.is_empty()));
    let render = Callback::new(render);
    let key: fn(&T) -> String = <T as Row>::key;
    move || match status.get() {
        0 => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
        1 => {
            let msg = query.state.with(|s| match s {
                QueryState::Failed { message, .. } => message.clone(),
                _ => String::new(),
            });
            view! { <EmptyState title="Couldn't load this list" detail=msg error=true /> }.into_any()
        }
        _ if is_empty.get() => match empty_icon {
            Some(icon) => view! { <EmptyState title=empty icon=icon /> }.into_any(),
            None => view! { <EmptyState title=empty /> }.into_any(),
        },
        _ => view! {
            <div class=format!("list-wrap {}", class.unwrap_or_default())>
                <VirtualList items=items key=key render=render selected=selected label=label primary=primary />
            </div>
        }
        .into_any(),
    }
}

/// Rows of `query` that pass `keep`, recomputed when the data or the filter's inputs change.
pub fn filtered<T: Row>(
    query: Query<Vec<T>>,
    keep: impl Fn(&T) -> bool + Send + Sync + 'static,
) -> Signal<Arc<Vec<T>>> {
    let memo = Memo::new_with_compare(
        move |_| {
            let data = query.data().unwrap_or_default();
            Arc::new(data.iter().filter(|t| keep(t)).cloned().collect::<Vec<T>>())
        },
        |_, _| true,
    );
    memo.into()
}

/// The selected row of `query`, looked up by id.
pub fn selected_row<T: Row>(query: Query<Vec<T>>, selected: RwSignal<Option<String>>) -> Signal<Option<T>> {
    Signal::derive(move || {
        let id = selected.get()?;
        query.state.with(|s| s.data().and_then(|d| d.iter().find(|t| t.key() == id).cloned()))
    })
}

/// "Nothing selected" placeholder for reading panes.
pub fn nothing_selected(title: &'static str, icon: IconName) -> AnyView {
    view! { <EmptyState title=title icon=icon /> }.into_any()
}
