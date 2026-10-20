//! Windowed list for very long collections (100k+ rows): only the rows in view (plus a little
//! overscan) are in the DOM. Fixed row height, read from the CSS token `--list-row-height` on
//! the list element unless given, so density is a CSS change.
//!
//! Accessibility: `role="listbox"` with `aria-activedescendant`; rows are `role="option"` with
//! `aria-selected`, `aria-setsize` and `aria-posinset` (the full size, not just the window).
//! Keys: ↑/↓, Home/End, PageUp/PageDown, Enter (activate); `j`/`k` arrive as
//! `view.list.next/previous` commands for the area's primary list.

use std::ops::Range;
use std::sync::Arc;

use leptos::html;
use leptos::prelude::*;

use crate::state::{ListNav, app_state};

/// Rows rendered above and below the viewport.
pub const OVERSCAN: usize = 6;
/// Row height when neither the prop nor the CSS token gives one.
pub const DEFAULT_ROW_HEIGHT: f64 = 40.0;
/// Viewport height assumed before the first layout measurement.
const DEFAULT_VIEWPORT: f64 = 800.0;

/// Indices to render for a scroll position (pure).
pub fn visible_range(scroll_top: f64, viewport: f64, row_height: f64, len: usize, overscan: usize) -> Range<usize> {
    if len == 0 || !(row_height.is_finite() && row_height > 0.0) {
        return 0..0;
    }
    let top = if scroll_top.is_finite() { scroll_top.max(0.0) } else { 0.0 };
    let viewport = if viewport.is_finite() { viewport.max(0.0) } else { 0.0 };
    let first = to_index(top / row_height).min(len);
    let visible = to_index((viewport / row_height).ceil()).saturating_add(1);
    let start = first.saturating_sub(overscan);
    let end = first.saturating_add(visible).saturating_add(overscan).min(len);
    start..end.max(start)
}

fn to_index(v: f64) -> usize {
    if v.is_finite() && v > 0.0 {
        // Saturating float-to-int cast; values are bounded by the checks above.
        v.floor() as usize
    } else {
        0
    }
}

/// The scroll offset that brings row `index` fully into view, or `None` if it already is.
pub fn scroll_to_reveal(index: usize, row_height: f64, scroll_top: f64, viewport: f64) -> Option<f64> {
    if !(row_height.is_finite() && row_height > 0.0 && viewport.is_finite() && scroll_top.is_finite()) {
        return None;
    }
    let top = index as f64 * row_height;
    let bottom = top + row_height;
    if top < scroll_top {
        Some(top)
    } else if bottom > scroll_top + viewport {
        Some((bottom - viewport).max(0.0))
    } else {
        None
    }
}

/// A navigation key inside a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKey {
    /// One down.
    Down,
    /// One up.
    Up,
    /// First row.
    Home,
    /// Last row.
    End,
    /// One page down.
    PageDown,
    /// One page up.
    PageUp,
}

impl NavKey {
    /// Maps `KeyboardEvent.key`.
    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "ArrowDown" => Self::Down,
            "ArrowUp" => Self::Up,
            "Home" => Self::Home,
            "End" => Self::End,
            "PageDown" => Self::PageDown,
            "PageUp" => Self::PageUp,
            _ => return None,
        })
    }
}

/// The index after pressing `key` (pure). With no selection, Down/Home select the first row
/// and Up/End the last. Clamps at the ends.
pub fn next_index(current: Option<usize>, key: NavKey, len: usize, page: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len - 1;
    let page = page.max(1);
    Some(match (current.map(|c| c.min(last)), key) {
        (_, NavKey::Home) => 0,
        (_, NavKey::End) => last,
        (None, NavKey::Down | NavKey::PageDown) => 0,
        (None, NavKey::Up | NavKey::PageUp) => last,
        (Some(c), NavKey::Down) => (c + 1).min(last),
        (Some(c), NavKey::Up) => c.saturating_sub(1),
        (Some(c), NavKey::PageDown) => c.saturating_add(page).min(last),
        (Some(c), NavKey::PageUp) => c.saturating_sub(page),
    })
}

/// A virtualised, keyboard-navigable, single-selection list.
#[component]
pub fn VirtualList<T>(
    /// The rows. `Arc` so large lists are never copied.
    #[prop(into)]
    items: Signal<Arc<Vec<T>>>,
    /// Stable key of a row (its id); the selection holds keys.
    key: fn(&T) -> String,
    /// Renders one row's content.
    render: Callback<T, AnyView>,
    /// Selected key.
    selected: RwSignal<Option<String>>,
    /// Accessible name of the list.
    #[prop(into)]
    label: String,
    /// Fixed row height in px; default: CSS `--list-row-height`.
    #[prop(optional)]
    row_height: Option<f64>,
    /// Enter / double-click on a row.
    #[prop(optional)]
    on_activate: Option<Callback<String>>,
    /// The area's primary list: target of `j`/`k`, Esc and area switches.
    #[prop(optional)]
    primary: bool,
) -> impl IntoView
where
    T: Clone + Send + Sync + 'static,
{
    let app = app_state();
    let list_id = crate::dom::unique_id("vlist");
    let node = NodeRef::<html::Div>::new();
    let scroll_top = RwSignal::new(0.0f64);
    let viewport = RwSignal::new(DEFAULT_VIEWPORT);
    let row_h = RwSignal::new(row_height.unwrap_or(DEFAULT_ROW_HEIGHT));

    // Runs from `request_animation_frame` and window resize, which can fire after the list
    // unmounted: every reactive access is `try_*` and bails once the owner is disposed.
    let measure = move || {
        let Some(el) = node.try_get_untracked().flatten() else { return };
        let h = f64::from(el.client_height());
        if h > 0.0 && viewport.try_set(h).is_some() {
            return;
        }
        if row_height.is_none()
            && let Some(px) = crate::dom::css_var(&el, "--list-row-height").and_then(|v| crate::dom::parse_px(&v))
        {
            let _ = row_h.try_set(px);
        }
    };
    Effect::new(move |_| {
        if node.get().is_some() {
            request_animation_frame(measure);
        }
    });
    let resize = window_event_listener(leptos::ev::resize, move |_| measure());
    on_cleanup(move || resize.remove());

    let len = Memo::new(move |_| items.with(|v| v.len()));
    // Bumps whenever the rows are replaced (a store change re-ran the query), so rows whose
    // data changed while their (index, id) key did not are rendered again.
    let generation = Memo::new(move |prev: Option<&u64>| {
        items.track();
        prev.map_or(0, |p| p.wrapping_add(1))
    });
    let range = Memo::new(move |_| visible_range(scroll_top.get(), viewport.get(), row_h.get(), len.get(), OVERSCAN));
    let selected_index = Memo::new(move |_| {
        let sel = selected.get()?;
        items.with(|v| v.iter().position(|t| key(t) == sel))
    });

    let reveal = move |i: usize| {
        let Some(el) = node.get_untracked() else { return };
        if let Some(top) =
            scroll_to_reveal(i, row_h.get_untracked(), scroll_top.get_untracked(), viewport.get_untracked())
        {
            el.set_scroll_top(top.round() as i32);
            scroll_top.set(top);
        }
    };
    let select_index = move |i: usize| {
        let k = items.with_untracked(|v| v.get(i).map(key));
        if let Some(k) = k {
            selected.set(Some(k));
            reveal(i);
        }
    };
    let navigate = move |nav: NavKey| {
        let page = to_index(viewport.get_untracked() / row_h.get_untracked()).max(1);
        if let Some(i) = next_index(selected_index.get_untracked(), nav, len.get_untracked(), page) {
            select_index(i);
        }
    };

    // When the selected row disappears (deleted, moved), select the row that took its place,
    // as mail clients do, so keyboard triage continues without a lost selection.
    let last_index = StoredValue::new(None::<usize>);
    Effect::new(move |_| match selected_index.get() {
        Some(i) => last_index.set_value(Some(i)),
        None => {
            let n = len.get_untracked();
            if n > 0
                && selected.with_untracked(Option::is_some)
                && let Some(i) = last_index.try_get_value().flatten()
            {
                select_index(i.min(n - 1));
            }
        }
    });

    // j / k (and other list commands) for the primary list.
    if primary {
        Effect::new(move |prev: Option<u64>| {
            let (n, req) = app.list_nav.get();
            if prev.is_some_and(|p| p != n)
                && let Some(req) = req
            {
                navigate(match req {
                    ListNav::Next => NavKey::Down,
                    ListNav::Previous => NavKey::Up,
                });
            }
            n
        });
    }

    let on_keydown = move |e: web_sys::KeyboardEvent| {
        if e.meta_key() || e.ctrl_key() || e.alt_key() {
            return;
        }
        if let Some(nav) = NavKey::from_key(&e.key()) {
            e.prevent_default();
            navigate(nav);
        } else if e.key() == "Enter"
            && let (Some(cb), Some(k)) = (on_activate, selected.get_untracked())
        {
            e.prevent_default();
            cb.run(k);
        }
    };

    let rows = move || {
        let r = range.get();
        let g = generation.get();
        items.with(|v| r.filter_map(|i| v.get(i).map(|t| (i, key(t), g, t.clone()))).collect::<Vec<_>>())
    };
    let opt_prefix = list_id.clone();
    let row_prefix = list_id.clone();
    let active_descendant = move || selected_index.get().map(|i| format!("{opt_prefix}-{i}"));

    view! {
        <div
            node_ref=node
            id=list_id
            class="vlist"
            role="listbox"
            tabindex="0"
            aria-label=label
            aria-activedescendant=active_descendant
            data-primary-list=primary.then_some("")
            on:scroll=move |_| {
                if let Some(el) = node.get_untracked() {
                    scroll_top.set(f64::from(el.scroll_top()));
                }
            }
            on:keydown=on_keydown
        >
            <div class="vlist-spacer" style:height=move || format!("{}px", len.get() as f64 * row_h.get())>
                <For
                    each=rows
                    key=|(i, k, g, _)| (*i, k.clone(), *g)
                    children=move |(i, k, _, item)| {
                        let k_sel = k.clone();
                        let k_click = k.clone();
                        let k_act = k.clone();
                        let is_selected = move || selected.with(|s| s.as_deref() == Some(k_sel.as_str()));
                        let is_sel_attr = is_selected.clone();
                        view! {
                            <div
                                id=format!("{row_prefix}-{i}")
                                class="vlist-row"
                                class:selected=is_selected
                                role="option"
                                aria-selected=move || if is_sel_attr() { "true" } else { "false" }
                                aria-setsize=move || len.get().to_string()
                                aria-posinset=(i + 1).to_string()
                                style:top=move || format!("{}px", i as f64 * row_h.get())
                                style:height=move || format!("{}px", row_h.get())
                                on:mousedown=move |_| {
                                    selected.set(Some(k_click.clone()));
                                }
                                on:dblclick=move |_| {
                                    if let Some(cb) = on_activate {
                                        cb.run(k_act.clone());
                                    }
                                }
                            >
                                {render.run(item)}
                            </div>
                        }
                    }
                />
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_math_for_100k_rows() {
        let len = 100_000;
        assert_eq!(visible_range(0.0, 400.0, 40.0, len, 0), 0..11);
        assert_eq!(visible_range(0.0, 400.0, 40.0, len, 5), 0..16);
        // Middle: first visible row 50_000.
        let r = visible_range(2_000_000.0, 400.0, 40.0, len, 5);
        assert_eq!(r, 49_995..50_016);
        // Scrolled past the end (elastic scroll): clamps, never panics.
        let r = visible_range(1e12, 400.0, 40.0, len, 5);
        assert!(r.end <= len && r.start <= r.end);
        // The window is always small.
        for top in [0.0, 123.4, 1e6, 3_999_600.0] {
            let r = visible_range(top, 900.0, 32.0, len, OVERSCAN);
            assert!(r.len() <= 900 / 32 + 2 + 2 * OVERSCAN, "{r:?}");
        }
    }

    #[test]
    fn window_math_rejects_bad_numbers() {
        assert_eq!(visible_range(0.0, 400.0, 0.0, 10, 2), 0..0);
        assert_eq!(visible_range(0.0, 400.0, f64::NAN, 10, 2), 0..0);
        assert_eq!(visible_range(f64::NAN, f64::INFINITY, 40.0, 10, 2), 0..3);
        assert_eq!(visible_range(-50.0, 400.0, 40.0, 0, 2), 0..0);
        assert_eq!(visible_range(-50.0, 80.0, 40.0, 10, 0), 0..3);
    }

    #[test]
    fn reveal_scrolls_minimally() {
        assert_eq!(scroll_to_reveal(0, 40.0, 0.0, 400.0), None);
        assert_eq!(scroll_to_reveal(9, 40.0, 0.0, 400.0), None);
        assert_eq!(scroll_to_reveal(10, 40.0, 0.0, 400.0), Some(40.0));
        assert_eq!(scroll_to_reveal(2, 40.0, 200.0, 400.0), Some(80.0));
        assert_eq!(scroll_to_reveal(2, f64::NAN, 200.0, 400.0), None);
    }

    #[test]
    fn keyboard_navigation_clamps() {
        assert_eq!(next_index(None, NavKey::Down, 0, 10), None);
        assert_eq!(next_index(None, NavKey::Down, 5, 10), Some(0));
        assert_eq!(next_index(None, NavKey::Up, 5, 10), Some(4));
        assert_eq!(next_index(Some(4), NavKey::Down, 5, 10), Some(4));
        assert_eq!(next_index(Some(0), NavKey::Up, 5, 10), Some(0));
        assert_eq!(next_index(Some(2), NavKey::PageDown, 100_000, 20), Some(22));
        assert_eq!(next_index(Some(5), NavKey::PageUp, 100_000, 20), Some(0));
        assert_eq!(next_index(Some(7), NavKey::End, 100_000, 20), Some(99_999));
        assert_eq!(next_index(Some(7), NavKey::Home, 100_000, 0), Some(0));
        // A stale selection beyond the end clamps.
        assert_eq!(next_index(Some(500), NavKey::Up, 10, 3), Some(8));
        assert_eq!(NavKey::from_key("j"), None);
    }
}
