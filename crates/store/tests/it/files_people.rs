use quorum_model::{DriveId, DriveItemId, ListId, Presence, SiteId, UserId};
use quorum_store::Store;

use crate::common::{drive_item, person, t};

fn did(s: &str) -> DriveId {
    DriveId::new(s).unwrap()
}

fn iid(s: &str) -> DriveItemId {
    DriveItemId::new(s).unwrap()
}

#[tokio::test]
async fn drive_children_folders_first_then_name_paged() {
    let store = Store::open_in_memory().unwrap();
    let drive: quorum_model::Drive =
        serde_json::from_value(serde_json::json!({"id": "d1", "driveType": "business", "name": "OneDrive"})).unwrap();
    let items = vec![
        drive_item("d1", "root", None, "root", true, "2026-01-01T00:00:00Z"),
        drive_item("d1", "f-b", Some("root"), "beta", true, "2026-01-01T00:00:00Z"),
        drive_item("d1", "f-a", Some("root"), "Alpha", true, "2026-01-01T00:00:00Z"),
        drive_item("d1", "x-z", Some("root"), "zeta.txt", false, "2026-01-05T00:00:00Z"),
        drive_item("d1", "x-a", Some("root"), "apple.txt", false, "2026-01-03T00:00:00Z"),
        drive_item("d1", "x-m", Some("root"), "Mango.txt", false, "2026-01-04T00:00:00Z"),
        drive_item("d1", "deep", Some("f-a"), "inside.txt", false, "2026-01-02T00:00:00Z"),
    ];
    store
        .write(move |tx| {
            tx.upsert_drives(&[drive])?;
            tx.upsert_drive_items(&items)
        })
        .await
        .unwrap();
    let mut names = Vec::new();
    let mut after = None;
    for _ in 0..10 {
        let page = store.drive_children(did("d1"), Some(iid("root")), after, 2).await.unwrap();
        names.extend(page.items.iter().map(|i| i.name.as_str().to_owned()));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(names, ["Alpha", "beta", "apple.txt", "Mango.txt", "zeta.txt"]);
    let roots = store.drive_children(did("d1"), None, None, 50).await.unwrap();
    assert_eq!(roots.items.len(), 1);

    let recent = store.recent_drive_items(3).await.unwrap();
    let names: Vec<_> = recent.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["zeta.txt", "Mango.txt", "apple.txt"]);

    // Deleting a folder deletes its subtree.
    store.apply_drive_delta(did("d1"), vec![], vec![iid("f-a")]).await.unwrap();
    assert!(store.read(|r| r.drive_item(&did("d1"), &iid("deep"))).await.unwrap().is_none());
    assert_eq!(store.read(|r| r.drives()).await.unwrap().items.len(), 1);
}

#[tokio::test]
async fn sites_lists_and_items() {
    let store = Store::open_in_memory().unwrap();
    let site: quorum_model::Site =
        serde_json::from_value(serde_json::json!({"id": "s1", "displayName": "Site"})).unwrap();
    let list: quorum_model::List =
        serde_json::from_value(serde_json::json!({"id": "l1", "displayName": "Tasks"})).unwrap();
    let items: Vec<quorum_model::ListItem> = (0..5)
        .map(|i| {
            serde_json::from_value(serde_json::json!({
                "id": format!("i{i}"), "fields": {"Title": format!("task {i}")},
                "lastModifiedDateTime": format!("2026-01-0{}T00:00:00Z", i + 1)
            }))
            .unwrap()
        })
        .collect();
    store
        .write(move |tx| {
            tx.upsert_sites(&[site])?;
            tx.upsert_lists(&SiteId::new("s1")?, &[list])?;
            tx.upsert_list_items(&ListId::new("l1")?, &items)?;
            tx.delete_list_items(&ListId::new("l1")?, &[quorum_model::ListItemId::new("i0")?])
        })
        .await
        .unwrap();
    let (sites, lists, page1) = store
        .read(|r| {
            let first = r.list_items(&ListId::new("l1")?, None, 2)?;
            Ok((r.sites()?, r.lists(&SiteId::new("s1")?)?, first))
        })
        .await
        .unwrap();
    assert_eq!(sites.items.len(), 1);
    assert_eq!(lists.items[0].site_id.as_ref().map(|s| s.as_str()), Some("s1"));
    let ids: Vec<_> = page1.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["i4", "i3"]);
    let next = page1.next.unwrap();
    let page2 = store.read(move |r| r.list_items(&ListId::new("l1")?, Some(&next), 10)).await.unwrap();
    assert_eq!(page2.items.len(), 2);
    assert_eq!(page2.items[0].field_str("Title"), Some("task 2"));
}

#[tokio::test]
async fn people_prefix_and_presence() {
    let store = Store::open_in_memory().unwrap();
    let mut nobody = person("x", "Nobody", "x@example.com");
    nobody.id = None;
    nobody.email = None;
    let skipped = store
        .upsert_people(vec![
            person("u1", "Ada Lovelace", "ada@example.com"),
            person("u2", "Adam Smith", "smith@example.com"),
            person("u3", "Grace Hopper", "grace@example.com"),
            nobody,
        ])
        .await
        .unwrap();
    assert_eq!(skipped, 1);
    let ada = store.people_by_prefix("ADA".into(), 10).await.unwrap();
    let names: Vec<_> = ada.items.iter().map(|p| p.display_name.as_str()).collect();
    assert_eq!(names, ["Ada Lovelace", "Adam Smith"]);
    let by_mail = store.people_by_prefix("smi".into(), 10).await.unwrap();
    assert_eq!(by_mail.items.len(), 1);
    assert!(store.people_by_prefix("  ".into(), 10).await.unwrap().items.is_empty());
    assert!(store.read(|r| r.person(&UserId::new("u3")?)).await.unwrap().is_some());

    let p: Presence =
        serde_json::from_value(serde_json::json!({"userId": "u1", "availability": "Busy", "activity": "InAMeeting"}))
            .unwrap();
    let anon = Presence::default();
    store.upsert_presences(vec![p.clone(), anon], t("2026-01-01T00:00:00Z")).await.unwrap();
    let got = store.presences(vec![UserId::new("u1").unwrap(), UserId::new("u9").unwrap()]).await.unwrap();
    assert_eq!(got.items.len(), 1);
    assert_eq!(got.items[0].presence, p);
    assert_eq!(got.items[0].fetched_at, t("2026-01-01T00:00:00Z"));
    let stale = store.read(|r| r.stale_presences(t("2026-01-02T00:00:00Z"), 10)).await.unwrap();
    assert_eq!(stale, vec![UserId::new("u1").unwrap()]);
}
