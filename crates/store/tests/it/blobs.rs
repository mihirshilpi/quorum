use quorum_store::{ErrorKind, Store};

use crate::common::open;

#[tokio::test]
async fn round_trip_replace_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).await;
    store.put_blob("k".into(), b"one".to_vec()).await.unwrap();
    store.put_blob("k".into(), b"two".to_vec()).await.unwrap();
    assert_eq!(store.get_blob("k".into()).await.unwrap().unwrap(), b"two");
    let files = std::fs::read_dir(dir.path().join(quorum_store::BLOB_DIR)).unwrap().count();
    assert_eq!(files, 1, "the replaced file was removed");
    store.delete_blob("k".into()).await.unwrap();
    assert!(store.get_blob("k".into()).await.unwrap().is_none());
    assert_eq!(std::fs::read_dir(dir.path().join(quorum_store::BLOB_DIR)).unwrap().count(), 0);
    for bad in ["", "a\nb", &"x".repeat(2000)] {
        assert_eq!(store.put_blob(bad.to_owned(), vec![1]).await.unwrap_err().kind(), ErrorKind::InvalidInput);
    }
}

#[tokio::test]
async fn quota_evicts_least_recently_used() {
    for file_backed in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let store = if file_backed { open(dir.path()).await } else { Store::open_in_memory().unwrap() };
        // Each 1000-byte blob is stored as 1000 + 41 bytes of ciphertext.
        store.set_blob_quota(3 * 1041).await.unwrap();
        for k in ["a", "b", "c"] {
            store.put_blob(k.into(), vec![7; 1000]).await.unwrap();
        }
        // Touch "a" so "b" becomes the least recently used.
        assert!(store.get_blob("a".into()).await.unwrap().is_some());
        store.put_blob("d".into(), vec![7; 1000]).await.unwrap();
        assert!(store.get_blob("b".into()).await.unwrap().is_none(), "LRU evicted");
        for k in ["a", "c", "d"] {
            assert!(store.get_blob(k.into()).await.unwrap().is_some(), "{k} kept");
        }
        let usage = store.blob_usage().await.unwrap();
        assert_eq!(usage.count, 3);
        assert!(usage.bytes <= usage.quota);
        // Lowering the quota evicts immediately.
        assert_eq!(store.set_blob_quota(1041).await.unwrap(), 2);
        assert_eq!(store.blob_usage().await.unwrap().count, 1);
        // A blob larger than the quota is refused.
        assert_eq!(store.put_blob("big".into(), vec![0; 5000]).await.unwrap_err().kind(), ErrorKind::InvalidInput);
        if file_backed {
            assert_eq!(std::fs::read_dir(dir.path().join(quorum_store::BLOB_DIR)).unwrap().count(), 1);
        }
    }
}

#[tokio::test]
async fn orphans_are_swept_and_missing_files_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = open(dir.path()).await;
        store.put_blob("kept".into(), b"x".to_vec()).await.unwrap();
        store.put_blob("lost".into(), b"y".to_vec()).await.unwrap();
    }
    let blob_dir = dir.path().join(quorum_store::BLOB_DIR);
    std::fs::write(blob_dir.join("orphan.bin.tmp"), b"junk").unwrap();
    let store = open(dir.path()).await;
    assert!(!blob_dir.join("orphan.bin.tmp").exists());
    // Delete one blob file behind the store's back.
    let files: Vec<_> = std::fs::read_dir(&blob_dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 2);
    for f in &files {
        std::fs::remove_file(f).unwrap();
    }
    assert!(store.get_blob("lost".into()).await.unwrap().is_none());
    assert_eq!(store.blob_usage().await.unwrap().count, 1);
}
