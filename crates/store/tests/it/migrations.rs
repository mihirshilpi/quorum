use quorum_store::testing::{create_at_version, execute_file};
use quorum_store::{ErrorKind, FixedKeySource, SCHEMA_VERSION, Store};

use crate::common::{message, open};

const TABLES: &[&str] = &[
    "setting",
    "ui_prefs",
    "delta_state",
    "outbox",
    "draft",
    "blob",
    "mail_folder",
    "message",
    "message_body",
    "attachment",
    "calendar",
    "event",
    "chat",
    "chat_member",
    "chat_message",
    "team",
    "channel",
    "channel_message",
    "drive",
    "drive_item",
    "site",
    "list",
    "list_item",
    "person",
    "presence",
    "mail_fts",
    "chat_fts",
    "channel_fts",
    "drive_fts",
    "person_fts",
];

async fn assert_full_schema(store: &Store) {
    assert_eq!(store.schema_version().await.unwrap(), SCHEMA_VERSION);
    let names = quorum_store::testing::query_strings(store, "SELECT name FROM sqlite_master WHERE type = 'table'")
        .await
        .unwrap();
    for t in TABLES {
        assert!(names.iter().any(|n| n == t), "missing table {t}");
    }
    let ok = quorum_store::testing::query_strings(store, "PRAGMA integrity_check").await.unwrap();
    assert_eq!(ok, vec!["ok".to_owned()]);
}

#[tokio::test]
async fn migrates_from_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).await;
    assert_full_schema(&store).await;
    drop(store);
    // Re-opening is a no-op.
    let store = open(dir.path()).await;
    assert_full_schema(&store).await;
}

#[tokio::test]
async fn in_memory_is_fully_migrated() {
    assert_full_schema(&Store::open_in_memory().unwrap()).await;
}

#[tokio::test]
async fn migrates_from_every_intermediate_version() {
    for v in 0..=SCHEMA_VERSION {
        let dir = tempfile::tempdir().unwrap();
        create_at_version(dir.path(), v).unwrap();
        if v >= 1 {
            execute_file(dir.path(), "INSERT INTO setting (key, value, updated_at) VALUES ('kept', '1', 0)").unwrap();
        }
        if v >= 2 {
            execute_file(
                dir.path(),
                "INSERT INTO message (id, folder_id, received_at, is_read, flag, is_focused, has_attachments,
                    categories, json)
                 VALUES ('old', 'f', 0, 0, 'notFlagged', 1, 0, '[]', '{\"id\":\"old\"}')",
            )
            .unwrap();
        }
        let store = open(dir.path()).await;
        assert_full_schema(&store).await;
        if v >= 1 {
            assert_eq!(store.setting::<i64>("kept".into()).await.unwrap(), Some(1), "data lost from v{v}");
        }
        if v >= 2 {
            let m = store.message(quorum_model::MessageId::new("old").unwrap()).await.unwrap();
            assert!(m.is_some(), "message lost from v{v}");
        }
        // The migrated store is fully usable, including FTS.
        let m = message("m1", "f", "2026-01-01T00:00:00Z", "migrated subject");
        store.apply_message_delta(vec![m], vec![]).await.unwrap();
        let hits = store.search("migrated".into(), vec![quorum_store::SearchScope::Mail], 10).await.unwrap();
        assert_eq!(hits.hits.len(), 1, "search broken after migrating from v{v}");
    }
}

#[tokio::test]
async fn refuses_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    drop(open(dir.path()).await);
    execute_file(dir.path(), &format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1)).unwrap();
    let err = Store::open(dir.path(), FixedKeySource::test_key()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Migration);
}

#[tokio::test]
async fn wrong_key_is_rejected_at_open() {
    let dir = tempfile::tempdir().unwrap();
    drop(open(dir.path()).await);
    let other = FixedKeySource::new(quorum_store::StoreKey::from_bytes([9; 32]));
    let err = Store::open(dir.path(), other).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Crypto);
}

#[tokio::test]
async fn failing_key_source_is_a_crypto_error() {
    struct Locked;
    impl quorum_store::KeySource for Locked {
        fn load_or_create(&self) -> Result<quorum_store::StoreKey, Box<dyn std::error::Error + Send + Sync + 'static>> {
            Err("keychain locked".into())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let err = Store::open(dir.path(), Locked).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Crypto);
}
