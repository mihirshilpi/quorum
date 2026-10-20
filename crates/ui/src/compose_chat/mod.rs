//! The chat / channel message composer: a rich-text `contenteditable` with @mentions, an emoji
//! picker, pasted images, file attachments and draft autosave.
//!
//! Keys: Enter sends, Shift+Enter adds a line, ↑ in an empty composer edits your last message,
//! Esc cancels an edit or closes a picker, Cmd+B/I/U format. The composer never sends by itself:
//! it hands an [`Outgoing`] to its owner, which runs the engine command (`chat.send`,
//! `channel.post`, `channel.reply`, `chat.edit`, ...).

pub mod logic;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::Reply;
use serde_json::{Value, json};
use wasm_bindgen::JsCast;

use crate::areas::chat_icons::{CIcon, ChatIcon};
use crate::areas::chat_logic::{EMOJI, MentionOut, Piece, mention_query, pieces_to_html};
use logic::{Candidate, at_to_chips, base64, match_people};

/// Largest pasted or attached image (Graph's hosted content limit).
pub const MAX_IMAGE: usize = 4 * 1024 * 1024;
/// Largest attached file.
pub const MAX_FILE: usize = 25 * 1024 * 1024;
/// Most attachments per message.
pub const MAX_ATTACH: usize = 10;
/// Pause in typing after which the draft is saved.
const DRAFT_DELAY_MS: u64 = 700;

/// A file or image waiting to be sent (base64 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// File name.
    pub name: String,
    /// MIME type.
    pub content_type: String,
    /// Base64 bytes.
    pub data: String,
    /// Size in bytes.
    pub size: usize,
    /// Inline image (sent as hosted content) rather than a file.
    pub image: bool,
}

/// What the composer hands to its owner.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outgoing {
    /// Restricted HTML (`<at id>` mentions, basic formatting).
    pub html: String,
    /// Mentions referenced from `html`.
    pub mentions: Vec<MentionOut>,
    /// Attachments.
    pub attachments: Vec<Attachment>,
    /// Subject (channel posts).
    pub subject: Option<String>,
}

impl Outgoing {
    /// Adds `html`, `mentions`, `images`, `files` (and `subject`) to command params.
    pub fn into_params(self, mut base: Value) -> Value {
        if let Some(obj) = base.as_object_mut() {
            obj.insert("html".into(), json!(self.html));
            obj.insert("mentions".into(), json!(self.mentions));
            let (imgs, files): (Vec<_>, Vec<_>) = self.attachments.into_iter().partition(|a| a.image);
            obj.insert(
                "images".into(),
                Value::Array(
                    imgs.into_iter().map(|a| json!({ "content_type": a.content_type, "data": a.data })).collect(),
                ),
            );
            obj.insert(
                "files".into(),
                Value::Array(
                    files
                        .into_iter()
                        .map(|a| json!({ "name": a.name, "content_type": a.content_type, "data": a.data }))
                        .collect(),
                ),
            );
            if let Some(s) = self.subject.filter(|s| !s.trim().is_empty()) {
                obj.insert("subject".into(), json!(s));
            }
        }
        base
    }
}

/// A message being edited in the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editing {
    /// Message id.
    pub id: String,
    /// Its current display HTML (sanitised).
    pub html: String,
}

/// Where drafts load from and save to.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftSpec {
    /// Draft query id (`chat.draft`, `channel.draft`).
    pub query: &'static str,
    /// Save command id (`chat.save_draft`, `channel.save_draft`).
    pub save: &'static str,
    /// Params naming the conversation (`{"chat": id}` or `{"team", "channel", "post"?}`).
    pub params: Value,
}

fn editor_el(node: NodeRef<html::Div>) -> Option<web_sys::HtmlElement> {
    node.try_get_untracked().flatten().map(|d| {
        let e: web_sys::HtmlElement = d.into();
        e
    })
}

/// Walks the editable DOM into [`Piece`]s (bounded depth and size).
fn walk(node: &web_sys::Node, out: &mut Vec<Piece>, depth: usize) {
    if depth > 24 || out.len() > 20_000 {
        return;
    }
    let children = node.child_nodes();
    for i in 0..children.length() {
        let Some(child) = children.item(i) else { continue };
        match child.node_type() {
            web_sys::Node::TEXT_NODE => out.push(Piece::Text(child.text_content().unwrap_or_default())),
            web_sys::Node::ELEMENT_NODE => {
                let Some(el) = child.dyn_ref::<web_sys::Element>() else { continue };
                let tag = el.tag_name().to_ascii_uppercase();
                if tag == "SPAN" && el.get_attribute("data-user").is_some() {
                    out.push(Piece::Mention {
                        user: el.get_attribute("data-user").unwrap_or_default(),
                        name: el.get_attribute("data-name").unwrap_or_else(|| el.text_content().unwrap_or_default()),
                    });
                    continue;
                }
                let fmt: Option<&'static str> = match tag.as_str() {
                    "B" | "STRONG" => Some("b"),
                    "I" | "EM" => Some("i"),
                    "U" => Some("u"),
                    "S" | "STRIKE" | "DEL" => Some("s"),
                    "CODE" => Some("code"),
                    _ => None,
                };
                match tag.as_str() {
                    "BR" => out.push(Piece::Break),
                    "DIV" | "P" | "LI" => {
                        if !out.is_empty() && out.last() != Some(&Piece::Break) {
                            out.push(Piece::Break);
                        }
                        walk(&child, out, depth + 1);
                    }
                    _ => {
                        if let Some(f) = fmt {
                            out.push(Piece::Open(f));
                        }
                        walk(&child, out, depth + 1);
                        if let Some(f) = fmt {
                            out.push(Piece::Close(f));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn serialise(node: NodeRef<html::Div>) -> (String, Vec<MentionOut>, bool) {
    let Some(el) = editor_el(node) else { return (String::new(), Vec::new(), false) };
    let mut pieces = Vec::new();
    walk(&el, &mut pieces, 0);
    pieces_to_html(&pieces)
}

fn html_doc() -> Option<web_sys::HtmlDocument> {
    crate::dom::document()?.dyn_into::<web_sys::HtmlDocument>().ok()
}

fn exec(cmd: &str, value: Option<&str>) {
    if let Some(d) = html_doc() {
        let _ = match value {
            Some(v) => d.exec_command_with_show_ui_and_value(cmd, false, v),
            None => d.exec_command(cmd),
        };
    }
}

/// Text from the start of the editor to the caret.
fn text_before_caret(editor: &web_sys::HtmlElement) -> Option<String> {
    let sel = web_sys::window()?.get_selection().ok()??;
    if sel.range_count() == 0 {
        return None;
    }
    let range = sel.get_range_at(0).ok()?;
    let node = range.start_container().ok()?;
    if !editor.contains(Some(&node)) {
        return None;
    }
    let pre = range.clone_range();
    pre.select_node_contents(editor).ok()?;
    pre.set_end(&node, range.start_offset().ok()?).ok()?;
    pre.to_string().as_string()
}

/// Replaces the `@query` before the caret with a mention chip and a space.
fn insert_mention(editor: &web_sys::HtmlElement, query_len: usize, c: &Candidate) -> bool {
    let Some(doc) = crate::dom::document() else { return false };
    let Some(sel) = web_sys::window().and_then(|w| w.get_selection().ok().flatten()) else { return false };
    let Ok(range) = sel.get_range_at(0) else { return false };
    let Ok(node) = range.start_container() else { return false };
    let Ok(offset) = range.start_offset() else { return false };
    if node.node_type() != web_sys::Node::TEXT_NODE || !editor.contains(Some(&node)) {
        return false;
    }
    // The query is ASCII-or-not text in UTF-16 units; count units of "@" + query.
    let text = node.text_content().unwrap_or_default();
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = usize::try_from(offset).unwrap_or(0).min(units.len());
    let before = String::from_utf16_lossy(units.get(..end).unwrap_or(&[]));
    let Some(at) = before.rfind('@') else { return false };
    let start_units = before.get(..at).map(|s| s.encode_utf16().count()).unwrap_or(0);
    let _ = query_len;
    let Ok(del) = doc.create_range() else { return false };
    if del.set_start(&node, u32::try_from(start_units).unwrap_or(0)).is_err() || del.set_end(&node, offset).is_err() {
        return false;
    }
    let _ = del.delete_contents();
    let Ok(span) = doc.create_element("span") else { return false };
    let _ = span.set_attribute("class", "mention");
    let _ = span.set_attribute("contenteditable", "false");
    let _ = span.set_attribute("data-user", &c.id);
    let _ = span.set_attribute("data-name", &c.name);
    span.set_text_content(Some(&c.name));
    let space = doc.create_text_node("\u{a0}");
    let _ = del.insert_node(&space);
    let _ = del.insert_node(&span);
    if let Ok(after) = doc.create_range() {
        let _ = after.set_start(&space, 1);
        after.collapse_with_to_start(true);
        let _ = sel.remove_all_ranges();
        let _ = sel.add_range(&after);
    }
    true
}

fn place_caret_at_end(editor: &web_sys::HtmlElement) {
    let (Some(doc), Some(sel)) =
        (crate::dom::document(), web_sys::window().and_then(|w| w.get_selection().ok().flatten()))
    else {
        return;
    };
    if let Ok(r) = doc.create_range() {
        let _ = r.select_node_contents(editor);
        r.collapse_with_to_start(false);
        let _ = sel.remove_all_ranges();
        let _ = sel.add_range(&r);
    }
}

async fn read_file(file: web_sys::File) -> Option<Vec<u8>> {
    let buf = wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await.ok()?;
    Some(js_sys::Uint8Array::new(&buf).to_vec())
}

fn add_files(files: Vec<web_sys::File>, attachments: RwSignal<Vec<Attachment>>, force_image: bool) {
    let state = crate::state::app_state();
    for f in files {
        let ct = f.type_();
        let image = force_image || matches!(ct.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp");
        let size = usize::try_from(f.size() as u64).unwrap_or(usize::MAX);
        let cap = if image { MAX_IMAGE } else { MAX_FILE };
        if size > cap {
            state.toast(crate::state::ToastLevel::Warning, format!("{} is too large to attach.", f.name()));
            continue;
        }
        if attachments.with_untracked(Vec::len) >= MAX_ATTACH {
            state.toast(crate::state::ToastLevel::Warning, "At most 10 attachments per message.");
            break;
        }
        spawn_local(async move {
            let name = f.name();
            if let Some(bytes) = read_file(f).await {
                let att = Attachment {
                    name: if name.is_empty() { "image.png".into() } else { name },
                    content_type: if ct.is_empty() { "application/octet-stream".into() } else { ct },
                    data: base64(&bytes),
                    size: bytes.len(),
                    image,
                };
                attachments.try_update(|v| v.push(att));
            }
        });
    }
}

fn file_list(list: Option<web_sys::FileList>) -> Vec<web_sys::File> {
    let Some(list) = list else { return Vec::new() };
    (0..list.length()).filter_map(|i| list.item(i)).collect()
}

fn size_label(n: usize) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{} KB", n / 1024),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// The composer.
#[component]
pub fn Composer(
    /// Draft storage for the current conversation (`None`: no drafts).
    #[prop(into)]
    draft: Signal<Option<DraftSpec>>,
    /// Placeholder text.
    #[prop(into)]
    placeholder: Signal<String>,
    /// People offered by the @mention picker (chat members etc.).
    #[prop(into)]
    people: Signal<Vec<Candidate>>,
    /// Send handler.
    on_send: Callback<Outgoing>,
    /// The message being edited, if any (set by the owner, e.g. on ↑ or "Edit").
    editing: RwSignal<Option<Editing>>,
    /// Save handler for an edit.
    #[prop(optional)]
    on_edit: Option<Callback<(Editing, Outgoing)>>,
    /// ↑ in an empty composer.
    #[prop(optional)]
    on_up: Option<Callback<()>>,
    /// Show a subject line (new channel posts).
    #[prop(optional)]
    subject: bool,
    /// Accessible name.
    #[prop(optional)]
    label: Option<&'static str>,
) -> impl IntoView {
    let editor = NodeRef::<html::Div>::new();
    let file_input = NodeRef::<html::Input>::new();
    let attachments = RwSignal::new(Vec::<Attachment>::new());
    let empty = RwSignal::new(true);
    let subject_text = RwSignal::new(String::new());
    let emoji_open = RwSignal::new(false);
    let format_open = RwSignal::new(false);
    // Mention picker: Some(query) while open.
    let mention = RwSignal::new(None::<String>);
    let mention_index = RwSignal::new(0usize);
    let remote_people = RwSignal::new(Vec::<Candidate>::new());
    let draft_gen = StoredValue::new(0u64);
    let saved_range = StoredValue::new_local(None::<web_sys::Range>);

    let refresh_empty = move || {
        let (html, _, has) = serialise(editor);
        empty.set(!has && html.trim().is_empty());
    };

    let set_content = move |html: &str| {
        if let Some(el) = editor_el(editor) {
            el.set_inner_html(html);
        }
        refresh_empty();
    };

    // Load the draft when the conversation changes.
    Effect::new(move |_| {
        let spec = draft.get();
        attachments.set(Vec::new());
        subject_text.set(String::new());
        mention.set(None);
        set_content("");
        let Some(spec) = spec else { return };
        spawn_local(async move {
            if let Reply::Ok { value } = crate::ipc::query(spec.query, spec.params.clone()).await
                && let Some(html) = value.get("html").and_then(Value::as_str)
                && draft.try_with_untracked(|d| d.as_ref() == Some(&spec)) == Some(true)
            {
                let mentions: Vec<(u32, String)> = value
                    .get("mentions")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| {
                                let id = u32::try_from(m.get("id")?.as_u64()?).ok()?;
                                Some((id, m.get("user")?.as_str()?.to_owned()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                set_content(&at_to_chips(html, &mentions));
            }
        });
    });

    // Entering edit mode loads the message; leaving it clears the editor.
    Effect::new(move |prev: Option<bool>| {
        let e = editing.get();
        match &e {
            Some(ed) => {
                set_content(
                    &ed.html.replace("<at id=", "<span class=\"mention\" data-at=").replace("</at>", "</span>"),
                );
                request_animation_frame(move || {
                    if let Some(el) = editor_el(editor) {
                        let _ = el.focus();
                        place_caret_at_end(&el);
                    }
                });
            }
            None if prev == Some(true) => set_content(""),
            None => {}
        }
        e.is_some()
    });

    let save_draft = move || {
        let Some(spec) = draft.get_untracked() else { return };
        if editing.with_untracked(Option::is_some) {
            return;
        }
        let n = draft_gen.try_get_value().unwrap_or(0).wrapping_add(1);
        draft_gen.set_value(n);
        set_timeout(
            move || {
                if draft_gen.try_get_value() != Some(n) {
                    return;
                }
                let (html, mentions, has) = serialise(editor);
                let mut params = spec.params.clone();
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("html".into(), json!(if has { html } else { String::new() }));
                    obj.insert("mentions".into(), json!(mentions));
                }
                spawn_local(async move {
                    let _ = crate::ipc::run(spec.save, params).await;
                });
            },
            std::time::Duration::from_millis(DRAFT_DELAY_MS),
        );
    };

    let suggestions = Memo::new(move |_| {
        let Some(q) = mention.get() else { return Vec::new() };
        let mut all = people.get();
        all.extend(remote_people.get());
        match_people(&all, &q, 8)
    });

    let lookup_remote = move |q: String| {
        if q.chars().count() < 2 {
            remote_people.set(Vec::new());
            return;
        }
        spawn_local(async move {
            if let Reply::Ok { value: Value::Array(rows) } =
                crate::ipc::query("people.search", json!({ "query": q, "limit": 8 })).await
            {
                let found: Vec<Candidate> = rows
                    .iter()
                    .filter_map(|r| {
                        Some(Candidate {
                            id: r.get("user_id")?.as_str()?.to_owned(),
                            name: r.get("display_name").or_else(|| r.get("name"))?.as_str()?.to_owned(),
                        })
                    })
                    .collect();
                remote_people.try_set(found);
            }
        });
    };

    let on_input = move |_: web_sys::Event| {
        refresh_empty();
        save_draft();
        let q = editor_el(editor).and_then(|el| text_before_caret(&el)).and_then(|t| mention_query(&t));
        if q != mention.get_untracked() {
            mention_index.set(0);
            if let Some(q) = q.clone() {
                lookup_remote(q);
            }
        }
        mention.set(q);
    };

    let choose = move |c: Candidate| {
        let q = mention.get_untracked().unwrap_or_default();
        if let Some(el) = editor_el(editor)
            && insert_mention(&el, q.chars().count(), &c)
        {
            mention.set(None);
            refresh_empty();
            save_draft();
        }
    };

    let clear = move || {
        set_content("");
        attachments.set(Vec::new());
        subject_text.set(String::new());
        mention.set(None);
        emoji_open.set(false);
    };

    let submit = move || {
        let (html, mentions, has) = serialise(editor);
        let atts = attachments.get_untracked();
        if !has && atts.is_empty() {
            return;
        }
        let out = Outgoing {
            html: if has { format!("<p>{html}</p>") } else { String::new() },
            mentions,
            attachments: atts,
            subject: subject.then(|| subject_text.get_untracked()),
        };
        // Reset the composer before running the callback: the owner may unmount it (a new
        // channel post closes its composer), after which its signals are gone.
        if let Some(ed) = editing.get_untracked() {
            editing.set(None);
            if let Some(cb) = on_edit {
                cb.run((ed, out));
            }
            return;
        }
        clear();
        // The engine deletes the draft with the send; cancel a pending save.
        draft_gen.update_value(|n| *n = n.wrapping_add(1));
        on_send.run(out);
    };

    let on_keydown = move |e: web_sys::KeyboardEvent| {
        if e.is_composing() {
            return;
        }
        let key = e.key();
        let primary = if crate::dom::is_mac() { e.meta_key() } else { e.ctrl_key() };
        if mention.with_untracked(Option::is_some) {
            let n = suggestions.with_untracked(Vec::len);
            match key.as_str() {
                "ArrowDown" if n > 0 => {
                    e.prevent_default();
                    mention_index.update(|i| *i = (*i + 1) % n);
                    return;
                }
                "ArrowUp" if n > 0 => {
                    e.prevent_default();
                    mention_index.update(|i| *i = (*i + n - 1) % n);
                    return;
                }
                "Enter" | "Tab" if n > 0 => {
                    e.prevent_default();
                    let i = mention_index.get_untracked();
                    if let Some(c) = suggestions.with_untracked(|s| s.get(i).cloned()) {
                        choose(c);
                    }
                    return;
                }
                "Escape" => {
                    e.prevent_default();
                    e.stop_propagation();
                    mention.set(None);
                    return;
                }
                _ => {}
            }
        }
        match key.as_str() {
            "Enter" if !e.shift_key() && !e.alt_key() => {
                e.prevent_default();
                submit();
            }
            "Escape" if editing.with_untracked(Option::is_some) || emoji_open.get_untracked() => {
                e.prevent_default();
                e.stop_propagation();
                emoji_open.set(false);
                editing.set(None);
            }
            "ArrowUp" if empty.get_untracked() && editing.with_untracked(Option::is_none) && !e.shift_key() => {
                if let Some(cb) = on_up {
                    e.prevent_default();
                    cb.run(());
                }
            }
            "b" | "i" | "u" if primary && !e.shift_key() => {
                e.prevent_default();
                exec(
                    match key.as_str() {
                        "b" => "bold",
                        "i" => "italic",
                        _ => "underline",
                    },
                    None,
                );
            }
            _ => {}
        }
    };

    let on_paste = move |e: web_sys::ClipboardEvent| {
        let Some(dt) = e.clipboard_data() else { return };
        let files = file_list(dt.files());
        if !files.is_empty() {
            e.prevent_default();
            add_files(files, attachments, false);
            return;
        }
        // Paste as plain text so foreign formatting and scripts never enter the editor.
        if let Ok(text) = dt.get_data("text/plain") {
            e.prevent_default();
            exec("insertText", Some(&text));
        }
    };

    let remember_range = move || {
        if let Some(sel) = web_sys::window().and_then(|w| w.get_selection().ok().flatten())
            && sel.range_count() > 0
            && let Ok(r) = sel.get_range_at(0)
            && let (Some(el), Ok(node)) = (editor_el(editor), r.start_container())
            && el.contains(Some(&node))
        {
            saved_range.set_value(Some(r));
        }
    };

    let insert_emoji = move |emoji: &'static str| {
        if let Some(el) = editor_el(editor) {
            let _ = el.focus();
            if let (Some(r), Some(sel)) =
                (saved_range.with_value(Clone::clone), web_sys::window().and_then(|w| w.get_selection().ok().flatten()))
            {
                let _ = sel.remove_all_ranges();
                let _ = sel.add_range(&r);
            } else {
                place_caret_at_end(&el);
            }
        }
        exec("insertText", Some(emoji));
        emoji_open.set(false);
        refresh_empty();
        save_draft();
    };

    let format_button = move |cmd: &'static str, text: &'static str, title: &'static str| {
        view! {
            <button
                class="fmt-button"
                type="button"
                title=title
                aria-label=title
                on:mousedown=move |e| e.prevent_default()
                on:click=move |_| exec(cmd, None)
            >
                {text}
            </button>
        }
    };

    let label = label.unwrap_or("Type a message");
    view! {
        <div class="composer" class:editing=move || editing.with(Option::is_some)>
            {move || {
                editing
                    .with(Option::is_some)
                    .then(|| {
                        view! {
                            <div class="composer-banner">
                                <CIcon name=ChatIcon::Edit />
                                <span>"Editing message"</span>
                                <button class="link-button" type="button" on:click=move |_| editing.set(None)>
                                    "Cancel (Esc)"
                                </button>
                            </div>
                        }
                    })
            }}
            {subject
                .then(|| {
                    view! {
                        <input
                            class="composer-subject"
                            type="text"
                            placeholder="Add a subject"
                            aria-label="Subject"
                            maxlength="250"
                            prop:value=move || subject_text.get()
                            on:input=move |e| subject_text.set(event_target_value(&e))
                        />
                    }
                })}
            {move || {
                format_open
                    .get()
                    .then(|| {
                        view! {
                            <div class="composer-format" role="toolbar" aria-label="Formatting">
                                {format_button("bold", "B", "Bold")}
                                {format_button("italic", "I", "Italic")}
                                {format_button("underline", "U", "Underline")}
                                {format_button("strikeThrough", "S", "Strikethrough")}
                                {format_button("insertUnorderedList", "•", "Bulleted list")}
                            </div>
                        }
                    })
            }}
            <div class="composer-box">
                <div
                    node_ref=editor
                    class="composer-editor"
                    contenteditable="true"
                    role="textbox"
                    aria-multiline="true"
                    aria-label=label
                    spellcheck="true"
                    data-placeholder=move || placeholder.get()
                    class:empty=move || empty.get()
                    on:input=on_input
                    on:keydown=on_keydown
                    on:paste=on_paste
                    on:keyup=move |_| remember_range()
                    on:mouseup=move |_| remember_range()
                    on:blur=move |_| remember_range()
                ></div>
                {move || {
                    let atts = attachments.get();
                    (!atts.is_empty())
                        .then(|| {
                            view! {
                                <ul class="composer-attachments" aria-label="Attachments">
                                    {atts
                                        .into_iter()
                                        .enumerate()
                                        .map(|(i, a)| {
                                            let preview = a
                                                .image
                                                .then(|| format!("data:{};base64,{}", a.content_type, a.data));
                                            view! {
                                                <li class="attachment-chip" class:image=a.image>
                                                    {match preview {
                                                        Some(src) => view! { <img src=src alt="" /> }.into_any(),
                                                        None => view! { <CIcon name=ChatIcon::Doc /> }.into_any(),
                                                    }}
                                                    <span class="attachment-name">{a.name.clone()}</span>
                                                    <span class="attachment-size">{size_label(a.size)}</span>
                                                    <button
                                                        class="icon-button small"
                                                        type="button"
                                                        aria-label=format!("Remove {}", a.name)
                                                        on:click=move |_| {
                                                            attachments
                                                                .update(|v| {
                                                                    if i < v.len() {
                                                                        v.remove(i);
                                                                    }
                                                                })
                                                        }
                                                    >
                                                        "×"
                                                    </button>
                                                </li>
                                            }
                                        })
                                        .collect_view()}
                                </ul>
                            }
                        })
                }}
                <div class="composer-tools">
                    <button
                        class="icon-button"
                        type="button"
                        title="Formatting"
                        aria-label="Formatting"
                        aria-pressed=move || if format_open.get() { "true" } else { "false" }
                        on:click=move |_| format_open.update(|v| *v = !*v)
                    >
                        <CIcon name=ChatIcon::Format />
                    </button>
                    <button
                        class="icon-button"
                        type="button"
                        title="Emoji"
                        aria-label="Emoji"
                        aria-expanded=move || if emoji_open.get() { "true" } else { "false" }
                        on:mousedown=move |e| e.prevent_default()
                        on:click=move |_| emoji_open.update(|v| *v = !*v)
                    >
                        <CIcon name=ChatIcon::Smile />
                    </button>
                    <button
                        class="icon-button"
                        type="button"
                        title="Mention someone (@)"
                        aria-label="Mention someone"
                        on:mousedown=move |e| e.prevent_default()
                        on:click=move |_| {
                            if let Some(el) = editor_el(editor) {
                                let _ = el.focus();
                            }
                            exec("insertText", Some("@"));
                            mention.set(Some(String::new()));
                        }
                    >
                        <CIcon name=ChatIcon::At />
                    </button>
                    <button
                        class="icon-button"
                        type="button"
                        title="Attach a file"
                        aria-label="Attach a file"
                        on:click=move |_| {
                            if let Some(i) = file_input.get_untracked() {
                                i.click();
                            }
                        }
                    >
                        <CIcon name=ChatIcon::Clip />
                    </button>
                    <input
                        node_ref=file_input
                        class="visually-hidden"
                        type="file"
                        multiple=true
                        tabindex="-1"
                        aria-hidden="true"
                        on:change=move |e| {
                            let input = e.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok());
                            if let Some(input) = input {
                                add_files(file_list(input.files()), attachments, false);
                                input.set_value("");
                            }
                        }
                    />
                    <span class="composer-spacer"></span>
                    <button
                        class="send-button"
                        type="button"
                        title=move || if editing.with(Option::is_some) { "Save (Enter)" } else { "Send (Enter)" }
                        aria-label=move || if editing.with(Option::is_some) { "Save edit" } else { "Send" }
                        disabled=move || empty.get() && attachments.with(Vec::is_empty)
                        on:click=move |_| submit()
                    >
                        <CIcon name=ChatIcon::Send />
                    </button>
                </div>
            </div>
            {move || {
                emoji_open
                    .get()
                    .then(|| {
                        view! {
                            <div class="emoji-picker" role="dialog" aria-label="Emoji">
                                {EMOJI
                                    .iter()
                                    .map(|(group, list)| {
                                        view! {
                                            <div class="emoji-group">
                                                <div class="emoji-group-title">{*group}</div>
                                                <div class="emoji-grid">
                                                    {list
                                                        .iter()
                                                        .map(|e| {
                                                            let e: &'static str = e;
                                                            view! {
                                                                <button
                                                                    class="emoji-button"
                                                                    type="button"
                                                                    aria-label=e
                                                                    on:mousedown=move |ev| ev.prevent_default()
                                                                    on:click=move |_| insert_emoji(e)
                                                                >
                                                                    {e}
                                                                </button>
                                                            }
                                                        })
                                                        .collect_view()}
                                                </div>
                                            </div>
                                        }
                                    })
                                    .collect_view()}
                            </div>
                        }
                    })
            }}
            {move || {
                let list = suggestions.get();
                (mention.with(Option::is_some) && !list.is_empty())
                    .then(|| {
                        view! {
                            <ul class="mention-picker" role="listbox" aria-label="People">
                                {list
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, c)| {
                                        let pick = c.clone();
                                        view! {
                                            <li
                                                role="option"
                                                class="mention-option"
                                                class:selected=move || mention_index.get() == i
                                                aria-selected=move || if mention_index.get() == i { "true" } else { "false" }
                                                on:mousedown=move |e| {
                                                    e.prevent_default();
                                                    choose(pick.clone());
                                                }
                                            >
                                                <crate::components::Avatar name=c.name.clone() size="sm" />
                                                <span>{c.name.clone()}</span>
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                        }
                    })
            }}
        </div>
    }
}
