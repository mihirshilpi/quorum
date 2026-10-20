//! `Debug` of types carrying personal content never shows that content (M-PUBLIC-DEBUG).

use quorum_model::{
    Chat, ChatMessage, EmailAddress, Event, ListItem, MailAddress, Message, MessageBody, Person, Presence, Sensitive,
};

const SECRET_SUBJECT: &str = "Confidential merger plans";
const SECRET_NAME: &str = "Zebulon Quartermaine";
const SECRET_ADDRESS: &str = "zebulon.q@contoso-secret.test";
const SECRET_BODY: &str = "the password is hunter2";

fn assert_no_leak(debug: &str) {
    for secret in
        [SECRET_SUBJECT, SECRET_NAME, SECRET_ADDRESS, SECRET_BODY, "Zebulon", "contoso-secret", "hunter2", "merger"]
    {
        assert!(!debug.contains(secret), "leaked {secret:?} in {debug}");
    }
}

#[test]
fn sensitive_and_addresses_redact() {
    assert_no_leak(&format!("{:?}", Sensitive::new(SECRET_BODY)));
    assert_no_leak(&format!("{:?}", MailAddress::new(SECRET_ADDRESS).unwrap()));
    assert_no_leak(&format!("{:#?}", EmailAddress::new(SECRET_NAME, SECRET_ADDRESS).unwrap()));
    assert_eq!(format!("{:?}", Sensitive::new("abc")), "Sensitive(3 bytes)");
}

#[test]
fn message_debug_redacts() {
    let json = serde_json::json!({
        "id": "m1",
        "subject": SECRET_SUBJECT,
        "bodyPreview": SECRET_BODY,
        "from": {"name": SECRET_NAME, "address": SECRET_ADDRESS},
        "toRecipients": [{"name": SECRET_NAME, "address": SECRET_ADDRESS}],
        "categories": ["Blue"],
    });
    let message: Message = serde_json::from_value(json).unwrap();
    assert_eq!(message.subject(), SECRET_SUBJECT);
    assert_no_leak(&format!("{message:?}"));
    assert_no_leak(&format!("{message:#?}"));
    let body: MessageBody =
        serde_json::from_value(serde_json::json!({"contentType": "html", "content": SECRET_BODY})).unwrap();
    assert_no_leak(&format!("{body:?}"));
}

#[test]
fn event_chat_presence_person_debug_redact() {
    let event: Event = serde_json::from_value(serde_json::json!({
        "id": "e1",
        "subject": SECRET_SUBJECT,
        "bodyPreview": SECRET_BODY,
        "start": {"dateTime": "2026-01-05T09:00:00.0000000", "timeZone": "UTC"},
        "end": {"dateTime": "2026-01-05T10:00:00.0000000", "timeZone": "UTC"},
        "organizer": {"name": SECRET_NAME, "address": SECRET_ADDRESS},
        "attendees": [{"emailAddress": {"name": SECRET_NAME, "address": SECRET_ADDRESS}, "type": "required"}],
        "location": {"displayName": SECRET_NAME},
    }))
    .unwrap();
    assert_no_leak(&format!("{event:?}"));

    let chat: Chat = serde_json::from_value(serde_json::json!({
        "id": "19:abc@thread.v2",
        "chatType": "group",
        "topic": SECRET_SUBJECT,
        "members": [{"displayName": SECRET_NAME, "email": SECRET_ADDRESS}],
        "lastMessagePreview": {"id": "1", "bodyPreview": SECRET_BODY, "from": {"displayName": SECRET_NAME}},
    }))
    .unwrap();
    assert_no_leak(&format!("{chat:?}"));

    let msg: ChatMessage = serde_json::from_value(serde_json::json!({
        "id": "1",
        "container": {"chat": "19:abc@thread.v2"},
        "from": {"kind": "user", "id": "u1", "displayName": SECRET_NAME},
        "body": {"contentType": "html", "content": SECRET_BODY},
        "mentions": [{"id": 0, "mentionText": SECRET_NAME}],
    }))
    .unwrap();
    assert_no_leak(&format!("{msg:?}"));

    let presence: Presence = serde_json::from_value(serde_json::json!({
        "availability": "Busy", "activity": "InAMeeting", "statusMessage": SECRET_BODY,
    }))
    .unwrap();
    assert_no_leak(&format!("{presence:?}"));

    let person: Person = serde_json::from_value(serde_json::json!({
        "displayName": SECRET_NAME, "email": SECRET_ADDRESS, "jobTitle": SECRET_SUBJECT,
    }))
    .unwrap();
    assert_no_leak(&format!("{person:?}"));

    let item: ListItem = serde_json::from_value(serde_json::json!({
        "id": "1", "fields": {"Title": SECRET_SUBJECT, "Owner": SECRET_NAME},
    }))
    .unwrap();
    assert_eq!(item.field_str("Title"), Some(SECRET_SUBJECT));
    assert_no_leak(&format!("{item:?}"));
}
