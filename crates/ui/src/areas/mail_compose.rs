//! The inline compose pane: recipient chips with people autocomplete, subject, a rich-text
//! editor (`contenteditable`, formatted with the browser's editing commands; no handwritten
//! JS), attachments, autosave (`mail.save_draft` one second after typing pauses), and Send
//! (`Cmd+Enter`) / Discard / Close.
//!
//! The pane keeps [`super::mail::MailUi::fields`] current, so `mail.send` / `mail.save_draft`
//! from the keyboard or palette send exactly what is on screen. Pasted content is inserted as
//! plain text so foreign markup never enters the main window's DOM.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::Reply;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wasm_bindgen::JsCast;

use super::mail::{close_compose, mail_ui};
use super::mail_icons::{MIcon, MailIcon};
use super::mail_util::{base64, looks_like_address, size};
use crate::components::{Icon, Spinner};
use crate::icons::IconName;
use crate::state::{AppState, ToastLevel, app_state};

/// The largest attachment the compose pane uploads (the engine's limit).
pub const MAX_ATTACHMENT_BYTES: f64 = 3.0 * 1024.0 * 1024.0;
/// Autosave delay after the last keystroke.
pub const AUTOSAVE_MS: u64 = 1000;
/// The most recipients one field takes.
pub const MAX_RECIPIENTS: usize = 500;

/// A recipient chip.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct Recipient {
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Address.
    #[serde(default)]
    pub address: String,
}

impl Recipient {
    fn label(&self) -> String {
        match &self.name {
            Some(n) if !n.trim().is_empty() => n.clone(),
            _ => self.address.clone(),
        }
    }
}

/// An attachment on the draft.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DraftAttachment {
    /// Attachment id.
    pub id: String,
    /// File name.
    #[serde(default)]
    pub name: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
}

/// A draft (`mail.draft`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DraftView {
    /// Draft id.
    pub draft_id: String,
    /// `new`, `reply`, `reply_all` or `forward`.
    #[serde(default)]
    pub mode: Option<String>,
    /// To.
    #[serde(default)]
    pub to: Vec<Recipient>,
    /// Cc.
    #[serde(default)]
    pub cc: Vec<Recipient>,
    /// Bcc.
    #[serde(default)]
    pub bcc: Vec<Recipient>,
    /// Subject.
    #[serde(default)]
    pub subject: String,
    /// Body (sanitised for the editor).
    #[serde(default)]
    pub body_html: String,
    /// The quoted original (read-only).
    #[serde(default)]
    pub quote_html: Option<String>,
    /// Attachments.
    #[serde(default)]
    pub attachments: Vec<DraftAttachment>,
    /// Being sent.
    #[serde(default)]
    pub sending: bool,
}

/// A people-search suggestion.
#[derive(Debug, Clone, Deserialize, PartialEq)]
struct Suggestion {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    address: Option<String>,
}

impl Suggestion {
    /// The recipient this suggestion stands for, if it has a usable address.
    fn recipient(self) -> Option<Recipient> {
        let address = self.email.or(self.address).filter(|e| looks_like_address(e))?;
        Some(Recipient { name: self.display_name.or(self.name), address })
    }
}

/// A recipient from typed text: `Name <a@x>` or `a@x`. `None` when it is not an address.
pub fn parse_recipient(text: &str) -> Option<Recipient> {
    let t = text.trim().trim_end_matches([',', ';']).trim();
    if let Some((name, rest)) = t.split_once('<') {
        let addr = rest.trim_end_matches('>').trim();
        let name = name.trim().trim_matches('"').trim();
        return looks_like_address(addr)
            .then(|| Recipient { name: (!name.is_empty()).then(|| name.to_owned()), address: addr.to_owned() });
    }
    looks_like_address(t).then(|| Recipient { name: None, address: t.to_owned() })
}

fn recipients_json(list: &[Recipient]) -> Value {
    Value::Array(list.iter().map(|r| json!({ "name": r.name, "address": r.address })).collect())
}

fn html_document() -> Option<web_sys::HtmlDocument> {
    crate::dom::document()?.dyn_into::<web_sys::HtmlDocument>().ok()
}

fn exec(command: &str, value: &str) {
    if let Some(d) = html_document() {
        let _ = d.exec_command_with_show_ui_and_value(command, false, value);
    }
}

/// Only web and mail links are allowed in the editor.
pub fn safe_link(url: &str) -> Option<String> {
    let u = url.trim();
    if u.is_empty() || u.len() > 2048 || u.chars().any(char::is_control) {
        return None;
    }
    let lower = u.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:") {
        Some(u.to_owned())
    } else if looks_like_address(u) {
        Some(format!("mailto:{u}"))
    } else if !lower.contains(':') && u.contains('.') {
        Some(format!("https://{u}"))
    } else {
        None
    }
}

fn insert_link() {
    let Some(w) = web_sys::window() else { return };
    // A user-initiated prompt (toolbar or Cmd+K), never from programmatic calls.
    let Ok(Some(url)) = w.prompt_with_message("Link address") else { return };
    if let Some(u) = safe_link(&url) {
        exec("createLink", &u);
    }
}

#[component]
fn RecipientField(
    label: &'static str,
    list: RwSignal<Vec<Recipient>>,
    on_change: Callback<()>,
    #[prop(optional)] autofocus: bool,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let suggestions = RwSignal::new(Vec::<Recipient>::new());
    let highlighted = RwSignal::new(0usize);
    let generation = StoredValue::new(0u64);
    let input = NodeRef::<leptos::html::Input>::new();
    if autofocus {
        Effect::new(move |_| {
            if let Some(el) = input.get() {
                request_animation_frame(move || {
                    let _ = el.focus();
                });
            }
        });
    }
    let add = move |r: Recipient| {
        list.update(|l| {
            if l.len() < MAX_RECIPIENTS && !l.iter().any(|x| x.address.eq_ignore_ascii_case(&r.address)) {
                l.push(r);
            }
        });
        text.set(String::new());
        suggestions.set(Vec::new());
        on_change.run(());
    };
    let commit = move || -> bool {
        let pick = suggestions.with_untracked(|s| s.get(highlighted.get_untracked()).cloned());
        if let Some(r) = pick {
            add(r);
            return true;
        }
        match parse_recipient(&text.get_untracked()) {
            Some(r) => {
                add(r);
                true
            }
            None => false,
        }
    };
    let search = move |q: String| {
        let n = generation.get_value().wrapping_add(1);
        generation.set_value(n);
        if q.trim().len() < 2 {
            suggestions.set(Vec::new());
            return;
        }
        set_timeout(
            move || {
                if generation.try_get_value() != Some(n) {
                    return;
                }
                spawn_local(async move {
                    let reply = crate::ipc::query("people.search", json!({ "query": q, "limit": 8 })).await;
                    if generation.try_get_value() != Some(n) {
                        return;
                    }
                    let found: Vec<Recipient> = match reply {
                        Reply::Ok { value } => serde_json::from_value::<Vec<Suggestion>>(value)
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(Suggestion::recipient)
                            .collect(),
                        Reply::Err { .. } => Vec::new(),
                    };
                    highlighted.try_set(0);
                    suggestions.try_set(found);
                });
            },
            Duration::from_millis(150),
        );
    };
    let list_id = crate::dom::unique_id("mail-suggest");
    let list_id2 = list_id.clone();
    view! {
        <div class="mail-field">
            <span class="mail-field-label">{label}</span>
            <div class="mail-chips" on:click=move |_| {
                if let Some(el) = input.try_get_untracked().flatten() {
                    let _ = el.focus();
                }
            }>
                {move || {
                    list.get()
                        .into_iter()
                        .enumerate()
                        .map(|(i, r)| {
                            let title = r.address.clone();
                            view! {
                                <span class="mail-chip" title=title>
                                    <span class="mail-chip-label">{r.label()}</span>
                                    <button class="mail-chip-remove" aria-label=format!("Remove {}", r.label()) tabindex="-1" on:click=move |e: web_sys::MouseEvent| {
                                        e.stop_propagation();
                                        list.update(|l| {
                                            if i < l.len() {
                                                l.remove(i);
                                            }
                                        });
                                        on_change.run(());
                                    }>
                                        <Icon name=IconName::Close />
                                    </button>
                                </span>
                            }
                        })
                        .collect_view()
                }}
                <input
                    node_ref=input
                    class="mail-chip-input"
                    type="text"
                    autocomplete="off"
                    spellcheck="false"
                    aria-label=label
                    aria-autocomplete="list"
                    aria-controls=list_id.clone()
                    prop:value=move || text.get()
                    on:input=move |e| {
                        let v = event_target_value(&e);
                        if v.ends_with([',', ';']) {
                            text.set(v);
                            commit();
                            return;
                        }
                        text.set(v.clone());
                        search(v);
                    }
                    on:keydown=move |e: web_sys::KeyboardEvent| {
                        let n = suggestions.with_untracked(Vec::len);
                        match e.key().as_str() {
                            "Enter" | "Tab" if !e.meta_key() && !e.ctrl_key() => {
                                if !text.get_untracked().trim().is_empty() && commit() {
                                    e.prevent_default();
                                }
                            }
                            "ArrowDown" if n > 0 => {
                                e.prevent_default();
                                highlighted.update(|h| *h = (*h + 1).min(n - 1));
                            }
                            "ArrowUp" if n > 0 => {
                                e.prevent_default();
                                highlighted.update(|h| *h = h.saturating_sub(1));
                            }
                            "Escape" if n > 0 => {
                                e.prevent_default();
                                e.stop_propagation();
                                suggestions.set(Vec::new());
                            }
                            "Backspace" if text.get_untracked().is_empty() => {
                                list.update(|l| {
                                    l.pop();
                                });
                                on_change.run(());
                            }
                            _ => {}
                        }
                    }
                    on:blur=move |_| {
                        if parse_recipient(&text.get_untracked()).is_some() {
                            suggestions.set(Vec::new());
                            commit();
                        }
                    }
                />
            </div>
            {move || {
                let items = suggestions.get();
                (!items.is_empty()).then(|| {
                    view! {
                        <div class="mail-suggest" role="listbox" id=list_id2.clone()>
                            {items
                                .into_iter()
                                .enumerate()
                                .map(|(i, r)| {
                                    let pick = r.clone();
                                    view! {
                                        <button
                                            class="mail-suggest-item"
                                            role="option"
                                            class:highlighted=move || highlighted.get() == i
                                            aria-selected=move || (highlighted.get() == i).to_string()
                                            on:mousedown=move |e: web_sys::MouseEvent| {
                                                e.prevent_default();
                                                add(pick.clone());
                                            }
                                        >
                                            <span class="mail-suggest-name">{r.label()}</span>
                                            <span class="mail-suggest-address">{r.address.clone()}</span>
                                        </button>
                                    }
                                })
                                .collect_view()}
                        </div>
                    }
                })
            }}
        </div>
    }
}

fn read_files(
    state: AppState,
    draft_id: String,
    files: web_sys::FileList,
    attachments: RwSignal<Vec<DraftAttachment>>,
) {
    for i in 0..files.length().min(20) {
        let Some(file) = files.get(i) else { continue };
        if file.size() > MAX_ATTACHMENT_BYTES {
            state.toast(ToastLevel::Warning, format!("{} is larger than 3 MB.", file.name()));
            continue;
        }
        let draft_id = draft_id.clone();
        spawn_local(async move {
            let Ok(buf) = wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await else {
                state.toast(ToastLevel::Error, "Couldn't read that file.");
                return;
            };
            let bytes = js_sys::Uint8Array::new(&buf).to_vec();
            let ct = file.type_();
            let params = json!({
                "draft_id": draft_id,
                "name": file.name(),
                "content_type": if ct.is_empty() { Value::Null } else { json!(ct) },
                "data_base64": base64(&bytes),
            });
            match crate::ipc::run("mail.attach", params).await {
                Reply::Ok { value } => {
                    if let Some(a) =
                        value.get("attachment").cloned().and_then(|a| serde_json::from_value::<DraftAttachment>(a).ok())
                    {
                        attachments.try_update(|l| l.push(a));
                    }
                }
                Reply::Err { message, .. } => state.toast(ToastLevel::Error, message),
            }
        });
    }
}

/// The compose pane for `draft_id`.
#[component]
pub fn ComposePane(
    /// The draft to edit.
    draft_id: String,
) -> impl IntoView {
    let state = app_state();
    let ui = mail_ui();
    let loaded = RwSignal::new(None::<Result<DraftView, String>>);
    let to = RwSignal::new(Vec::<Recipient>::new());
    let cc = RwSignal::new(Vec::<Recipient>::new());
    let bcc = RwSignal::new(Vec::<Recipient>::new());
    let show_cc = RwSignal::new(false);
    let subject = RwSignal::new(String::new());
    let attachments = RwSignal::new(Vec::<DraftAttachment>::new());
    let saved = RwSignal::new(None::<&'static str>);
    let editor = NodeRef::<leptos::html::Div>::new();
    let file_input = NodeRef::<leptos::html::Input>::new();
    let save_gen = StoredValue::new(0u64);
    let ready = StoredValue::new(false);

    {
        let id = draft_id.clone();
        spawn_local(async move {
            let reply = crate::ipc::query("mail.draft", json!({ "draft_id": id })).await;
            let r = match reply {
                Reply::Ok { value } => serde_json::from_value::<DraftView>(value).map_err(|e| e.to_string()),
                Reply::Err { message, .. } => Err(message),
            };
            if let Ok(d) = &r {
                to.try_set(d.to.clone());
                cc.try_set(d.cc.clone());
                bcc.try_set(d.bcc.clone());
                show_cc.try_set(!d.cc.is_empty() || !d.bcc.is_empty());
                subject.try_set(d.subject.clone());
                attachments.try_set(d.attachments.clone());
            }
            loaded.try_set(Some(r));
        });
    }
    // Fill the editor once, when it mounts (never re-render it while the user types).
    Effect::new(move |_| {
        let Some(el) = editor.get() else { return };
        if ready.get_value() {
            return;
        }
        let html = loaded.with_untracked(|l| match l {
            Some(Ok(d)) => Some(d.body_html.clone()),
            _ => None,
        });
        if let Some(h) = html {
            el.set_inner_html(&h);
            ready.set_value(true);
            let is_new = loaded.with_untracked(|l| matches!(l, Some(Ok(d)) if d.mode.as_deref() != Some("new")));
            if is_new || !to.get_untracked().is_empty() {
                request_animation_frame(move || {
                    let _ = el.focus();
                });
            }
        }
    });

    let fields_sig = ui.fields.clone();
    let id_fields = draft_id.clone();
    let sync_fields = Callback::new(move |()| {
        let body = editor.try_get_untracked().flatten().map(|e| e.inner_html()).unwrap_or_default();
        fields_sig.set(json!({
            "draft_id": id_fields,
            "to": recipients_json(&to.get_untracked()),
            "cc": recipients_json(&cc.get_untracked()),
            "bcc": recipients_json(&bcc.get_untracked()),
            "subject": subject.get_untracked(),
            "body_html": body,
        }));
    });
    let fields_save = ui.fields.clone();
    let changed = {
        Callback::new(move |()| {
            if !ready.get_value() {
                return;
            }
            sync_fields.run(());
            saved.set(None);
            let n = save_gen.get_value().wrapping_add(1);
            save_gen.set_value(n);
            let fields = fields_save.clone();
            set_timeout(
                move || {
                    if save_gen.try_get_value() != Some(n) {
                        return;
                    }
                    let p = fields.get_untracked();
                    if p.get("draft_id").is_none() {
                        return;
                    }
                    saved.try_set(Some("Saving…"));
                    spawn_local(async move {
                        let reply = crate::ipc::run("mail.save_draft", p).await;
                        if save_gen.try_get_value() == Some(n) {
                            saved.try_set(Some(if matches!(reply, Reply::Ok { .. }) {
                                "Draft saved"
                            } else {
                                "Not saved"
                            }));
                        }
                    });
                },
                Duration::from_millis(AUTOSAVE_MS),
            );
        })
    };

    let send = Callback::new(move |()| {
        if !ready.get_value() {
            return;
        }
        sync_fields.run(());
        save_gen.update_value(|n| *n = n.wrapping_add(1));
        crate::commands::dispatch(state, "mail.send", Value::Null);
    });
    let id_discard = draft_id.clone();
    let discard = Callback::new(move |()| {
        let ok = web_sys::window().and_then(|w| w.confirm_with_message("Discard this draft?").ok()).unwrap_or(false);
        if ok {
            save_gen.update_value(|n| *n = n.wrapping_add(1));
            crate::commands::dispatch(state, "mail.discard_draft", json!({ "draft_id": id_discard.clone() }));
        }
    });
    let close = {
        let fields = ui.fields.clone();
        Callback::new(move |()| {
            if ready.get_value() {
                sync_fields.run(());
                let p = fields.get_untracked();
                save_gen.update_value(|n| *n = n.wrapping_add(1));
                spawn_local(async move {
                    let _ = crate::ipc::run("mail.save_draft", p).await;
                });
            }
            close_compose();
        })
    };

    let root_keys = {
        move |e: web_sys::KeyboardEvent| {
            if e.default_prevented() {
                return;
            }
            let primary = if crate::dom::is_mac() { e.meta_key() } else { e.ctrl_key() };
            match e.key().as_str() {
                "Escape" => {
                    e.prevent_default();
                    close.run(());
                }
                "Enter" if primary => {
                    e.prevent_default();
                    send.run(());
                }
                "s" | "S" if primary && !e.shift_key() => {
                    e.prevent_default();
                    changed.run(());
                }
                _ => {}
            }
        }
    };
    let editor_keys = move |e: web_sys::KeyboardEvent| {
        let primary = if crate::dom::is_mac() { e.meta_key() } else { e.ctrl_key() };
        if !primary || e.alt_key() || e.shift_key() {
            return;
        }
        let cmd = match e.key().to_ascii_lowercase().as_str() {
            "b" => "bold",
            "i" => "italic",
            "u" => "underline",
            "k" => {
                e.prevent_default();
                e.stop_propagation();
                insert_link();
                changed.run(());
                return;
            }
            _ => return,
        };
        e.prevent_default();
        e.stop_propagation();
        exec(cmd, "");
        changed.run(());
    };
    let paste = move |e: web_sys::ClipboardEvent| {
        e.prevent_default();
        let text = e.clipboard_data().and_then(|d| d.get_data("text/plain").ok()).unwrap_or_default();
        if !text.is_empty() {
            exec("insertText", &text);
            changed.run(());
        }
    };
    let id_drop = draft_id.clone();
    let drop = move |e: web_sys::DragEvent| {
        e.prevent_default();
        if let Some(files) = e.data_transfer().and_then(|d| d.files())
            && files.length() > 0
        {
            read_files(state, id_drop.clone(), files, attachments);
        }
    };
    let id_pick = draft_id.clone();
    let on_file = move |_| {
        let Some(el) = file_input.try_get_untracked().flatten() else { return };
        if let Some(files) = el.files() {
            read_files(state, id_pick.clone(), files, attachments);
        }
        el.set_value("");
    };
    let tool = move |cmd: &'static str, label: &'static str, icon: MailIcon| {
        view! {
            <button class="mail-tool-button" title=label aria-label=label tabindex="-1"
                on:mousedown=|e: web_sys::MouseEvent| e.prevent_default()
                on:click=move |_| {
                    if cmd == "createLink" {
                        insert_link();
                    } else {
                        exec(cmd, "");
                    }
                    changed.run(());
                }>
                <MIcon name=icon />
            </button>
        }
    };
    let id_remove = draft_id.clone();

    let body = move || match loaded.get() {
        None => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
        Some(Err(message)) => view! {
            <crate::components::EmptyState title="Couldn't open this draft" detail=message error=true />
        }
        .into_any(),
        Some(Ok(d)) => {
            let title = match d.mode.as_deref() {
                Some("reply") => "Reply",
                Some("reply_all") => "Reply all",
                Some("forward") => "Forward",
                _ => "New message",
            };
            let id_remove = id_remove.clone();
            let autofocus_to = d.mode.as_deref() != Some("reply") && d.mode.as_deref() != Some("reply_all");
            view! {
                <div class="mail-compose-head">
                    <h2 class="mail-compose-title">{title}</h2>
                    <span class="mail-compose-status" aria-live="polite">{move || saved.get().unwrap_or("")}</span>
                    <button class="mail-icon-button" title="Close (Esc)" aria-label="Close" on:click=move |_| close.run(())>
                        <Icon name=IconName::Close />
                    </button>
                </div>
                <div class="mail-compose-fields">
                    <div class="mail-field-row">
                        <RecipientField label="To" list=to on_change=changed autofocus=autofocus_to />
                        {move || (!show_cc.get()).then(|| view! {
                            <button class="mail-link-button" on:click=move |_| show_cc.set(true)>"Cc Bcc"</button>
                        })}
                    </div>
                    {move || show_cc.get().then(|| view! {
                        <RecipientField label="Cc" list=cc on_change=changed />
                        <RecipientField label="Bcc" list=bcc on_change=changed />
                    })}
                    <div class="mail-field">
                        <span class="mail-field-label">"Subject"</span>
                        <input class="mail-subject-input" type="text" aria-label="Subject" maxlength="4096"
                            prop:value=move || subject.get()
                            on:input=move |e| {
                                subject.set(event_target_value(&e));
                                changed.run(());
                            } />
                    </div>
                </div>
                <div class="mail-toolbar" role="toolbar" aria-label="Formatting">
                    {tool("bold", "Bold (⌘B)", MailIcon::Bold)}
                    {tool("italic", "Italic (⌘I)", MailIcon::Italic)}
                    {tool("underline", "Underline (⌘U)", MailIcon::Underline)}
                    <span class="mail-toolbar-sep"></span>
                    {tool("insertUnorderedList", "Bulleted list", MailIcon::ListBullet)}
                    {tool("insertOrderedList", "Numbered list", MailIcon::ListNumber)}
                    {tool("createLink", "Link (⌘K)", MailIcon::Link)}
                    <span class="mail-toolbar-sep"></span>
                    <button class="mail-tool-button" title="Attach files" aria-label="Attach files" on:click=move |_| {
                        if let Some(el) = file_input.try_get_untracked().flatten() {
                            el.click();
                        }
                    }>
                        <Icon name=IconName::Paperclip />
                    </button>
                    <input node_ref=file_input type="file" multiple class="mail-file-input" tabindex="-1" aria-hidden="true" on:change=on_file.clone() />
                </div>
                {move || {
                    let list = attachments.get();
                    (!list.is_empty()).then(|| {
                        let id_remove = id_remove.clone();
                        view! {
                            <div class="mail-attachments compose">
                                {list.into_iter().map(|a| {
                                    let (att, draft) = (a.id.clone(), id_remove.clone());
                                    view! {
                                        <span class="mail-attachment static">
                                            <Icon name=IconName::Paperclip />
                                            <span class="mail-attachment-text">
                                                <span class="mail-attachment-name">{a.name.clone()}</span>
                                                <span class="mail-attachment-size">{size(a.size)}</span>
                                            </span>
                                            <button class="mail-chip-remove" aria-label=format!("Remove {}", a.name) on:click=move |_| {
                                                let (att, draft) = (att.clone(), draft.clone());
                                                attachments.update(|l| l.retain(|x| x.id != att));
                                                spawn_local(async move {
                                                    let _ = crate::ipc::run("mail.remove_attachment", json!({ "draft_id": draft, "attachment": att })).await;
                                                });
                                            }>
                                                <Icon name=IconName::Close />
                                            </button>
                                        </span>
                                    }
                                }).collect_view()}
                            </div>
                        }
                    })
                }}
                <div
                    node_ref=editor
                    class="mail-editor"
                    contenteditable="true"
                    role="textbox"
                    aria-multiline="true"
                    aria-label="Message body"
                    data-pane-focus=""
                    on:input=move |_| changed.run(())
                    on:keydown=editor_keys
                    on:paste=paste
                    on:dragover=|e: web_sys::DragEvent| e.prevent_default()
                    on:drop=drop.clone()
                ></div>
                {d.quote_html.clone().filter(|q| !q.is_empty()).map(|q| view! {
                    <details class="mail-quote">
                        <summary>"Original message"</summary>
                        <iframe class="mail-frame quote" sandbox="" title="Original message" srcdoc=super::mail_util::frame_document(&q)></iframe>
                    </details>
                })}
                <div class="mail-compose-actions">
                    <button class="mail-button primary" title="Send (⌘↩)" on:click=move |_| send.run(())>
                        <Icon name=IconName::Send />
                        <span>"Send"</span>
                    </button>
                    <button class="mail-button" on:click=move |_| discard.run(())>"Discard"</button>
                </div>
            }
            .into_any()
        }
    };
    view! {
        <div class="mail-compose" on:keydown=root_keys on:dragover=|e: web_sys::DragEvent| e.prevent_default()>
            {body}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_recipients_parse() {
        assert_eq!(parse_recipient("a@example.com,").map(|r| r.address), Some("a@example.com".into()));
        let r = parse_recipient("\"Avery Example\" <avery@example.com>").unwrap_or_default();
        assert_eq!(r.name.as_deref(), Some("Avery Example"));
        assert_eq!(r.address, "avery@example.com");
        assert!(parse_recipient("avery").is_none());
        assert!(parse_recipient("<>").is_none());
        assert!(parse_recipient("").is_none());
    }

    #[test]
    fn only_safe_links_are_inserted() {
        assert_eq!(safe_link("https://example.com").as_deref(), Some("https://example.com"));
        assert_eq!(safe_link("example.com/x").as_deref(), Some("https://example.com/x"));
        assert_eq!(safe_link("a@example.com").as_deref(), Some("mailto:a@example.com"));
        assert!(safe_link("javascript:alert(1)").is_none());
        assert!(safe_link("data:text/html,x").is_none());
        assert!(safe_link("").is_none());
    }

    #[test]
    fn people_rows_become_suggestions() {
        let rows: Vec<Suggestion> = serde_json::from_value(json!([
            { "name": "A", "display_name": "Avery", "email": "a@example.com" },
            { "display_name": "B", "address": "b@example.com" },
            { "name": "No address" },
        ]))
        .unwrap();
        let r: Vec<Recipient> = rows.into_iter().filter_map(Suggestion::recipient).collect();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name.as_deref(), Some("Avery"));
    }

    #[test]
    fn drafts_decode() {
        let d: DraftView = serde_json::from_value(
            json!({ "draft_id": "d", "to": [{ "name": null, "address": "a@example.com" }], "quote_html": null }),
        )
        .unwrap();
        assert_eq!(d.to.len(), 1);
        assert!(d.quote_html.is_none());
    }
}
