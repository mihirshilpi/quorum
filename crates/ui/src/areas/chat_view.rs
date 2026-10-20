//! Message rendering shared by Chat and Teams: rows from `chat.messages` / `channel.posts`,
//! bubbles with reactions, files, quotes and hover actions, and the scrolling conversation.

use leptos::html;
use leptos::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use super::chat_icons::{CIcon, ChatIcon};
use super::chat_logic::{Entry, MsgMeta, QUICK_REACTIONS, clock, group, parse_time};
use crate::components::Avatar;
use crate::components::person_card::PersonAvatar;

/// A reaction summary.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ReactionRow {
    /// The emoji.
    pub emoji: String,
    /// How many people.
    #[serde(default)]
    pub count: u32,
    /// Whether you reacted.
    #[serde(default)]
    pub mine: bool,
    /// Some of the names.
    #[serde(default)]
    pub names: Vec<String>,
}

/// A file attached by reference.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FileRow {
    /// File name.
    pub name: String,
    /// Web link.
    #[serde(default)]
    pub url: Option<String>,
}

/// A quoted message.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct QuoteRow {
    /// Quoted message id.
    #[serde(default)]
    pub id: Option<String>,
    /// Preview text.
    #[serde(default)]
    pub preview: String,
    /// Sender.
    #[serde(default)]
    pub from: Option<String>,
}

/// A message (`chat.messages` item, channel post or reply).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MessageRow {
    /// Id.
    pub id: String,
    /// Parent post (channel replies).
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Sender id.
    #[serde(default)]
    pub from_id: Option<String>,
    /// Sender name.
    #[serde(default)]
    pub from_name: String,
    /// Sent by you.
    #[serde(default)]
    pub mine: bool,
    /// RFC 3339 time.
    #[serde(default)]
    pub time: Option<String>,
    /// Edited.
    #[serde(default)]
    pub edited: bool,
    /// Deleted.
    #[serde(default)]
    pub deleted: bool,
    /// System/event message.
    #[serde(default)]
    pub system: bool,
    /// Not sent yet.
    #[serde(default)]
    pub pending: bool,
    /// Subject (channel posts).
    #[serde(default)]
    pub subject: Option<String>,
    /// `normal`, `high`, `urgent`.
    #[serde(default)]
    pub importance: String,
    /// Sanitised HTML.
    #[serde(default)]
    pub html: String,
    /// Reactions.
    #[serde(default)]
    pub reactions: Vec<ReactionRow>,
    /// Files.
    #[serde(default)]
    pub files: Vec<FileRow>,
    /// Number of cards (not rendered).
    #[serde(default)]
    pub cards: u32,
    /// Quoted message.
    #[serde(default)]
    pub quote: Option<QuoteRow>,
    /// Mentions you.
    #[serde(default)]
    pub mentions_me: bool,
}

impl MessageRow {
    /// Grouping metadata.
    pub fn meta(&self) -> MsgMeta {
        MsgMeta {
            from: self.from_id.clone().or_else(|| (!self.from_name.is_empty()).then(|| self.from_name.clone())),
            time: self.time.as_deref().and_then(parse_time),
            system: self.system,
        }
    }
}

/// The current time and local UTC offset (minutes east).
pub fn now_and_offset() -> (i64, i64) {
    let d = js_sys::Date::new_0();
    let now = d.get_time();
    let now = if now.is_finite() { now as i64 } else { 0 };
    let off = d.get_timezone_offset();
    let off = if off.is_finite() { -(off as i64) } else { 0 };
    (now, off)
}

/// What a message action does; the owner turns it into an engine command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Toggle a reaction (`remove` when you already reacted).
    React {
        /// Message.
        id: String,
        /// Emoji.
        emoji: String,
        /// Remove instead of add.
        remove: bool,
    },
    /// Quote / reply to this message.
    Reply(String),
    /// Edit (your own).
    Edit(String),
    /// Delete (your own), after confirmation.
    Delete(String),
    /// Open a file link.
    OpenFile(String),
}

/// Opens `url` through the engine (`app.open_url` checks the scheme and asks the OS).
pub fn open_url(url: String) {
    let state = crate::state::app_state();
    crate::commands::dispatch(state, "app.open_url", json!({ "url": url }));
}

/// Opens links inside rendered message HTML through the engine instead of navigating.
pub fn intercept_links(e: web_sys::MouseEvent) {
    use wasm_bindgen::JsCast;
    let Some(el) = e.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
    let Ok(Some(a)) = el.closest("a[href]") else { return };
    let href = a.get_attribute("href").unwrap_or_default();
    e.prevent_default();
    let target = quorum_link_target(&href).unwrap_or(href);
    if target.starts_with("https://") || target.starts_with("http://") || target.starts_with("mailto:") {
        open_url(target);
    }
}

/// Decodes a `quorum-link:<percent-encoded>` href (pure).
pub fn quorum_link_target(href: &str) -> Option<String> {
    let enc = href.strip_prefix("quorum-link:")?;
    let bytes = enc.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = enc.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// One message bubble with its meta line, reactions and hover actions.
#[component]
pub fn Bubble(
    /// The message.
    m: MessageRow,
    /// Show avatar and name (first of a group).
    header: bool,
    /// Last of a group.
    tail: bool,
    /// Action handler.
    on_action: Callback<Action>,
    /// Offer "reply" (quote) in the actions.
    #[prop(optional)]
    reply_label: Option<&'static str>,
    /// Local UTC offset in minutes.
    offset: i64,
) -> impl IntoView {
    let confirm = RwSignal::new(false);
    let time_ms = m.time.as_deref().and_then(parse_time);
    let time = time_ms.map(|t| clock(t, offset)).unwrap_or_default();
    if m.system {
        let text = if m.html.trim().is_empty() || m.html.contains("systemEventMessage") {
            "Chat updated".to_owned()
        } else {
            m.html.clone()
        };
        return view! { <div class="msg-system" role="note">{text}" · "{time}</div> }.into_any();
    }
    let id = m.id.clone();
    let mine = m.mine;
    let can_act = !m.pending && !m.deleted;
    let name = m.from_name.clone();
    let from_id = m.from_id.clone();
    let html = m.html.clone();
    let deleted = m.deleted;
    let reactions = m.reactions.clone();
    let files = m.files.clone();
    let quote = m.quote.clone();
    let act = move |a: Action| on_action.run(a);
    let id_r = id.clone();
    let id_e = id.clone();
    let id_d = id.clone();
    let id_reply = id.clone();
    view! {
        <div
            class="msg"
            class:mine=mine
            class:header=header
            class:tail=tail
            class:pending=m.pending
            class:mentioned=m.mentions_me
            class:important=m.importance != "normal" && !m.importance.is_empty()
            data-message=id.clone()
            tabindex="-1"
        >
            <div class="msg-gutter">
                {(header && !mine).then(|| match from_id.clone().filter(|id| !id.is_empty()) {
                    Some(user) => view! { <PersonAvatar user=user name=name.clone() size="sm" /> }.into_any(),
                    None => view! { <Avatar name=name.clone() size="sm" /> }.into_any(),
                })}
            </div>
            <div class="msg-main">
                {header
                    .then(|| {
                        view! {
                            <div class="msg-meta">
                                {(!mine).then(|| view! { <span class="msg-name">{name.clone()}</span> })}
                                <time class="msg-time">{time.clone()}</time>
                                {m.edited.then(|| view! { <span class="msg-edited">"Edited"</span> })}
                                {m.pending.then(|| view! { <span class="msg-sending">"Sending…"</span> })}
                            </div>
                        }
                    })}
                {m.subject.clone().map(|s| view! { <div class="msg-subject">{s}</div> })}
                {quote
                    .map(|q| {
                        view! {
                            <blockquote class="msg-quote">
                                {q.from.map(|f| view! { <span class="msg-quote-from">{f}</span> })}
                                <span class="msg-quote-text">{q.preview}</span>
                            </blockquote>
                        }
                    })}
                <div class="msg-bubble-row">
                    {if deleted {
                        view! { <div class="msg-bubble deleted">"This message has been deleted."</div> }.into_any()
                    } else {
                        view! {
                            <div class="msg-bubble">
                                {(!html.trim().is_empty())
                                    .then(|| view! { <div class="msg-html" inner_html=html.clone() on:click=intercept_links></div> })}
                                {(!files.is_empty())
                                    .then(|| {
                                        view! {
                                            <div class="msg-files">
                                                {files
                                                    .iter()
                                                    .map(|f| {
                                                        let url = f.url.clone();
                                                        let has = url.is_some();
                                                        view! {
                                                            <button
                                                                class="file-card"
                                                                type="button"
                                                                disabled=!has
                                                                title=f.name.clone()
                                                                on:click=move |_| {
                                                                    if let Some(u) = url.clone() {
                                                                        act(Action::OpenFile(u));
                                                                    }
                                                                }
                                                            >
                                                                <span class="file-card-icon"><CIcon name=ChatIcon::Doc /></span>
                                                                <span class="file-card-name">{f.name.clone()}</span>
                                                            </button>
                                                        }
                                                    })
                                                    .collect_view()}
                                            </div>
                                        }
                                    })}
                                {(m.cards > 0).then(|| view! { <div class="msg-card-note">"Card (open in Teams to view)"</div> })}
                            </div>
                        }
                            .into_any()
                    }}
                    {can_act
                        .then(|| {
                            view! {
                                <div class="msg-actions" role="toolbar" aria-label="Message actions">
                                    {QUICK_REACTIONS
                                        .iter()
                                        .map(|e| {
                                            let e: &'static str = e;
                                            let id = id_r.clone();
                                            let already = reactions.iter().any(|r| r.emoji == e && r.mine);
                                            view! {
                                                <button
                                                    class="msg-action emoji"
                                                    type="button"
                                                    aria-label=format!("React {e}")
                                                    on:click=move |_| act(Action::React { id: id.clone(), emoji: e.into(), remove: already })
                                                >
                                                    {e}
                                                </button>
                                            }
                                        })
                                        .collect_view()}
                                    {reply_label
                                        .map(|label| {
                                            let id = id_reply.clone();
                                            view! {
                                                <button
                                                    class="msg-action"
                                                    type="button"
                                                    title=label
                                                    aria-label=label
                                                    on:click=move |_| act(Action::Reply(id.clone()))
                                                >
                                                    <CIcon name=ChatIcon::Quote />
                                                </button>
                                            }
                                        })}
                                    {mine
                                        .then(|| {
                                            let (ide, idd) = (id_e.clone(), id_d.clone());
                                            view! {
                                                <button
                                                    class="msg-action"
                                                    type="button"
                                                    title="Edit"
                                                    aria-label="Edit"
                                                    on:click=move |_| act(Action::Edit(ide.clone()))
                                                >
                                                    <CIcon name=ChatIcon::Edit />
                                                </button>
                                                <button
                                                    class="msg-action danger"
                                                    type="button"
                                                    title="Delete"
                                                    aria-label="Delete"
                                                    on:click=move |_| confirm.set(true)
                                                >
                                                    <CIcon name=ChatIcon::Trash />
                                                </button>
                                                {move || {
                                                    let idd = idd.clone();
                                                    confirm
                                                        .get()
                                                        .then(|| {
                                                            view! {
                                                                <div class="msg-confirm" role="alertdialog" aria-label="Delete message?">
                                                                    <span>"Delete this message?"</span>
                                                                    <button
                                                                        class="danger-button"
                                                                        type="button"
                                                                        on:click=move |_| {
                                                                            confirm.set(false);
                                                                            act(Action::Delete(idd.clone()));
                                                                        }
                                                                    >
                                                                        "Delete"
                                                                    </button>
                                                                    <button class="link-button" type="button" on:click=move |_| confirm.set(false)>
                                                                        "Cancel"
                                                                    </button>
                                                                </div>
                                                            }
                                                        })
                                                }}
                                            }
                                        })}
                                </div>
                            }
                        })}
                </div>
                {(!reactions.is_empty())
                    .then(|| {
                        view! {
                            <div class="msg-reactions">
                                {reactions
                                    .iter()
                                    .map(|r| {
                                        let (id, emoji, remove) = (id.clone(), r.emoji.clone(), r.mine);
                                        let title = if r.names.is_empty() { r.emoji.clone() } else { r.names.join(", ") };
                                        view! {
                                            <button
                                                class="reaction-chip"
                                                class:mine=r.mine
                                                type="button"
                                                title=title
                                                aria-pressed=if r.mine { "true" } else { "false" }
                                                disabled=!can_act
                                                on:click=move |_| act(Action::React { id: id.clone(), emoji: emoji.clone(), remove })
                                            >
                                                <span class="reaction-emoji">{r.emoji.clone()}</span>
                                                <span class="reaction-count">{r.count}</span>
                                            </button>
                                        }
                                    })
                                    .collect_view()}
                            </div>
                        }
                    })}
            </div>
        </div>
    }
    .into_any()
}

/// Renders grouped messages (oldest first) with day separators.
pub fn render_messages(
    items: &[MessageRow],
    on_action: Callback<Action>,
    reply_label: Option<&'static str>,
) -> impl IntoView + use<> {
    let (now, offset) = now_and_offset();
    let metas: Vec<MsgMeta> = items.iter().map(MessageRow::meta).collect();
    group(&metas, now, offset)
        .into_iter()
        .filter_map(|e| match e {
            Entry::Day { label, .. } => {
                Some(view! { <div class="day-separator" role="separator"><span>{label}</span></div> }.into_any())
            }
            Entry::Message { index, header, tail } => items.get(index).cloned().map(|m| match reply_label {
                Some(l) => {
                    view! { <Bubble m=m header=header tail=tail on_action=on_action reply_label=l offset=offset /> }
                        .into_any()
                }
                None => view! { <Bubble m=m header=header tail=tail on_action=on_action offset=offset /> }.into_any(),
            }),
        })
        .collect_view()
}

/// Scroll helpers for a conversation container.
pub fn at_bottom(el: &web_sys::HtmlElement) -> bool {
    let gap = el.scroll_height() - el.scroll_top() - el.client_height();
    gap < 48
}

/// Scrolls to the bottom (smoothly unless the user prefers reduced motion; CSS decides).
pub fn scroll_to_bottom(node: NodeRef<html::Div>, smooth: bool) {
    if let Some(el) = node.try_get_untracked().flatten() {
        let el: web_sys::HtmlElement = el.into();
        if smooth {
            let opts = web_sys::ScrollToOptions::new();
            opts.set_top(f64::from(el.scroll_height()));
            opts.set_behavior(web_sys::ScrollBehavior::Smooth);
            el.scroll_to_with_scroll_to_options(&opts);
        } else {
            el.set_scroll_top(el.scroll_height());
        }
    }
}

/// Params to react through an engine command id with `base` params.
pub fn react_params(mut base: Value, emoji: &str, remove: bool) -> Value {
    if let Some(o) = base.as_object_mut() {
        o.insert("reaction".into(), json!(emoji));
        o.insert("remove".into(), json!(remove));
    }
    base
}

/// Plain text of an HTML snippet (tags dropped, common entities decoded), for quote previews.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_quorum_links() {
        assert_eq!(
            quorum_link_target("quorum-link:https%3A%2F%2Fexample.com%2Fa"),
            Some("https://example.com/a".into())
        );
        assert_eq!(quorum_link_target("https://x"), None);
        assert_eq!(quorum_link_target("quorum-link:%zz"), None);
        assert_eq!(quorum_link_target("quorum-link:%e"), None);
    }

    #[test]
    fn rows_parse_leniently() {
        let m: MessageRow = serde_json::from_value(json!({"id": "1"})).unwrap();
        assert_eq!(m.meta(), MsgMeta { from: None, time: None, system: false });
        let m: MessageRow =
            serde_json::from_value(json!({"id": "1", "from_name": "A", "time": "1970-01-01T00:00:01Z"})).unwrap();
        assert_eq!(m.meta().from.as_deref(), Some("A"));
        assert_eq!(m.meta().time, Some(1000));
    }
}
