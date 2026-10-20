//! Calendar settings kept in the store: visible sources and colours, opened people, the
//! mailbox time zone and working hours, and the view the user last looked at.

use std::collections::{BTreeMap, BTreeSet};

use jiff::civil::Date;
use serde::{Deserialize, Serialize};

/// Setting: [`SourcePrefs`].
pub const SOURCES_KEY: &str = "calendar.sources";
/// Setting: the opened people (`Vec<OpenedPerson>`).
pub const PEOPLE_KEY: &str = "calendar.people";
/// Setting: the mailbox time zone name (Windows or IANA), from `mailboxSettings`.
pub const TIME_ZONE_KEY: &str = "calendar.time_zone";
/// Setting: [`WorkingHours`], from `mailboxSettings`.
pub const WORK_HOURS_KEY: &str = "calendar.working_hours";
/// Setting: the signed-in user's address (decides own vs shared calendars).
pub const ME_KEY: &str = "calendar.me";
/// Setting: calendar id → calendar group name.
pub const GROUPS_KEY: &str = "calendar.groups";
/// UI preference: [`ViewState`].
pub const VIEW_PREF: &str = "calendar.view";
/// Prefix of optimistic event ids (created locally, not yet on the server).
pub const LOCAL_ID_PREFIX: &str = "local-";
/// Prefix of free/busy-only source ids (`person:<address>`).
pub const PERSON_SOURCE_PREFIX: &str = "person:";
/// The most people a user may open at once.
pub const MAX_OPENED_PEOPLE: usize = 50;

/// The setting that maps an optimistic id to the server id once the create replayed.
#[must_use]
pub fn local_map_key(local_id: &str) -> String {
    format!("calendar.local:{local_id}")
}

/// Which layers are hidden and any colour overrides, by source id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePrefs {
    /// Hidden source ids.
    #[serde(default)]
    pub hidden: BTreeSet<String>,
    /// `#rrggbb` overrides by source id.
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
}

/// How much of an opened person's calendar the user may see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonMode {
    /// Not checked yet.
    #[default]
    Pending,
    /// Their calendar is shared with the user: events are synced.
    Details,
    /// Not shared: free/busy blocks only (`getSchedule`).
    FreeBusy,
}

/// A person whose calendar the user opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenedPerson {
    /// Their address (or object id).
    pub address: String,
    /// Display name, if known.
    #[serde(default)]
    pub name: String,
    /// Access level found by sync.
    #[serde(default)]
    pub mode: PersonMode,
}

impl OpenedPerson {
    /// The free/busy source id of this person.
    #[must_use]
    pub fn source_id(&self) -> String {
        format!("{PERSON_SOURCE_PREFIX}{}", self.address.to_ascii_lowercase())
    }
}

/// Working hours in minutes after local midnight, and working days (`0` = Monday).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingHours {
    /// Start, minutes after midnight.
    pub start_min: u16,
    /// End, minutes after midnight.
    pub end_min: u16,
    /// Working days, `0` = Monday … `6` = Sunday.
    #[serde(default)]
    pub days: Vec<u8>,
}

impl Default for WorkingHours {
    fn default() -> Self {
        Self { start_min: 8 * 60, end_min: 17 * 60, days: vec![0, 1, 2, 3, 4] }
    }
}

impl WorkingHours {
    /// Parses Graph's `HH:mm:ss.fffffff` times; `None` if either is malformed or empty.
    #[must_use]
    pub fn from_graph(start: &str, end: &str, days: &[quorum_model::DayOfWeek]) -> Option<Self> {
        let start_min = parse_hhmm(start)?;
        let end_min = parse_hhmm(end)?;
        if end_min <= start_min {
            return None;
        }
        use quorum_model::DayOfWeek as D;
        let days = days
            .iter()
            .filter_map(|d| match d {
                D::Monday => Some(0),
                D::Tuesday => Some(1),
                D::Wednesday => Some(2),
                D::Thursday => Some(3),
                D::Friday => Some(4),
                D::Saturday => Some(5),
                D::Sunday => Some(6),
                _ => None,
            })
            .collect();
        Some(Self { start_min, end_min, days })
    }
}

/// Parses `HH:mm[:ss[.f]]` to minutes after midnight (at most 24:00).
#[must_use]
pub fn parse_hhmm(s: &str) -> Option<u16> {
    let mut parts = s.trim().split(':');
    let h: u16 = parts.next()?.parse().ok()?;
    let m: u16 = parts.next()?.get(..2)?.parse().ok()?;
    if h > 24 || m > 59 || (h == 24 && m > 0) {
        return None;
    }
    h.checked_mul(60)?.checked_add(m)
}

/// The calendar view kinds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewKind {
    /// One day.
    Day,
    /// Monday to Friday.
    WorkWeek,
    /// Seven days.
    #[default]
    Week,
    /// A month grid (six weeks).
    Month,
    /// A list of the coming two weeks.
    Agenda,
}

impl ViewKind {
    /// Every view, in switcher order.
    pub const ALL: [Self; 5] = [Self::Day, Self::WorkWeek, Self::Week, Self::Month, Self::Agenda];

    /// Parses the wire name (`day`, `work_week`, `week`, `month`, `agenda`; `workweek` too).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "day" => Some(Self::Day),
            "work_week" | "workweek" => Some(Self::WorkWeek),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "agenda" | "list" => Some(Self::Agenda),
            _ => None,
        }
    }

    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::WorkWeek => "work_week",
            Self::Week => "week",
            Self::Month => "month",
            Self::Agenda => "agenda",
        }
    }

    /// Display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Day => "Day",
            Self::WorkWeek => "Work week",
            Self::Week => "Week",
            Self::Month => "Month",
            Self::Agenda => "Agenda",
        }
    }
}

/// What the calendar shows: the view kind and the date it is anchored on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewState {
    /// The view kind.
    #[serde(default)]
    pub view: ViewKind,
    /// The anchor date (`None`: today).
    #[serde(default)]
    pub date: Option<Date>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self { view: ViewKind::Week, date: None }
    }
}

/// Our own palette for calendar layers (not Microsoft artwork: plain colours).
pub const PALETTE: [&str; 10] =
    ["#3b82f6", "#10b981", "#f59e0b", "#8b5cf6", "#ef4444", "#14b8a6", "#ec4899", "#a16207", "#6366f1", "#64748b"];

/// The colour for a calendar's Graph colour theme (or palette slot `index` for `auto`).
#[must_use]
pub fn color_for(color: &quorum_model::CalendarColor, index: usize) -> &'static str {
    use quorum_model::CalendarColor as C;
    match color {
        C::LightBlue => "#4f9ded",
        C::LightGreen => "#4caf7a",
        C::LightOrange => "#f0954a",
        C::LightGray => "#8a94a3",
        C::LightYellow => "#d9b42c",
        C::LightTeal => "#2bb3a8",
        C::LightPink => "#e46aa6",
        C::LightBrown => "#a7794f",
        C::LightRed => "#e05a5a",
        _ => PALETTE.get(index % PALETTE.len()).copied().unwrap_or("#3b82f6"),
    }
}

/// Accepts `#rgb` / `#rrggbb` (any case); anything else is rejected.
#[must_use]
pub fn valid_hex(s: &str) -> Option<String> {
    let hex = s.trim().strip_prefix('#')?;
    if (hex.len() == 3 || hex.len() == 6) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(format!("#{}", hex.to_ascii_lowercase()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_working_hours_and_views() {
        assert_eq!(parse_hhmm("08:00:00.0000000"), Some(480));
        assert_eq!(parse_hhmm("17:30"), Some(1050));
        assert_eq!(parse_hhmm("25:00"), None);
        assert_eq!(parse_hhmm("x"), None);
        assert_eq!(parse_hhmm(""), None);
        assert_eq!(ViewKind::parse("Work-Week"), Some(ViewKind::WorkWeek));
        assert_eq!(ViewKind::parse("year"), None);
        for v in ViewKind::ALL {
            assert_eq!(ViewKind::parse(v.as_str()), Some(v));
        }
        assert_eq!(valid_hex("#ABCDEF").as_deref(), Some("#abcdef"));
        assert_eq!(valid_hex("red"), None);
        assert_eq!(valid_hex("#12345g"), None);
    }
}
