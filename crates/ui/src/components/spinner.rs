//! Loading indicator.

use leptos::prelude::*;

/// An indeterminate spinner with an accessible label.
#[component]
pub fn Spinner(
    /// Accessible label (default "Loading").
    #[prop(optional, into)]
    label: Option<String>,
) -> impl IntoView {
    let label = label.unwrap_or_else(|| "Loading".into());
    view! { <span class="spinner" role="status" aria-label=label></span> }
}
