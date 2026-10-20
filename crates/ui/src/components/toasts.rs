//! The toast stack (bottom of the window). Errors use `role="alert"`, others `role="status"`.

use leptos::prelude::*;

use super::icon::Icon;
use crate::icons::IconName;
use crate::state::{ToastLevel, app_state};

/// Renders `AppState::toasts`.
#[component]
pub fn Toasts() -> impl IntoView {
    let state = app_state();
    view! {
        <div class="toasts" aria-live="polite">
            <For each=move || state.toasts.get() key=|t| t.id let:toast>
                <div
                    class=format!("toast toast-{}", toast.level.as_str())
                    role=if toast.level == ToastLevel::Error { "alert" } else { "status" }
                >
                    <span class="toast-message">{toast.message.clone()}</span>
                    <button
                        class="icon-button toast-close"
                        aria-label="Dismiss"
                        on:click=move |_| state.dismiss_toast(toast.id)
                    >
                        <Icon name=IconName::Close />
                    </button>
                </div>
            </For>
        </div>
    }
}
