//! The window shell (Office-style): top bar (search in the centre, account and presence on the
//! right), a narrow app rail, and the current area's panes in the main region.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::areas;
use crate::components::{Icon, Toasts};
use crate::icons::IconName;
use crate::palette::CommandPalette;
use crate::state::{AppState, Area};

/// Root component.
#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new(crate::ipc::is_preview());
    provide_context(state);
    crate::ipc::subscribe(move |ev| state.handle_event(ev));
    crate::ipc::start_event_bridge();
    state.load_commands();
    state.load_account();
    crate::keys::install(state);
    crate::components::person_card::init();
    // Created in the root owner so it outlives the Settings area.
    let _ = crate::areas::settings::section_signal();

    let shown = crate::state::shown_area(state.area);

    view! {
        <div class="app" class:preview=state.preview>
            <TopBar />
            <Rail />
            <main class="main" aria-label=move || shown.get().label()>
                {move || areas::view_for(shown.get())}
            </main>
            <CommandPalette />
            <Toasts />
            <crate::onboarding::Overlays />
            <crate::components::person_card::PersonCardHost />
        </div>
    }
}

fn shortcut_for(state: AppState, id: &str) -> Option<String> {
    let mac = crate::dom::is_mac();
    state
        .all_commands_tracked()
        .into_iter()
        .find(|c| c.id == id)
        .and_then(|c| c.shortcut)
        .map(|s| crate::keys::display_shortcut(&s, mac))
}

#[component]
fn TopBar() -> impl IntoView {
    let state = crate::state::app_state();
    let search_hint = move || shortcut_for(state, "view.search");
    let palette_hint = move || shortcut_for(state, "palette.open").map(|s| format!("Command palette ({s})"));
    view! {
        <header class="topbar" data-pane="" tabindex="-1">
            <div class="topbar-start">
                <span class="brand">"Quorum"</span>
            </div>
            <div class="topbar-search" role="search">
                <Icon name=IconName::Search />
                <input
                    id="global-search"
                    data-pane-focus=""
                    type="search"
                    placeholder="Search"
                    aria-label="Search mail, chats, files and people"
                    autocomplete="off"
                    spellcheck="false"
                    prop:value=move || state.search_query.get()
                    on:input=move |e| {
                        let v = e
                            .target()
                            .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
                            .map(|i| i.value())
                            .unwrap_or_default();
                        state.search_query.set(v);
                    }
                    on:keydown=move |e: web_sys::KeyboardEvent| {
                        if e.key() == "Enter" {
                            e.prevent_default();
                            state.area.set(Area::Search);
                        }
                    }
                />
                {move || search_hint().map(|s| view! { <kbd class="search-hint" aria-hidden="true">{s}</kbd> })}
            </div>
            <div class="topbar-end">
                {state.preview.then(|| view! { <span class="badge" title="Synthetic data, no backend">"Preview"</span> })}
                <button
                    class="icon-button"
                    aria-label=palette_hint
                    title=palette_hint
                    on:click=move |_| crate::commands::dispatch(state, "palette.open", serde_json::Value::Null)
                >
                    <Icon name=IconName::Command />
                </button>
                <crate::components::presence_picker::PresencePicker />
            </div>
        </header>
    }
}

#[component]
fn Rail() -> impl IntoView {
    let state = crate::state::app_state();
    let item = move |area: Area| {
        let id = area.view_command();
        let title = move || match shortcut_for(state, &id) {
            Some(s) => format!("{} ({s})", area.label()),
            None => area.label().to_owned(),
        };
        let cmd = area.view_command();
        view! {
            <button
                class="rail-item"
                class:selected=move || state.area.get() == area
                aria-current=move || (state.area.get() == area).then_some("page")
                data-pane-focus=move || (state.area.get() == area).then_some("")
                aria-label=area.label()
                title=title
                on:click=move |_| crate::commands::dispatch(state, &cmd, serde_json::Value::Null)
            >
                <Icon name=area.icon() />
                <span class="rail-label" aria-hidden="true">{area.label()}</span>
            </button>
        }
    };
    view! {
        <nav class="rail" aria-label="Apps" data-pane="" tabindex="-1">
            {Area::RAIL.into_iter().map(item).collect_view()}
            <span class="rail-spacer"></span>
            {item(Area::Settings)}
        </nav>
    }
}
