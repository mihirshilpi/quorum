//! Placeholder for empty lists, nothing selected, and errors.

use leptos::prelude::*;

use super::icon::Icon;
use crate::icons::IconName;

/// A centred icon, title and optional detail line.
#[component]
pub fn EmptyState(
    /// Main line.
    #[prop(into)]
    title: String,
    /// Secondary line.
    #[prop(optional, into)]
    detail: Option<String>,
    /// Illustration icon.
    #[prop(optional)]
    icon: Option<IconName>,
    /// Marks the state as an error (`role="alert"`).
    #[prop(optional)]
    error: bool,
) -> impl IntoView {
    view! {
        <div class="empty-state" class:error=error role=if error { "alert" } else { "note" }>
            {icon.map(|i| view! { <span class="empty-state-icon"><Icon name=i /></span> })}
            <p class="empty-state-title">{title}</p>
            {detail.map(|d| view! { <p class="empty-state-detail">{d}</p> })}
        </div>
    }
}
