//! Browser-preview answers for the files, sites and lists queries and commands, with
//! synthetic data in the same shapes the engine returns. Wired into [`crate::mock`].

use std::cell::RefCell;

use quorum_ipc::{CommandInfo, CoreEvent, Reply};
use serde_json::{Value, json};

use crate::mock::person;

/// Query ids answered here.
pub const QUERIES: &[&str] = &[
    "files.locations",
    "files.children",
    "files.item",
    "files.preview_url",
    "files.search",
    "files.versions",
    "files.uploads",
    "sites.list",
    "sites.lists",
    "lists.items",
];

/// Command ids answered here.
pub const COMMANDS: &[&str] = &[
    "files.open",
    "files.open_in_browser",
    "files.open_in_desktop",
    "files.download",
    "files.upload",
    "files.new_folder",
    "files.rename",
    "files.move",
    "files.copy",
    "files.delete",
    "files.share",
    "files.copy_link",
    "files.unshare",
    "files.versions",
    "files.restore_version",
    "files.checkout",
    "files.checkin",
    "files.follow_site",
    "files.unfollow_site",
    "lists.create_item",
    "lists.update_item",
    "lists.delete_item",
];

/// The registry entries the preview shows (labels and shortcuts as the engine has them).
pub fn commands() -> Vec<CommandInfo> {
    let c = |id: &str, label: &str, menu: Option<&str>, shortcut: Option<&str>, destructive: bool| CommandInfo {
        id: id.into(),
        label: label.into(),
        menu: menu.map(Into::into),
        shortcut: shortcut.map(Into::into),
        destructive,
        enabled: true,
    };
    vec![
        c("files.open", "Open", Some("Files/Open"), None, false),
        c("files.open_in_browser", "Open in Browser", Some("Files/Open in Browser"), None, false),
        c("files.open_in_desktop", "Open in Desktop App", Some("Files/Open in Desktop App"), None, false),
        c("files.download", "Download", Some("Files/Download"), Some("Cmd+Shift+D"), false),
        c("files.upload", "Upload", Some("Files/Upload"), Some("Cmd+U"), false),
        c("files.new_folder", "New Folder", Some("Files/New Folder"), Some("Cmd+Shift+N"), false),
        c("files.rename", "Rename", Some("Files/Rename"), None, false),
        c("files.move", "Move To", Some("Files/Move To"), Some("Cmd+Shift+M"), false),
        c("files.copy", "Copy To", Some("Files/Copy To"), None, false),
        c("files.delete", "Delete", Some("Files/Delete"), Some("Cmd+Backspace"), true),
        c("files.share", "Share", Some("Files/Share"), None, true),
        c("files.copy_link", "Copy Link", Some("Files/Copy Link"), Some("Cmd+Shift+C"), true),
        c("files.versions", "Version History", Some("Files/Version History"), None, false),
        c("files.checkout", "Check Out", Some("Files/Check Out"), None, false),
        c("files.checkin", "Check In", Some("Files/Check In"), None, false),
        c("files.follow_site", "Follow Site", Some("Files/Follow Site"), None, false),
        c("lists.create_item", "New List Item", Some("Files/New List Item"), None, false),
    ]
}

const MINE: &str = "b!preview-mine";
const LIB_A: &str = "b!preview-marketing";
const LIB_B: &str = "b!preview-brand";
const LIB_C: &str = "b!preview-engineering";
const OTHER: &str = "b!preview-shared";

#[derive(Debug, Clone)]
struct Item {
    drive: &'static str,
    id: String,
    parent: Option<String>,
    name: String,
    folder: bool,
    size: u64,
    /// Minutes before "now".
    age: u64,
    by: String,
    shared: bool,
    pending: bool,
}

#[derive(Default)]
struct State {
    items: Vec<Item>,
    next: u64,
    uploads: Vec<(String, String, u64, u64)>,
    seeded: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut s = s.try_borrow_mut().ok()?;
        if !s.seeded {
            seed(&mut s);
        }
        Some(f(&mut s))
    })
}

const TOPICS: &[&str] = &[
    "Quarterly planning",
    "Design review",
    "Budget draft",
    "Release notes",
    "Onboarding checklist",
    "Offsite logistics",
    "Customer feedback",
    "Roadmap",
    "Hiring loop",
    "Security review",
    "Launch retro",
    "Weekly status",
    "Vendor contract",
];
const EXT: &[&str] = &["docx", "xlsx", "pptx", "pdf", "png", "txt", "mp4", "zip", "vsdx", "one", "md", "jpg"];

fn seed(s: &mut State) {
    s.seeded = true;
    let mut add = |drive: &'static str,
                   id: &str,
                   parent: Option<&str>,
                   name: &str,
                   folder: bool,
                   size: u64,
                   age: u64,
                   who: usize,
                   shared: bool| {
        s.items.push(Item {
            drive,
            id: id.to_owned(),
            parent: parent.map(str::to_owned),
            name: name.to_owned(),
            folder,
            size,
            age,
            by: person(who).0,
            shared,
            pending: false,
        });
    };
    let root = "root";
    add(MINE, "f-docs", Some(root), "Documents", true, 5_400_000, 300, 0, false);
    add(MINE, "f-proj", Some(root), "Projects", true, 12_000_000, 900, 0, true);
    add(MINE, "f-photos", Some(root), "Photos", true, 48_000_000, 4_000, 0, false);
    add(MINE, "f-arch", Some(root), "Archive", true, 90_000_000, 20_000, 0, false);
    add(MINE, "f-big", Some(root), "Exports 2019", true, 400_000_000, 300_000, 3, false);
    add(MINE, "i-welcome", Some(root), "Welcome.docx", false, 24_576, 45, 1, true);
    add(MINE, "i-notes", Some(root), "Meeting notes.one", false, 110_000, 2_000, 0, false);
    add(MINE, "i-exp", Some(root), "Expenses.xlsx", false, 58_000, 3_000, 2, false);
    add(MINE, "i-readme", Some(root), "readme.md", false, 2_100, 50_000, 0, false);
    add(MINE, "i-q4", Some("f-docs"), "Q4 plan.docx", false, 81_920, 20, 1, true);
    add(MINE, "i-budget", Some("f-docs"), "Budget 2027.xlsx", false, 220_000, 1_500, 2, false);
    add(MINE, "i-deck", Some("f-docs"), "Board deck.pptx", false, 3_400_000, 2_900, 0, false);
    add(MINE, "i-txt", Some("f-docs"), "Notes.txt", false, 900, 9_000, 0, false);
    add(MINE, "i-contract", Some("f-docs"), "Contract.pdf", false, 640_000, 12_000, 4, false);
    add(MINE, "f-drafts", Some("f-docs"), "Drafts", true, 40_000, 15_000, 0, false);
    add(MINE, "i-prop", Some("f-drafts"), "Proposal draft.docx", false, 40_000, 15_000, 0, false);
    add(MINE, "f-apollo", Some("f-proj"), "Apollo", true, 9_000_000, 900, 1, true);
    add(MINE, "i-road", Some("f-apollo"), "Roadmap.xlsx", false, 300_000, 900, 1, false);
    add(MINE, "i-arch", Some("f-apollo"), "Architecture.vsdx", false, 1_200_000, 4_000, 0, false);
    add(MINE, "i-time", Some("f-apollo"), "Timeline.mpp", false, 700_000, 7_000, 2, false);
    add(MINE, "i-team", Some("f-photos"), "Team offsite.jpg", false, 4_200_000, 4_000, 0, false);
    add(MINE, "i-wb", Some("f-photos"), "Whiteboard.png", false, 1_900_000, 8_000, 0, false);
    add(MINE, "i-demo", Some("f-photos"), "Demo.mp4", false, 42_000_000, 9_500, 0, false);
    add(MINE, "i-zip", Some("f-arch"), "Old export.zip", false, 90_000_000, 20_000, 0, false);
    for i in 0..1_200usize {
        let topic = TOPICS.get(i % TOPICS.len()).copied().unwrap_or("File");
        let ext = EXT.get(i % EXT.len()).copied().unwrap_or("txt");
        let name = format!("{topic} {}.{ext}", i / TOPICS.len() + 1);
        add(
            MINE,
            &format!("i-bulk-{i}"),
            Some("f-big"),
            &name,
            false,
            1_024 * ((i as u64 * 37) % 9_000 + 12),
            300_000 + i as u64 * 60,
            i * 7,
            false,
        );
    }
    for (d, prefix) in [(LIB_A, "mk"), (LIB_B, "br"), (LIB_C, "en")] {
        add(d, &format!("{prefix}-pol"), Some(root), "Policies", true, 60_000, 2_000, 0, false);
        add(d, &format!("{prefix}-hb"), Some(root), "Handbook.docx", false, 410_000, 600, 1, false);
        add(d, &format!("{prefix}-bg"), Some(root), "Brand guide.pdf", false, 5_100_000, 6_000, 4, false);
        add(d, &format!("{prefix}-q3"), Some(root), "Q3 results.xlsx", false, 180_000, 20_000, 2, false);
        add(
            d,
            &format!("{prefix}-tp"),
            Some(&format!("{prefix}-pol")),
            "Travel policy.docx",
            false,
            60_000,
            2_000,
            0,
            false,
        );
    }
    add(OTHER, "s-launch", Some("x"), "Launch checklist.xlsx", false, 72_000, 120, 1, true);
    add(OTHER, "s-design", Some("x"), "Design review.pptx", false, 6_200_000, 1_400, 1, true);
    add(OTHER, "s-int", Some("x"), "Interview notes.docx", false, 38_000, 9_000, 2, true);
    add(OTHER, "s-assets", Some("x"), "Shared assets", true, 88_000_000, 15_000, 4, true);
}

fn ext(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, e)) if !stem.is_empty() => e.to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn kind(name: &str, folder: bool) -> &'static str {
    if folder {
        return "folder";
    }
    match ext(name).as_str() {
        "doc" | "docx" | "rtf" => "word",
        "xls" | "xlsx" | "csv" => "excel",
        "ppt" | "pptx" => "powerpoint",
        "one" => "onenote",
        "vsdx" => "visio",
        "mpp" => "project",
        "pdf" => "pdf",
        "png" | "jpg" | "jpeg" | "gif" => "image",
        "mp4" | "mov" => "video",
        "mp3" | "wav" => "audio",
        "zip" => "archive",
        "rs" | "json" | "js" => "code",
        "txt" | "md" => "text",
        _ => "file",
    }
}

fn label(age: u64) -> String {
    if age < 600 {
        let h = 14u64.saturating_sub(age / 60);
        format!("{h:02}:{:02}", 59 - age % 60)
    } else {
        let days = age / 1_440;
        let months = ["Oct", "Sep", "Aug", "Jul", "Jun", "May", "Apr", "Mar", "Feb", "Jan"];
        let m = months.get(usize::try_from(days / 30).unwrap_or(0) % months.len()).copied().unwrap_or("Oct");
        let d = 28 - days % 28;
        if days > 300 { format!("{m} {d}, 2025") } else { format!("{m} {d}") }
    }
}

fn rid(it: &Item) -> String {
    format!("{}|{}", it.drive, it.id)
}

fn row(s: &State, it: &Item) -> Value {
    let count = s.items.iter().filter(|c| c.drive == it.drive && c.parent.as_deref() == Some(it.id.as_str())).count();
    json!({
        "id": rid(it),
        "drive": it.drive,
        "item": it.id,
        "name": it.name,
        "folder": it.folder,
        "child_count": count,
        "ext": ext(&it.name),
        "kind": kind(&it.name, it.folder),
        "size": it.size,
        "modified": Value::Null,
        "modified_label": label(it.age),
        "modified_by": it.by,
        "shared": it.shared,
        "web_url": format!("https://contoso.example/preview/{}", it.id),
        "parent": it.parent.as_ref().map(|p| format!("{}|{p}", it.drive)),
        "pending": it.pending,
    })
}

fn sorted(mut v: Vec<Item>, sort: &str, desc: bool) -> Vec<Item> {
    v.sort_by(|a, b| {
        let by_name = || a.name.to_lowercase().cmp(&b.name.to_lowercase());
        let key = match sort {
            "modified" => b.age.cmp(&a.age).then_with(by_name),
            "size" => a.size.cmp(&b.size).then_with(by_name),
            "modified_by" => a.by.cmp(&b.by).then_with(by_name),
            _ => by_name(),
        };
        b.folder.cmp(&a.folder).then(if desc { key.reverse() } else { key })
    });
    v
}

fn location_drive(loc: &str) -> Option<&'static str> {
    match loc {
        "mine" => Some(MINE),
        "drive:b!preview-marketing" => Some(LIB_A),
        "drive:b!preview-brand" => Some(LIB_B),
        "drive:b!preview-engineering" => Some(LIB_C),
        _ => None,
    }
}

fn drive_label(d: &str) -> &'static str {
    match d {
        MINE => "My files",
        LIB_B => "Brand assets",
        _ => "Documents",
    }
}

fn drive_location(d: &str) -> String {
    if d == MINE { "mine".to_owned() } else { format!("drive:{d}") }
}

fn children(s: &State, params: &Value) -> Value {
    let text = |k: &str| params.get(k).and_then(Value::as_str).map(str::to_owned);
    let sort = text("sort").unwrap_or_else(|| "name".to_owned());
    let desc = params.get("desc").and_then(Value::as_bool).unwrap_or(false);
    let (location, drive, folder, items, crumbs): (String, Option<&str>, Option<String>, Vec<Item>, Vec<Value>) =
        match (text("folder"), text("location").unwrap_or_else(|| "mine".to_owned())) {
            (Some(f), _) => {
                let (d, i) = f.split_once('|').unwrap_or(("", ""));
                let drive = [MINE, LIB_A, LIB_B, LIB_C, OTHER].into_iter().find(|x| *x == d);
                let items: Vec<Item> = s
                    .items
                    .iter()
                    .filter(|x| Some(x.drive) == drive && x.parent.as_deref() == Some(i))
                    .cloned()
                    .collect();
                let mut chain = Vec::new();
                let mut cur = Some(i.to_owned());
                for _ in 0..32 {
                    let Some(c) = cur.take() else { break };
                    if let Some(it) = s.items.iter().find(|x| Some(x.drive) == drive && x.id == c) {
                        chain.push(json!({ "id": rid(it), "name": it.name }));
                        cur = it.parent.clone();
                    }
                }
                chain.push(json!({ "id": drive_location(d), "name": drive_label(d) }));
                chain.reverse();
                (drive_location(d), drive, Some(f), items, chain)
            }
            (None, loc) if loc == "recent" => {
                let mut v: Vec<Item> = s.items.iter().filter(|x| !x.folder && x.age < 3_000).cloned().collect();
                v.sort_by_key(|x| x.age);
                (loc, None, None, v, vec![json!({ "id": "recent", "name": "Recent" })])
            }
            (None, loc) if loc == "shared" => {
                let v: Vec<Item> = s.items.iter().filter(|x| x.drive == OTHER).cloned().collect();
                (loc, None, None, v, vec![json!({ "id": "shared", "name": "Shared with me" })])
            }
            (None, loc) => {
                let d = location_drive(&loc).unwrap_or(MINE);
                let v: Vec<Item> =
                    s.items.iter().filter(|x| x.drive == d && x.parent.as_deref() == Some("root")).cloned().collect();
                let crumbs = vec![json!({ "id": drive_location(d), "name": drive_label(d) })];
                (drive_location(d), Some(d), Some(format!("{d}|root")), v, crumbs)
            }
        };
    let items = if location == "recent" && text("sort").is_none() { items } else { sorted(items, &sort, desc) };
    json!({
        "location": location,
        "drive": drive,
        "folder": folder,
        "breadcrumb": crumbs,
        "items": items.iter().map(|it| row(s, it)).collect::<Vec<_>>(),
        "total": items.len(),
        "next": Value::Null,
        "syncing": false,
        "sort": sort,
        "desc": desc,
    })
}

fn locations() -> Value {
    json!([
        { "id": "mine", "name": "My files", "kind": "mine", "depth": 0, "drive": MINE },
        { "id": "recent", "name": "Recent", "kind": "recent", "depth": 0 },
        { "id": "shared", "name": "Shared with me", "kind": "shared", "depth": 0 },
        { "id": "sites", "name": "Sites", "kind": "header", "depth": 0 },
        { "id": "site:mk", "name": "Marketing Hub", "kind": "site", "depth": 1, "site": "mk" },
        { "id": format!("drive:{LIB_A}"), "name": "Documents", "kind": "library", "depth": 2, "site": "mk", "drive": LIB_A },
        { "id": format!("drive:{LIB_B}"), "name": "Brand assets", "kind": "library", "depth": 2, "site": "mk", "drive": LIB_B },
        { "id": "list:mk|campaigns", "name": "Campaign tracker", "kind": "list", "depth": 2, "site": "mk", "list": "campaigns" },
        { "id": "site:en", "name": "Engineering", "kind": "site", "depth": 1, "site": "en" },
        { "id": format!("drive:{LIB_C}"), "name": "Documents", "kind": "library", "depth": 2, "site": "en", "drive": LIB_C },
        { "id": "list:en|releases", "name": "Release checklist", "kind": "list", "depth": 2, "site": "en", "list": "releases" },
    ])
}

fn details(s: &State, id: &str) -> Option<Value> {
    let it = s.items.iter().find(|x| rid(x) == id)?;
    let shared = it.shared;
    let permissions = if shared {
        json!([
            { "id": "p-link", "roles": ["read"], "kind": "link", "link_type": "view", "link_scope": "organization", "granted_to": [], "inherited": false },
            { "id": "p-user", "roles": ["write"], "kind": "direct", "granted_to": [person(4).0], "inherited": false },
            { "id": "p-user2", "roles": ["read"], "kind": "direct", "granted_to": [person(9).0], "inherited": true },
        ])
    } else {
        json!([])
    };
    let versions: Vec<Value> = if it.folder {
        Vec::new()
    } else {
        (0..4u64)
            .map(|v| json!({ "id": format!("{}.0", 4 - v), "modified_label": label(it.age + v * 1_700), "modified_by": person(usize::try_from(v).unwrap_or(0) * 3).0, "size": it.size.saturating_sub(v * 1_000) }))
            .collect()
    };
    let desktop = match kind(&it.name, it.folder) {
        "word" => Some("ms-word"),
        "excel" => Some("ms-excel"),
        "powerpoint" => Some("ms-powerpoint"),
        "visio" => Some("ms-visio"),
        "project" => Some("ms-project"),
        "onenote" => Some("ms-onenote"),
        _ => None,
    };
    Some(json!({
        "item": row(s, it),
        "created_label": label(it.age + 20_000),
        "permissions": permissions,
        "sharing_summary": if shared { "Shared with 2 people and 1 link" } else { "Only you" },
        "versions_count": versions.len(),
        "versions": versions,
        "desktop_app": desktop,
        "stale": false,
    }))
}

fn list_items(list: &str) -> Value {
    let statuses = ["Not started", "In progress", "Completed"];
    let rows: Vec<Value> = (0..40usize)
        .map(|i| {
            let topic =
                TOPICS.get((i + if list == "releases" { 5 } else { 0 }) % TOPICS.len()).copied().unwrap_or("Task");
            json!({
                "id": (i + 1).to_string(),
                "fields": {
                    "Title": format!("{topic} {}", i / TOPICS.len() + 1),
                    "Status": statuses.get(i % 3).copied().unwrap_or(""),
                    "Priority": i % 3 + 1,
                    "DueDate": format!("2026-{:02}-{:02}T00:00:00Z", 10 + i % 3, i % 27 + 1),
                    "Done": i % 3 == 2,
                    "AssignedTo": person(i * 5).0,
                },
                "modified_label": label(u64::try_from(i).unwrap_or(0) * 700),
                "pending": false,
            })
        })
        .collect();
    json!({
        "site": "mk",
        "list": list,
        "columns": [
            { "name": "Title", "display_name": "Title", "kind": "text", "required": true },
            { "name": "Status", "display_name": "Status", "kind": "choice", "choices": statuses },
            { "name": "Priority", "display_name": "Priority", "kind": "number" },
            { "name": "DueDate", "display_name": "Due date", "kind": "dateTime" },
            { "name": "Done", "display_name": "Done", "kind": "boolean" },
            { "name": "AssignedTo", "display_name": "Assigned to", "kind": "personOrGroup" },
        ],
        "items": rows,
        "next": Value::Null,
        "error": Value::Null,
    })
}

fn ok(value: Value) -> Reply {
    Reply::Ok { value }
}

fn err(kind: &str, message: &str) -> Reply {
    Reply::Err { kind: kind.into(), message: message.into() }
}

/// Answers a files query.
pub fn query(id: &str, params: &Value) -> Option<Reply> {
    let text = |k: &str| params.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(match id {
        "files.locations" => ok(locations()),
        "files.children" => ok(with(|s| children(s, params))?),
        "files.item" => {
            let key = text("item").or_else(|| text("id")).unwrap_or_default();
            match with(|s| details(s, &key))? {
                Some(v) => ok(v),
                None => err("not_found", "that file is not in the cache"),
            }
        }
        "files.versions" => {
            let key = text("item").or_else(|| text("id")).unwrap_or_default();
            ok(
                json!({ "versions": with(|s| details(s, &key))?.and_then(|d| d.get("versions").cloned()), "stale": false }),
            )
        }
        "files.preview_url" => err("not_available", "Previews need the desktop app."),
        "files.search" => {
            let q = text("query").unwrap_or_default().to_lowercase();
            let rows = with(|s| {
                s.items
                    .iter()
                    .filter(|x| !q.is_empty() && x.name.to_lowercase().contains(&q))
                    .take(200)
                    .map(|x| row(s, x))
                    .collect::<Vec<_>>()
            })?;
            ok(json!({ "items": rows, "remote": false }))
        }
        "files.uploads" => {
            let rows = with(|s| {
                for u in &mut s.uploads {
                    u.2 = (u.2 + u.3 / 3).min(u.3);
                }
                s.uploads
                    .iter()
                    .map(|(op, name, sent, total)| {
                        json!({ "op": op, "name": name, "sent": sent, "total": total, "state": if sent >= total { "done" } else { "uploading" }, "updated": 0 })
                    })
                    .collect::<Vec<_>>()
            })?;
            ok(Value::Array(rows))
        }
        "sites.list" => ok(json!([{ "id": "mk", "name": "Marketing Hub" }, { "id": "en", "name": "Engineering" }])),
        "sites.lists" => {
            ok(json!([{ "id": "campaigns", "name": "Campaign tracker", "template": "genericList", "library": false }]))
        }
        "lists.items" => ok(list_items(&text("list").unwrap_or_default())),
        _ => return None,
    })
}

fn changed() -> CoreEvent {
    CoreEvent::StoreChanged { tables: vec!["drive_item".into(), "setting".into()] }
}

fn toast(level: &str, message: impl Into<String>) -> CoreEvent {
    CoreEvent::Toast { level: level.into(), message: message.into() }
}

fn split(key: &str) -> Option<(String, String)> {
    key.split_once('|').map(|(d, i)| (d.to_owned(), i.to_owned()))
}

/// Runs a files command against the preview data.
pub fn run(id: &str, params: &Value) -> Option<(Reply, Vec<CoreEvent>)> {
    let text = |k: &str| params.get(k).and_then(Value::as_str).map(str::to_owned);
    let target = text("item").or_else(|| text("id"));
    let need = || (err("invalid_params", "Select a file first."), vec![]);
    Some(match id {
        "files.new_folder" | "files.upload" => {
            let (drive, parent) =
                text("folder").and_then(|f| split(&f)).unwrap_or_else(|| (MINE.to_owned(), "root".to_owned()));
            let drive = [MINE, LIB_A, LIB_B, LIB_C].into_iter().find(|d| *d == drive).unwrap_or(MINE);
            let names: Vec<(String, u64)> = if id == "files.new_folder" {
                vec![(text("name").unwrap_or_else(|| "New folder".to_owned()), 0)]
            } else {
                params
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|f| {
                                Some((
                                    f.get("name")?.as_str()?.to_owned(),
                                    f.get("data")?.as_str()?.len() as u64 * 3 / 4,
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            if names.is_empty() {
                return Some((err("invalid_params", "`paths` or `files` is required"), vec![]));
            }
            let folder = id == "files.new_folder";
            let made = with(|s| {
                let mut ids = Vec::new();
                for (name, size) in names {
                    s.next = s.next.wrapping_add(1);
                    let local = format!("local-{}", s.next);
                    if !folder {
                        s.uploads.push((local.clone(), name.clone(), 0, size.max(1)));
                    }
                    s.items.push(Item {
                        drive,
                        id: local.clone(),
                        parent: Some(parent.clone()),
                        name,
                        folder,
                        size,
                        age: 0,
                        by: "Avery Example".into(),
                        shared: false,
                        pending: false,
                    });
                    ids.push(format!("{drive}|{local}"));
                }
                ids
            })?;
            let msg = if folder { "Folder created".to_owned() } else { format!("Uploading {} file(s)", made.len()) };
            (ok(json!({ "id": made.first(), "queued": made })), vec![changed(), toast("success", msg)])
        }
        "files.rename" => {
            let (Some(t), Some(name)) = (target, text("name")) else { return Some(need()) };
            with(|s| {
                if let Some(it) = s.items.iter_mut().find(|x| rid(x) == t) {
                    it.name = name;
                    it.age = 0;
                }
            })?;
            (ok(json!({ "outbox": 1 })), vec![changed()])
        }
        "files.move" | "files.copy" => {
            let (Some(t), Some(folder)) = (target, text("folder")) else { return Some(need()) };
            let Some((_, parent)) = split(&folder) else {
                return Some((err("invalid_params", "Pick a folder."), vec![]));
            };
            with(|s| {
                if let Some(pos) = s.items.iter().position(|x| rid(x) == t) {
                    if id == "files.move" {
                        if let Some(it) = s.items.get_mut(pos) {
                            it.parent = Some(parent);
                        }
                    } else if let Some(mut copy) = s.items.get(pos).cloned() {
                        s.next = s.next.wrapping_add(1);
                        copy.id = format!("copy-{}", s.next);
                        copy.parent = Some(parent);
                        s.items.push(copy);
                    }
                }
            })?;
            let msg = if id == "files.move" { "Moved" } else { "Copied" };
            (ok(json!({ "outbox": 1 })), vec![changed(), toast("success", msg)])
        }
        "files.delete" => {
            let mut keys: Vec<String> = params
                .get("items")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            if keys.is_empty() {
                let Some(t) = target else { return Some(need()) };
                keys.push(t);
            }
            with(|s| s.items.retain(|x| !keys.contains(&rid(x))))?;
            (ok(json!({ "outbox": [1] })), vec![changed(), toast("success", "Moved to the recycle bin")])
        }
        "files.share" => {
            let Some(t) = target else { return Some(need()) };
            with(|s| {
                if let Some(it) = s.items.iter_mut().find(|x| rid(x) == t) {
                    it.shared = true;
                }
            })?;
            (ok(json!({ "outbox": 1 })), vec![changed(), toast("success", "Shared")])
        }
        "files.copy_link" => {
            let Some(t) = target else { return Some(need()) };
            (ok(json!({ "url": format!("https://contoso.example/s/{}", t.replace('|', "-")) })), vec![])
        }
        "files.download" => {
            let Some(t) = target else { return Some(need()) };
            let name = with(|s| s.items.iter().find(|x| rid(x) == t).map(|x| x.name.clone()))?.unwrap_or_default();
            (ok(json!({ "path": format!("~/Downloads/{name}") })), vec![toast("success", format!("Downloaded {name}"))])
        }
        "files.open" | "files.open_in_browser" | "files.open_in_desktop" => {
            let Some(t) = target else { return Some(need()) };
            let it = with(|s| s.items.iter().find(|x| rid(x) == t).cloned())?;
            let Some(it) = it else { return Some((err("not_found", "that file is not in the cache"), vec![])) };
            let office =
                matches!(kind(&it.name, it.folder), "word" | "excel" | "powerpoint" | "visio" | "project" | "onenote");
            let ev = if it.folder && id == "files.open" {
                CoreEvent::Ui { command: "files.show".into(), params: json!({ "id": t, "folder": true }) }
            } else if id == "files.open_in_browser" || (office && id == "files.open") || id == "files.open_in_desktop" {
                let target = if id == "files.open_in_browser" { "browser" } else { "desktop" };
                CoreEvent::Ui {
                    command: "files.launch".into(),
                    params: json!({ "url": format!("https://contoso.example/preview/{}", it.id), "target": target, "opened": false }),
                }
            } else {
                CoreEvent::Ui {
                    command: "files.show".into(),
                    params: json!({ "id": t, "folder": false, "preview": true }),
                }
            };
            (ok(Value::Null), vec![ev])
        }
        "files.follow_site"
        | "files.unfollow_site"
        | "files.unshare"
        | "files.restore_version"
        | "files.checkout"
        | "files.checkin"
        | "files.versions" => (ok(json!({ "outbox": 1 })), vec![changed(), toast("info", "Done (preview data)")]),
        "lists.create_item" | "lists.update_item" | "lists.delete_item" => (
            ok(json!({ "outbox": 1 })),
            vec![CoreEvent::StoreChanged { tables: vec!["list_item".into()] }, toast("info", "Saved (preview data)")],
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(r: Option<Reply>) -> Value {
        match r {
            Some(Reply::Ok { value }) => value,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_query_answers_with_engine_shapes() {
        for q in QUERIES {
            assert!(
                query(q, &json!({ "item": "b!preview-mine|i-q4", "list": "campaigns", "query": "plan" })).is_some(),
                "{q}"
            );
        }
        let root = value(query("files.children", &json!({ "location": "mine" })));
        let items = root["items"].as_array().unwrap();
        assert_eq!(items[0]["folder"], true);
        for key in [
            "id",
            "drive",
            "item",
            "name",
            "folder",
            "kind",
            "size",
            "modified_label",
            "modified_by",
            "shared",
            "pending",
        ] {
            assert!(items[0].get(key).is_some(), "{key}");
        }
        let big = value(query("files.children", &json!({ "folder": "b!preview-mine|f-big" })));
        assert_eq!(big["items"].as_array().unwrap().len(), 1_200);
        assert_eq!(big["breadcrumb"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn commands_change_the_preview_data() {
        let (r, ev) = run("files.new_folder", &json!({ "folder": "b!preview-mine|root", "name": "Plans" })).unwrap();
        assert!(matches!(r, Reply::Ok { .. }));
        assert!(!ev.is_empty());
        let root = value(query("files.children", &json!({ "location": "mine" })));
        assert!(root["items"].as_array().unwrap().iter().any(|r| r["name"] == "Plans"));
        let (r, _) = run("files.delete", &json!({ "item": "b!preview-mine|i-readme" })).unwrap();
        assert!(matches!(r, Reply::Ok { .. }));
        assert!(matches!(run("files.rename", &Value::Null), Some((Reply::Err { .. }, _))));
        let (_, ev) = run("files.open", &json!({ "item": "b!preview-mine|f-docs" })).unwrap();
        assert!(matches!(ev.first(), Some(CoreEvent::Ui { command, .. }) if command == "files.show"));
        assert!(run("nope", &Value::Null).is_none());
    }
}
