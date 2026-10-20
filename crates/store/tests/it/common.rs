//! Synthetic fixtures shared by the integration tests (no tenant data).

use jiff::Timestamp;
use quorum_model::{Chat, ChatMessage, DriveItem, Event, MailFolder, Message, Person};
use quorum_store::{FixedKeySource, Store};
use serde_json::json;

pub fn t(s: &str) -> Timestamp {
    s.parse().unwrap()
}

pub fn folder(id: &str, name: &str) -> MailFolder {
    serde_json::from_value(json!({"id": id, "displayName": name})).unwrap()
}

pub fn message(id: &str, folder: &str, received: &str, subject: &str) -> Message {
    serde_json::from_value(json!({
        "id": id,
        "parentFolderId": folder,
        "conversationId": format!("conv-{}", id.split('-').next().unwrap_or(id)),
        "subject": subject,
        "from": {"emailAddress": {"name": "Ada Example", "address": "ada@example.com"}},
        "receivedDateTime": received,
        "bodyPreview": format!("preview of {subject}"),
        "isRead": false,
        "inferenceClassification": "focused"
    }))
    .unwrap()
}

pub fn event(id: &str, cal: &str, start: &str, end: &str, tz: &str, all_day: bool) -> Event {
    serde_json::from_value(json!({
        "id": id,
        "calendarId": cal,
        "subject": format!("event {id}"),
        "start": {"dateTime": start, "timeZone": tz},
        "end": {"dateTime": end, "timeZone": tz},
        "isAllDay": all_day,
        "type": "singleInstance"
    }))
    .unwrap()
}

pub fn chat(id: &str, last: &str, read: Option<&str>) -> Chat {
    serde_json::from_value(json!({
        "id": id,
        "chatType": "group",
        "topic": format!("topic {id}"),
        "members": [{"userId": "u1", "displayName": "Ada"}, {"userId": "u2", "displayName": "Bob"}],
        "lastMessagePreview": {"id": "p1", "createdDateTime": last, "bodyPreview": "hi"},
        "lastMessageReadDateTime": read
    }))
    .unwrap()
}

pub fn chat_message(chat: &str, id: &str, created: &str, text: &str) -> ChatMessage {
    serde_json::from_value(json!({
        "id": id,
        "container": {"chat": chat},
        "createdDateTime": created,
        "from": {"kind": "user", "id": "u1", "displayName": "Ada Example"},
        "body": {"contentType": "html", "content": format!("<p>{text}</p>")}
    }))
    .unwrap()
}

pub fn channel_message(
    team: &str,
    channel: &str,
    id: &str,
    reply_to: Option<&str>,
    created: &str,
    text: &str,
) -> ChatMessage {
    serde_json::from_value(json!({
        "id": id,
        "container": {"channel": {"teamId": team, "channelId": channel}},
        "replyToId": reply_to,
        "createdDateTime": created,
        "subject": if reply_to.is_none() { Some(format!("post {id}")) } else { None },
        "from": {"kind": "user", "id": "u2", "displayName": "Bob Example"},
        "body": {"contentType": "text", "content": text}
    }))
    .unwrap()
}

pub fn drive_item(drive: &str, id: &str, parent: Option<&str>, name: &str, folder: bool, modified: &str) -> DriveItem {
    let mut v = json!({
        "id": id,
        "driveId": drive,
        "parentId": parent,
        "name": name,
        "size": 10,
        "lastModifiedDateTime": modified
    });
    if folder {
        v["folder"] = json!({"childCount": 0});
    } else {
        v["file"] = json!({"mimeType": "text/plain"});
    }
    serde_json::from_value(v).unwrap()
}

pub fn person(id: &str, name: &str, email: &str) -> Person {
    serde_json::from_value(json!({"id": id, "displayName": name, "email": email})).unwrap()
}

pub async fn open(dir: &std::path::Path) -> Store {
    Store::open(dir, FixedKeySource::test_key()).await.unwrap()
}
