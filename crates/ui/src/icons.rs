//! Quorum's own icons: simple geometric strokes on a 24x24 grid, authored for this project.
//!
//! Rules (CLAUDE.md, assets): no Microsoft, Fluent or other third-party icon artwork, ever.
//! Each icon is a few hand-written path commands, stroked with `currentColor` so colour, weight
//! and size come from CSS (`--icon-stroke`, `--icon-size`).

/// The icons the UI can draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconName {
    /// Envelope.
    Mail,
    /// Page with binding rings.
    Calendar,
    /// Speech bubble.
    Chat,
    /// Two people (group).
    Teams,
    /// Folder.
    Folder,
    /// One person.
    People,
    /// Magnifier.
    Search,
    /// Two sliders.
    Settings,
    /// Plus.
    Plus,
    /// Arrow bending back.
    Reply,
    /// Arrow bending forward.
    Forward,
    /// Bin.
    Trash,
    /// Pennant on a pole.
    Flag,
    /// Clip.
    Paperclip,
    /// Paper dart.
    Send,
    /// Cross.
    Close,
    /// Folder with an arrow (move to).
    Move,
    /// Tray (inbox).
    Inbox,
    /// Pencil (drafts, edit).
    Pencil,
    /// Box with lid (archive).
    Archive,
    /// Circle with a bar (junk, blocked).
    Blocked,
    /// Sheet with a folded corner (file).
    File,
    /// Clock.
    Clock,
    /// Right chevron.
    ChevronRight,
    /// Command key style loop (palette).
    Command,
    /// Three dots (more / overflow).
    More,
}

impl IconName {
    /// Path data, each string one `<path d=...>`.
    pub const fn paths(self) -> &'static [&'static str] {
        match self {
            Self::Mail => &["M3.5 6.5h17v11h-17z", "M3.5 7l8.5 6.5 8.5-6.5"],
            Self::Calendar => &["M4 6h16v14H4z", "M4 10.5h16", "M8.5 3.5v4", "M15.5 3.5v4"],
            Self::Chat => &["M4 5h16v11H10l-4.5 3.5V16H4z"],
            Self::Teams => &[
                "M9 11.5a3 3 0 1 0 0-6 3 3 0 0 0 0 6z",
                "M3.5 19.5c0-3 2.5-5 5.5-5s5.5 2 5.5 5",
                "M16 11a2.5 2.5 0 1 0 0-5",
                "M17 14c2.2.4 3.5 2.2 3.5 5",
            ],
            Self::Folder => &["M3.5 6h6l2 2.5h9v10h-17z"],
            Self::People => {
                &["M12 11.5a3.75 3.75 0 1 0 0-7.5 3.75 3.75 0 0 0 0 7.5z", "M4.5 20c0-4 3.4-6 7.5-6s7.5 2 7.5 6"]
            }
            Self::Search => &["M10.5 17a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13z", "M15.5 15.5l5 5"],
            Self::Settings => &[
                "M4 7.5h8.5",
                "M17.5 7.5H20",
                "M4 16.5h2.5",
                "M11.5 16.5H20",
                "M15 10a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5z",
                "M9 19a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5z",
            ],
            Self::Plus => &["M12 5v14", "M5 12h14"],
            Self::Reply => &["M9.5 7.5L5 12l4.5 4.5", "M5 12h8.5a5.5 5.5 0 0 1 5.5 5.5V19"],
            Self::Forward => &["M14.5 7.5L19 12l-4.5 4.5", "M19 12h-8.5A5.5 5.5 0 0 0 5 17.5V19"],
            Self::Trash => &["M4.5 7h15", "M9.5 7V4.5h5V7", "M6.5 7l1 13h9l1-13", "M10.5 11v5.5", "M13.5 11v5.5"],
            Self::Flag => &["M6 21V4", "M6 4.5h11l-2.5 4 2.5 4H6"],
            Self::Paperclip => &["M8.5 12.5l6-6a3 3 0 0 1 4.2 4.2l-7.6 7.6a4.8 4.8 0 0 1-6.8-6.8l6.9-6.9"],
            Self::Send => &["M3.5 11.5l17-7.5-7.5 17-2-7.5z", "M11 13.5l9.5-9.5"],
            Self::Close => &["M6.5 6.5l11 11", "M17.5 6.5l-11 11"],
            Self::Move => &["M3.5 6h6l2 2.5h9v10h-17z", "M9 13.5h7", "M13.5 11l2.5 2.5-2.5 2.5"],
            Self::Inbox => &["M4 5h16v14H4z", "M4 13.5h4.5l1.5 2.5h4l1.5-2.5H20"],
            Self::Pencil => &["M5 19l1-4.5L15.5 5l3.5 3.5L9.5 18z", "M13.5 7l3.5 3.5"],
            Self::Archive => &["M3.5 5h17v4h-17z", "M5 9v10h14V9", "M10 13h4"],
            Self::Blocked => &["M12 20.5a8.5 8.5 0 1 0 0-17 8.5 8.5 0 0 0 0 17z", "M6 18L18 6"],
            Self::File => &["M6 3.5h8l4 4v13H6z", "M14 3.5v4h4"],
            Self::Clock => &["M12 20.5a8.5 8.5 0 1 0 0-17 8.5 8.5 0 0 0 0 17z", "M12 7.5V12l3 2"],
            Self::ChevronRight => &["M10 6.5l5.5 5.5-5.5 5.5"],
            Self::Command => &["M5 5h14v14H5z", "M9 12h6", "M12 9v6"],
            Self::More => &[
                "M6 13a1 1 0 1 0 0-2 1 1 0 0 0 0 2z",
                "M12 13a1 1 0 1 0 0-2 1 1 0 0 0 0 2z",
                "M18 13a1 1 0 1 0 0-2 1 1 0 0 0 0 2z",
            ],
        }
    }

    /// Every icon, for tests and the settings gallery.
    pub const ALL: &'static [IconName] = &[
        Self::Mail,
        Self::Calendar,
        Self::Chat,
        Self::Teams,
        Self::Folder,
        Self::People,
        Self::Search,
        Self::Settings,
        Self::Plus,
        Self::Reply,
        Self::Forward,
        Self::Trash,
        Self::Flag,
        Self::Paperclip,
        Self::Send,
        Self::Close,
        Self::Move,
        Self::Inbox,
        Self::Pencil,
        Self::Archive,
        Self::Blocked,
        Self::File,
        Self::Clock,
        Self::ChevronRight,
        Self::Command,
        Self::More,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_paths_on_the_24_grid() {
        for icon in IconName::ALL {
            let paths = icon.paths();
            assert!(!paths.is_empty(), "{icon:?}");
            for d in paths {
                assert!(d.starts_with('M'), "{icon:?}: {d}");
                // Absolute coordinates stay inside the 24x24 box.
                for n in d.split(|c: char| !(c.is_ascii_digit() || c == '.')).filter(|s| !s.is_empty()) {
                    let v: f64 = n.parse().unwrap_or(0.0);
                    assert!(v <= 24.0, "{icon:?}: {v}");
                }
            }
        }
    }
}
