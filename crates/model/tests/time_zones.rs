//! Windows time zone names map to IANA names that jiff can resolve.

use quorum_model::{DateTimeZone, ErrorKind, TimeZoneName, windows_to_iana};

#[test]
fn common_windows_zones_map() {
    for (win, iana) in [
        ("Pacific Standard Time", "America/Los_Angeles"),
        ("Eastern Standard Time", "America/New_York"),
        ("Central Standard Time", "America/Chicago"),
        ("Mountain Standard Time", "America/Denver"),
        ("GMT Standard Time", "Europe/London"),
        ("W. Europe Standard Time", "Europe/Berlin"),
        ("Romance Standard Time", "Europe/Paris"),
        ("India Standard Time", "Asia/Kolkata"),
        ("China Standard Time", "Asia/Shanghai"),
        ("Tokyo Standard Time", "Asia/Tokyo"),
        ("AUS Eastern Standard Time", "Australia/Sydney"),
        ("New Zealand Standard Time", "Pacific/Auckland"),
        ("E. South America Standard Time", "America/Sao_Paulo"),
        ("UTC", "Etc/UTC"),
        ("  pacific standard time ", "America/Los_Angeles"),
    ] {
        assert_eq!(windows_to_iana(win), Some(iana), "{win}");
    }
    assert_eq!(windows_to_iana("Martian Standard Time"), None);
}

#[test]
fn every_mapped_zone_resolves_in_jiff() {
    // Spot-check that the table's IANA names exist in the tz database.
    for win in [
        "Dateline Standard Time",
        "Hawaiian Standard Time",
        "Alaskan Standard Time",
        "US Mountain Standard Time",
        "Atlantic Standard Time",
        "Newfoundland Standard Time",
        "Argentina Standard Time",
        "Azores Standard Time",
        "Central Europe Standard Time",
        "GTB Standard Time",
        "Egypt Standard Time",
        "South Africa Standard Time",
        "FLE Standard Time",
        "Israel Standard Time",
        "Russian Standard Time",
        "Arabian Standard Time",
        "Iran Standard Time",
        "Pakistan Standard Time",
        "Nepal Standard Time",
        "SE Asia Standard Time",
        "Singapore Standard Time",
        "Korea Standard Time",
        "Cen. Australia Standard Time",
        "Line Islands Standard Time",
    ] {
        let iana = windows_to_iana(win).unwrap();
        assert!(jiff::tz::TimeZone::get(iana).is_ok(), "{win} -> {iana} not in tzdb");
    }
}

#[test]
fn time_zone_name_keeps_raw_and_detects_iana() {
    assert_eq!(TimeZoneName::new("Europe/Berlin").iana(), Some("Europe/Berlin"));
    assert_eq!(TimeZoneName::new("Tokyo Standard Time").iana(), Some("Asia/Tokyo"));
    let unknown = TimeZoneName::new("Customized Time Zone");
    assert_eq!(unknown.iana(), None);
    assert_eq!(unknown.as_str(), "Customized Time Zone");
    assert!(TimeZoneName::new("x".repeat(1000)).as_str().len() <= 128);
}

#[test]
fn unknown_zone_is_an_error_not_a_panic() {
    let dtz: DateTimeZone = serde_json::from_value(serde_json::json!({
        "dateTime": "2026-01-01T00:00:00", "timeZone": "Customized Time Zone"
    }))
    .unwrap();
    assert_eq!(dtz.to_timestamp().unwrap_err().kind(), ErrorKind::UnknownTimeZone);
    let bogus: DateTimeZone = serde_json::from_value(serde_json::json!({
        "dateTime": "2026-01-01T00:00:00", "timeZone": "Not/AZone"
    }))
    .unwrap();
    assert!(bogus.to_timestamp().is_err());
}

#[test]
fn dst_gap_resolves() {
    // 02:30 does not exist on 2026-03-08 in New York; jiff's compatible strategy moves forward.
    let dtz: DateTimeZone = serde_json::from_value(serde_json::json!({
        "dateTime": "2026-03-08T02:30:00.0000000", "timeZone": "Eastern Standard Time"
    }))
    .unwrap();
    assert!(dtz.to_timestamp().is_ok());
}
