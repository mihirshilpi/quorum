//! UI-side handlers for the people and search areas: the UI-only commands the engine forwards
//! (`people.card.show`, `people.card.hide`, `search.show`) and debounced `ui.report`s.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::state::{AppState, Area};

/// UI-only command ids handled here.
pub const UI_IDS: &[&str] = &["people.card.show", "people.card.hide", "search.show"];

/// Handles a UI-only people/search command; returns `false` for other ids.
pub fn run_ui(state: AppState, id: &str, params: &Value) -> bool {
    match id {
        "people.card.show" => {
            let user = params.get("user").or_else(|| params.get("id")).and_then(Value::as_str).unwrap_or_default();
            crate::components::person_card::open_card(user, None);
        }
        "people.card.hide" => crate::components::person_card::close_card(),
        "search.show" => {
            if let Some(q) = params.get("query").and_then(Value::as_str) {
                let q: String = q.chars().filter(|c| !c.is_control()).take(256).collect();
                state.search_query.set(q);
            }
            state.go(Area::Search);
            request_animation_frame(|| {
                let _ = crate::dom::focus_selector("#global-search");
            });
        }
        _ => return false,
    }
    true
}

thread_local! {
    static REPORT_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Reports the area's selection and view to the engine (`ui.report`), debounced.
pub fn report(area: &'static str, selection: Vec<String>, view: &'static str) {
    let generation = REPORT_GEN.with(|g| {
        let n = g.get().wrapping_add(1);
        g.set(n);
        n
    });
    set_timeout(
        move || {
            if REPORT_GEN.with(std::cell::Cell::get) != generation {
                return;
            }
            spawn_local(async move {
                let _ =
                    crate::ipc::run("ui.report", json!({ "area": area, "selection": selection, "view": view })).await;
            });
        },
        Duration::from_millis(300),
    );
}
