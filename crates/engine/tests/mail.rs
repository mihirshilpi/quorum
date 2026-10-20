//! Mail commands and queries end to end in mock mode: sync from the fixture Graph, query
//! shapes, optimistic writes and their outbox entries, compose / reply / send / undo, and
//! hostile parameters for every mail command and query.

use quorum_engine::{ErrorKind, Session, SessionOptions};
use quorum_sync::mail::{FoldersResource, MailModule};
use quorum_sync::{Module, Outcome, Resource};
use serde_json::{Value, json};

/// A signed-in mock session with mail synced from the fixtures and sync paused, so outbox
/// entries stay queued for inspection.
async fn session() -> Session {
    let s = Session::new(SessionOptions { mock_graph: true }).unwrap();
    s.start().await;
    let h = s.sync().unwrap();
    h.pause();
    let ctx = h.ctx().clone();
    FoldersResource::new().run(&ctx).await.unwrap();
    for r in MailModule::new().discover(&ctx).await.unwrap() {
        for _ in 0..20 {
            if r.run(&ctx).await.unwrap() == Outcome::CaughtUp {
                break;
            }
        }
    }
    s
}

async fn q(s: &Session, id: &str, p: Value) -> Value {
    s.query(id, p).await.unwrap_or_else(|e| panic!("{id}: {e}"))
}

async fn run(s: &Session, id: &str, p: Value) -> Value {
    s.execute(id, p).await.unwrap_or_else(|e| panic!("{id}: {e}"))
}

async fn outbox_kinds(s: &Session) -> Vec<String> {
    let store = s.store().unwrap();
    store.read(|r| r.outbox_entries(500)).await.unwrap().items.into_iter().map(|e| e.kind).collect()
}

async fn inbox(s: &Session) -> Vec<Value> {
    q(s, "mail.list", json!({ "folder": "inbox" })).await.as_array().cloned().unwrap()
}

#[tokio::test]
async fn folders_and_list_have_the_ui_shape() {
    let s = session().await;
    let folders = q(&s, "mail.folders", Value::Null).await;
    let folders = folders.as_array().unwrap();
    let inbox_row = folders.iter().find(|f| f["kind"] == "inbox").unwrap();
    assert_eq!(inbox_row["name"], "Inbox");
    assert!(inbox_row["unread"].as_u64().unwrap() > 0);
    assert_eq!(inbox_row["favourite"], true);
    assert_eq!(folders.first().unwrap()["kind"], "inbox", "inbox first");
    let projects = folders.iter().find(|f| f["name"] == "Projects").unwrap();
    assert_eq!(projects["depth"], 1, "child folders nest under the inbox");
    for kind in ["drafts", "sent", "deleted", "archive", "junk"] {
        assert!(folders.iter().any(|f| f["kind"] == kind), "{kind}");
    }

    let rows = inbox(&s).await;
    assert_eq!(rows.len(), 16);
    let r = rows.first().unwrap();
    for key in [
        "id",
        "cursor",
        "from_name",
        "from_email",
        "subject",
        "preview",
        "received",
        "unread",
        "is_read",
        "flagged",
        "has_attachments",
        "importance",
        "conversation_id",
        "categories",
        "focused",
    ] {
        assert!(r.get(key).is_some(), "row has {key}");
    }
    let times: Vec<&str> = rows.iter().map(|r| r["received"].as_str().unwrap()).collect();
    let mut sorted = times.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(times, sorted, "newest first");

    let focused = q(&s, "mail.list", json!({ "folder": "inbox", "focused": true })).await;
    let other = q(&s, "mail.list", json!({ "folder": "inbox", "focused": false })).await;
    assert_eq!(focused.as_array().unwrap().len() + other.as_array().unwrap().len(), 16);
    assert_eq!(other.as_array().unwrap().len(), 3);

    let unread = q(&s, "mail.list", json!({ "folder": "inbox", "unread_only": true })).await;
    assert!(unread.as_array().unwrap().iter().all(|r| r["unread"] == true));

    // Keyset paging with the row cursor.
    let first = q(&s, "mail.list", json!({ "folder": "inbox", "limit": 5 })).await;
    let after = first.as_array().unwrap().last().unwrap()["cursor"].clone();
    let next = q(&s, "mail.list", json!({ "folder": "inbox", "limit": 5, "after": after })).await;
    assert_eq!(next.as_array().unwrap().len(), 5);
    assert_ne!(next[0]["id"], first[0]["id"]);

    let grouped = q(&s, "mail.list", json!({ "folder": "inbox", "group": "conversation" })).await;
    assert!(grouped.as_array().unwrap().len() < 16);
    assert!(grouped.as_array().unwrap().iter().any(|r| r["count"].as_u64() == Some(2)));
}

#[tokio::test]
async fn message_body_is_sanitised_with_remote_images_blocked() {
    let s = session().await;
    let rows = inbox(&s).await;
    let mut saw_blocked = false;
    for r in &rows {
        let m = q(&s, "mail.message", json!({ "id": r["id"] })).await;
        let html = m["body_html"].as_str().expect("prefetched body");
        assert!(!html.contains("<script"));
        assert!(!html.contains("href=\"https://"), "links are rewritten to quorum-link:");
        assert!(m["to_recipients"].is_array());
        if m["remote_images_blocked"] == true {
            saw_blocked = true;
            assert!(!html.contains("https://images.example.com"));
            let shown = q(&s, "mail.message", json!({ "id": r["id"], "allow_remote_images": true })).await;
            assert_eq!(shown["remote_images_blocked"], false);
            assert!(shown["body_html"].as_str().unwrap().contains("quorum-remote:"));
            // Always allow this sender.
            run(&s, "mail.allow_remote_images", json!({ "id": r["id"] })).await;
            let again = q(&s, "mail.message", json!({ "id": r["id"] })).await;
            assert_eq!(again["remote_images_allowed"], true);
        }
    }
    assert!(saw_blocked);
    let with_att = rows.iter().find(|r| r["has_attachments"] == true).unwrap();
    let m = q(&s, "mail.message", json!({ "id": with_att["id"] })).await;
    let att = &m["attachments"][0];
    assert_eq!(att["name"], "synthetic-plan.pdf");
    let dl = run(&s, "mail.download_attachment", json!({ "id": with_att["id"], "attachment": att["id"] })).await;
    assert!(dl["size"].as_u64().unwrap() > 0);
    let conv = q(&s, "mail.conversation", json!({ "message": rows[0]["id"] })).await;
    assert!(!conv.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn writes_are_optimistic_and_queued() {
    let s = session().await;
    let rows = inbox(&s).await;
    let unread = rows.iter().find(|r| r["unread"] == true).unwrap()["id"].clone();
    run(&s, "mail.mark_read", json!({ "id": unread })).await;
    assert!(inbox(&s).await.iter().any(|r| r["id"] == unread && r["unread"] == false));
    run(&s, "mail.toggle_read", json!({ "id": unread })).await;
    assert!(inbox(&s).await.iter().any(|r| r["id"] == unread && r["unread"] == true));

    let id = rows[0]["id"].clone();
    let flagged = |rows: &[Value]| rows.iter().find(|r| r["id"] == id).map(|r| r["flagged"].clone());
    let before = flagged(&rows).unwrap();
    run(&s, "mail.flag", json!({ "id": id })).await;
    assert_ne!(flagged(&inbox(&s).await).unwrap(), before, "flag toggles");
    run(&s, "mail.categorize", json!({ "id": id, "categories": ["Green category"] })).await;
    let m = q(&s, "mail.message", json!({ "id": id })).await;
    assert!(m["categories"].as_array().unwrap().contains(&json!("Green category")));
    run(&s, "mail.mark_other", json!({ "id": id })).await;
    assert_eq!(q(&s, "mail.message", json!({ "id": id })).await["focused"], false);

    let archived = rows[1]["id"].clone();
    run(&s, "mail.archive", json!({ "id": archived })).await;
    let archive = q(&s, "mail.list", json!({ "folder": "archive" })).await;
    assert!(archive.as_array().unwrap().iter().any(|r| r["id"] == archived));
    assert!(!inbox(&s).await.iter().any(|r| r["id"] == archived));

    let doomed = rows[2]["id"].clone();
    run(&s, "mail.delete", json!({ "id": doomed })).await;
    let trash = q(&s, "mail.list", json!({ "folder": "deleteditems" })).await;
    assert!(trash.as_array().unwrap().iter().any(|r| r["id"] == doomed));
    // Deleting from Deleted Items deletes permanently.
    run(&s, "mail.delete", json!({ "id": doomed })).await;
    assert!(s.query("mail.message", json!({ "id": doomed })).await.is_err());

    let projects = q(&s, "mail.folders", Value::Null).await;
    let projects = projects.as_array().unwrap().iter().find(|f| f["name"] == "Projects").unwrap()["id"].clone();
    let moved = rows[3]["id"].clone();
    run(&s, "mail.move", json!({ "id": moved, "folder": projects })).await;
    let in_projects = q(&s, "mail.list", json!({ "folder": projects })).await;
    assert!(in_projects.as_array().unwrap().iter().any(|r| r["id"] == moved));
    assert_eq!(
        s.execute("mail.move", json!({ "id": moved, "folder": "no-such-folder" })).await.unwrap_err().kind(),
        ErrorKind::NotFound
    );

    let kinds = outbox_kinds(&s).await;
    for k in ["mail.update", "mail.move", "mail.delete_permanent"] {
        assert!(kinds.iter().any(|x| x == k), "{k} queued: {kinds:?}");
    }
    // Bulk: many ids at once.
    let ids: Vec<Value> = inbox(&s).await.iter().map(|r| r["id"].clone()).collect();
    let n = run(&s, "mail.mark_read", json!({ "ids": ids })).await;
    assert!(n["changed"].as_u64().is_some());
    assert!(inbox(&s).await.iter().all(|r| r["unread"] == false));
}

#[tokio::test]
async fn compose_reply_send_and_undo() {
    let s = session().await;
    let d = run(&s, "mail.compose", json!({ "to": ["test.user@example.com"], "subject": "Synthetic hello" })).await;
    let draft = d["draft_id"].as_str().unwrap().to_owned();
    run(
        &s,
        "mail.save_draft",
        json!({ "draft_id": draft, "body_html": "<p>Hi <a href=\"quorum-link:https%3A%2F%2Fexample.com%2F\">there</a><img src=x onerror=alert(1)></p>", "cc": "Casey <casey@example.com>; dev@example.com" }),
    )
    .await;
    let v = q(&s, "mail.draft", json!({ "draft_id": draft })).await;
    assert_eq!(v["subject"], "Synthetic hello");
    assert_eq!(v["cc"].as_array().unwrap().len(), 2);
    assert!(!v["body_html"].as_str().unwrap().contains("onerror"), "editor HTML is sanitised");
    // The stored draft carries the real link (restored for sending).
    let stored = s.store().unwrap().draft(draft.clone()).await.unwrap().unwrap();
    assert!(stored.body.as_str().contains("https://example.com/"));
    assert!(outbox_kinds(&s).await.contains(&"mail.draft_sync".to_owned()));
    // Autosave on every pause queues only one sync while one is waiting.
    run(&s, "mail.save_draft", json!({ "draft_id": draft, "subject": "Synthetic hello 2" })).await;
    assert_eq!(outbox_kinds(&s).await.iter().filter(|k| *k == "mail.draft_sync").count(), 1);

    let attached =
        run(&s, "mail.attach", json!({ "draft_id": draft, "name": "../note.txt", "data_base64": "aGVsbG8=" })).await;
    assert_eq!(attached["attachment"]["name"], "note.txt");

    let sent = run(&s, "mail.send", json!({ "draft_id": draft, "delay_seconds": 30 })).await;
    assert!(sent["undo_until"].is_string());
    let v = q(&s, "mail.draft", json!({ "draft_id": draft })).await;
    assert_eq!(v["sending"], true);
    assert!(
        s.execute("mail.save_draft", json!({ "draft_id": draft, "subject": "x" })).await.is_err(),
        "no edits while sending"
    );
    let undone = run(&s, "mail.undo_send", Value::Null).await;
    assert_eq!(undone["undone"], true);
    assert_eq!(q(&s, "mail.draft", json!({ "draft_id": draft })).await["sending"], false);
    assert!(!outbox_kinds(&s).await.contains(&"mail.send".to_owned()), "the queued send was discarded");
    assert!(s.execute("mail.undo_send", json!({ "draft_id": draft })).await.is_err());

    // A send with no recipients is refused before queueing.
    let empty = run(&s, "mail.compose", Value::Null).await;
    assert_eq!(
        s.execute("mail.send", json!({ "draft_id": empty["draft_id"] })).await.unwrap_err().kind(),
        ErrorKind::InvalidParams
    );
    run(&s, "mail.discard_draft", json!({ "draft_id": empty["draft_id"] })).await;
    assert!(s.query("mail.draft", json!({ "draft_id": empty["draft_id"] })).await.is_err());

    // Reply and reply all prefill recipients and subject; a second reply reuses the draft.
    let rows = inbox(&s).await;
    let src = rows.iter().find(|r| !r["subject"].as_str().unwrap().starts_with("RE:")).unwrap();
    let r = run(&s, "mail.reply", json!({ "id": src["id"] })).await;
    let reply = q(&s, "mail.draft", json!({ "draft_id": r["draft_id"] })).await;
    assert_eq!(reply["mode"], "reply");
    assert_eq!(reply["to"][0]["address"], src["from_email"]);
    assert!(reply["subject"].as_str().unwrap().starts_with("RE: "));
    assert!(reply["quote_html"].as_str().is_some_and(|q| !q.is_empty()), "the original shows below");
    let again = run(&s, "mail.reply", json!({ "id": src["id"] })).await;
    assert_eq!(again["draft_id"], r["draft_id"]);
    let fw = run(&s, "mail.forward", json!({ "id": src["id"] })).await;
    let fw = q(&s, "mail.draft", json!({ "draft_id": fw["draft_id"] })).await;
    assert!(fw["subject"].as_str().unwrap().starts_with("FW: "));
    assert!(fw["to"].as_array().unwrap().is_empty());
    let all = run(&s, "mail.reply_all", json!({ "id": src["id"] })).await;
    let all = q(&s, "mail.draft", json!({ "draft_id": all["draft_id"] })).await;
    assert!(!all["to"].as_array().unwrap().is_empty());

    let sent =
        run(&s, "mail.send", json!({ "draft_id": r["draft_id"], "body_html": "<p>Thanks</p>", "delay_seconds": 0 }))
            .await;
    assert!(sent["outbox_id"].as_i64().is_some());
    assert!(outbox_kinds(&s).await.contains(&"mail.send".to_owned()));
    let drafts = q(&s, "mail.drafts", Value::Null).await;
    assert!(drafts.as_array().unwrap().iter().any(|d| d["sending"] == true));
}

#[tokio::test]
async fn search_auto_reply_and_categories() {
    let s = session().await;
    let hits = q(&s, "mail.search", json!({ "query": "planning", "remote": false })).await;
    assert!(!hits.as_array().unwrap().is_empty());
    assert!(hits[0]["snippet"].is_string());
    run(&s, "mail.set_auto_reply", json!({ "status": "alwaysEnabled", "internal_message": "<p>Away</p>" })).await;
    assert_eq!(q(&s, "mail.auto_reply", Value::Null).await["status"], "alwaysEnabled");
    assert!(s.execute("mail.set_auto_reply", json!({ "status": "scheduled" })).await.is_err());
    assert!(q(&s, "mail.categories", Value::Null).await.is_array());
}

#[tokio::test]
async fn hostile_params_never_panic_in_mail() {
    let s = session().await;
    let real = inbox(&s).await[0]["id"].clone();
    let deep = (0..200).fold(json!(1), |acc, _| json!([acc]));
    let long = "x".repeat(100_000);
    let values = vec![
        Value::Null,
        json!(true),
        json!(-1),
        json!("id"),
        json!([]),
        json!({}),
        json!({ "id": "" }),
        json!({ "id": long }),
        json!({ "id": "\u{0}\u{202e}" }),
        json!({ "id": "../../etc" }),
        json!({ "ids": (0..2000).map(|i| format!("m{i}")).collect::<Vec<_>>() }),
        json!({ "ids": [1, 2, 3] }),
        json!({ "id": real, "folder": "\u{0}" }),
        json!({ "id": real, "folder": long }),
        json!({ "id": real, "categories": [long] }),
        json!({ "id": real, "categories": "x", "mode": "explode" }),
        json!({ "id": real, "attachment": "nope" }),
        json!({ "draft_id": "nope" }),
        json!({ "draft_id": long, "to": "not an address" }),
        json!({ "draft_id": "d", "data_base64": "!!!", "name": "x" }),
        json!({ "folder": "inbox", "limit": -9_007_199_254_740_991_i64, "after": "garbage" }),
        json!({ "folder": "inbox", "after": "99999999999999999999999:x" }),
        json!({ "query": long }),
        json!({ "query": "\"*:^(", "remote": false }),
        json!({ "status": "scheduled", "start": "yesterday" }),
        json!({ "to": [{ "address": 5 }], "subject": long }),
        json!({ "sender": "a@b@c" }),
        deep,
    ];
    let commands: Vec<String> =
        s.registry().iter().filter(|c| c.id.starts_with("mail.")).map(|c| c.id.to_owned()).collect();
    let queries: Vec<String> =
        s.queries().iter().filter(|c| c.id.starts_with("mail.")).map(|c| c.id.to_owned()).collect();
    assert!(commands.len() >= 25 && queries.len() >= 9);
    for v in &values {
        for id in &commands {
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
}

#[tokio::test]
async fn mail_shortcuts_do_not_collide() {
    let s = Session::new(SessionOptions { mock_graph: true }).unwrap();
    let mail: Vec<(String, String)> = s
        .registry()
        .iter()
        .filter(|c| c.id.starts_with("mail."))
        .filter_map(|c| c.shortcut.map(|k| (c.id.to_owned(), k.to_owned())))
        .collect();
    let mut keys: Vec<&String> = mail.iter().map(|(_, k)| k).collect();
    keys.sort_unstable();
    let n = keys.len();
    keys.dedup();
    assert_eq!(n, keys.len(), "mail shortcuts are unique: {mail:?}");
    // No mail shortcut shadows a global UI command.
    for (id, k) in &mail {
        assert!(!quorum_ipc::UI_COMMANDS.iter().any(|u| u.shortcut == Some(k.as_str())), "{id} uses {k}");
    }
}

#[tokio::test]
async fn conversations_span_folders_and_hide_quoted_history() {
    let s = session().await;
    // The offsite thread: two from Avery and Casey in Archive, your two replies in Sent Items,
    // and Avery's latest in the Inbox.
    let conv = q(&s, "mail.conversation", json!({ "message": "mock-msg-0000" })).await;
    let conv = conv.as_array().unwrap();
    assert_eq!(conv.len(), 5);
    let kinds: Vec<&str> = conv.iter().map(|m| m["folder_kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["archive", "sent", "archive", "sent", "inbox"], "oldest first, across folders");
    assert!(conv.iter().filter(|m| m["from_me"] == true).count() == 2);
    let first = conv.first().unwrap();
    assert_eq!(first["has_quoted"], false, "the first message quotes nothing");
    for reply in conv.iter().skip(1) {
        assert_eq!(reply["has_quoted"], true, "{}", reply["id"]);
        let new = reply["body_new_html"].as_str().unwrap();
        let full = reply["body_html"].as_str().unwrap();
        assert!(new.len() < full.len() && !new.is_empty());
        assert!(!new.contains("From:"), "quoted header stays out of the new text");
    }
    let newest = q(&s, "mail.conversation", json!({ "message": "mock-msg-0000", "order": "newest_first" })).await;
    assert_eq!(newest[0]["id"], "mock-msg-0000");
    // Gmail- and Apple-style replies split too.
    for id in ["mock-msg-0005", "mock-msg-0008"] {
        let m = q(&s, "mail.message", json!({ "id": id })).await;
        assert_eq!(m["has_quoted"], true, "{id}");
        assert!(!m["body_new_html"].as_str().unwrap().contains("wrote:"), "{id}");
    }
    // A designed newsletter keeps its own page.
    let news = q(&s, "mail.message", json!({ "id": "mock-msg-0026" })).await;
    assert_eq!(news["own_background"], true);
    // Grouped list rows count the whole conversation and name who took part.
    let grouped = q(&s, "mail.list", json!({ "folder": "inbox", "group": "conversation" })).await;
    let row = grouped.as_array().unwrap().iter().find(|r| r["id"] == "mock-msg-0000").unwrap().clone();
    assert_eq!(row["count"], 5);
    assert_eq!(row["participants"], json!(["Avery", "me", "Casey"]));
    // Hostile params.
    for p in [
        json!({ "message": "mock-msg-0000", "order": 5 }),
        json!({ "message": "mock-msg-0000", "allow_remote_ids": "x" }),
    ] {
        assert!(s.query("mail.conversation", p).await.is_err());
    }
}
