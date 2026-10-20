//! Person avatar: photo when given, else initials on a colour picked from a hash of the name,
//! with an optional presence dot. Colours are CSS tokens (`--avatar-0..7`, `--presence-*`).

use leptos::prelude::*;

/// Number of avatar colour tokens (`--avatar-0` .. `--avatar-7`).
pub const AVATAR_COLOURS: u32 = 8;

/// Teams presence, as Graph reports `availability`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Presence {
    /// Available.
    Available,
    /// Busy (or in a meeting / call).
    Busy,
    /// Do not disturb (or presenting).
    DoNotDisturb,
    /// Be right back.
    BeRightBack,
    /// Away.
    Away,
    /// Offline.
    Offline,
    /// Unknown or not loaded.
    #[default]
    Unknown,
}

impl Presence {
    /// Parses Graph `availability` / our snake_case form; unknown values are `Unknown`.
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().replace('_', "").as_str() {
            "available" | "availableidle" => Self::Available,
            "busy" | "busyidle" | "inameeting" | "inacall" => Self::Busy,
            "donotdisturb" | "dnd" | "presenting" => Self::DoNotDisturb,
            "berightback" | "brb" => Self::BeRightBack,
            "away" => Self::Away,
            "offline" | "presenceunknown" => Self::Offline,
            _ => Self::Unknown,
        }
    }

    /// CSS modifier (`presence-<id>`).
    pub const fn id(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Busy => "busy",
            Self::DoNotDisturb => "dnd",
            Self::BeRightBack => "brb",
            Self::Away => "away",
            Self::Offline => "offline",
            Self::Unknown => "unknown",
        }
    }

    /// Spoken label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::Busy => "Busy",
            Self::DoNotDisturb => "Do not disturb",
            Self::BeRightBack => "Be right back",
            Self::Away => "Away",
            Self::Offline => "Offline",
            Self::Unknown => "Presence unknown",
        }
    }
}

/// Up to two initials: first letters of the first and last words; `?` when there are none.
pub fn initials(name: &str) -> String {
    let name = name.split('@').next().unwrap_or(name);
    let words: Vec<&str> =
        name.split(|c: char| c.is_whitespace() || c == '@' || c == '.').filter(|w| !w.is_empty()).collect();
    let first_letter =
        |w: &str| w.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().next().unwrap_or(c).to_string());
    let out: String = match (words.first(), words.get(1).and(words.last())) {
        (Some(a), Some(b)) => [first_letter(a), first_letter(b)].into_iter().flatten().collect(),
        (Some(a), None) => first_letter(a).unwrap_or_default(),
        _ => String::new(),
    };
    if out.is_empty() { "?".into() } else { out }
}

/// Stable colour index for `name` (FNV-1a), in `0..AVATAR_COLOURS`.
pub fn colour_index(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.trim().to_lowercase().bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h % AVATAR_COLOURS
}

/// A round avatar.
#[component]
pub fn Avatar(
    /// Display name (used for initials, colour and the accessible label).
    #[prop(into)]
    name: String,
    /// Photo URL (`asset:` / `data:` only, per the CSP), if loaded.
    #[prop(optional, into)]
    photo: Option<String>,
    /// Presence dot; `None` hides it.
    #[prop(optional, into)]
    presence: Option<Signal<Presence>>,
    /// Size modifier: `sm`, `md` (default) or `lg`.
    #[prop(optional)]
    size: Option<&'static str>,
) -> impl IntoView {
    let colour = colour_index(&name);
    let text = initials(&name);
    let label_name = name.clone();
    let label = move || match presence {
        // `try_get`: a presence signal owned by a parent can be disposed just before this
        // avatar is (e.g. while switching areas); show Unknown rather than panic.
        Some(p) => format!("{label_name}, {}", p.try_get().unwrap_or(Presence::Unknown).label()),
        None => label_name.clone(),
    };
    view! {
        <span
            class=format!("avatar avatar-{} avatar-c{colour}", size.unwrap_or("md"))
            role="img"
            aria-label=label
        >
            {match photo {
                Some(src) => view! { <img class="avatar-photo" src=src alt="" /> }.into_any(),
                None => view! { <span class="avatar-initials" aria-hidden="true">{text}</span> }.into_any(),
            }}
            {presence
                .map(|p| {
                    view! { <span class=move || format!("presence-dot presence-{}", p.try_get().unwrap_or(Presence::Unknown).id()) aria-hidden="true"></span> }
                })}
        </span>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_cover_edge_cases() {
        assert_eq!(initials("Avery Lin"), "AL");
        assert_eq!(initials("Avery Quinn Lin"), "AL");
        assert_eq!(initials("avery"), "A");
        assert_eq!(initials("avery.lin@example.com"), "AL");
        assert_eq!(initials(""), "?");
        assert_eq!(initials("   "), "?");
        assert_eq!(initials("(( ))"), "?");
        assert_eq!(initials("élodie ßeta"), "ÉS");
    }

    #[test]
    fn colour_is_stable_and_bounded() {
        assert_eq!(colour_index("Avery Lin"), colour_index("avery lin "));
        for n in ["", "a", "Blake Ito", "😀"] {
            assert!(colour_index(n) < AVATAR_COLOURS);
        }
    }

    #[test]
    fn presence_parses_graph_values() {
        assert_eq!(Presence::parse("Available"), Presence::Available);
        assert_eq!(Presence::parse("DoNotDisturb"), Presence::DoNotDisturb);
        assert_eq!(Presence::parse("be_right_back"), Presence::BeRightBack);
        assert_eq!(Presence::parse("InAMeeting"), Presence::Busy);
        assert_eq!(Presence::parse("SomethingNew"), Presence::Unknown);
    }
}
