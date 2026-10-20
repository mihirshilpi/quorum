//! UI-only commands and the one dispatcher every UI action goes through.
//!
//! The command table itself is [`quorum_ipc::UI_COMMANDS`] (shared with the engine, which can
//! forward each id as `CoreEvent::Ui` so MCP and the control channel drive the UI too). This
//! module implements them. Buttons, menus, shortcuts and the palette all call [`dispatch`]:
//! UI ids are handled here; everything else goes to the engine with `ipc::run`, and failures
//! become toasts.

use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::{CommandInfo, Reply, UI_COMMANDS};
use serde_json::{Value, json};

use crate::state::{AppState, Area, ListNav, ToastLevel};

/// Whether `id` is a UI-only command.
pub fn is_ui(id: &str) -> bool {
    UI_COMMANDS.iter().any(|c| c.id == id)
}

/// UI commands first, then engine commands whose ids the UI does not already own.
pub fn merged(engine: &[CommandInfo]) -> Vec<CommandInfo> {
    let mut out: Vec<CommandInfo> = UI_COMMANDS.iter().map(|c| c.info()).collect();
    out.extend(engine.iter().filter(|c| !is_ui(&c.id)).cloned());
    out
}

/// Params a command gets when invoked from the UI without explicit params: the current
/// selection of the command's own area as `{"id": ...}`; otherwise `null` (defaults).
///
/// Feature agents with richer context (several selected items, a folder) extend this.
pub fn context_params(state: AppState, id: &str) -> Value {
    let area = state.area.get_untracked();
    if !area.command_prefixes().iter().any(|p| id.starts_with(p)) {
        return Value::Null;
    }
    match state.selection(area).item.get_untracked() {
        Some(sel) => json!({ "id": sel }),
        None => Value::Null,
    }
}

/// Runs command `id`: UI commands locally, the rest in the engine. Never panics; errors toast.
pub fn dispatch(state: AppState, id: &str, params: Value) {
    if run_ui(state, id, &params) {
        return;
    }
    if let Some(cmd) = state.engine_commands.with_untracked(|c| c.iter().find(|c| c.id == id).cloned())
        && !cmd.enabled
    {
        state.toast(ToastLevel::Warning, format!("{} isn't available right now.", cmd.label));
        return;
    }
    let id = id.to_owned();
    spawn_local(async move {
        if let Reply::Err { message, .. } = crate::ipc::run(&id, params).await {
            state.toast(ToastLevel::Error, message);
        }
    });
}

fn run_ui(state: AppState, id: &str, params: &Value) -> bool {
    if let Some(area) = id.strip_prefix("view.").and_then(Area::from_id) {
        state.go(area);
        if area == Area::Search {
            request_animation_frame(|| {
                let _ = crate::dom::focus_selector("#global-search");
            });
        }
        return true;
    }
    if crate::areas::mail::intercept(state, id, params) {
        return true;
    }
    if crate::areas::calendar::UI_IDS.contains(&id) {
        return crate::areas::calendar::run_ui(state, id, params);
    }
    match id {
        "palette.open" => {
            let q = params.get("query").and_then(Value::as_str).unwrap_or_default().to_owned();
            state.palette_query.set(q);
            state.palette_open.set(true);
        }
        "view.pane.next" => cycle_pane(1),
        "view.pane.previous" => cycle_pane(-1),
        "view.list.next" => state.nav_list(ListNav::Next),
        "view.list.previous" => state.nav_list(ListNav::Previous),
        "view.sidebar.toggle" => {
            state.nav_pane_visible.update(|v| *v = !*v);
            let v = if state.nav_pane_visible.get_untracked() { "shown" } else { "hidden" };
            crate::dom::storage_set("quorum.nav-pane", v);
        }
        "view.escape" => escape(state),
        id if crate::areas::people_ui::UI_IDS.contains(&id) => {
            return crate::areas::people_ui::run_ui(state, id, params);
        }
        "view.open" => open(state, params),
        id if id.starts_with("view.thread.") => return crate::areas::mail_thread::run_ui(id),
        id if id.starts_with("files.") => return crate::areas::files::run_ui(state, id, params),
        _ => return false,
    }
    true
}

/// `view.open {area, id, nav?|team?}`: switch area, select the nav entry and item, and publish
/// the params in `AppState::open_request` for the area to read extra keys (message, post).
fn open(state: AppState, params: &Value) {
    let text = |k: &str| params.get(k).and_then(Value::as_str).map(str::to_owned);
    let Some(area) = text("area").as_deref().and_then(Area::from_id) else {
        state.toast(ToastLevel::Warning, "Couldn't open that item.");
        return;
    };
    let sel = state.selection(area);
    if let Some(nav) = text("nav").or_else(|| text("team")) {
        sel.nav.set(Some(nav));
    }
    if let Some(id) = text("id")
        && sel.item.with_untracked(|cur| cur.as_deref() != Some(id.as_str()))
    {
        sel.item.set(Some(id));
    }
    state.open_request.set(Some(params.clone()));
    state.go(area);
}

/// Esc outside the palette: clear and leave the search box, or return focus to the list.
fn escape(state: AppState) {
    let in_search = crate::dom::active_element().is_some_and(|e| e.id() == "global-search");
    if in_search && !state.search_query.get_untracked().is_empty() {
        state.search_query.set(String::new());
        return;
    }
    let _ = crate::dom::focus_selector("[data-primary-list]");
}

/// Moves focus to the next/previous pane (`[data-pane]`, document order), wrapping (F6).
fn cycle_pane(step: isize) {
    let panes = crate::dom::query_all("[data-pane]");
    if panes.is_empty() {
        return;
    }
    let active = crate::dom::active_element();
    let current = active.as_ref().and_then(|a| panes.iter().position(|p| p == a || p.contains(Some(&**a))));
    let next = next_pane(current, panes.len(), step);
    if let Some(pane) = panes.get(next) {
        // Focus the pane's preferred target: its list, else its first focusable, else itself.
        let target = ["[data-pane-focus]", "[role=listbox]", "input", "button, [tabindex='0']"]
            .iter()
            .find_map(|sel| pane.query_selector(sel).ok().flatten());
        let _ = crate::dom::focus_element(target.as_ref().unwrap_or(pane));
    }
}

/// The pane index after `current` moving by `step`, wrapping (pure, for tests).
pub fn next_pane(current: Option<usize>, len: usize, step: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len_i = isize::try_from(len).unwrap_or(isize::MAX);
    match current {
        None if step < 0 => len - 1,
        None => 0,
        Some(c) => {
            let c = isize::try_from(c).unwrap_or(0);
            usize::try_from((c + step).rem_euclid(len_i)).unwrap_or(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_commands_shadow_engine_duplicates() {
        let engine = vec![
            CommandInfo {
                id: "palette.open".into(),
                label: "Engine palette".into(),
                menu: None,
                shortcut: Some("Cmd+K".into()),
                destructive: false,
                enabled: true,
            },
            CommandInfo {
                id: "mail.reply".into(),
                label: "Reply".into(),
                menu: None,
                shortcut: None,
                destructive: false,
                enabled: true,
            },
        ];
        let all = merged(&engine);
        assert_eq!(all.iter().filter(|c| c.id == "palette.open").count(), 1);
        assert!(all.iter().any(|c| c.id == "mail.reply"));
        assert_eq!(all.len(), UI_COMMANDS.len() + 1);
    }

    #[test]
    fn every_area_has_a_view_command() {
        for a in Area::ALL {
            assert!(is_ui(&a.view_command()), "{a:?}");
        }
    }

    #[test]
    fn pane_cycling_wraps() {
        assert_eq!(next_pane(None, 4, 1), 0);
        assert_eq!(next_pane(None, 4, -1), 3);
        assert_eq!(next_pane(Some(3), 4, 1), 0);
        assert_eq!(next_pane(Some(0), 4, -1), 3);
        assert_eq!(next_pane(Some(1), 4, 1), 2);
        assert_eq!(next_pane(Some(9), 0, 1), 0);
    }
}
