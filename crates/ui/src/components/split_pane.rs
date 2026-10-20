//! Two panes side by side with a draggable, keyboard-adjustable divider. The start pane's
//! width persists in `localStorage` (`quorum.split.<id>`); storage failures are ignored.
//!
//! Layout is CSS: the component only sets `--split-start` on its root.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

/// Keyboard step for the divider, in px.
const STEP: f64 = 16.0;

/// Clamps a width into `min..=max`; non-finite input gives `fallback` (clamped too).
pub fn clamp_width(w: f64, min: f64, max: f64, fallback: f64) -> f64 {
    let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
    let w = if w.is_finite() { w } else { fallback };
    if w.is_finite() { w.clamp(lo, hi) } else { lo }
}

/// A horizontal split.
#[component]
pub fn SplitPane(
    /// Persistence key, unique per split (e.g. `mail.list`).
    #[prop(into)]
    id: String,
    /// Default start-pane width in px.
    #[prop(default = 320.0)]
    initial: f64,
    /// Minimum width.
    #[prop(default = 180.0)]
    min: f64,
    /// Maximum width.
    #[prop(default = 720.0)]
    max: f64,
    /// Start (left) pane.
    #[prop(into)]
    start: ViewFn,
    /// End (right) pane, takes the remaining space.
    #[prop(into)]
    end: ViewFn,
    /// Accessible name of the divider.
    #[prop(optional, into)]
    label: Option<String>,
    /// Extra class on the root.
    #[prop(optional, into)]
    class: Option<String>,
) -> impl IntoView {
    let key = format!("quorum.split.{id}");
    let stored = crate::dom::storage_get(&key).and_then(|v| v.parse::<f64>().ok());
    let width = RwSignal::new(clamp_width(stored.unwrap_or(initial), min, max, initial));
    let drag = StoredValue::new(None::<(f64, f64)>);
    let dragging = RwSignal::new(false);
    let key_store = StoredValue::new(key);

    let persist = move || {
        let w = width.get_untracked();
        key_store.with_value(|k| crate::dom::storage_set(k, &format!("{w:.0}")));
    };
    let set = move |w: f64| width.set(clamp_width(w, min, max, initial));

    view! {
        <div
            class=format!("split {}", class.unwrap_or_default())
            class:dragging=move || dragging.get()
            style=move || format!("--split-start: {:.0}px", width.get())
        >
            <div class="split-start">{start.run()}</div>
            <div
                class="split-handle"
                role="separator"
                tabindex="0"
                aria-orientation="vertical"
                aria-label=label.unwrap_or_else(|| "Resize".into())
                aria-valuemin=format!("{min:.0}")
                aria-valuemax=format!("{max:.0}")
                aria-valuenow=move || format!("{:.0}", width.get())
                on:pointerdown=move |e: web_sys::PointerEvent| {
                    if e.button() != 0 {
                        return;
                    }
                    e.prevent_default();
                    if let Some(el) = e.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                        let _ = el.set_pointer_capture(e.pointer_id());
                    }
                    drag.set_value(Some((f64::from(e.client_x()), width.get_untracked())));
                    dragging.set(true);
                }
                on:pointermove=move |e: web_sys::PointerEvent| {
                    if let Some((x0, w0)) = drag.get_value() {
                        set(w0 + f64::from(e.client_x()) - x0);
                    }
                }
                on:pointerup=move |_| {
                    if drag.get_value().is_some() {
                        drag.set_value(None);
                        dragging.set(false);
                        persist();
                    }
                }
                on:pointercancel=move |_| {
                    drag.set_value(None);
                    dragging.set(false);
                }
                on:dblclick=move |_| {
                    set(initial);
                    persist();
                }
                on:keydown=move |e: web_sys::KeyboardEvent| {
                    let w = width.get_untracked();
                    let next = match e.key().as_str() {
                        "ArrowLeft" => w - STEP,
                        "ArrowRight" => w + STEP,
                        "Home" => min,
                        "End" => max,
                        _ => return,
                    };
                    e.prevent_default();
                    set(next);
                    persist();
                }
            ></div>
            <div class="split-end">{end.run()}</div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_handles_bad_input() {
        assert_eq!(clamp_width(100.0, 180.0, 720.0, 320.0), 180.0);
        assert_eq!(clamp_width(900.0, 180.0, 720.0, 320.0), 720.0);
        assert_eq!(clamp_width(f64::NAN, 180.0, 720.0, 320.0), 320.0);
        assert_eq!(clamp_width(f64::INFINITY, 180.0, 720.0, f64::NAN), 180.0);
        assert_eq!(clamp_width(500.0, 720.0, 180.0, 320.0), 500.0); // swapped bounds
    }
}
