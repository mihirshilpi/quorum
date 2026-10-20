//! Files, sites and lists end to end in mock mode (fixture Graph, in-memory store, running sync):
//! query shapes, optimistic writes queued in the outbox, open/launch events, hostile params.

use std::time::Duration;

use quorum_engine::{ErrorKind, Session, SessionOptions};
use quorum_ipc::CoreEvent;
use serde_json::{Value, json};

const D: &str = "b!synthetic-drive-0001";
const LIB: &str = "b!synthetic-library-0001";
const SITE: &str = "contoso.sharepoint.com,00000000-0000-0000-0000-0000000000c1,00000000-0000-0000-0000-0000000000c2";
const TASKS: &str = "00000000-0000-0000-0000-0000000000d1";
const DOCS: &str = "01MOCKFOLDER00000000000000000001";
const Q4: &str = "01MOCKITEM0000000000000000000005";

fn rid(item: &str) -> String {
    format!("{D}|{item}")
}

async fn session() -> Session {
    let s = Session::new(SessionOptions { mock_graph: true }).unwrap();
    s.start().await;
    s
}

/// Polls `query` until `ok` holds (sync runs in the background).
async fn wait_for(s: &Session, query: &str, params: Value, ok: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    for _ in 0..200 {
        match s.query(query, params.clone()).await {
            Ok(v) if ok(&v) => return v,
            Ok(v) => last = v,
            Err(e) => last = json!(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{query} never satisfied the condition; last: {last}");
}

fn items(v: &Value) -> Vec<Value> {
    v.get("items").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn names(v: &Value) -> Vec<String> {
    items(v).iter().map(|i| i["name"].as_str().unwrap_or_default().to_owned()).collect()
}

async fn synced(s: &Session) -> Value {
    wait_for(s, "files.children", json!({ "location": "mine" }), |v| items(v).len() >= 8).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn my_files_have_the_documented_shape() {
    let s = session().await;
    let root = synced(&s).await;
    assert_eq!(root["location"], "mine");
    assert_eq!(root["breadcrumb"], json!([{ "id": "mine", "name": "My files" }]));
    let rows = items(&root);
    // Folders first, then files, by name.
    assert_eq!(names(&root)[..4], ["Archive", "Documents", "Photos", "Projects"]);
    let first = &rows[0];
    for key in [
        "id",
        "drive",
        "item",
        "name",
        "folder",
        "child_count",
        "ext",
        "kind",
        "size",
        "modified",
        "modified_label",
        "modified_by",
        "shared",
        "web_url",
        "parent",
        "pending",
    ] {
        assert!(first.get(key).is_some(), "file row lacks {key}: {first}");
    }
    let welcome = rows.iter().find(|r| r["name"] == "Welcome.docx").unwrap();
    assert_eq!(welcome["kind"], "word");
    assert_eq!(welcome["modified_by"], "Alex Sample");
    assert_eq!(welcome["shared"], true);

    let by_size = s.query("files.children", json!({ "location": "mine", "sort": "size", "desc": true })).await.unwrap();
    assert_eq!(names(&by_size)[0], "Archive", "folders stay on top, largest first");
    let paged = s.query("files.children", json!({ "location": "mine", "limit": 3 })).await.unwrap();
    assert_eq!(items(&paged).len(), 3);
    let rest = s.query("files.children", json!({ "location": "mine", "after": paged["next"] })).await.unwrap();
    assert_eq!(items(&rest).len() + 3, items(&root).len());
    assert!(s.query("files.children", json!({ "sort": "colour" })).await.is_err());

    let docs = s.query("files.children", json!({ "folder": rid(DOCS) })).await.unwrap();
    assert_eq!(names(&docs)[0], "Drafts");
    assert!(names(&docs).contains(&"Q4 plan.docx".to_owned()));
    assert_eq!(
        docs["breadcrumb"],
        json!([{ "id": "mine", "name": "My files" }, { "id": rid(DOCS), "name": "Documents" }])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locations_recent_shared_and_libraries() {
    let s = session().await;
    let locs = wait_for(&s, "files.locations", json!({}), |v| {
        v.as_array().is_some_and(|a| a.iter().any(|r| r["kind"] == "library") && a.iter().any(|r| r["kind"] == "list"))
    })
    .await;
    let rows = locs.as_array().unwrap();
    let ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(ids[..3], ["mine", "recent", "shared"]);
    let site = rows.iter().find(|r| r["kind"] == "site").unwrap();
    assert_eq!(site["name"], "Synthetic Site");
    let lib = rows.iter().find(|r| r["kind"] == "library").unwrap();
    assert_eq!(lib["id"], format!("drive:{LIB}"));
    let list = rows.iter().find(|r| r["kind"] == "list").unwrap();
    assert_eq!(list["name"], "Synthetic Tasks");
    assert!(!rows.iter().any(|r| r["kind"] == "list" && r["name"] == "Documents"), "libraries are not lists");

    let recent = wait_for(&s, "files.children", json!({ "location": "recent" }), |v| items(v).len() >= 3).await;
    assert_eq!(names(&recent)[0], "Q4 plan.docx", "Recent keeps Graph's order");
    let shared = wait_for(&s, "files.children", json!({ "location": "shared" }), |v| items(v).len() >= 4).await;
    assert!(names(&shared).contains(&"Design review.pptx".to_owned()));

    let library =
        wait_for(&s, "files.children", json!({ "location": format!("drive:{LIB}") }), |v| items(v).len() >= 4).await;
    assert_eq!(names(&library)[0], "Policies");
    assert_eq!(library["breadcrumb"][0]["name"], "Documents");

    let sites = s.query("sites.list", json!({})).await.unwrap();
    assert_eq!(sites[0]["name"], "Synthetic Site");
    let lists = s.query("sites.lists", json!({ "site": SITE })).await.unwrap();
    assert_eq!(lists.as_array().unwrap().len(), 2);
    let tasks = s.query("lists.items", json!({ "site": SITE, "list": TASKS })).await.unwrap();
    assert_eq!(items(&tasks).len(), 6);
    let cols: Vec<&str> = tasks["columns"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!(cols, ["Title", "Status", "Priority", "DueDate", "Done", "AssignedTo"]);
    assert_eq!(tasks["columns"][1]["kind"], "choice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn details_preview_and_versions() {
    let s = session().await;
    synced(&s).await;
    wait_for(&s, "files.children", json!({ "folder": rid(DOCS) }), |v| items(v).len() >= 5).await;
    let d = s.query("files.item", json!({ "item": rid(Q4) })).await.unwrap();
    assert_eq!(d["item"]["name"], "Q4 plan.docx");
    assert_eq!(d["desktop_app"], "ms-word");
    assert_eq!(d["versions_count"], 2);
    assert_eq!(d["sharing_summary"], "Shared with 1 person and 1 link");
    assert!(!d.to_string().contains("SECRETTOKEN"), "link URLs never reach the details");
    let v = s.query("files.versions", json!({ "item": rid(Q4) })).await.unwrap();
    assert_eq!(v["versions"][0]["id"], "3.0");
    let p = s.query("files.preview_url", json!({ "item": rid(Q4) })).await.unwrap();
    assert!(p["url"].as_str().unwrap().starts_with("https://"));
    // A bare item id resolves against My files.
    let bare = s.query("files.item", json!({ "id": Q4 })).await.unwrap();
    assert_eq!(bare["item"]["id"], rid(Q4));
    let missing = s.query("files.item", json!({ "item": rid("NOPE") })).await.err().unwrap();
    assert_eq!(missing.kind(), ErrorKind::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_are_optimistic_and_queued() {
    let s = session().await;
    synced(&s).await;
    let made = s.execute("files.new_folder", json!({ "name": "Plans" })).await.unwrap();
    assert!(made["id"].as_str().unwrap().contains("|local-"));
    let root = s.query("files.children", json!({ "location": "mine" })).await.unwrap();
    // The placeholder shows at once; the outbox may already have replaced it with the
    // (fixture) server's folder.
    let n = names(&root);
    assert!(n.contains(&"Plans".to_owned()) || n.contains(&"New folder".to_owned()), "{n:?}");

    s.execute("files.rename", json!({ "item": rid("01MOCKITEM0000000000000000000004"), "name": "README.md" }))
        .await
        .unwrap();
    let root = s.query("files.children", json!({ "location": "mine" })).await.unwrap();
    assert!(names(&root).contains(&"README.md".to_owned()));
    let bad = s
        .execute("files.rename", json!({ "item": rid("01MOCKITEM0000000000000000000004"), "name": "a/b" }))
        .await
        .err()
        .unwrap();
    assert_eq!(bad.kind(), ErrorKind::InvalidParams);

    let into_self = s
        .execute("files.move", json!({ "item": rid(DOCS), "folder": rid("01MOCKFOLDER00000000000000000004") }))
        .await
        .err()
        .unwrap();
    assert_eq!(into_self.kind(), ErrorKind::InvalidParams);
    s.execute("files.move", json!({ "item": rid(Q4), "folder": rid("01MOCKFOLDER00000000000000000002") }))
        .await
        .unwrap();
    let projects =
        s.query("files.children", json!({ "folder": rid("01MOCKFOLDER00000000000000000002") })).await.unwrap();
    assert!(names(&projects).contains(&"Q4 plan.docx".to_owned()));

    s.execute("files.delete", json!({ "item": rid("01MOCKFOLDER00000000000000000003") })).await.unwrap();
    let root = s.query("files.children", json!({ "location": "mine" })).await.unwrap();
    assert!(!names(&root).contains(&"Photos".to_owned()));

    let share = s
        .execute(
            "files.share",
            json!({ "item": rid(Q4), "type": "edit", "scope": "users", "recipients": ["robin@example.com"] }),
        )
        .await;
    assert!(share.is_ok());
    let bad = s.execute("files.share", json!({ "item": rid(Q4), "scope": "planet" })).await.err().unwrap();
    assert_eq!(bad.kind(), ErrorKind::InvalidParams);
    let bad =
        s.execute("files.share", json!({ "item": rid(Q4), "recipients": ["not an address"] })).await.err().unwrap();
    assert_eq!(bad.kind(), ErrorKind::InvalidParams);

    let created = s
        .execute("lists.create_item", json!({ "site": SITE, "list": TASKS, "fields": { "Title": "Plan offsite" } }))
        .await
        .unwrap();
    assert!(created["id"].as_str().unwrap().starts_with("local-"));
    let bad = s
        .execute("lists.create_item", json!({ "site": SITE, "list": TASKS, "fields": { "Bad Name": 1 } }))
        .await
        .err()
        .unwrap();
    assert_eq!(bad.kind(), ErrorKind::InvalidParams);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_stage_inline_data_and_report_progress() {
    let s = session().await;
    synced(&s).await;
    let out =
        s.execute("files.upload", json!({ "files": [{ "name": "hello.txt", "data": "aGVsbG8=" }] })).await.unwrap();
    assert_eq!(out["queued"][0]["name"], "hello.txt");
    let ups = s.query("files.uploads", json!({})).await.unwrap();
    assert_eq!(ups[0]["name"], "hello.txt");
    assert_eq!(ups[0]["total"], 5);
    let missing = s.execute("files.upload", json!({ "paths": ["/definitely/not/here.txt"] })).await.err().unwrap();
    assert_eq!(missing.kind(), ErrorKind::InvalidParams);
    let relative = s.execute("files.upload", json!({ "paths": ["relative.txt"] })).await.err().unwrap();
    assert_eq!(relative.kind(), ErrorKind::InvalidParams);
    let bad = s.execute("files.upload", json!({ "files": [{ "name": "x.txt", "data": "%%%" }] })).await.err().unwrap();
    assert_eq!(bad.kind(), ErrorKind::InvalidParams);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_commands_emit_launch_events_without_dialogs() {
    let s = session().await;
    synced(&s).await;
    wait_for(&s, "files.children", json!({ "folder": rid(DOCS) }), |v| items(v).len() >= 5).await;
    let mut events = s.subscribe();
    let out = s.execute("files.open", json!({ "item": rid(Q4) })).await.unwrap();
    assert_eq!(out["action"], "desktop");
    assert!(out["url"].as_str().unwrap().starts_with("ms-word:ofe|u|https://"));
    let pdf = s.execute("files.open", json!({ "item": rid("01MOCKITEM0000000000000000000009") })).await.unwrap();
    assert_eq!(pdf["action"], "preview");
    let folder = s.execute("files.open", json!({ "item": rid(DOCS) })).await.unwrap();
    assert_eq!(folder["action"], "navigate");
    let not_office = s
        .execute("files.open_in_desktop", json!({ "item": rid("01MOCKITEM0000000000000000000009") }))
        .await
        .err()
        .unwrap();
    assert_eq!(not_office.kind(), ErrorKind::InvalidParams);
    let web = s.execute("files.open_in_browser", json!({ "item": rid(Q4) })).await.unwrap();
    assert!(web["url"].as_str().unwrap().starts_with("https://"));
    let mut seen = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let CoreEvent::Ui { command, .. } = ev {
            seen.push(command);
        }
    }
    assert!(seen.contains(&"files.launch".to_owned()));
    assert!(seen.contains(&"files.show".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_finds_cached_names() {
    let s = session().await;
    synced(&s).await;
    wait_for(&s, "files.children", json!({ "folder": rid(DOCS) }), |v| items(v).len() >= 5).await;
    let hits = s.query("files.search", json!({ "query": "budget" })).await.unwrap();
    assert!(names(&hits).contains(&"Budget 2027.xlsx".to_owned()));
    let empty = s.query("files.search", json!({ "query": "\u{0}  " })).await.unwrap();
    assert!(items(&empty).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_commands_survive_hostile_params_when_signed_in() {
    let s = session().await;
    synced(&s).await;
    let hostile = [
        Value::Null,
        json!([]),
        json!("x"),
        json!({ "item": 5 }),
        json!({ "item": "|" }),
        json!({ "item": "x".repeat(5000) }),
        json!({ "item": "\u{0}|\u{202e}" }),
        json!({ "item": rid(Q4), "name": "\u{0}", "folder": "drive:", "dest": "relative", "paths": ["\u{0}"] }),
        json!({ "folder": "|||", "location": "drive:\u{0}", "after": "-1", "limit": -5, "sort": [] }),
        json!({ "site": "", "list": "", "item": "", "fields": { "": 1 } }),
        json!({ "files": [{ "name": "../../x", "data": "aGk=" }] }),
        json!({ "items": vec!["a|b"; 600] }),
    ];
    let ids: Vec<String> = s
        .registry()
        .iter()
        .map(|c| c.id.to_owned())
        .filter(|id| id.starts_with("files.") || id.starts_with("lists.") || id.starts_with("sites."))
        .collect();
    let queries: Vec<String> = s
        .queries()
        .iter()
        .map(|c| c.id.to_owned())
        .filter(|id| id.starts_with("files.") || id.starts_with("lists.") || id.starts_with("sites."))
        .collect();
    assert!(ids.len() >= 20 && queries.len() >= 9, "{ids:?} {queries:?}");
    for v in &hostile {
        for id in &ids {
            if id == "files.download" {
                continue; // writes to ~/Downloads; covered with a temp folder below
            }
            if let Err(e) = s.execute(id, v.clone()).await {
                assert_ne!(e.kind(), ErrorKind::Bug, "{id} panicked on {v}");
            }
        }
        for id in &queries {
            if let Err(e) = s.query(id, v.clone()).await {
                assert_ne!(e.kind(), ErrorKind::Bug, "{id} panicked on {v}");
            }
        }
    }
    let e = s.execute("files.download", json!({ "item": rid(Q4), "dest": "relative/dir" })).await.err().unwrap();
    assert_eq!(e.kind(), ErrorKind::InvalidParams);
}
