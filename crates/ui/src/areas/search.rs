//! Universal search results (`search.all`): hits grouped by Mail, Chats, Channels, Files,
//! People and Events, with highlighted snippets. ↑/↓ (or `j`/`k`) move, Enter opens the hit
//! through its own `open` command, Esc returns to the search box.

use std::time::Duration;

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::components::{EmptyState, Icon, Spinner};
use crate::icons::IconName;
use crate::state::{Area, ListNav, QueryState, app_state, use_query};

/// Debounce between typing and searching.
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);

/// A snippet with byte highlight ranges.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Snippet {
    /// Plain text.
    #[serde(default)]
    pub text: String,
    /// `[start, end)` byte ranges.
    #[serde(default)]
    pub highlights: Vec<(u32, u32)>,
}

/// The command that opens a hit.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct OpenCommand {
    /// Command id.
    #[serde(default)]
    pub command: String,
    /// Its params.
    #[serde(default)]
    pub params: Value,
}

/// One hit.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Hit {
    /// Unique key.
    pub key: String,
    /// Group id.
    #[serde(default)]
    pub scope: String,
    /// Title.
    #[serde(default)]
    pub title: Option<String>,
    /// Second line.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Excerpt.
    #[serde(default)]
    pub snippet: Snippet,
    /// RFC 3339 instant.
    #[serde(default)]
    pub when: Option<String>,
    /// How to open it.
    #[serde(default)]
    pub open: OpenCommand,
}

/// A group of hits.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Group {
    /// Group id (`mail`, `chats`, ...).
    pub scope: String,
    /// Heading.
    #[serde(default)]
    pub label: String,
    /// Hits.
    #[serde(default)]
    pub hits: Vec<Hit>,
}

/// The `search.all` reply.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Results {
    /// The query as searched.
    #[serde(default)]
    pub query: String,
    /// Whether Microsoft Search answered.
    #[serde(default)]
    pub remote: bool,
    /// Groups, in display order.
    #[serde(default)]
    pub groups: Vec<Group>,
}

/// Splits `text` into (run, highlighted) parts by byte ranges; bad ranges are ignored (pure).
pub fn highlight_runs(text: &str, ranges: &[(u32, u32)]) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut sorted: Vec<(usize, usize)> = ranges.iter().map(|(a, b)| (*a as usize, *b as usize)).collect();
    sorted.sort_unstable();
    for (a, b) in sorted {
        if a < pos || b <= a {
            continue;
        }
        let (Some(before), Some(mid)) = (text.get(pos..a), text.get(a..b)) else { continue };
        if !before.is_empty() {
            out.push((before.to_owned(), false));
        }
        out.push((mid.to_owned(), true));
        pos = b;
    }
    if let Some(rest) = text.get(pos..)
        && !rest.is_empty()
    {
        out.push((rest.to_owned(), false));
    }
    out
}

/// A short date label from an RFC 3339 instant (`2026-10-17T…` → `2026-10-17`).
pub fn when_label(when: Option<&str>) -> String {
    when.and_then(|w| w.get(..10)).unwrap_or_default().to_owned()
}

fn icon_for(scope: &str) -> IconName {
    match scope {
        "mail" => IconName::Mail,
        "chats" => IconName::Chat,
        "channels" => IconName::Teams,
        "files" => IconName::File,
        "people" => IconName::People,
        "events" => IconName::Calendar,
        _ => IconName::Search,
    }
}

/// Runs a hit's open command.
pub fn open_hit(state: crate::state::AppState, hit: &Hit) {
    if hit.open.command.is_empty() {
        return;
    }
    crate::commands::dispatch(state, &hit.open.command, hit.open.params.clone());
}

/// The Search area.
#[component]
pub fn SearchArea() -> impl IntoView {
    let state = app_state();
    let debounced = RwSignal::new(state.search_query.get_untracked().trim().to_owned());
    let generation = StoredValue::new(0u64);
    Effect::new(move |_| {
        let q = state.search_query.get().trim().to_owned();
        let g = generation.try_get_value().unwrap_or(0).wrapping_add(1);
        generation.set_value(g);
        set_timeout(
            move || {
                if generation.try_get_value() == Some(g) {
                    debounced.try_set(q);
                }
            },
            SEARCH_DEBOUNCE,
        );
    });
    let results = use_query::<Results>(
        "search.all",
        move || json!({ "query": debounced.get() }),
        &["message", "chat_message", "channel_message", "drive_item", "person", "event"],
    );
    let flat = Memo::new(move |_| {
        results
            .data()
            .map(|r| r.groups.iter().flat_map(|g| g.hits.iter().cloned()).collect::<Vec<Hit>>())
            .unwrap_or_default()
    });
    let active = RwSignal::new(None::<usize>);
    Effect::new(move |_| {
        let _ = debounced.get();
        active.set(None);
    });
    let step = move |delta: isize| {
        let len = flat.with_untracked(Vec::len);
        if len == 0 {
            return;
        }
        let next = match active.get_untracked() {
            None if delta < 0 => len - 1,
            None => 0,
            Some(i) => crate::commands::next_pane(Some(i), len, delta),
        };
        active.set(Some(next));
        request_animation_frame(move || {
            if let Some(el) = crate::dom::document().and_then(|d| d.get_element_by_id(&format!("search-hit-{next}"))) {
                let opts = web_sys::ScrollIntoViewOptions::new();
                opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
                el.scroll_into_view_with_scroll_into_view_options(&opts);
            }
        });
    };
    let open_active = move || {
        if let Some(i) = active.get_untracked()
            && let Some(h) = flat.with_untracked(|f| f.get(i).cloned())
        {
            open_hit(state, &h);
        }
    };
    // j/k from the global keymap.
    Effect::new(move |prev: Option<u64>| {
        let (n, nav) = state.list_nav.get();
        if prev.is_some_and(|p| p != n) && state.area.get_untracked() == Area::Search {
            match nav {
                Some(ListNav::Next) => step(1),
                Some(ListNav::Previous) => step(-1),
                None => {}
            }
        }
        n
    });
    // Arrow keys and Enter from the search box.
    let keys = window_event_listener(leptos::ev::keydown, move |e| {
        let in_box = crate::dom::active_element().is_some_and(|a| a.id() == "global-search");
        if !in_box || state.area.get_untracked() != Area::Search {
            return;
        }
        match e.key().as_str() {
            "ArrowDown" => {
                e.prevent_default();
                step(1);
            }
            "ArrowUp" => {
                e.prevent_default();
                step(-1);
            }
            "Enter" if active.get_untracked().is_some() => open_active(),
            _ => {}
        }
    });
    on_cleanup(move || keys.remove());
    Effect::new(move |_| {
        let sel = active.get().and_then(|i| flat.with(|f| f.get(i).map(|h| h.key.clone())));
        super::people_ui::report("search", sel.into_iter().collect(), "results");
    });
    let on_list_key = move |e: web_sys::KeyboardEvent| match e.key().as_str() {
        "ArrowDown" => {
            e.prevent_default();
            step(1);
        }
        "ArrowUp" => {
            e.prevent_default();
            step(-1);
        }
        "Enter" => {
            e.prevent_default();
            open_active();
        }
        _ => {}
    };

    let body = move || {
        let q = debounced.get();
        if q.is_empty() {
            return view! {
                <EmptyState
                    title="Search mail, chats, channels, files, people and events"
                    detail="Type in the search box above, then use ↑ ↓ and Enter."
                    icon=IconName::Search
                />
            }
            .into_any();
        }
        match results.state.get() {
            QueryState::Loading => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
            QueryState::Failed { message, .. } => {
                view! { <EmptyState title="Search failed" detail=message error=true /> }.into_any()
            }
            QueryState::Ready(r) if r.groups.is_empty() => view! {
                <EmptyState title=format!("No results for “{q}”") detail="Try fewer or different words." icon=IconName::Search />
            }
            .into_any(),
            QueryState::Ready(r) => {
                let mut index = 0usize;
                let groups = r
                    .groups
                    .iter()
                    .map(|g| {
                        let rows = g
                            .hits
                            .iter()
                            .map(|h| {
                                let i = index;
                                index = index.saturating_add(1);
                                hit_row(h.clone(), i, active)
                            })
                            .collect_view();
                        view! {
                            <section class="search-group" aria-label=g.label.clone()>
                                <h2 class="search-group-title">
                                    <Icon name=icon_for(&g.scope) />
                                    <span>{g.label.clone()}</span>
                                    <span class="search-group-count">{g.hits.len()}</span>
                                </h2>
                                <div class="search-hits">{rows}</div>
                            </section>
                        }
                    })
                    .collect_view();
                view! {
                    {(!r.remote).then(|| view! { <p class="search-note">"Showing results on this Mac."</p> })}
                    {groups}
                }
                .into_any()
            }
        }
    };
    view! {
        <section
            class="pane pane-full search-results"
            data-pane=""
            tabindex="-1"
            aria-label="Search results"
            role="listbox"
            aria-activedescendant=move || active.get().map(|i| format!("search-hit-{i}"))
            on:keydown=on_list_key
        >
            <div class="search-inner" data-pane-focus="" data-primary-list="" tabindex="0">{body}</div>
        </section>
    }
}

fn hit_row(h: Hit, i: usize, active: RwSignal<Option<usize>>) -> impl IntoView {
    let runs = highlight_runs(&h.snippet.text, &h.snippet.highlights);
    let title = h.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "(untitled)".into());
    let when = when_label(h.when.as_deref());
    let target = h.clone();
    let state = app_state();
    view! {
        <div
            class="search-hit"
            id=format!("search-hit-{i}")
            role="option"
            aria-selected=move || if active.get() == Some(i) { "true" } else { "false" }
            class:active=move || active.get() == Some(i)
            on:click=move |_| {
                active.set(Some(i));
                open_hit(state, &target);
            }
        >
            <span class="search-hit-icon"><Icon name=icon_for(&h.scope) /></span>
            <div class="search-hit-text">
                <div class="item-line">
                    <span class="item-primary">{title}</span>
                    <span class="item-meta">{when}</span>
                </div>
                {h.subtitle.clone().filter(|s| !s.is_empty()).map(|s| view! { <span class="item-secondary">{s}</span> })}
                <span class="search-snippet">
                    {runs
                        .into_iter()
                        .map(|(t, hl)| if hl { view! { <mark>{t}</mark> }.into_any() } else { t.into_any() })
                        .collect_view()}
                </span>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_split_on_ranges_and_ignore_bad_ones() {
        let runs = highlight_runs("Hello Synthetic world", &[(6, 15)]);
        assert_eq!(runs, vec![("Hello ".into(), false), ("Synthetic".into(), true), (" world".into(), false)]);
        assert_eq!(highlight_runs("é", &[(1, 2)]), vec![("é".to_owned(), false)]);
        assert_eq!(highlight_runs("abc", &[(2, 1), (0, 99), (1, 2), (1, 3)]).len(), 3);
        assert!(highlight_runs("", &[(0, 0)]).is_empty());
    }

    #[test]
    fn results_parse_leniently() {
        let r: Results =
            serde_json::from_value(json!({ "groups": [{ "scope": "mail", "hits": [{ "key": "mail:1" }] }] }))
                .unwrap_or_default();
        assert_eq!(r.groups.first().map(|g| g.hits.len()), Some(1));
        assert_eq!(when_label(Some("2026-10-17T09:00:00Z")), "2026-10-17");
        assert_eq!(when_label(Some("x")), "");
    }
}
