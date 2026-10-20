//! The layers of the merged calendar: own calendars (by calendar group), shared calendars,
//! Microsoft 365 group calendars and opened people, each with a colour and a visibility toggle.

use std::collections::BTreeMap;

use quorum_model::{Calendar, CalendarSource};
use serde::Serialize;

use super::prefs::{OpenedPerson, PersonMode, SourcePrefs, color_for, valid_hex};

/// One toggleable, colour-coded layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceInfo {
    /// Calendar id, or `person:<address>` for a free/busy-only person.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `own`, `shared`, `group` or `person`.
    pub kind: &'static str,
    /// Heading in the sources list (calendar group, "Shared calendars", "Groups", "People").
    pub group: String,
    /// `#rrggbb`.
    pub color: String,
    /// Whether the layer is shown.
    pub visible: bool,
    /// Whether the user can add and edit events.
    pub can_edit: bool,
    /// The user's default calendar.
    pub is_default: bool,
    /// Owner name or address.
    pub owner: String,
    /// The opened person's address (people layers).
    pub person: Option<String>,
    /// `pending`, `details` or `free_busy` (people layers).
    pub mode: Option<PersonMode>,
    /// The layer shows free/busy blocks, not events.
    pub free_busy_only: bool,
}

fn kind_of(source: &CalendarSource) -> &'static str {
    match source {
        CalendarSource::Shared => "shared",
        CalendarSource::Group => "group",
        CalendarSource::OpenedUser => "person",
        _ => "own",
    }
}

fn rank(kind: &str) -> u8 {
    match kind {
        "own" => 0,
        "shared" => 1,
        "group" => 2,
        _ => 3,
    }
}

/// Builds every layer, ordered own (default first), shared, groups, people.
#[must_use]
pub fn build_sources(
    calendars: &[Calendar],
    groups: &BTreeMap<String, String>,
    prefs: &SourcePrefs,
    people: &[OpenedPerson],
) -> Vec<SourceInfo> {
    let mut out: Vec<SourceInfo> = Vec::with_capacity(calendars.len().saturating_add(people.len()));
    for (i, c) in calendars.iter().enumerate() {
        let id = c.id.as_str().to_owned();
        let kind = kind_of(&c.source);
        let owner_addr = c.owner.as_ref().and_then(|o| o.address.as_ref()).map(|a| a.as_str().to_ascii_lowercase());
        let owner = c
            .owner
            .as_ref()
            .map(
                |o| if o.name.is_empty() { owner_addr.clone().unwrap_or_default() } else { o.name.as_str().to_owned() },
            )
            .unwrap_or_default();
        let person = people.iter().find(|p| Some(p.address.to_ascii_lowercase()) == owner_addr);
        let group = match kind {
            "own" => groups.get(&id).cloned().unwrap_or_else(|| "My calendars".to_owned()),
            "shared" => "Shared calendars".to_owned(),
            "group" => "Groups".to_owned(),
            _ => "People".to_owned(),
        };
        let theme = c.hex_color.as_deref().and_then(valid_hex);
        let color = prefs.colors.get(&id).cloned().or(theme).unwrap_or_else(|| color_for(&c.color, i).to_owned());
        let name = match (kind, person) {
            ("person", Some(p)) if !p.name.is_empty() && c.is_default_calendar => p.name.clone(),
            _ if c.name.is_empty() => "Calendar".to_owned(),
            _ => c.name.as_str().to_owned(),
        };
        out.push(SourceInfo {
            visible: !prefs.hidden.contains(&id),
            id,
            name,
            kind,
            group,
            color,
            can_edit: c.can_edit,
            is_default: c.is_default_calendar && kind == "own",
            owner,
            person: if kind == "person" { owner_addr } else { None },
            mode: if kind == "person" { Some(PersonMode::Details) } else { None },
            free_busy_only: false,
        });
    }
    let base = out.len();
    for (i, p) in people.iter().enumerate() {
        let has_calendars = out.iter().any(|s| s.person.as_deref() == Some(p.address.to_ascii_lowercase().as_str()));
        if p.mode == PersonMode::Details && has_calendars {
            continue;
        }
        let id = p.source_id();
        let color =
            prefs.colors.get(&id).cloned().unwrap_or_else(|| color_for(&Default::default(), base + i).to_owned());
        out.push(SourceInfo {
            visible: !prefs.hidden.contains(&id),
            name: if p.name.is_empty() { p.address.clone() } else { p.name.clone() },
            id,
            kind: "person",
            group: "People".to_owned(),
            color,
            can_edit: false,
            is_default: false,
            owner: p.address.clone(),
            person: Some(p.address.to_ascii_lowercase()),
            mode: Some(p.mode),
            free_busy_only: true,
        });
    }
    out.sort_by(|a, b| {
        rank(a.kind).cmp(&rank(b.kind)).then(b.is_default.cmp(&a.is_default)).then(a.group.cmp(&b.group))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use quorum_model::{CalendarColor, CalendarId, EmailAddress};

    fn cal(id: &str, source: CalendarSource, default: bool, owner: &str) -> Calendar {
        Calendar {
            id: CalendarId::new(id).unwrap(),
            name: id.into(),
            color: CalendarColor::Auto,
            hex_color: None,
            owner: EmailAddress::new("Owner", owner).ok(),
            can_edit: true,
            can_share: true,
            is_default_calendar: default,
            source,
        }
    }

    #[test]
    fn orders_layers_and_applies_prefs() {
        let cals = vec![
            cal("shared1", CalendarSource::Shared, false, "a@example.com"),
            cal("own2", CalendarSource::Own, false, "me@example.com"),
            cal("own1", CalendarSource::Own, true, "me@example.com"),
            cal("p1", CalendarSource::OpenedUser, true, "bo@example.com"),
        ];
        let mut prefs = SourcePrefs::default();
        prefs.hidden.insert("own2".into());
        prefs.colors.insert("own1".into(), "#123456".into());
        let people = vec![
            OpenedPerson { address: "bo@example.com".into(), name: "Bo".into(), mode: PersonMode::Details },
            OpenedPerson { address: "cy@example.com".into(), name: "Cy".into(), mode: PersonMode::FreeBusy },
        ];
        let s = build_sources(&cals, &BTreeMap::new(), &prefs, &people);
        let ids: Vec<&str> = s.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["own1", "own2", "shared1", "p1", "person:cy@example.com"]);
        assert_eq!(s.first().map(|s| s.color.as_str()), Some("#123456"));
        assert!(s.iter().any(|s| s.id == "own2" && !s.visible));
        assert!(s.iter().any(|s| s.id == "p1" && s.name == "Bo"));
        assert!(s.iter().any(|s| s.id.starts_with("person:") && s.free_busy_only));
    }
}
