//! Graph-shaped JSON parses into the domain types; unknown enum values never fail.

use jiff::Timestamp;
use quorum_model::{
    Activity, AttachmentKind, Availability, Calendar, CalendarSource, ChannelMembershipType, Chat, ChatMessage,
    ChatType, DateTimeZone, DriveItem, Event, FlagStatus, FreeBusyStatus, GraphEnum, Importance,
    InferenceClassification, MailFolder, MeetingTimeSuggestion, Message, MessageContainer, Presence, ResponseType,
    ScheduleInformation, WellKnownFolder,
};

#[test]
fn message_parses_and_round_trips() {
    let json = serde_json::json!({
        "id": "AAMkAD=",
        "conversationId": "AAQkAD=",
        "parentFolderId": "AQMkAD=",
        "subject": "Lunch?",
        "from": {"name": "Alex", "address": "alex@example.test"},
        "sender": {"name": "Alex", "address": "/o=ExchangeLabs/ou=Exchange Administrative Group/cn=x"},
        "toRecipients": [{"name": "Bo", "address": "bo@example.test"}],
        "ccRecipients": null,
        "receivedDateTime": "2026-03-01T10:15:00Z",
        "sentDateTime": "2026-03-01T10:14:59.1234567Z",
        "bodyPreview": "Shall we?",
        "isRead": false,
        "isDraft": null,
        "importance": "HIGH",
        "flagStatus": "flagged",
        "categories": ["Red category"],
        "hasAttachments": true,
        "inferenceClassification": "other",
        "webLink": "https://outlook.office.test/x",
        "someFutureField": {"nested": [1, 2, 3]}
    });
    let m: Message = serde_json::from_value(json).unwrap();
    assert_eq!(m.importance, Importance::High);
    assert!(m.is_flagged());
    assert_eq!(m.inference_classification, InferenceClassification::Other);
    assert!(m.cc_recipients.is_empty());
    assert!(m.sender.as_ref().unwrap().address.is_none(), "X.500 sender has no SMTP address");
    assert_eq!(m.from.as_ref().unwrap().display(), "Alex");
    assert_eq!(m.recipients().count(), 1);
    assert_eq!(m.received_date_time, Some("2026-03-01T10:15:00Z".parse::<Timestamp>().unwrap()));
    let back: Message = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
    assert_eq!(back, m);
}

#[test]
fn unknown_enum_values_are_kept() {
    assert_eq!(Importance::from_graph("superUrgent"), Importance::Unknown("superUrgent".to_owned()));
    assert_eq!(Importance::Unknown("x".into()).as_graph(), "x");
    let a: Availability = serde_json::from_str("\"Focusing\"").unwrap();
    assert_eq!(a, Availability::Unknown("Focusing".to_owned()));
    assert_eq!(serde_json::to_string(&a).unwrap(), "\"Focusing\"");
    let long = "x".repeat(10_000);
    match FlagStatus::from_graph(&long) {
        FlagStatus::Unknown(raw) => assert!(raw.len() <= 256),
        other => panic!("{other:?}"),
    }
    assert!(serde_json::from_str::<Importance>("5").is_err());
    let m: Message = serde_json::from_value(serde_json::json!({
        "id": "m", "importance": "weird", "flagStatus": "", "inferenceClassification": "x"
    }))
    .unwrap();
    assert_eq!(m.importance, Importance::Unknown("weird".to_owned()));
}

#[test]
fn folder_and_attachment_kinds() {
    let f: MailFolder = serde_json::from_value(serde_json::json!({
        "id": "f1", "displayName": "Inbox", "wellKnownName": "inbox",
        "unreadItemCount": 3, "totalItemCount": 10, "childFolderCount": null
    }))
    .unwrap();
    assert_eq!(f.well_known_name, Some(WellKnownFolder::Inbox));
    assert_eq!(f.unread_item_count, 3);
    assert_eq!(AttachmentKind::from_odata_type("#microsoft.graph.fileAttachment"), AttachmentKind::File);
    assert_eq!(AttachmentKind::from_odata_type("#microsoft.graph.referenceAttachment"), AttachmentKind::Reference);
}

#[test]
fn event_parses_with_windows_zone() {
    let e: Event = serde_json::from_value(serde_json::json!({
        "id": "e1",
        "subject": "Standup",
        "start": {"dateTime": "2026-07-01T09:00:00.0000000", "timeZone": "Pacific Standard Time"},
        "end": {"dateTime": "2026-07-01T09:15:00.0000000", "timeZone": "Pacific Standard Time"},
        "showAs": "workingElsewhere",
        "type": "occurrence",
        "seriesMasterId": "master1",
        "isOnlineMeeting": true,
        "onlineMeetingProvider": "teamsForBusiness",
        "onlineMeetingJoinUrl": "https://teams.example.test/join",
        "responseStatus": {"response": "tentativelyAccepted", "time": "0001-01-01T00:00:00Z"},
        "attendees": [
            {"emailAddress": {"name": "Room 1", "address": "room1@example.test"}, "type": "resource",
             "status": {"response": "accepted"}}
        ],
        "recurrence": null
    }))
    .unwrap();
    assert_eq!(e.show_as, FreeBusyStatus::WorkingElsewhere);
    assert!(e.is_recurring());
    assert_eq!(e.response_status.response, ResponseType::TentativelyAccepted);
    // 09:00 PDT (UTC-7) is 16:00 UTC.
    assert_eq!(e.start.to_timestamp().unwrap(), "2026-07-01T16:00:00Z".parse::<Timestamp>().unwrap());
}

#[test]
fn schedule_and_suggestions_parse() {
    let s: ScheduleInformation = serde_json::from_value(serde_json::json!({
        "scheduleId": "someone@example.test",
        "availabilityView": "0123x",
        "scheduleItems": [{
            "status": "busy", "isPrivate": false,
            "start": {"dateTime": "2026-07-01T09:00:00", "timeZone": "UTC"},
            "end": {"dateTime": "2026-07-01T10:00:00", "timeZone": "UTC"}
        }]
    }))
    .unwrap();
    assert_eq!(
        s.availability(),
        vec![
            FreeBusyStatus::Free,
            FreeBusyStatus::Tentative,
            FreeBusyStatus::Busy,
            FreeBusyStatus::Oof,
            FreeBusyStatus::Unknown("x".to_owned())
        ]
    );
    let m: MeetingTimeSuggestion = serde_json::from_value(serde_json::json!({
        "confidence": 250.0,
        "organizerAvailability": "free",
        "meetingTimeSlot": {
            "start": {"dateTime": "2026-07-01T09:00:00", "timeZone": "UTC"},
            "end": {"dateTime": "2026-07-01T10:00:00", "timeZone": "UTC"}
        }
    }))
    .unwrap();
    assert!((m.confidence_percent() - 100.0).abs() < f64::EPSILON);
}

#[test]
fn calendar_teams_presence_files_parse() {
    let c: Calendar = serde_json::from_value(serde_json::json!({
        "id": "c1", "name": "Team", "color": "lightTeal", "canEdit": true, "source": "openedUser"
    }))
    .unwrap();
    assert_eq!(c.source, CalendarSource::OpenedUser);

    let chat: Chat = serde_json::from_value(serde_json::json!({
        "id": "19:x@thread.v2", "chatType": "oneOnOne",
        "lastMessagePreview": {"id": "2", "createdDateTime": "2026-01-02T00:00:00Z"},
        "lastMessageReadDateTime": "2026-01-01T00:00:00Z"
    }))
    .unwrap();
    assert_eq!(chat.chat_type, ChatType::OneOnOne);
    assert!(chat.has_unread());

    let msg: ChatMessage = serde_json::from_value(serde_json::json!({
        "id": "3", "replyToId": "1",
        "container": {"channel": {"teamId": "t1", "channelId": "19:c@thread.tacv2"}},
        "deletedDateTime": "2026-01-02T00:00:00Z",
        "importance": "urgent",
        "reactions": [{"reactionType": "like"}]
    }))
    .unwrap();
    assert!(msg.is_deleted());
    assert!(matches!(msg.container, MessageContainer::Channel { .. }));

    let ch: quorum_model::Channel = serde_json::from_value(serde_json::json!({
        "id": "19:c@thread.tacv2", "displayName": "General", "membershipType": "shared"
    }))
    .unwrap();
    assert_eq!(ch.membership_type, ChannelMembershipType::Shared);

    let p: Presence = serde_json::from_value(serde_json::json!({
        "availability": "DoNotDisturb", "activity": "Presenting",
        "statusMessage": "Back at 3", "expiry": "2026-01-01T00:00:00Z"
    }))
    .unwrap();
    assert_eq!(p.activity, Activity::Presenting);
    let before: Timestamp = "2025-12-31T00:00:00Z".parse().unwrap();
    let after: Timestamp = "2026-01-02T00:00:00Z".parse().unwrap();
    assert_eq!(p.current_status_message(before), Some("Back at 3"));
    assert_eq!(p.current_status_message(after), None);

    let item: DriveItem = serde_json::from_value(serde_json::json!({
        "id": "i1", "driveId": "d1", "name": "Plan.docx", "size": 1234,
        "file": {"mimeType": "application/vnd.openxmlformats-officedocument.wordprocessingml.document"},
        "eTag": "\"{1},1\"", "cTag": "\"c:{1},1\"", "shared": true
    }))
    .unwrap();
    assert!(!item.is_folder());
    assert!(item.mime_type().unwrap().contains("wordprocessingml"));
}

#[test]
fn utc_date_time_zone_round_trips() {
    let ts: Timestamp = "2026-05-05T12:34:56Z".parse().unwrap();
    let dtz = DateTimeZone::from_timestamp_utc(ts);
    assert_eq!(dtz.to_timestamp().unwrap(), ts);
    let json = serde_json::to_value(&dtz).unwrap();
    assert_eq!(json["timeZone"], "UTC");
    let back: DateTimeZone = serde_json::from_value(json).unwrap();
    assert_eq!(back, dtz);
}
