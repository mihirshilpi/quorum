//! Dialogs the Files commands open when run from the UI without their parameters: new folder,
//! rename, move/copy (folder picker), share, and delete confirmation. Each confirms by running
//! the engine command with the collected parameters; Enter confirms, Esc cancels.

use leptos::prelude::*;
use serde_json::json;

use super::files::{Children, FileRow, exec};
use super::files_details::copy_link;
use super::files_icons::FileIcon;
use crate::components::{Icon, Spinner};
use crate::icons::IconName;
use crate::state::{AppState, QueryState, ToastLevel, app_state, use_query};

/// An open dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// Name a new folder in `folder` (a folder row id or a location).
    NewFolder {
        /// Where.
        folder: String,
    },
    /// Rename row `id`.
    Rename {
        /// Row id.
        id: String,
        /// Current name.
        name: String,
    },
    /// Pick a destination folder for `id`.
    Move {
        /// Row id.
        id: String,
        /// Its name.
        name: String,
        /// Copy instead of move.
        copy: bool,
        /// The location to start browsing in.
        start: String,
    },
    /// Share `id`.
    Share {
        /// Row id.
        id: String,
        /// Its name.
        name: String,
    },
    /// Confirm deleting `id`.
    Delete {
        /// Row id.
        id: String,
        /// Its name.
        name: String,
        /// A folder (deletes its contents too).
        folder: bool,
    },
}

fn close(dialog: RwSignal<Option<Dialog>>) {
    dialog.set(None);
    request_animation_frame(|| {
        let _ = crate::dom::focus_selector("[data-primary-list]");
    });
}

fn on_keys(
    dialog: RwSignal<Option<Dialog>>,
    confirm: impl Fn() + 'static,
) -> impl Fn(web_sys::KeyboardEvent) + 'static {
    move |e: web_sys::KeyboardEvent| match e.key().as_str() {
        "Escape" => {
            e.prevent_default();
            e.stop_propagation();
            close(dialog);
        }
        "Enter" if !e.shift_key() => {
            e.prevent_default();
            confirm();
        }
        _ => {}
    }
}

fn focus_first_field() {
    request_animation_frame(|| {
        if !crate::dom::focus_selector(".files-dialog input[data-autofocus]") {
            let _ = crate::dom::focus_selector(".files-dialog button.primary");
        }
        // Select the stem of a file name so typing replaces it, as Finder does.
        if let Some(el) = crate::dom::active_element()
            && let Ok(input) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlInputElement>(el)
        {
            let v = input.value();
            let end = match v.rfind('.') {
                Some(i) if i > 0 => i,
                _ => v.chars().count(),
            };
            let _ = input.set_selection_range(0, u32::try_from(end).unwrap_or(0));
        }
    });
}

/// Renders the open dialog, if any.
#[component]
pub fn DialogHost(
    /// The dialog signal from the area.
    dialog: RwSignal<Option<Dialog>>,
) -> impl IntoView {
    let state = app_state();
    move || {
        let d = dialog.get()?;
        focus_first_field();
        let body = match d {
            Dialog::NewFolder { folder } => {
                name_dialog(state, dialog, "New folder", "Create", "New folder".into(), move |name| {
                    exec(state, "files.new_folder", json!({ "folder": folder, "name": name }), move |_| {});
                })
            }
            Dialog::Rename { id, name } => name_dialog(state, dialog, "Rename", "Rename", name, move |name| {
                exec(state, "files.rename", json!({ "item": id, "name": name }), move |_| {});
            }),
            Dialog::Move { id, name, copy, start } => {
                view! { <MoveDialog id=id name=name copy=copy start=start dialog=dialog /> }.into_any()
            }
            Dialog::Share { id, name } => view! { <ShareDialog id=id name=name dialog=dialog /> }.into_any(),
            Dialog::Delete { id, name, folder } => {
                let confirm = move || {
                    exec(state, "files.delete", json!({ "item": id.clone() }), move |_| {
                        state.toast(ToastLevel::Success, "Moved to the recycle bin");
                    });
                    close(dialog);
                };
                let confirm2 = confirm.clone();
                view! {
                    <div class="files-dialog" role="alertdialog" aria-modal="true" aria-label="Delete" tabindex="-1" on:keydown=on_keys(dialog, confirm.clone())>
                        <h2>{format!("Delete “{name}”?")}</h2>
                        <p class="files-dialog-text">
                            {if folder { "The folder and everything in it go to the recycle bin." } else { "It goes to the recycle bin, where you can restore it." }}
                        </p>
                        <div class="files-dialog-actions">
                            <button class="files-button" on:click=move |_| close(dialog)>"Cancel"</button>
                            <button class="files-button primary danger" on:click=move |_| confirm2()>"Delete"</button>
                        </div>
                    </div>
                }
                .into_any()
            }
        };
        Some(view! {
            <div class="files-backdrop" on:mousedown=move |e| {
                if e.target() == e.current_target() {
                    close(dialog);
                }
            }>
                {body}
            </div>
        })
    }
}

fn name_dialog(
    _state: AppState,
    dialog: RwSignal<Option<Dialog>>,
    title: &'static str,
    action: &'static str,
    initial: String,
    run: impl Fn(String) + Clone + 'static,
) -> AnyView {
    let value = RwSignal::new(initial.clone());
    let invalid = move || {
        let v = value.get();
        let t = v.trim();
        t.is_empty() || t.chars().any(|c| c.is_control() || "\"*:<>?/\\|".contains(c)) || t == "." || t == ".."
    };
    let submit = {
        let run = run.clone();
        move || {
            if invalid() {
                return;
            }
            run(value.get_untracked().trim().to_owned());
            close(dialog);
        }
    };
    let submit2 = submit.clone();
    view! {
        <div class="files-dialog" role="dialog" aria-modal="true" aria-label=title on:keydown=on_keys(dialog, submit.clone())>
            <h2>{title}</h2>
            <input
                class="files-input"
                type="text"
                data-autofocus=""
                prop:value=initial
                aria-invalid=move || invalid().to_string()
                on:input=move |e| value.set(event_target_value(&e))
            />
            <p class="files-dialog-hint" class:error=invalid>"Names can't contain \" * : < > ? / \\ |"</p>
            <div class="files-dialog-actions">
                <button class="files-button" on:click=move |_| close(dialog)>"Cancel"</button>
                <button class="files-button primary" disabled=invalid on:click=move |_| submit2()>{action}</button>
            </div>
        </div>
    }
    .into_any()
}

/// Browses folders (`files.children`) to pick a destination.
#[component]
fn MoveDialog(id: String, name: String, copy: bool, start: String, dialog: RwSignal<Option<Dialog>>) -> impl IntoView {
    let state = app_state();
    let at = RwSignal::new(None::<String>);
    let loc = if start == "mine" || start.starts_with("drive:") { start } else { "mine".to_owned() };
    let loc2 = loc.clone();
    let listing = use_query::<Children>(
        "files.children",
        move || match at.get() {
            Some(f) => json!({ "folder": f }),
            None => json!({ "location": loc2 }),
        },
        &["drive_item"],
    );
    let moving = id.clone();
    let folders = move || -> Vec<FileRow> {
        listing
            .data()
            .map(|c| c.items.iter().filter(|r| r.folder && r.id != moving && !r.pending).cloned().collect())
            .unwrap_or_default()
    };
    let here = move || listing.data().and_then(|c| c.folder.clone());
    let crumbs = move || listing.data().map(|c| c.breadcrumb.clone()).unwrap_or_default();
    let verb = if copy { "Copy" } else { "Move" };
    let cmd = if copy { "files.copy" } else { "files.move" };
    let confirm = move || {
        let Some(dest) = here() else { return };
        let what = verb;
        exec(state, cmd, json!({ "item": id.clone(), "folder": dest }), move |_| {
            state.toast(ToastLevel::Success, if what == "Copy" { "Copying…" } else { "Moved" });
        });
        close(dialog);
    };
    let confirm2 = confirm.clone();
    view! {
        <div class="files-dialog files-move" role="dialog" aria-modal="true" aria-label=format!("{verb} {name}") tabindex="-1" on:keydown=on_keys(dialog, confirm.clone())>
            <h2>{format!("{verb} “{name}” to…")}</h2>
            <nav class="files-crumbs small" aria-label="Destination path">
                {move || crumbs().into_iter().enumerate().map(|(i, c)| {
                    let cid = c.id.clone();
                    view! {
                        {(i > 0).then(|| view! { <span class="files-crumb-sep"><Icon name=IconName::ChevronRight /></span> })}
                        <button class="files-crumb" on:click=move |_| at.set(cid.contains('|').then(|| cid.clone()))>{c.name}</button>
                    }
                }).collect_view()}
            </nav>
            <ul class="files-move-list" role="listbox" aria-label="Folders">
                {move || match listing.state.get() {
                    QueryState::Loading => view! { <li class="files-move-empty"><Spinner /></li> }.into_any(),
                    _ => {
                        let list = folders();
                        if list.is_empty() {
                            view! { <li class="files-move-empty">"No folders here"</li> }.into_any()
                        } else {
                            list.into_iter().map(|f| {
                                let fid = f.id.clone();
                                view! {
                                    <li>
                                        <button class="files-move-row" on:dblclick=move |_| at.set(Some(fid.clone())) on:click={
                                            let fid = f.id.clone();
                                            move |_| at.set(Some(fid.clone()))
                                        }>
                                            <FileIcon kind="folder" />
                                            <span>{f.name.clone()}</span>
                                            <Icon name=IconName::ChevronRight />
                                        </button>
                                    </li>
                                }
                            }).collect_view().into_any()
                        }
                    }
                }}
            </ul>
            <div class="files-dialog-actions">
                <button class="files-button" on:click=move |_| close(dialog)>"Cancel"</button>
                <button class="files-button primary" disabled=move || here().is_none() on:click=move |_| confirm2()>{format!("{verb} here")}</button>
            </div>
        </div>
    }
}

/// Share options: a link (view/edit, organization/specific people/anyone) or an invitation.
#[component]
fn ShareDialog(id: String, name: String, dialog: RwSignal<Option<Dialog>>) -> impl IntoView {
    let state = app_state();
    let link_type = RwSignal::new("view".to_owned());
    let scope = RwSignal::new("organization".to_owned());
    let recipients = RwSignal::new(String::new());
    let message = RwSignal::new(String::new());
    let people = move || -> Vec<String> {
        recipients
            .get()
            .split([',', ';', ' ', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let bad_address = move || people().iter().any(|p| !p.contains('@') || p.starts_with('@') || p.ends_with('@'));
    let (id_send, id_copy) = (id.clone(), id.clone());
    let send = move || {
        let to = people();
        if bad_address() {
            return;
        }
        let params = if to.is_empty() {
            json!({ "item": id_send.clone(), "type": link_type.get_untracked(), "scope": scope.get_untracked() })
        } else {
            json!({ "item": id_send.clone(), "type": link_type.get_untracked(), "recipients": to, "message": message.get_untracked() })
        };
        exec(state, "files.share", params, move |_| state.toast(ToastLevel::Success, "Shared"));
        close(dialog);
    };
    let send2 = send.clone();
    let choice = |sig: RwSignal<String>, value: &'static str, label: &'static str| {
        view! {
            <button class="files-seg" class:active=move || sig.get() == value aria-pressed=move || (sig.get() == value).to_string() on:click=move |_| sig.set(value.to_owned())>
                {label}
            </button>
        }
    };
    view! {
        <div class="files-dialog files-share" role="dialog" aria-modal="true" aria-label=format!("Share {name}") on:keydown=on_keys(dialog, send.clone())>
            <h2>{format!("Share “{name}”")}</h2>
            <label class="files-field">
                <span>"People (email addresses)"</span>
                <input class="files-input" type="text" data-autofocus="" placeholder="name@example.com, …" aria-invalid=move || bad_address().to_string() on:input=move |e| recipients.set(event_target_value(&e)) />
            </label>
            {move || (!people().is_empty()).then(|| view! {
                <label class="files-field">
                    <span>"Message (optional)"</span>
                    <input class="files-input" type="text" on:input=move |e| message.set(event_target_value(&e)) />
                </label>
            })}
            <div class="files-field">
                <span>"Permission"</span>
                <div class="files-segmented" role="group">{choice(link_type, "view", "Can view")}{choice(link_type, "edit", "Can edit")}</div>
            </div>
            {move || people().is_empty().then(|| view! {
                <div class="files-field">
                    <span>"Link works for"</span>
                    <div class="files-segmented" role="group">
                        {choice(scope, "organization", "Your organization")}
                        {choice(scope, "users", "Specific people")}
                        {choice(scope, "anonymous", "Anyone")}
                    </div>
                </div>
            })}
            <div class="files-dialog-actions">
                <button class="files-button" on:click=move |_| {
                    copy_link(state, id_copy.clone(), &link_type.get_untracked(), &scope.get_untracked());
                    close(dialog);
                }>"Copy link"</button>
                <span class="files-bar-fill"></span>
                <button class="files-button" on:click=move |_| close(dialog)>"Cancel"</button>
                <button class="files-button primary" disabled=bad_address on:click=move |_| send2()>
                    {move || if people().is_empty() { "Create link" } else { "Send" }}
                </button>
            </div>
        </div>
    }
}
