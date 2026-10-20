//! The details pane of a file or folder (`files.item`): preview, facts, sharing and versions.
//!
//! The preview is Graph's embeddable viewer (`files.preview_url`) in a sandboxed iframe in the
//! desktop app; the browser preview shows a placeholder instead (no tenant).

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use super::files::{FileRow, TABLES, copy_to_clipboard, exec, format_size};
use super::files_icons::FileIcon;
use crate::components::{EmptyState, Icon, Spinner};
use crate::icons::IconName;
use crate::state::{QueryState, ToastLevel, app_state, use_query};

/// A permission (`files.item`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PermissionRow {
    /// Permission id.
    pub id: String,
    /// Roles.
    #[serde(default)]
    pub roles: Vec<String>,
    /// `link` or `direct`.
    #[serde(default)]
    pub kind: String,
    /// `view` / `edit`.
    #[serde(default)]
    pub link_type: Option<String>,
    /// `organization` / `users` / `anonymous`.
    #[serde(default)]
    pub link_scope: Option<String>,
    /// Who.
    #[serde(default)]
    pub granted_to: Vec<String>,
    /// Inherited.
    #[serde(default)]
    pub inherited: bool,
}

/// A version (`files.item`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct VersionRow {
    /// Version id.
    pub id: String,
    /// When.
    #[serde(default)]
    pub modified_label: String,
    /// Who.
    #[serde(default)]
    pub modified_by: Option<String>,
    /// Size.
    #[serde(default)]
    pub size: u64,
}

/// `files.item`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ItemDetails {
    /// The row.
    pub item: FileRow,
    /// Created label.
    #[serde(default)]
    pub created_label: String,
    /// Permissions.
    #[serde(default)]
    pub permissions: Vec<PermissionRow>,
    /// "Shared with 2 people".
    #[serde(default)]
    pub sharing_summary: String,
    /// Versions, newest first.
    #[serde(default)]
    pub versions: Vec<VersionRow>,
    /// The Office scheme that opens it, if any.
    #[serde(default)]
    pub desktop_app: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct PreviewUrl {
    url: String,
}

/// Kinds Graph's viewer renders.
fn previewable(kind: &str) -> bool {
    matches!(kind, "word" | "excel" | "powerpoint" | "pdf" | "image" | "video" | "audio" | "text" | "code" | "visio")
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "folder" => "Folder",
        "word" => "Document",
        "excel" => "Spreadsheet",
        "powerpoint" => "Presentation",
        "onenote" => "Notebook",
        "visio" => "Diagram",
        "project" => "Project plan",
        "pdf" => "PDF document",
        "image" => "Image",
        "video" => "Video",
        "audio" => "Audio",
        "archive" => "Archive",
        "code" => "Source code",
        "text" => "Text",
        _ => "File",
    }
}

fn scope_label(p: &PermissionRow) -> String {
    let what = if p.link_type.as_deref() == Some("edit") { "can edit" } else { "can view" };
    match p.link_scope.as_deref() {
        Some("anonymous") => format!("Anyone with the link {what}"),
        Some("organization") => format!("People in your organization {what}"),
        Some("users") => format!("Specific people {what}"),
        _ => format!("Link: {what}"),
    }
}

#[component]
fn Preview(id: String, kind: String) -> impl IntoView {
    let state = app_state();
    if state.preview || !previewable(&kind) {
        let note = if state.preview { "Previews show in the desktop app." } else { "No preview for this type." };
        return view! {
            <div class="files-preview placeholder">
                <FileIcon kind=kind class="huge" />
                <span class="files-preview-note">{note}</span>
            </div>
        }
        .into_any();
    }
    let url = use_query::<PreviewUrl>("files.preview_url", move || json!({ "item": id }), &[]);
    view! {
        <div class="files-preview">
            {move || match url.state.get() {
                QueryState::Ready(u) if u.url.starts_with("https://") => view! {
                    <iframe class="files-preview-frame" src=u.url.clone() title="Preview" sandbox="allow-scripts allow-same-origin allow-forms allow-popups" referrerpolicy="no-referrer"></iframe>
                }
                .into_any(),
                QueryState::Loading => view! { <Spinner /> }.into_any(),
                _ => view! {
                    <div class="files-preview placeholder">
                        <FileIcon kind=kind.clone() class="huge" />
                        <span class="files-preview-note">"Preview unavailable"</span>
                    </div>
                }
                .into_any(),
            }}
        </div>
    }
    .into_any()
}

/// The details of row `id`.
#[component]
pub fn Details(
    /// `"<drive>|<item>"`.
    id: String,
) -> impl IntoView {
    let state = app_state();
    let key = id.clone();
    let details = use_query::<ItemDetails>("files.item", move || json!({ "item": key }), TABLES);
    move || match details.state.get() {
        QueryState::Loading => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
        QueryState::Failed { message, .. } => {
            view! { <EmptyState title="No details yet" detail=message icon=IconName::File /> }.into_any()
        }
        QueryState::Ready(d) => {
            let it = d.item.clone();
            let rid = it.id.clone();
            let (r_open, r_link, r_share, r_desk, r_web) =
                (rid.clone(), rid.clone(), rid.clone(), rid.clone(), rid.clone());
            let size = if it.folder { format!("{} items", it.child_count) } else { format_size(it.size) };
            let versions = d.versions.clone();
            let perms = d.permissions.clone();
            let desktop = d.desktop_app.is_some();
            view! {
                <article class="reading files-details">
                    <header class="files-details-head">
                        <FileIcon kind=it.kind.clone() class="large" />
                        <div class="files-details-title">
                            <h1 class="reading-title">{it.name.clone()}</h1>
                            <span class="files-details-kind">{kind_label(&it.kind)}{it.pending.then_some(" · uploading")}</span>
                        </div>
                    </header>
                    <div class="files-details-actions">
                        <button class="files-button primary" on:click=move |_| crate::commands::dispatch(state, "files.open", json!({ "item": r_open }))>
                            {if it.folder { "Open" } else if desktop { "Open in app" } else { "Open" }}
                        </button>
                        {desktop.then(|| view! {
                            <button class="files-button" on:click=move |_| crate::commands::dispatch(state, "files.open_in_browser", json!({ "item": r_web }))>"Open in browser"</button>
                        })}
                        {(!desktop && !it.folder).then(|| view! {
                            <button class="files-button" on:click=move |_| crate::commands::dispatch(state, "files.open_in_browser", json!({ "item": r_desk }))>"Open in browser"</button>
                        })}
                        <button class="files-button" on:click=move |_| crate::commands::dispatch(state, "files.share", json!({ "id": r_share }))>"Share"</button>
                        <button class="files-button" title="Copy link" on:click=move |_| crate::commands::dispatch(state, "files.copy_link", json!({ "item": r_link }))>
                            "Copy link"
                        </button>
                    </div>
                    {(!it.folder).then(|| view! { <Preview id=rid.clone() kind=it.kind.clone() /> })}
                    <section class="files-section">
                        <h2>"Details"</h2>
                        <dl class="facts">
                            <dt>"Type"</dt><dd>{kind_label(&it.kind)}</dd>
                            <dt>"Size"</dt><dd>{size}</dd>
                            <dt>"Modified"</dt><dd>{format!("{} by {}", it.modified_label, it.modified_by)}</dd>
                            {(!d.created_label.is_empty()).then(|| view! { <dt>"Created"</dt><dd>{d.created_label.clone()}</dd> })}
                        </dl>
                    </section>
                    <section class="files-section">
                        <h2>"Sharing"</h2>
                        <p class="files-sharing-summary">
                            <Icon name=if it.shared { IconName::People } else { IconName::Blocked } />
                            {d.sharing_summary.clone()}
                        </p>
                        <ul class="files-perms">
                            {perms
                                .into_iter()
                                .filter(|p| !p.roles.iter().any(|r| r == "owner"))
                                .map(|p| {
                                    let who = if p.kind == "link" { scope_label(&p) } else { p.granted_to.join(", ") };
                                    let role = if p.roles.iter().any(|r| r == "write") { "Can edit" } else { "Can view" };
                                    let (pid, item) = (p.id.clone(), rid.clone());
                                    view! {
                                        <li class="files-perm">
                                            <Icon name=if p.kind == "link" { IconName::Command } else { IconName::People } />
                                            <span class="files-perm-who">{who}</span>
                                            <span class="files-perm-role">{role}{p.inherited.then_some(" · inherited")}</span>
                                            {(!p.inherited).then(|| view! {
                                                <button class="files-link-button" title="Remove access" on:click=move |_| {
                                                    exec(state, "files.unshare", json!({ "item": item, "permission": pid }), move |_| {
                                                        state.toast(ToastLevel::Success, "Access removed");
                                                    });
                                                }>"Remove"</button>
                                            })}
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                    </section>
                    {(!it.folder).then(|| view! {
                        <section class="files-section">
                            <h2>{format!("Versions ({})", versions.len())}</h2>
                            <ul class="files-versions">
                                {versions
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, v)| {
                                        let (vid, item) = (v.id.clone(), rid.clone());
                                        view! {
                                            <li class="files-version">
                                                <span class="files-version-id">{format!("v{}", v.id)}</span>
                                                <span class="files-version-meta">{format!("{} · {} · {}", v.modified_label, v.modified_by.clone().unwrap_or_default(), format_size(v.size))}</span>
                                                {if i == 0 {
                                                    view! { <span class="files-version-current">"Current"</span> }.into_any()
                                                } else {
                                                    view! {
                                                        <button class="files-link-button" on:click=move |_| {
                                                            exec(state, "files.restore_version", json!({ "item": item, "version": vid }), move |_| {
                                                                state.toast(ToastLevel::Success, "Version restored");
                                                            });
                                                        }>"Restore"</button>
                                                    }
                                                    .into_any()
                                                }}
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                        </section>
                    })}
                </article>
            }
            .into_any()
        }
    }
}

/// Copies the link of `id` (used by the share dialog).
pub fn copy_link(state: crate::state::AppState, id: String, link_type: &str, scope: &str) {
    exec(state, "files.copy_link", json!({ "item": id, "type": link_type, "scope": scope }), move |v: Value| {
        if let Some(url) = v.get("url").and_then(Value::as_str) {
            copy_to_clipboard(state, url.to_owned());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(kind_label("excel"), "Spreadsheet");
        assert_eq!(kind_label("weird"), "File");
        assert!(previewable("pdf"));
        assert!(!previewable("archive"));
        let p = PermissionRow {
            id: "1".into(),
            roles: vec!["read".into()],
            kind: "link".into(),
            link_type: Some("edit".into()),
            link_scope: Some("organization".into()),
            granted_to: vec![],
            inherited: false,
        };
        assert_eq!(scope_label(&p), "People in your organization can edit");
    }
}
