//! Chat-specific icons: our own simple strokes on a 24x24 grid (no third-party artwork).

use leptos::prelude::*;

/// Chat icon names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatIcon {
    /// Smiling face (emoji picker, reactions).
    Smile,
    /// `@` (mention).
    At,
    /// Three dots.
    More,
    /// Push pin.
    Pin,
    /// Downward chevron.
    ChevronDown,
    /// Quotation marks (reply with quote).
    Quote,
    /// Paper plane (send).
    Send,
    /// Paper clip (attach).
    Clip,
    /// Pencil (edit).
    Edit,
    /// Bin (delete).
    Trash,
    /// Speech bubble with lines (replies).
    Thread,
    /// Document (file card).
    Doc,
    /// Plus inside a circle (new).
    Compose,
    /// Text formatting (A with underline).
    Format,
}

impl ChatIcon {
    /// Path data.
    pub const fn paths(self) -> &'static [&'static str] {
        match self {
            Self::Smile => {
                &["M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z", "M8.5 14.5c1.9 2 5.1 2 7 0", "M9 9.5h.01", "M15 9.5h.01"]
            }
            Self::At => &["M16 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0z", "M16 12v1.5a2.5 2.5 0 0 0 5 0V12a9 9 0 1 0-3.6 7.2"],
            Self::More => &["M5 12h.01", "M12 12h.01", "M19 12h.01"],
            Self::Pin => &["M9 4h6l-1 6 3 3H7l3-3-1-6z", "M12 13v7"],
            Self::ChevronDown => &["M6 9l6 6 6-6"],
            Self::Quote => &["M7 17c-2 0-3-1.5-3-3.5S5.5 9 9 7", "M17 17c-2 0-3-1.5-3-3.5S15.5 9 19 7"],
            Self::Send => &["M4 12l16-8-6 16-2.5-6.5L4 12z", "M11.5 13.5L20 4"],
            Self::Clip => &[
                "M20 11.5l-7.8 7.8a5 5 0 0 1-7.1-7.1l8.5-8.5a3.3 3.3 0 0 1 4.7 4.7l-8.5 8.5a1.7 1.7 0 0 1-2.4-2.4l7.8-7.8",
            ],
            Self::Edit => &["M4 20h4L19 9l-4-4L4 16v4z", "M13.5 6.5l4 4"],
            Self::Trash => &["M5 7h14", "M10 7V4h4v3", "M7 7l1 13h8l1-13"],
            Self::Thread => &["M4 5h16v11H9l-5 4V5z", "M8 9h8", "M8 12h5"],
            Self::Doc => &["M6 3h8l4 4v14H6V3z", "M14 3v4h4", "M9 12h6", "M9 16h6"],
            Self::Compose => &["M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z", "M12 8v8", "M8 12h8"],
            Self::Format => &["M7 17l5-12 5 12", "M9 12.5h6", "M5 20h14"],
        }
    }
}

/// Draws a [`ChatIcon`] (decorative; label the control instead).
#[component]
pub fn CIcon(
    /// Which icon.
    name: ChatIcon,
) -> impl IntoView {
    view! {
        <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false">
            {name.paths().iter().map(|d| view! { <path d=*d /> }).collect_view()}
        </svg>
    }
}
