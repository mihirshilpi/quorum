//! Renders an [`IconName`] as inline SVG (stroke = `currentColor`, size from CSS).

use leptos::prelude::*;

use crate::icons::IconName;

/// A decorative icon (hidden from assistive tech; label the control that holds it instead).
#[component]
pub fn Icon(
    /// Which icon.
    name: IconName,
) -> impl IntoView {
    view! {
        <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" aria-hidden="true" focusable="false">
            {name.paths().iter().map(|d| view! { <path d=*d /> }).collect_view()}
        </svg>
    }
}
