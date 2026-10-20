//! Calendar icons: our own simple strokes on a 24x24 grid (no third-party artwork).

use leptos::prelude::*;

/// Calendar icon names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalIcon {
    /// Left chevron (previous).
    Prev,
    /// Right chevron (next).
    Next,
    /// Video camera (online meeting, join).
    Video,
    /// Map pin (location).
    Place,
    /// Circular arrows (recurring).
    Repeat,
    /// Two heads (attendees).
    Group,
    /// Tick (accepted).
    Check,
    /// Question mark (tentative).
    Maybe,
    /// Cross (declined, close).
    Cross,
    /// Clock (time).
    Time,
    /// Bell (reminder).
    Bell,
    /// Padlock (private).
    Lock,
    /// Plus (new).
    Add,
    /// Door (room).
    Room,
    /// Sparkle (suggested times).
    Spark,
}

impl CalIcon {
    /// Path data.
    pub const fn paths(self) -> &'static [&'static str] {
        match self {
            Self::Prev => &["M14 6.5L8.5 12l5.5 5.5"],
            Self::Next => &["M10 6.5l5.5 5.5-5.5 5.5"],
            Self::Video => &["M3.5 7h11v10h-11z", "M14.5 10.5L20.5 7v10l-6-3.5"],
            Self::Place => &[
                "M12 21s-6.5-6-6.5-11a6.5 6.5 0 0 1 13 0c0 5-6.5 11-6.5 11z",
                "M12 12.2a2.2 2.2 0 1 0 0-4.4 2.2 2.2 0 0 0 0 4.4z",
            ],
            Self::Repeat => &[
                "M4 11V9.5A3.5 3.5 0 0 1 7.5 6H19",
                "M16 3l3 3-3 3",
                "M20 13v1.5a3.5 3.5 0 0 1-3.5 3.5H5",
                "M8 21l-3-3 3-3",
            ],
            Self::Group => &[
                "M9 11a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7z",
                "M2.5 20c.7-3.4 3.3-5.5 6.5-5.5s5.8 2.1 6.5 5.5",
                "M16 4.3a3.5 3.5 0 0 1 0 6.4",
                "M18 14.8c1.8.8 3 2.6 3.5 5.2",
            ],
            Self::Check => &["M5 12.5l4.5 4.5L19 7.5"],
            Self::Maybe => &["M9.2 9a2.9 2.9 0 1 1 4.2 2.6c-.9.5-1.4 1.2-1.4 2.2v.7", "M12 18h.01"],
            Self::Cross => &["M6.5 6.5l11 11", "M17.5 6.5l-11 11"],
            Self::Time => &["M12 20.5a8.5 8.5 0 1 0 0-17 8.5 8.5 0 0 0 0 17z", "M12 7.5V12l3 2"],
            Self::Bell => &["M6 16V11a6 6 0 0 1 12 0v5l1.5 2h-15L6 16z", "M10 20.5a2 2 0 0 0 4 0"],
            Self::Lock => &["M6 11h12v9H6z", "M8.5 11V8a3.5 3.5 0 0 1 7 0v3"],
            Self::Add => &["M12 5v14", "M5 12h14"],
            Self::Room => &["M6 20.5V4h12v16.5", "M3.5 20.5h17", "M14.5 12.5h.01"],
            Self::Spark => &[
                "M12 3.5l1.8 5 5 1.8-5 1.8-1.8 5-1.8-5-5-1.8 5-1.8z",
                "M18.5 15.5l.8 2 2 .8-2 .8-.8 2-.8-2-2-.8 2-.8z",
            ],
        }
    }
}

/// Draws a [`CalIcon`] (decorative; label the control instead).
#[component]
pub fn KIcon(
    /// Which icon.
    name: CalIcon,
) -> impl IntoView {
    view! {
        <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false">
            {name.paths().iter().map(|d| view! { <path d=*d /> }).collect_view()}
        </svg>
    }
}
