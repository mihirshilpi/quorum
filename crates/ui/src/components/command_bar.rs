//! The simplified command bar above a list (New, Reply, Delete, Move, Flag, ...).
//!
//! Every button is a command id; clicking dispatches it with the context params, exactly like
//! the shortcut and the palette would. Labels and shortcut hints come from the registry when
//! the command is known there (tooltip).

use leptos::prelude::*;

use super::icon::Icon;
use crate::icons::IconName;
use crate::state::app_state;

/// One command bar button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandButton {
    /// Command id to dispatch.
    pub id: &'static str,
    /// Short button label (the registry's full label shows in the tooltip).
    pub label: &'static str,
    /// Icon.
    pub icon: IconName,
    /// Primary action (styled prominently, e.g. New).
    pub primary: bool,
}

impl CommandButton {
    /// A regular button.
    pub const fn new(id: &'static str, label: &'static str, icon: IconName) -> Self {
        Self { id, label, icon, primary: false }
    }

    /// The prominent first button.
    pub const fn primary(id: &'static str, label: &'static str, icon: IconName) -> Self {
        Self { id, label, icon, primary: true }
    }
}

/// Width reserved for the overflow ("…") button, px.
const MORE_WIDTH: f64 = 40.0;
/// Width of the overflow menu, px (matches `.command-menu { width }`).
const MENU_WIDTH: f64 = 200.0;
/// Gap between buttons, px (matches `.command-bar { gap }`).
const GAP: f64 = 2.0;

/// How many buttons fit in `available` px (pure). `widths` are the measured button widths;
/// when not all fit, room is kept for the overflow button. Unmeasured (0) widths count as
/// fitting so the first layout shows everything and can be measured.
pub fn buttons_that_fit(widths: &[f64], available: f64) -> usize {
    let w = |x: f64| if x.is_finite() && x > 0.0 { x + GAP } else { 0.0 };
    let total: f64 = widths.iter().map(|x| w(*x)).sum();
    if !available.is_finite() || available <= 0.0 || total <= available {
        return widths.len();
    }
    let room = available - MORE_WIDTH;
    let mut used = 0.0;
    let mut n = 0;
    for x in widths {
        used += w(*x);
        if used > room {
            break;
        }
        n += 1;
    }
    n
}

/// A toolbar of command buttons. Buttons that don't fit the width move into an overflow
/// ("…") menu, so narrow panes keep every command reachable.
#[component]
pub fn CommandBar(
    /// The buttons, in order.
    buttons: &'static [CommandButton],
    /// Accessible name of the toolbar.
    #[prop(into)]
    label: String,
) -> impl IntoView {
    let state = app_state();
    let mac = crate::dom::is_mac();
    let bar = NodeRef::<leptos::html::Div>::new();
    let fit = RwSignal::new(buttons.len());
    let widths = StoredValue::new(vec![0.0f64; buttons.len()]);
    let menu_open = RwSignal::new(false);
    let menu_pos = RwSignal::new((0.0f64, 0.0f64));

    // Runs from animation frames and window listeners that can fire after unmount: every
    // reactive access is `try_*`.
    let measure = move || {
        let Some(el) = bar.try_get_untracked().flatten() else { return };
        if let Ok(list) = el.query_selector_all(".command-button[data-index]") {
            let _ = widths.try_update_value(|w| {
                for i in 0..list.length() {
                    let Some(node) = list.item(i) else { continue };
                    let Ok(btn) = wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(node) else { continue };
                    let idx = btn.get_attribute("data-index").and_then(|v| v.parse::<usize>().ok());
                    let width = btn.get_bounding_client_rect().width();
                    if let (Some(idx), true) = (idx, width > 0.0)
                        && let Some(slot) = w.get_mut(idx)
                    {
                        *slot = width;
                    }
                }
            });
        }
        let padding = 16.0;
        let available = f64::from(el.client_width()) - padding;
        if let Some(n) = widths.try_with_value(|w| buttons_that_fit(w, available)) {
            let _ = fit.try_set(n);
        }
    };
    Effect::new(move |_| {
        if bar.get().is_some() {
            request_animation_frame(measure);
        }
    });
    let resize = window_event_listener(leptos::ev::resize, move |_| measure());
    // Split panes resize the bar without a window resize; re-measure after a drag.
    let pointer = window_event_listener(leptos::ev::pointerup, move |_| request_animation_frame(measure));
    on_cleanup(move || {
        resize.remove();
        pointer.remove();
    });

    let info_of = move |id: &'static str| state.engine_commands.with(|c| c.iter().find(|c| c.id == id).cloned());
    let run = move |id: &'static str| {
        crate::commands::dispatch(state, id, crate::commands::context_params(state, id));
    };
    let overflow = move || buttons.iter().enumerate().skip(fit.get()).map(|(_, b)| *b).collect::<Vec<_>>();

    view! {
        <div class="command-bar" role="toolbar" aria-label=label node_ref=bar>
            {buttons
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    let b = *b;
                    let info = move || info_of(b.id);
                    // Short label on the button; the registry's full label and shortcut in the tooltip.
                    let title = move || {
                        let info = info();
                        let full = info.as_ref().map(|i| i.label.clone()).unwrap_or_else(|| b.label.to_owned());
                        match info.and_then(|i| i.shortcut).map(|s| crate::keys::display_shortcut(&s, mac)) {
                            Some(s) => format!("{full} ({s})"),
                            None => full,
                        }
                    };
                    let disabled = move || info().is_some_and(|i| !i.enabled);
                    view! {
                        <button
                            class="command-button"
                            class:primary=b.primary
                            class:command-hidden=Signal::derive(move || i >= fit.get())
                            data-command=b.id
                            data-index=i.to_string()
                            title=title
                            disabled=disabled
                            on:click=move |_| run(b.id)
                        >
                            <Icon name=b.icon />
                            <span class="command-label">{b.label}</span>
                        </button>
                    }
                })
                .collect_view()}
            <Show when=move || fit.get() < buttons.len()>
                <span class="command-more-wrap">
                    <button
                        class="command-button command-more"
                        title="More commands"
                        aria-label="More commands"
                        aria-haspopup="menu"
                        aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                        on:click=move |e: web_sys::MouseEvent| {
                            if let Some(el) = e
                                .current_target()
                                .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
                            {
                                let r = el.get_bounding_client_rect();
                                menu_pos.set(((r.right() - MENU_WIDTH).max(8.0), r.bottom() + 4.0));
                            }
                            menu_open.update(|o| *o = !*o);
                        }
                    >
                        <Icon name=IconName::More />
                    </button>
                    <Show when=move || menu_open.get()>
                        <div class="command-menu-backdrop" on:mousedown=move |_| menu_open.set(false)></div>
                        <div
                            class="command-menu"
                            role="menu"
                            style=move || {
                                let (x, y) = menu_pos.get();
                                format!("left: {x:.0}px; top: {y:.0}px;")
                            }
                            on:keydown=move |e: web_sys::KeyboardEvent| {
                                if e.key() == "Escape" {
                                    e.stop_propagation();
                                    menu_open.set(false);
                                }
                            }
                        >
                            {move || {
                                overflow()
                                    .into_iter()
                                    .map(|b| {
                                        let disabled = move || info_of(b.id).is_some_and(|i| !i.enabled);
                                        view! {
                                            <button
                                                class="command-menu-item"
                                                role="menuitem"
                                                data-command=b.id
                                                disabled=disabled
                                                on:click=move |_| {
                                                    menu_open.set(false);
                                                    run(b.id);
                                                }
                                            >
                                                <Icon name=b.icon />
                                                <span>{b.label}</span>
                                            </button>
                                        }
                                    })
                                    .collect_view()
                            }}
                        </div>
                    </Show>
                </span>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_keeps_room_for_the_more_button() {
        let w = [100.0, 60.0, 60.0, 60.0];
        // 4 buttons + gaps = 288: all fit in 300.
        assert_eq!(buttons_that_fit(&w, 300.0), 4);
        // 200 px: room 160 → 102 + 62 = 164 > 160 → one button.
        assert_eq!(buttons_that_fit(&w, 200.0), 1);
        assert_eq!(buttons_that_fit(&w, 230.0), 2);
        assert_eq!(buttons_that_fit(&w, 10.0), 0);
        // Unmeasured or bad numbers never hide anything.
        assert_eq!(buttons_that_fit(&[0.0, 0.0], 10.0), 2);
        assert_eq!(buttons_that_fit(&w, f64::NAN), 4);
        assert_eq!(buttons_that_fit(&[], 100.0), 0);
    }
}
