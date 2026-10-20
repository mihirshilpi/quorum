//! Id newtypes validate on construction and on deserialisation.

use quorum_model::{ChatId, ErrorKind, MAX_ID_LEN, MailAddress, MessageId, UserId};

#[test]
fn valid_ids_round_trip() {
    let raw = "AAMkAGI2TG93AAA=";
    let id = MessageId::new(raw).unwrap();
    assert_eq!(id.as_str(), raw);
    assert_eq!(id.to_string(), raw);
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, format!("\"{raw}\""));
    assert_eq!(serde_json::from_str::<MessageId>(&json).unwrap(), id);
    assert_eq!(raw.parse::<MessageId>().unwrap(), id);
}

#[test]
fn invalid_ids_are_rejected() {
    assert_eq!(MessageId::new("").unwrap_err().kind(), ErrorKind::Empty);
    assert_eq!(ChatId::new("x".repeat(MAX_ID_LEN + 1)).unwrap_err().kind(), ErrorKind::TooLong { max: MAX_ID_LEN });
    assert!(ChatId::new("x".repeat(MAX_ID_LEN)).is_ok());
    assert_eq!(UserId::new("abc\ndef").unwrap_err().kind(), ErrorKind::ControlCharacter);
    assert_eq!(UserId::new("a\u{0}").unwrap_err().kind(), ErrorKind::ControlCharacter);
}

#[test]
fn invalid_ids_fail_deserialisation() {
    assert!(serde_json::from_str::<MessageId>("\"\"").is_err());
    assert!(serde_json::from_str::<MessageId>("\"a\\u0007b\"").is_err());
    assert!(serde_json::from_str::<MessageId>("42").is_err());
    assert!(serde_json::from_str::<MessageId>("null").is_err());
}

#[test]
fn errors_name_the_type_and_not_the_value() {
    let err = UserId::new("secret\u{1}value").unwrap_err();
    assert_eq!(err.type_name(), "UserId");
    let shown = format!("{err} {err:?}");
    assert!(!shown.contains("secret"), "{shown}");
}

#[test]
fn mail_addresses_are_validated() {
    let a = MailAddress::new("  someone@example.com ").unwrap();
    assert_eq!(a.as_str(), "someone@example.com");
    assert_eq!(a.local_part(), "someone");
    assert_eq!(a.domain(), "example.com");
    assert!(a.eq_ignore_case(&MailAddress::new("SomeOne@Example.com").unwrap()));
    for bad in [
        "",
        "   ",
        "no-at-sign",
        "@example.com",
        "someone@",
        "some one@example.com",
        "a@b@",
        "a@.example.com",
        "<a@example.com>",
        "a@exa\u{7}mple.com",
        "/o=ExchangeLabs/ou=Exchange Administrative Group/cn=Recipients/cn=abc",
    ] {
        assert!(MailAddress::new(bad).is_err(), "{bad:?} accepted");
    }
    assert!(MailAddress::new(format!("{}@example.com", "a".repeat(400))).is_err());
}
