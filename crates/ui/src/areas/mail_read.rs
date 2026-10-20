//! The Mail reading pane: headers, categories, remote-images bar, attachment chips, the other
//! messages of the conversation, and the body in a sandboxed frame.
//!
//! The body never enters the main DOM. On the desktop the frame loads
//! `quorum-mail://localhost/<id>` (the shell serves the core's sanitised `body_html`); in
//! browser preview it gets the same sanitised HTML as `srcdoc`. Either way the frame is
//! `sandbox=""` (no scripts, no same-origin, no forms, no popups) and the document carries a CSP.

use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::Reply;
use serde::Deserialize;
use serde_json::json;

use super::mail::{category_class, mail_ui, now_and_offset};
use super::mail_icons::{MIcon, MailIcon};
use super::mail_util::{display_name, frame_document, frame_url, long_time, short_time, size};
use crate::components::person_card::PersonAvatar;
use crate::components::{Avatar, EmptyState, Icon, Spinner};
use crate::icons::IconName;
use crate::state::{Area, QueryState, ToastLevel, app_state, use_query};

/// Tables whose changes refresh the open message.
pub const MESSAGE_TABLES: &[&str] = &["message", "message_body", "attachment", "setting"];

/// A recipient.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Address {
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Address.
    #[serde(default)]
    pub address: Option<String>,
}

impl Address {
    fn label(&self) -> String {
        display_name(self.name.as_deref().unwrap_or_default(), self.address.as_deref().unwrap_or_default())
    }
}

/// An attachment of the open message.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Attachment {
    /// Attachment id.
    pub id: String,
    /// File name.
    #[serde(default)]
    pub name: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
    /// MIME type.
    #[serde(default)]
    pub content_type: Option<String>,
}

/// The open message (`mail.message`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MessageView {
    /// Message id.
    pub id: String,
    /// Subject.
    #[serde(default)]
    pub subject: String,
    /// Sender.
    #[serde(default)]
    pub from_address: Option<Address>,
    /// Sender name (fallback).
    #[serde(default)]
    pub from_name: String,
    /// Sender address (fallback).
    #[serde(default)]
    pub from_email: String,
    /// To.
    #[serde(default)]
    pub to_recipients: Vec<Address>,
    /// Cc.
    #[serde(default)]
    pub cc_recipients: Vec<Address>,
    /// Received (RFC 3339).
    #[serde(default)]
    pub received: Option<String>,
    /// Sanitised body.
    #[serde(default)]
    pub body_html: Option<String>,
    /// The body is being downloaded.
    #[serde(default)]
    pub body_loading: bool,
    /// Remote images were blocked.
    #[serde(default)]
    pub remote_images_blocked: bool,
    /// Attachments (not inline images).
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Categories.
    #[serde(default)]
    pub categories: Vec<String>,
    /// Flagged.
    #[serde(default)]
    pub flagged: bool,
    /// `high` importance.
    #[serde(default)]
    pub importance: String,
    /// Conversation id.
    #[serde(default)]
    pub conversation_id: Option<String>,
}

/// A message of the conversation strip (`mail.conversation`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ConversationRow {
    /// Message id.
    pub id: String,
    /// Sender name.
    #[serde(default)]
    pub from_name: String,
    /// Sender address.
    #[serde(default)]
    pub from_email: String,
    /// Preview.
    #[serde(default)]
    pub preview: String,
    /// Received.
    #[serde(default)]
    pub received: Option<String>,
    /// Unread.
    #[serde(default)]
    pub unread: bool,
}

fn recipients_line(list: &[Address]) -> String {
    list.iter().map(Address::label).collect::<Vec<_>>().join("; ")
}

/// The sandboxed body frame.
#[component]
fn MailFrame(id: String, html: String, remote: bool) -> impl IntoView {
    let title = "Message body";
    if crate::ipc::backend() == crate::ipc::Backend::Tauri {
        view! { <iframe class="mail-frame" sandbox="" referrerpolicy="no-referrer" title=title src=frame_url(&id, remote)></iframe> }
            .into_any()
    } else {
        view! { <iframe class="mail-frame" sandbox="" referrerpolicy="no-referrer" title=title srcdoc=frame_document(&html)></iframe> }
            .into_any()
    }
}

#[component]
pub(crate) fn RemoteImagesBar(id: String, sender: String) -> impl IntoView {
    let state = app_state();
    let ui = mail_ui();
    let id_once = id.clone();
    let has_sender = !sender.is_empty();
    view! {
        <div class="mail-banner" role="status">
            <MIcon name=MailIcon::Image />
            <span>"Pictures from the internet were blocked to protect your privacy."</span>
            <button class="mail-banner-button" on:click=move |_| {
                ui.remote_once.update(|s| {
                    s.insert(id_once.clone());
                });
            }>"Show pictures"</button>
            {has_sender.then(|| {
                let id = id.clone();
                view! {
                    <button class="mail-banner-button" on:click=move |_| {
                        let id = id.clone();
                        spawn_local(async move {
                            let reply = crate::ipc::run("mail.allow_remote_images", json!({ "id": id })).await;
                            match reply {
                                Reply::Ok { .. } => state.toast(ToastLevel::Success, "Pictures from this sender will show automatically"),
                                Reply::Err { message, .. } => state.toast(ToastLevel::Error, message),
                            }
                        });
                    }>"Always show from this sender"</button>
                }
            })}
        </div>
    }
}

#[component]
pub(crate) fn AttachmentChip(message: String, att: Attachment) -> impl IntoView {
    let state = app_state();
    let ext = att.name.rsplit_once('.').map(|(_, e)| e.to_ascii_uppercase()).unwrap_or_default();
    let label = format!("Download {}", att.name);
    let busy = RwSignal::new(false);
    let att_id = att.id.clone();
    view! {
        <button class="mail-attachment" title=label.clone() aria-label=label on:click=move |_| {
            if busy.get_untracked() {
                return;
            }
            busy.set(true);
            let (message, att_id) = (message.clone(), att_id.clone());
            spawn_local(async move {
                let reply = crate::ipc::run("mail.download_attachment", json!({ "id": message, "attachment": att_id })).await;
                busy.try_set(false);
                if let Reply::Err { message, .. } = reply {
                    state.toast(ToastLevel::Error, message);
                }
            });
        }>
            <span class="mail-attachment-kind" aria-hidden="true">{if ext.len() <= 4 && !ext.is_empty() { ext } else { "FILE".into() }}</span>
            <span class="mail-attachment-text">
                <span class="mail-attachment-name">{att.name.clone()}</span>
                <span class="mail-attachment-size">{size(att.size)}</span>
            </span>
            {move || if busy.get() { view! { <Spinner label="Downloading" /> }.into_any() } else { view! { <MIcon name=MailIcon::Download /> }.into_any() }}
        </button>
    }
}

#[component]
fn ConversationStrip(id: String, conversation: String) -> impl IntoView {
    let state = app_state();
    let rows = use_query::<Vec<ConversationRow>>(
        "mail.conversation",
        move || json!({ "id": conversation.clone() }),
        &["message"],
    );
    move || {
        let others: Vec<ConversationRow> =
            rows.data().map(|r| r.iter().filter(|r| r.id != id).cloned().collect()).unwrap_or_default();
        (!others.is_empty()).then(|| {
            let (now, offset) = now_and_offset();
            view! {
                <div class="mail-thread" aria-label="Other messages in this conversation">
                    <div class="mail-thread-title">{format!("{} more in this conversation", others.len())}</div>
                    {others
                        .into_iter()
                        .map(|r| {
                            let who = display_name(&r.from_name, &r.from_email);
                            let when = r.received.as_deref().map(|t| short_time(t, now, offset)).unwrap_or_default();
                            let target = r.id.clone();
                            view! {
                                <button class="mail-thread-row" class:unread=r.unread on:click=move |_| {
                                    state.selection(Area::Mail).item.set(Some(target.clone()));
                                }>
                                    <span class="mail-thread-from">{who}</span>
                                    <span class="mail-thread-preview">{r.preview.clone()}</span>
                                    <span class="mail-thread-time">{when}</span>
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
            }
        })
    }
}

fn action(state: crate::state::AppState, cmd: &'static str, id: String) -> impl Fn(web_sys::MouseEvent) + 'static {
    move |_| crate::commands::dispatch(state, cmd, json!({ "id": id.clone() }))
}

/// What the reading pane shows.
#[derive(Debug, Clone, PartialEq)]
enum Shown {
    Loading,
    Failed(String),
    Ready(Box<MessageView>),
}

/// The reading pane for message `id`.
#[component]
pub fn ReadingPane(
    /// The message to show.
    id: String,
) -> impl IntoView {
    let state = app_state();
    let ui = mail_ui();
    let remote_once = ui.remote_once.clone();
    let qid = id.clone();
    let message = use_query::<MessageView>(
        "mail.message",
        move || {
            let allow = remote_once.with(|s| s.contains(&qid));
            json!({ "id": qid.clone(), "allow_remote_images": allow })
        },
        MESSAGE_TABLES,
    );
    // The body arrives in the background: poll briefly until it does.
    Effect::new(move |_| {
        let loading = message.state.with(|s| s.data().is_some_and(|m| m.body_loading));
        if loading {
            set_timeout(move || message.refetch(), std::time::Duration::from_millis(1200));
        }
    });
    let remote_set = ui.remote_once.clone();
    // Re-render (and reload the frame) only when what is shown changes, not on every refresh.
    let shown = Memo::new(move |_| match message.state.get() {
        QueryState::Loading => Shown::Loading,
        QueryState::Failed { message, .. } => Shown::Failed(message),
        QueryState::Ready(m) => Shown::Ready(Box::new((*m).clone())),
    });
    move || match shown.get() {
        Shown::Loading => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
        Shown::Failed(message) => {
            view! { <EmptyState title="Couldn't open this message" detail=message error=true /> }.into_any()
        }
        Shown::Ready(m) => {
            let m = *m;
            let (_, offset) = now_and_offset();
            let from = m.from_address.clone().unwrap_or_default();
            let from_name = from.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| m.from_name.clone());
            let from_addr = from.address.clone().unwrap_or_else(|| m.from_email.clone());
            let shown = display_name(&from_name, &from_addr);
            let when = m.received.as_deref().map(|t| long_time(t, offset)).unwrap_or_default();
            let subject = if m.subject.trim().is_empty() { "(no subject)".to_owned() } else { m.subject.clone() };
            let remote = remote_set.with(|s| s.contains(&m.id));
            let to = recipients_line(&m.to_recipients);
            let cc = recipients_line(&m.cc_recipients);
            let body = match (&m.body_html, m.body_loading) {
                (Some(html), _) if !html.is_empty() => {
                    view! { <MailFrame id=m.id.clone() html=html.clone() remote=remote /> }.into_any()
                }
                (_, true) => {
                    view! { <div class="mail-body-loading"><Spinner label="Downloading message" /></div> }.into_any()
                }
                _ => view! { <MailFrame id=m.id.clone() html=String::new() remote=remote /> }.into_any(),
            };
            let (r1, r2, r3) = (m.id.clone(), m.id.clone(), m.id.clone());
            view! {
                <article class="mail-reading" tabindex="-1" data-pane-focus="" aria-label=subject.clone()>
                    <header class="mail-read-header">
                        <h1 class="mail-read-subject">
                            {(m.importance == "high").then(|| view! { <span class="mail-important" title="High importance"><MIcon name=MailIcon::Important /></span> })}
                            {subject.clone()}
                        </h1>
                        {(!m.categories.is_empty()).then(|| view! {
                            <div class="mail-categories">
                                {m.categories.iter().map(|c| view! { <span class=format!("mail-category {}", category_class(c))>{c.clone()}</span> }).collect_view()}
                            </div>
                        })}
                        <div class="mail-read-from">
                            {match crate::components::person_card::mail_user(&from_addr) {
                                Some(user) => view! { <PersonAvatar user=user name=shown.clone() /> }.into_any(),
                                None => view! { <Avatar name=shown.clone() /> }.into_any(),
                            }}
                            <div class="mail-read-people">
                                <div class="mail-read-sender">
                                    <span class="mail-read-name">{shown.clone()}</span>
                                    {(!from_addr.is_empty() && from_addr != shown).then(|| view! { <span class="mail-read-address">{format!("<{from_addr}>")}</span> })}
                                </div>
                                {(!to.is_empty()).then(|| view! { <div class="mail-read-to"><span class="mail-read-label">"To: "</span>{to.clone()}</div> })}
                                {(!cc.is_empty()).then(|| view! { <div class="mail-read-to"><span class="mail-read-label">"Cc: "</span>{cc.clone()}</div> })}
                            </div>
                            <div class="mail-read-side">
                                <div class="mail-read-actions">
                                    <button class="mail-read-action" title="Reply" aria-label="Reply" on:click=action(state, "mail.reply", r1)>
                                        <Icon name=IconName::Reply />
                                    </button>
                                    <button class="mail-read-action" title="Reply all" aria-label="Reply all" on:click=action(state, "mail.reply_all", r2)>
                                        <MIcon name=MailIcon::ReplyAll />
                                    </button>
                                    <button class="mail-read-action" title="Forward" aria-label="Forward" on:click=action(state, "mail.forward", r3)>
                                        <Icon name=IconName::Forward />
                                    </button>
                                </div>
                                <time class="mail-read-time">{when}</time>
                            </div>
                        </div>
                    </header>
                    {m.remote_images_blocked.then(|| view! { <RemoteImagesBar id=m.id.clone() sender=from_addr.clone() /> })}
                    {(!m.attachments.is_empty()).then(|| {
                        let id = m.id.clone();
                        view! {
                            <div class="mail-attachments" aria-label="Attachments">
                                {m.attachments.iter().cloned().map(|a| view! { <AttachmentChip message=id.clone() att=a /> }).collect_view()}
                            </div>
                        }
                    })}
                    <div class="mail-body">{body}</div>
                    {m.conversation_id.clone().map(|c| view! { <ConversationStrip id=m.id.clone() conversation=c /> })}
                </article>
            }
            .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_view_decodes_minimal_and_full_shapes() {
        let m: MessageView = serde_json::from_value(json!({ "id": "m" })).unwrap();
        assert!(m.body_html.is_none() && m.attachments.is_empty());
        let m: MessageView = serde_json::from_value(json!({
            "id": "m", "from_address": { "name": null, "address": "a@example.com" },
            "to_recipients": [{ "name": "B", "address": "b@example.com" }, { "address": "c@example.com" }],
            "attachments": [{ "id": "1", "name": "x.pdf", "size": 10 }],
        }))
        .unwrap();
        assert_eq!(m.from_address.unwrap_or_default().label(), "a@example.com");
        assert_eq!(recipients_line(&m.to_recipients), "B; c@example.com");
    }
}
