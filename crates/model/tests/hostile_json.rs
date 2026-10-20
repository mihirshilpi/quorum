//! Random and mutated Graph JSON never panics the model types.

use quorum_model::{
    Attachment, Calendar, Channel, Chat, ChatMessage, DateTimeZone, Drive, DriveItem, EmailAddress, Event, ListItem,
    MailFolder, MeetingTimeSuggestion, Message, MessageBody, Person, Presence, ScheduleInformation, Site, Team,
};
use serde_json::{Map, Value};

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }
}

const KEYS: &[&str] = &[
    "id",
    "subject",
    "from",
    "sender",
    "toRecipients",
    "ccRecipients",
    "name",
    "address",
    "emailAddress",
    "receivedDateTime",
    "sentDateTime",
    "createdDateTime",
    "start",
    "end",
    "dateTime",
    "timeZone",
    "importance",
    "flagStatus",
    "isRead",
    "chatType",
    "members",
    "lastMessagePreview",
    "container",
    "chat",
    "channel",
    "teamId",
    "channelId",
    "body",
    "content",
    "contentType",
    "mentions",
    "reactions",
    "availability",
    "activity",
    "statusMessage",
    "expiry",
    "driveId",
    "driveType",
    "file",
    "folder",
    "childCount",
    "size",
    "fields",
    "attendees",
    "status",
    "response",
    "type",
    "recurrence",
    "pattern",
    "interval",
    "daysOfWeek",
    "showAs",
    "meetingTimeSlot",
    "confidence",
    "scheduleItems",
    "availabilityView",
    "kind",
    "displayName",
    "unreadItemCount",
    "wellKnownName",
    "location",
    "locations",
];

const STRINGS: &[&str] = &[
    "",
    "x",
    "inbox",
    "high",
    "Busy",
    "oneOnOne",
    "html",
    "2026-01-01T00:00:00Z",
    "2026-01-01T00:00:00.0000000",
    "Pacific Standard Time",
    "not a date",
    "9999-99-99",
    "a@b.c",
    "\u{0}",
    "\u{7}bell",
    "😀",
    "-1",
    "1e400",
    "Tokyo Standard Time",
    "UTC",
    "user",
    "application",
    "file",
    "teamsForBusiness",
];

fn random_value(rng: &mut Rng, depth: usize) -> Value {
    let choice = if depth > 5 { rng.below(5) } else { rng.below(8) };
    match choice {
        0 => Value::Null,
        1 => Value::Bool(rng.below(2) == 0),
        2 => match rng.below(4) {
            0 => Value::from(rng.next_u64()),
            1 => Value::from(-(rng.below(1000) as i64)),
            2 => serde_json::Number::from_f64((rng.next_u64() % 1000) as f64 / 7.0).map_or(Value::Null, Value::Number),
            _ => Value::from(u32::MAX as u64 + 1),
        },
        3 | 4 => {
            let s = STRINGS[rng.below(STRINGS.len())];
            if rng.below(10) == 0 { Value::from(s.repeat(rng.below(3000))) } else { Value::from(s) }
        }
        5 => {
            let n = rng.below(4);
            Value::Array((0..n).map(|_| random_value(rng, depth + 1)).collect())
        }
        _ => {
            let n = rng.below(10);
            let mut map = Map::new();
            for _ in 0..n {
                let key = KEYS[rng.below(KEYS.len())].to_owned();
                map.insert(key, random_value(rng, depth + 1));
            }
            Value::Object(map)
        }
    }
}

fn try_all(json: &str) {
    let _ = serde_json::from_str::<Message>(json);
    let _ = serde_json::from_str::<MessageBody>(json);
    let _ = serde_json::from_str::<MailFolder>(json);
    let _ = serde_json::from_str::<Attachment>(json);
    let _ = serde_json::from_str::<EmailAddress>(json);
    let _ = serde_json::from_str::<Calendar>(json);
    let _ = serde_json::from_str::<Event>(json);
    let _ = serde_json::from_str::<DateTimeZone>(json).map(|d| d.to_timestamp());
    let _ = serde_json::from_str::<ScheduleInformation>(json).map(|s| s.availability());
    let _ = serde_json::from_str::<MeetingTimeSuggestion>(json).map(|m| m.confidence_percent());
    let _ = serde_json::from_str::<Chat>(json).map(|c| c.has_unread());
    let _ = serde_json::from_str::<ChatMessage>(json);
    let _ = serde_json::from_str::<Team>(json);
    let _ = serde_json::from_str::<Channel>(json);
    let _ = serde_json::from_str::<Presence>(json);
    let _ = serde_json::from_str::<Drive>(json);
    let _ = serde_json::from_str::<DriveItem>(json);
    let _ = serde_json::from_str::<Site>(json);
    let _ = serde_json::from_str::<ListItem>(json).map(|i| format!("{i:?}"));
    let _ = serde_json::from_str::<Person>(json);
}

#[test]
fn hostile_graph_json_never_panics() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..20_000 {
        let value = random_value(&mut rng, 0);
        let json = value.to_string();
        try_all(&json);
        // Also feed objects with a valid id, so parsing gets past the required fields.
        if let Value::Object(mut map) = value {
            map.insert("id".to_owned(), Value::from("x1"));
            map.insert("driveId".to_owned(), Value::from("d1"));
            map.insert("chatType".to_owned(), Value::from("group"));
            map.insert("container".to_owned(), serde_json::json!({"chat": "c1"}));
            try_all(&Value::Object(map).to_string());
        }
    }
}

#[test]
fn mutated_json_bytes_never_panic() {
    let seed = serde_json::json!({
        "id": "m1", "subject": "Hi", "from": {"name": "A", "address": "a@b.test"},
        "start": {"dateTime": "2026-01-01T09:00:00", "timeZone": "W. Europe Standard Time"},
        "end": {"dateTime": "2026-01-01T10:00:00", "timeZone": "UTC"},
        "chatType": "group", "container": {"chat": "c1"}, "driveId": "d1",
        "attendees": [{"emailAddress": {"address": "x@y.test"}, "type": "optional"}]
    })
    .to_string();
    let bytes = seed.as_bytes();
    let mut rng = Rng(12345);
    for _ in 0..20_000 {
        let mut mutated = bytes.to_vec();
        for _ in 0..1 + rng.below(4) {
            let i = rng.below(mutated.len());
            match rng.below(3) {
                0 => mutated[i] = (rng.next_u64() & 0x7f) as u8,
                1 => {
                    mutated.remove(i);
                }
                _ => mutated.insert(i, b"{}[]\",:0\\"[rng.below(9)]),
            }
        }
        if let Ok(text) = std::str::from_utf8(&mutated) {
            try_all(text);
        }
    }
}

#[test]
fn deeply_nested_json_is_an_error_not_a_crash() {
    let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    try_all(&deep);
    let deep_obj = format!("{}1{}", "{\"from\":".repeat(10_000), "}".repeat(10_000));
    try_all(&deep_obj);
}
