//! Mail-only icons, drawn in code on the same 24×24 stroke grid as `crate::icons` (our own
//! artwork; no third-party icon sets).

use leptos::prelude::*;

/// Extra icons the Mail area draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailIcon {
    /// Reply to everyone (double arrow).
    ReplyAll,
    /// An opened envelope (mark read / unread).
    EnvelopeOpen,
    /// A tag (categorize).
    Tag,
    /// A funnel (filter).
    Filter,
    /// A small downward chevron.
    ChevronDown,
    /// Bold.
    Bold,
    /// Italic.
    Italic,
    /// Underline.
    Underline,
    /// Bulleted list.
    ListBullet,
    /// Numbered list.
    ListNumber,
    /// A link.
    Link,
    /// An image.
    Image,
    /// High importance.
    Important,
    /// Stacked messages (conversation view).
    Conversation,
    /// Undo arrow.
    Undo,
    /// A star (favourites).
    Star,
    /// Download.
    Download,
    /// Two chevrons apart (expand every message).
    ExpandAll,
    /// Two chevrons together (collapse every message).
    CollapseAll,
    /// Up and down arrows (sort order).
    Sort,
    /// A sun (show on a light background).
    Sun,
    /// Three dots in a pill (show quoted text).
    Ellipsis,
}

impl MailIcon {
    /// Path data on the 24×24 grid.
    pub const fn paths(self) -> &'static [&'static str] {
        match self {
            Self::ReplyAll => &["M8 7.5L3.5 12 8 16.5", "M12.5 7.5L8 12l4.5 4.5", "M8 12h6a5.5 5.5 0 0 1 5.5 5.5V19"],
            Self::EnvelopeOpen => &["M3.5 10l8.5-6 8.5 6v9.5h-17z", "M3.5 10l8.5 6 8.5-6"],
            Self::Tag => &["M4 4h7.5l8.5 8.5-7.5 7.5L4 11.5z", "M8.5 9a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1z"],
            Self::Filter => &["M4 5.5h16l-6 7.5v6l-4 1.5V13z"],
            Self::ChevronDown => &["M7 10l5 5 5-5"],
            Self::Bold => &["M7.5 5h5.5a3.5 3.5 0 0 1 0 7H7.5z", "M7.5 12h6.5a3.5 3.5 0 0 1 0 7H7.5z"],
            Self::Italic => &["M10 5h7", "M7 19h7", "M14 5l-4 14"],
            Self::Underline => &["M7.5 4.5V11a4.5 4.5 0 0 0 9 0V4.5", "M6 20h12"],
            Self::ListBullet => &[
                "M9.5 7h10",
                "M9.5 12h10",
                "M9.5 17h10",
                "M5 7.5a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1z",
                "M5 12.5a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1z",
                "M5 17.5a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1z",
            ],
            Self::ListNumber => &["M10 7h10", "M10 12h10", "M10 17h10", "M4.5 5.5l1.5-1v4", "M4.5 14.5h2l-2 3h2"],
            Self::Link => &[
                "M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1",
                "M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1",
            ],
            Self::Image => &["M4 5h16v14H4z", "M4 16l5-5 4 4 3-3 4 4", "M15.5 9.5a1 1 0 1 0 0-2 1 1 0 0 0 0 2z"],
            Self::Important => &["M12 4.5v10", "M12 19v.5"],
            Self::Conversation => &["M4 8h16v11H4z", "M6 5.5h12", "M8 3h8"],
            Self::Undo => &["M8.5 5L4 9.5 8.5 14", "M4 9.5h10a5.5 5.5 0 0 1 0 11h-3"],
            Self::Star => &["M12 4l2.4 5 5.4.6-4 3.7 1.1 5.4L12 16l-4.9 2.7 1.1-5.4-4-3.7 5.4-.6z"],
            Self::Download => &["M12 4v11", "M7.5 10.5L12 15l4.5-4.5", "M5 19.5h14"],
            Self::ExpandAll => &["M7.5 9L12 4.5 16.5 9", "M7.5 15l4.5 4.5 4.5-4.5"],
            Self::CollapseAll => &["M7.5 4.5L12 9l4.5-4.5", "M7.5 19.5L12 15l4.5 4.5"],
            Self::Sort => &["M8 4.5v15", "M4.5 8L8 4.5 11.5 8", "M16 19.5v-15", "M12.5 16l3.5 3.5 3.5-3.5"],
            Self::Sun => &[
                "M12 15.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7z",
                "M12 3v2",
                "M12 19v2",
                "M3 12h2",
                "M19 12h2",
                "M5.6 5.6L7 7",
                "M17 17l1.4 1.4",
                "M5.6 18.4L7 17",
                "M17 7l1.4-1.4",
            ],
            Self::Ellipsis => &[
                "M7 12.5a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1z",
                "M12 12.5a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1z",
                "M17 12.5a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1z",
            ],
        }
    }
}

/// Renders a [`MailIcon`] like `components::Icon` (decorative, sized and coloured by CSS).
#[component]
pub fn MIcon(
    /// Which icon.
    name: MailIcon,
) -> impl IntoView {
    view! {
        <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" aria-hidden="true" focusable="false">
            {name.paths().iter().map(|d| view! { <path d=*d /> }).collect_view()}
        </svg>
    }
}
