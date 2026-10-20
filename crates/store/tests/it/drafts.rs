use quorum_store::{DeltaState, Draft, DraftKind, ErrorKind, Store};

#[tokio::test]
async fn drafts_save_load_delete() {
    let store = Store::open_in_memory().unwrap();
    let mut d = Draft::new("draft-1", DraftKind::Chat);
    d.context = Some("chat:c1".into());
    d.body = "half a thought".into();
    let first = store.save_draft(d.clone()).await.unwrap();
    // Typing continues: saving again replaces.
    d.body = "half a thought, finished".into();
    let second = store.save_draft(d.clone()).await.unwrap();
    assert!(second >= first);
    let got = store.draft("draft-1".into()).await.unwrap().unwrap();
    assert_eq!(got.body.as_str(), "half a thought, finished");
    assert_eq!(got.updated_at, ms_precision(second));
    let by_ctx = store.read(|r| r.draft_for_context(DraftKind::Chat, "chat:c1")).await.unwrap().unwrap();
    assert_eq!(by_ctx.id, "draft-1");
    store.save_draft(Draft::new("draft-2", DraftKind::Mail)).await.unwrap();
    assert_eq!(store.drafts(None, 10).await.unwrap().items.len(), 2);
    assert_eq!(store.drafts(Some(DraftKind::Mail), 10).await.unwrap().items.len(), 1);
    assert!(store.delete_draft("draft-1".into()).await.unwrap());
    assert!(!store.delete_draft("draft-1".into()).await.unwrap());
    assert!(store.draft("draft-1".into()).await.unwrap().is_none());
    let err = store.save_draft(Draft::new("", DraftKind::Mail)).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

/// Stored instants have millisecond precision.
fn ms_precision(ts: jiff::Timestamp) -> jiff::Timestamp {
    jiff::Timestamp::from_millisecond(ts.as_millisecond()).unwrap()
}

#[tokio::test]
async fn many_quick_saves_are_cheap() {
    let store = Store::open_in_memory().unwrap();
    let start = std::time::Instant::now();
    for i in 0..200 {
        let mut d = Draft::new("typing", DraftKind::Mail);
        d.body = "x".repeat(i).into();
        store.save_draft(d).await.unwrap();
    }
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(store.draft("typing".into()).await.unwrap().unwrap().body.len(), 199);
}

#[tokio::test]
async fn delta_state_settings_and_prefs() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.delta_state("mail:inbox".into()).await.unwrap().is_none());
    let mut s = DeltaState::new("mail:inbox");
    s.next_link = Some("https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages/delta?$skiptoken=abc".into());
    s.backfill_cursor = Some("2026-01-01T00:00:00Z".into());
    store.set_delta_state(s.clone()).await.unwrap();
    let got = store.delta_state("mail:inbox".into()).await.unwrap().unwrap();
    assert_eq!(got.next_link, s.next_link);
    assert_eq!(got.backfill_cursor, s.backfill_cursor);
    assert!(!got.backfill_complete);
    assert!(got.updated_at > jiff::Timestamp::UNIX_EPOCH);
    store.write(|tx| tx.clear_delta_state("mail:inbox")).await.unwrap();
    assert!(store.delta_state("mail:inbox".into()).await.unwrap().is_none());

    store.set_setting("mail.retention_days".into(), 90u32).await.unwrap();
    assert_eq!(store.setting::<u32>("mail.retention_days".into()).await.unwrap(), Some(90));
    assert_eq!(store.setting::<u32>("missing".into()).await.unwrap(), None);
    assert_eq!(store.setting::<String>("mail.retention_days".into()).await.unwrap_err().kind(), ErrorKind::Corrupt);
    let err = store.set_setting("store.key_check".into(), 1).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "reserved prefix");
    store.set_ui_pref("sidebar.width".into(), serde_json::json!({"px": 240})).await.unwrap();
    let v: serde_json::Value = store.ui_pref("sidebar.width".into()).await.unwrap().unwrap();
    assert_eq!(v["px"], 240);
}
