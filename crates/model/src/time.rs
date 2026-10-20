//! Graph's `{dateTime, timeZone}` pairs and Windows time zone names.

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::de::truncate_on_char_boundary;
use crate::error::{Error, ErrorKind};

/// The longest time zone name kept; longer hostile values are truncated.
const MAX_TZ_NAME_LEN: usize = 128;

/// A time zone name as Graph returns it: a Windows name (`"Pacific Standard Time"`), an IANA
/// name (`"Europe/Berlin"`) or something else.
///
/// The raw string is always kept, even when it cannot be mapped, so it round-trips to Graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TimeZoneName(String);

impl TimeZoneName {
    /// Wraps a time zone name (truncated to 128 bytes).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        let mut name = name.into();
        truncate_on_char_boundary(&mut name, MAX_TZ_NAME_LEN);
        Self(name)
    }

    /// The UTC zone, as Graph spells it.
    #[must_use]
    pub fn utc() -> Self {
        Self("UTC".to_owned())
    }

    /// Returns the raw name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the IANA name: mapped from Windows, or the raw name if it already looks like an
    /// IANA name (`Area/Location`, `UTC`, `Etc/...`). Returns `None` for unknown names.
    #[must_use]
    pub fn iana(&self) -> Option<&str> {
        let raw = self.0.trim();
        if let Some(mapped) = windows_to_iana(raw) {
            return Some(mapped);
        }
        let looks_iana = raw.contains('/')
            && !raw.contains(char::is_whitespace)
            && !raw.starts_with('/')
            && !raw.starts_with("tzone:");
        if looks_iana || raw == "GMT" { Some(raw) } else { None }
    }
}

impl Serialize for TimeZoneName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TimeZoneName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::new(Option::<String>::deserialize(d)?.unwrap_or_default()))
    }
}

/// A local date and time in a named time zone, as in Graph's `dateTimeTimeZone`.
///
/// Graph returns event times in the zone requested by the `Prefer: outlook.timezone` header (or
/// the event's own zone). Use [`DateTimeZone::to_timestamp`] to get the instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DateTimeZone {
    /// The wall-clock date and time (Graph sends up to 7 fractional digits).
    pub date_time: DateTime,
    /// The zone the wall-clock time is in.
    pub time_zone: TimeZoneName,
}

impl DateTimeZone {
    /// Creates a value from a civil date-time and zone name.
    #[must_use]
    pub fn new(date_time: DateTime, time_zone: TimeZoneName) -> Self {
        Self { date_time, time_zone }
    }

    /// Creates a UTC value from an instant.
    #[must_use]
    pub fn from_timestamp_utc(ts: Timestamp) -> Self {
        Self { date_time: ts.to_zoned(TimeZone::UTC).datetime(), time_zone: TimeZoneName::utc() }
    }

    /// Resolves the wall-clock time in its zone to an instant.
    ///
    /// This looks the zone up in the time zone database (the only I/O in this crate). Ambiguous
    /// or skipped local times (DST transitions) resolve with jiff's "compatible" strategy.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::UnknownTimeZone`] if the zone cannot be mapped or found, and
    /// [`ErrorKind::InvalidDateTime`] if the time cannot be resolved.
    pub fn to_timestamp(&self) -> Result<Timestamp, Error> {
        let tz = self.time_zone()?;
        tz.to_timestamp(self.date_time).map_err(|_| Error::new(ErrorKind::InvalidDateTime, "DateTimeZone"))
    }

    /// Returns the resolved [`TimeZone`].
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::UnknownTimeZone`] if the zone cannot be mapped or found.
    pub fn time_zone(&self) -> Result<TimeZone, Error> {
        let unknown = || Error::new(ErrorKind::UnknownTimeZone, "DateTimeZone");
        let raw = self.time_zone.as_str().trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("UTC") || raw.eq_ignore_ascii_case("tzone://Microsoft/Utc") {
            return Ok(TimeZone::UTC);
        }
        let iana = self.time_zone.iana().ok_or_else(unknown)?;
        TimeZone::get(iana).map_err(|_| unknown())
    }
}

/// Windows time zone ids (as used by Exchange and Graph) to IANA names, from the CLDR
/// `windowsZones` "001" territory mapping.
const WINDOWS_TO_IANA: &[(&str, &str)] = &[
    ("Dateline Standard Time", "Etc/GMT+12"),
    ("UTC-11", "Etc/GMT+11"),
    ("Aleutian Standard Time", "America/Adak"),
    ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("Marquesas Standard Time", "Pacific/Marquesas"),
    ("Alaskan Standard Time", "America/Anchorage"),
    ("UTC-09", "Etc/GMT+9"),
    ("Pacific Standard Time (Mexico)", "America/Tijuana"),
    ("UTC-08", "Etc/GMT+8"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("US Mountain Standard Time", "America/Phoenix"),
    ("Mountain Standard Time (Mexico)", "America/Mazatlan"),
    ("Mountain Standard Time", "America/Denver"),
    ("Yukon Standard Time", "America/Whitehorse"),
    ("Central America Standard Time", "America/Guatemala"),
    ("Central Standard Time", "America/Chicago"),
    ("Easter Island Standard Time", "Pacific/Easter"),
    ("Central Standard Time (Mexico)", "America/Mexico_City"),
    ("Canada Central Standard Time", "America/Regina"),
    ("SA Pacific Standard Time", "America/Bogota"),
    ("Eastern Standard Time (Mexico)", "America/Cancun"),
    ("Eastern Standard Time", "America/New_York"),
    ("Haiti Standard Time", "America/Port-au-Prince"),
    ("Cuba Standard Time", "America/Havana"),
    ("US Eastern Standard Time", "America/Indiana/Indianapolis"),
    ("Turks And Caicos Standard Time", "America/Grand_Turk"),
    ("Paraguay Standard Time", "America/Asuncion"),
    ("Atlantic Standard Time", "America/Halifax"),
    ("Venezuela Standard Time", "America/Caracas"),
    ("Central Brazilian Standard Time", "America/Cuiaba"),
    ("SA Western Standard Time", "America/La_Paz"),
    ("Pacific SA Standard Time", "America/Santiago"),
    ("Newfoundland Standard Time", "America/St_Johns"),
    ("Tocantins Standard Time", "America/Araguaina"),
    ("E. South America Standard Time", "America/Sao_Paulo"),
    ("SA Eastern Standard Time", "America/Cayenne"),
    ("Argentina Standard Time", "America/Argentina/Buenos_Aires"),
    ("Greenland Standard Time", "America/Nuuk"),
    ("Montevideo Standard Time", "America/Montevideo"),
    ("Magallanes Standard Time", "America/Punta_Arenas"),
    ("Saint Pierre Standard Time", "America/Miquelon"),
    ("Bahia Standard Time", "America/Bahia"),
    ("UTC-02", "Etc/GMT+2"),
    ("Azores Standard Time", "Atlantic/Azores"),
    ("Cape Verde Standard Time", "Atlantic/Cape_Verde"),
    ("UTC", "Etc/UTC"),
    ("Coordinated Universal Time", "Etc/UTC"),
    ("GMT Standard Time", "Europe/London"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("Sao Tome Standard Time", "Africa/Sao_Tome"),
    ("Morocco Standard Time", "Africa/Casablanca"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Romance Standard Time", "Europe/Paris"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("W. Central Africa Standard Time", "Africa/Lagos"),
    ("Jordan Standard Time", "Asia/Amman"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("Middle East Standard Time", "Asia/Beirut"),
    ("Egypt Standard Time", "Africa/Cairo"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("Syria Standard Time", "Asia/Damascus"),
    ("West Bank Standard Time", "Asia/Hebron"),
    ("South Africa Standard Time", "Africa/Johannesburg"),
    ("FLE Standard Time", "Europe/Kyiv"),
    ("Israel Standard Time", "Asia/Jerusalem"),
    ("South Sudan Standard Time", "Africa/Juba"),
    ("Kaliningrad Standard Time", "Europe/Kaliningrad"),
    ("Sudan Standard Time", "Africa/Khartoum"),
    ("Libya Standard Time", "Africa/Tripoli"),
    ("Namibia Standard Time", "Africa/Windhoek"),
    ("Arabic Standard Time", "Asia/Baghdad"),
    ("Turkey Standard Time", "Europe/Istanbul"),
    ("Arab Standard Time", "Asia/Riyadh"),
    ("Belarus Standard Time", "Europe/Minsk"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("E. Africa Standard Time", "Africa/Nairobi"),
    ("Volgograd Standard Time", "Europe/Volgograd"),
    ("Iran Standard Time", "Asia/Tehran"),
    ("Arabian Standard Time", "Asia/Dubai"),
    ("Astrakhan Standard Time", "Europe/Astrakhan"),
    ("Azerbaijan Standard Time", "Asia/Baku"),
    ("Russia Time Zone 3", "Europe/Samara"),
    ("Mauritius Standard Time", "Indian/Mauritius"),
    ("Saratov Standard Time", "Europe/Saratov"),
    ("Georgian Standard Time", "Asia/Tbilisi"),
    ("Caucasus Standard Time", "Asia/Yerevan"),
    ("Afghanistan Standard Time", "Asia/Kabul"),
    ("West Asia Standard Time", "Asia/Tashkent"),
    ("Ekaterinburg Standard Time", "Asia/Yekaterinburg"),
    ("Pakistan Standard Time", "Asia/Karachi"),
    ("Qyzylorda Standard Time", "Asia/Qyzylorda"),
    ("India Standard Time", "Asia/Kolkata"),
    ("Sri Lanka Standard Time", "Asia/Colombo"),
    ("Nepal Standard Time", "Asia/Kathmandu"),
    ("Central Asia Standard Time", "Asia/Bishkek"),
    ("Bangladesh Standard Time", "Asia/Dhaka"),
    ("Omsk Standard Time", "Asia/Omsk"),
    ("Myanmar Standard Time", "Asia/Yangon"),
    ("SE Asia Standard Time", "Asia/Bangkok"),
    ("Altai Standard Time", "Asia/Barnaul"),
    ("W. Mongolia Standard Time", "Asia/Hovd"),
    ("North Asia Standard Time", "Asia/Krasnoyarsk"),
    ("N. Central Asia Standard Time", "Asia/Novosibirsk"),
    ("Tomsk Standard Time", "Asia/Tomsk"),
    ("China Standard Time", "Asia/Shanghai"),
    ("North Asia East Standard Time", "Asia/Irkutsk"),
    ("Singapore Standard Time", "Asia/Singapore"),
    ("W. Australia Standard Time", "Australia/Perth"),
    ("Taipei Standard Time", "Asia/Taipei"),
    ("Ulaanbaatar Standard Time", "Asia/Ulaanbaatar"),
    ("Aus Central W. Standard Time", "Australia/Eucla"),
    ("Transbaikal Standard Time", "Asia/Chita"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("North Korea Standard Time", "Asia/Pyongyang"),
    ("Korea Standard Time", "Asia/Seoul"),
    ("Yakutsk Standard Time", "Asia/Yakutsk"),
    ("Cen. Australia Standard Time", "Australia/Adelaide"),
    ("AUS Central Standard Time", "Australia/Darwin"),
    ("E. Australia Standard Time", "Australia/Brisbane"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("West Pacific Standard Time", "Pacific/Port_Moresby"),
    ("Tasmania Standard Time", "Australia/Hobart"),
    ("Vladivostok Standard Time", "Asia/Vladivostok"),
    ("Lord Howe Standard Time", "Australia/Lord_Howe"),
    ("Bougainville Standard Time", "Pacific/Bougainville"),
    ("Russia Time Zone 10", "Asia/Srednekolymsk"),
    ("Magadan Standard Time", "Asia/Magadan"),
    ("Norfolk Standard Time", "Pacific/Norfolk"),
    ("Sakhalin Standard Time", "Asia/Sakhalin"),
    ("Central Pacific Standard Time", "Pacific/Guadalcanal"),
    ("Russia Time Zone 11", "Asia/Kamchatka"),
    ("New Zealand Standard Time", "Pacific/Auckland"),
    ("UTC+12", "Etc/GMT-12"),
    ("Fiji Standard Time", "Pacific/Fiji"),
    ("Chatham Islands Standard Time", "Pacific/Chatham"),
    ("UTC+13", "Etc/GMT-13"),
    ("Tonga Standard Time", "Pacific/Tongatapu"),
    ("Samoa Standard Time", "Pacific/Apia"),
    ("Line Islands Standard Time", "Pacific/Kiritimati"),
];

/// Maps a Windows time zone id (e.g. `"W. Europe Standard Time"`) to its IANA name
/// (`"Europe/Berlin"`).
///
/// Matching ignores ASCII case and surrounding whitespace. Returns `None` for names not in the
/// table; callers should then keep the raw string (see [`TimeZoneName`]).
///
/// ```
/// assert_eq!(quorum_model::windows_to_iana("Pacific Standard Time"), Some("America/Los_Angeles"));
/// assert_eq!(quorum_model::windows_to_iana("Mars Standard Time"), None);
/// ```
#[must_use]
pub fn windows_to_iana(windows_name: &str) -> Option<&'static str> {
    let name = windows_name.trim();
    WINDOWS_TO_IANA.iter().find(|(win, _)| win.eq_ignore_ascii_case(name)).map(|(_, iana)| *iana)
}
