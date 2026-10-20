//! Outbox handlers: replay queued chat and channel writes against Graph.
//!
//! Each handler parses its [`payload`](super::payload) type; a payload that does not parse, or
//! a request Graph's validation rejects, is a permanent failure (the user retries or discards).
//! On success the handler replaces the optimistic local item with Graph's version and returns
//! `{"id": ..., "poke": <resource keys>}` so the affected resources re-sync right away.

use std::sync::Arc;

use futures::future::BoxFuture;
use jiff::Timestamp;
use quorum_graph::ChunkSize;
use quorum_graph::api::chat::{
    FileReference, HostedImage, MentionTarget, OutgoingMention, OutgoingMessage, QuotedMessage,
};
use quorum_graph::api::files::{ConflictBehavior, MAX_SMALL_UPLOAD_BYTES};
use quorum_model::{
    BodyType, ChannelId, ChatBody, ChatImportance, ChatType, DriveItem, DriveItemId, GraphEnum, Sensitive, TeamId,
    UserId,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::payload::{
    CreatePayload, DeletePayload, EditPayload, MemberPayload, MentionKind, MentionPayload, PinPayload, ReactPayload,
    ReadPayload, SendPayload, Target,
};
use super::{CHATS_KEY, OPEN_CHAT_SETTING, chat_key, graph_err};
use crate::{Ctx, Error, ErrorKind, OutboxHandler, Result};

/// The OneDrive folder chat attachments are uploaded to, as Teams does.
pub const CHAT_FILES_FOLDER: &str = "Microsoft Teams Chat Files";
/// Setting prefix mapping a local chat id to the created chat's id: `chat.created:<local id>`.
pub const CREATED_PREFIX: &str = "chat.created:";

/// Every outbox kind this module handles.
pub const KINDS: &[&str] = &[
    "chat.send",
    "chat.edit",
    "chat.delete",
    "chat.react",
    "chat.unreact",
    "chat.mark_read",
    "chat.mark_unread",
    "chat.create",
    "chat.add_member",
    "chat.remove_member",
    "chat.pin_message",
    "chat.unpin_message",
    "channel.post",
    "channel.reply",
    "channel.edit",
    "channel.delete",
    "channel.react",
];

type ApplyFn = for<'a> fn(&'a Ctx, Value) -> BoxFuture<'a, Result<Value>>;
type RevertFn = for<'a> fn(&'a Ctx, Value) -> BoxFuture<'a, Result<()>>;

struct FnHandler {
    kind: &'static str,
    apply: ApplyFn,
    revert: Option<RevertFn>,
}

impl OutboxHandler for FnHandler {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        (self.apply)(ctx, payload)
    }

    fn revert<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<()>> {
        match self.revert {
            Some(f) => f(ctx, payload),
            None => Box::pin(async { Ok(()) }),
        }
    }
}

/// The handlers, one per entry of [`KINDS`].
#[must_use]
pub fn handlers() -> Vec<Arc<dyn OutboxHandler>> {
    let h = |kind: &'static str, apply: ApplyFn, revert: Option<RevertFn>| -> Arc<dyn OutboxHandler> {
        Arc::new(FnHandler { kind, apply, revert })
    };
    vec![
        h("chat.send", send, Some(revert_send)),
        h("channel.post", send, Some(revert_send)),
        h("channel.reply", send, Some(revert_send)),
        h("chat.edit", edit, Some(revert_edit)),
        h("channel.edit", edit, Some(revert_edit)),
        h("chat.delete", delete, Some(revert_delete)),
        h("channel.delete", delete, Some(revert_delete)),
        h("chat.react", react, Some(revert_react)),
        h("chat.unreact", react, Some(revert_react)),
        h("channel.react", react, Some(revert_react)),
        h("chat.mark_read", mark_read, None),
        h("chat.mark_unread", mark_unread, None),
        h("chat.create", create, Some(revert_create)),
        h("chat.add_member", add_member, None),
        h("chat.remove_member", remove_member, None),
        h("chat.pin_message", pin, None),
        h("chat.unpin_message", unpin, None),
    ]
}

fn parse<T: DeserializeOwned>(payload: Value) -> Result<T> {
    serde_json::from_value(payload)
        .map_err(|e| Error::new(ErrorKind::Permanent, "outbox payload is not valid").with_source(e))
}

fn permanent(msg: &str) -> Error {
    Error::new(ErrorKind::Permanent, msg.to_owned())
}

fn mention(m: MentionPayload) -> Result<OutgoingMention> {
    let name = Sensitive::new(m.text.clone());
    let target = match m.kind {
        MentionKind::User => MentionTarget::User {
            id: UserId::new(m.target).map_err(|_| permanent("invalid mentioned user"))?,
            display_name: name.clone(),
        },
        MentionKind::Channel => MentionTarget::Channel {
            id: ChannelId::new(m.target).map_err(|_| permanent("invalid mentioned channel"))?,
            display_name: name.clone(),
        },
        MentionKind::Team => MentionTarget::Team {
            id: TeamId::new(m.target).map_err(|_| permanent("invalid mentioned team"))?,
            display_name: name.clone(),
        },
        MentionKind::Tag => MentionTarget::Tag { id: m.target, display_name: name.clone() },
    };
    Ok(OutgoingMention { id: m.id, text: name, target })
}

async fn blob(ctx: &Ctx, key: &str) -> Result<Vec<u8>> {
    ctx.store.get_blob(key.to_owned()).await?.ok_or_else(|| permanent("attachment is no longer cached"))
}

/// Finds or creates the user's "Microsoft Teams Chat Files" folder.
async fn chat_files_folder(ctx: &Ctx) -> Result<(quorum_model::DriveId, DriveItemId)> {
    let drive = ctx.graph.my_drive().await.map_err(graph_err)?;
    match ctx.graph.get_drive_item_by_path(&drive.id, CHAT_FILES_FOLDER).await {
        Ok(item) => Ok((drive.id, item.id)),
        Err(e) if e.kind() == quorum_graph::ErrorKind::NotFound => {
            let root = DriveItemId::new("root").map_err(|_| Error::new(ErrorKind::Bug, "root id"))?;
            let folder = ctx
                .graph
                .create_folder(&drive.id, &root, CHAT_FILES_FOLDER, ConflictBehavior::Fail)
                .await
                .map_err(graph_err)?;
            Ok((drive.id, folder.id))
        }
        Err(e) => Err(graph_err(e)),
    }
}

/// Uploads one attachment into the chat files folder (simple upload up to 4 MiB, else a
/// resumable upload session).
async fn upload_file(ctx: &Ctx, name: &str, content_type: &str, bytes: Vec<u8>) -> Result<DriveItem> {
    let (drive, folder) = chat_files_folder(ctx).await?;
    let ct = if content_type.is_empty() { "application/octet-stream" } else { content_type };
    if bytes.len() <= MAX_SMALL_UPLOAD_BYTES {
        ctx.graph.upload_small(&drive, &folder, name, ct, bytes, ConflictBehavior::Rename).await.map_err(graph_err)
    } else {
        ctx.graph
            .upload_large(&drive, &folder, name, ConflictBehavior::Rename, &bytes, ChunkSize::default())
            .await
            .map_err(graph_err)
    }
}

/// Builds the Graph message from a send payload (reading images and uploading files).
async fn outgoing(ctx: &Ctx, p: &SendPayload) -> Result<OutgoingMessage> {
    let mut msg = OutgoingMessage::html(p.html.clone());
    msg.subject = p.subject.clone().filter(|s| !s.trim().is_empty()).map(Sensitive::new);
    msg.importance = p.importance.as_deref().map(ChatImportance::from_graph).unwrap_or_default();
    msg.mentions = p.mentions.iter().cloned().map(mention).collect::<Result<_>>()?;
    for img in &p.images {
        msg.images.push(HostedImage {
            temporary_id: img.temp_id.clone(),
            content_type: img.content_type.clone(),
            bytes: blob(ctx, &img.blob).await?,
        });
    }
    for f in &p.files {
        let bytes = blob(ctx, &f.blob).await?;
        let item = upload_file(ctx, &f.name, &f.content_type, bytes).await?;
        let reference = FileReference::from_drive_item(&item).ok_or_else(|| permanent("uploaded file has no link"))?;
        msg.files.push(reference);
    }
    if let Some(q) = &p.quote {
        msg.quote = Some(QuotedMessage {
            message_id: q.message_id.clone(),
            preview: Sensitive::new(q.preview.clone()),
            sender: q.sender_id.clone().map(|id| (id, Sensitive::new(q.sender_name.clone().unwrap_or_default()))),
        });
    }
    Ok(msg)
}

fn send(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: SendPayload = parse(payload)?;
        let msg = outgoing(ctx, &p).await?;
        let sent = match &p.target {
            Target::Chat { chat, .. } => ctx.graph.send_chat_message(chat, &msg).await,
            Target::Channel { team, channel, message: None, .. } => {
                ctx.graph.post_channel_message(team, channel, &msg).await
            }
            Target::Channel { team, channel, message: Some(post), .. } => {
                ctx.graph.reply_to_channel_message(team, channel, post, &msg).await
            }
        }
        .map_err(graph_err)?;
        let container = p.target.container();
        let local = p.local_id.clone();
        let id = sent.id.clone();
        ctx.store
            .write(move |tx| {
                tx.delete_chat_messages(&container, std::slice::from_ref(&local))?;
                tx.upsert_chat_messages(std::slice::from_ref(&sent))?;
                Ok(())
            })
            .await?;
        for key in p.images.iter().map(|i| &i.blob).chain(p.files.iter().map(|f| &f.blob)) {
            let _ = ctx.store.delete_blob(key.clone()).await;
        }
        Ok(json!({ "id": id, "poke": p.target.resource_key() }))
    })
}

fn revert_send(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: SendPayload = parse(payload)?;
        let container = p.target.container();
        ctx.store.write(move |tx| tx.delete_chat_messages(&container, std::slice::from_ref(&p.local_id))).await?;
        Ok(())
    })
}

fn target_ref(t: &Target) -> Result<quorum_graph::api::chat::MessageRef> {
    t.message_ref().ok_or_else(|| permanent("the target names no message"))
}

async fn restore(ctx: &Ctx, before: Option<quorum_model::ChatMessage>) -> Result<()> {
    if let Some(m) = before {
        ctx.store.upsert_chat_messages(vec![m]).await?;
    }
    Ok(())
}

fn edit(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: EditPayload = parse(payload)?;
        let r = target_ref(&p.target)?;
        let body = ChatBody { content_type: BodyType::Html, content: Sensitive::new(p.html.clone()) };
        let mentions: Vec<OutgoingMention> = p.mentions.iter().cloned().map(mention).collect::<Result<_>>()?;
        ctx.graph.edit_message(&r, &body, &mentions).await.map_err(graph_err)?;
        Ok(json!({ "poke": p.target.resource_key() }))
    })
}

fn revert_edit(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: EditPayload = parse(payload)?;
        restore(ctx, p.before).await
    })
}

fn delete(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: DeletePayload = parse(payload)?;
        let r = target_ref(&p.target)?;
        match ctx.graph.soft_delete_message(&r).await {
            Ok(()) => {}
            // Already gone: the goal is reached.
            Err(e) if e.kind() == quorum_graph::ErrorKind::NotFound => {}
            Err(e) => return Err(graph_err(e)),
        }
        Ok(json!({ "poke": p.target.resource_key() }))
    })
}

fn revert_delete(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: DeletePayload = parse(payload)?;
        restore(ctx, p.before).await
    })
}

fn react(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: ReactPayload = parse(payload)?;
        let r = target_ref(&p.target)?;
        if p.remove {
            ctx.graph.unset_reaction(&r, &p.reaction).await.map_err(graph_err)?;
        } else {
            ctx.graph.set_reaction(&r, &p.reaction).await.map_err(graph_err)?;
        }
        Ok(json!({ "poke": p.target.resource_key() }))
    })
}

fn revert_react(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: ReactPayload = parse(payload)?;
        restore(ctx, p.before).await
    })
}

fn mark_read(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: ReadPayload = parse(payload)?;
        ctx.graph.mark_chat_read(&p.chat, &p.user, &p.tenant).await.map_err(graph_err)?;
        Ok(json!({ "poke": CHATS_KEY }))
    })
}

fn mark_unread(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: ReadPayload = parse(payload)?;
        let last_read = p.last_read.unwrap_or_else(Timestamp::now);
        ctx.graph.mark_chat_unread(&p.chat, &p.user, &p.tenant, last_read).await.map_err(graph_err)?;
        Ok(json!({ "poke": CHATS_KEY }))
    })
}

fn create(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: CreatePayload = parse(payload)?;
        let chat_type = ChatType::from_graph(&p.chat_type);
        let chat = ctx.graph.create_chat(&chat_type, &p.members, p.topic.as_deref()).await.map_err(graph_err)?;
        let id = chat.id.clone();
        let local = p.local_id.clone();
        {
            let (id, local, chat) = (id.clone(), local.clone(), chat.clone());
            ctx.store
                .write(move |tx| {
                    let local_messages: Vec<_> = tx.read().chat_messages(&local, None, 50)?.items;
                    tx.delete_chats(std::slice::from_ref(&local))?;
                    tx.upsert_chats(std::slice::from_ref(&chat))?;
                    // Keep optimistic messages visible under the real chat until the sync replaces them.
                    let moved: Vec<_> = local_messages
                        .into_iter()
                        .map(|mut m| {
                            m.container = quorum_model::MessageContainer::Chat(id.clone());
                            m
                        })
                        .collect();
                    tx.upsert_chat_messages(&moved)?;
                    tx.set_setting(&format!("{CREATED_PREFIX}{}", local.as_str()), &id.as_str())?;
                    let open: Option<String> = tx.read().setting(OPEN_CHAT_SETTING)?;
                    if open.as_deref() == Some(local.as_str()) {
                        tx.set_setting(OPEN_CHAT_SETTING, &id.as_str())?;
                    }
                    Ok(())
                })
                .await?;
        }
        if let Some(html) = p.message.filter(|m| !m.trim().is_empty()) {
            let sent = ctx.graph.send_chat_message(&id, &OutgoingMessage::html(html)).await.map_err(graph_err)?;
            let chat_id = id.clone();
            ctx.store
                .write(move |tx| {
                    let stale: Vec<_> = tx
                        .read()
                        .chat_messages(&chat_id, None, 50)?
                        .items
                        .into_iter()
                        .filter(|m| m.id.as_str().starts_with("local-"))
                        .map(|m| m.id)
                        .collect();
                    tx.delete_chat_messages(&sent.container, &stale)?;
                    tx.upsert_chat_messages(std::slice::from_ref(&sent))?;
                    Ok(())
                })
                .await?;
        }
        Ok(json!({ "id": id, "local_id": local, "poke": [CHATS_KEY, chat_key(&id)] }))
    })
}

fn revert_create(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: CreatePayload = parse(payload)?;
        ctx.store.write(move |tx| tx.delete_chats(std::slice::from_ref(&p.local_id))).await?;
        Ok(())
    })
}

fn add_member(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: MemberPayload = parse(payload)?;
        let history = if p.share_history { "1900-01-01T00:00:00Z".parse::<Timestamp>().ok() } else { None };
        ctx.graph.add_chat_member(&p.chat, &p.user, history).await.map_err(graph_err)?;
        Ok(json!({ "poke": [CHATS_KEY, chat_key(&p.chat)] }))
    })
}

fn remove_member(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: MemberPayload = parse(payload)?;
        let members = ctx.graph.list_chat_members(&p.chat).await.map_err(graph_err)?;
        let wanted = p.user.as_str();
        let found = members.into_iter().find(|m| {
            m.member.user_id.as_ref().is_some_and(|u| u.as_str().eq_ignore_ascii_case(wanted))
                || m.member.email.as_ref().is_some_and(|e| e.as_str().eq_ignore_ascii_case(wanted))
        });
        if let Some(m) = found {
            ctx.graph.remove_chat_member(&p.chat, &m.membership_id).await.map_err(graph_err)?;
        }
        Ok(json!({ "poke": [CHATS_KEY, chat_key(&p.chat)] }))
    })
}

fn pin(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: PinPayload = parse(payload)?;
        let id = ctx.graph.pin_message(&p.chat, &p.message).await.map_err(graph_err)?;
        Ok(json!({ "pinned_id": id }))
    })
}

fn unpin(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: PinPayload = parse(payload)?;
        let pins = ctx.graph.list_pinned_messages(&p.chat).await.map_err(graph_err)?;
        for pin in pins.into_iter().filter(|x| x.message.as_ref().is_some_and(|m| m.id == p.message)) {
            ctx.graph.unpin_message(&p.chat, &pin.pinned_id).await.map_err(graph_err)?;
        }
        Ok(Value::Null)
    })
}
