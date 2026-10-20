//! Outbox handlers: replay queued mail writes against Graph.

use std::sync::Arc;

use futures::future::BoxFuture;
use quorum_graph::ErrorKind as GraphKind;
use quorum_graph::api::mail::{FileAttachmentUpload, Mailbox, MailboxSettings, MessageUpdate};
use quorum_model::{InferenceClassification, MessageId};
use quorum_store::{Draft, DraftKind};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::compose::Compose;
use super::payload::{
    AutoReplyPayload, DeletePayload, DraftDeletePayload, DraftSyncPayload, KIND_AUTO_REPLY, KIND_DELETE_PERMANENT,
    KIND_DRAFT_DELETE, KIND_DRAFT_SYNC, KIND_MOVE, KIND_SEND, KIND_UPDATE, MovePayload, SendPayload, UpdatePayload,
};
use crate::{Ctx, Error, ErrorKind, OutboxHandler, Result};

/// Every mail handler.
pub(crate) fn all() -> Vec<Arc<dyn OutboxHandler>> {
    vec![
        Arc::new(UpdateHandler),
        Arc::new(MoveHandler),
        Arc::new(DeletePermanentHandler),
        Arc::new(SendHandler),
        Arc::new(DraftSyncHandler),
        Arc::new(DraftDeleteHandler),
        Arc::new(AutoReplyHandler),
    ]
}

fn parse<T: DeserializeOwned>(payload: Value) -> Result<T> {
    serde_json::from_value(payload).map_err(|_| Error::new(ErrorKind::Permanent, "malformed mail outbox payload"))
}

/// Graph says the item is gone: for deletes and updates of a deleted message, the goal is met.
fn is_gone(e: &quorum_graph::Error) -> bool {
    e.kind() == GraphKind::NotFound
}

/// Reads, changes and re-saves a local draft's fields (no-op if the draft is gone).
async fn persist(ctx: &Ctx, draft_id: &str, f: impl FnOnce(&mut Compose) + Send + 'static) -> Result<()> {
    let id = draft_id.to_owned();
    ctx.store
        .write(move |tx| {
            let Some(mut d) = tx.read().draft(&id)? else { return Ok(()) };
            let mut c: Compose = serde_json::from_value(d.fields.clone()).unwrap_or_default();
            f(&mut c);
            d.fields = serde_json::to_value(&c).unwrap_or(Value::Null);
            tx.save_draft(&d).map(|_| ())
        })
        .await?;
    Ok(())
}

/// Creates the server draft for `c` if it has none yet (a response draft for replies and
/// forwards, whose quoted original is kept in `quote_html`), and records it locally.
async fn ensure_server_draft(ctx: &Ctx, draft_id: &str, c: &mut Compose) -> Result<MessageId> {
    if let Some(id) = &c.server_id {
        return Ok(id.clone());
    }
    let (server, quote) = match (c.mode.reply_kind(), &c.source) {
        (Some(kind), Some(source)) => {
            let mut overrides = c.outgoing("");
            overrides.body = quorum_model::MessageBody::default();
            let d = ctx.graph.create_response_draft(&Mailbox::Me, source, kind, Some(&overrides)).await?;
            let quote = ctx.graph.get_message(&Mailbox::Me, &d.id).await.ok().map(|full| full.body.content);
            (d.id, quote)
        }
        _ => {
            let d = ctx.graph.create_draft(&Mailbox::Me, &c.outgoing("")).await?;
            (d.id, None)
        }
    };
    c.server_id = Some(server.clone());
    c.quote_html = quote.clone();
    let (s, q) = (server.clone(), quote);
    persist(ctx, draft_id, move |c| {
        c.server_id = Some(s);
        c.quote_html = q;
    })
    .await?;
    Ok(server)
}

async fn load_attachments(ctx: &Ctx, c: &Compose, skip: &[String]) -> Result<Vec<(String, FileAttachmentUpload)>> {
    let mut out = Vec::new();
    for a in c.attachments.iter().filter(|a| !skip.contains(&a.id)) {
        let bytes = ctx
            .store
            .get_blob(a.blob_key.clone())
            .await?
            .ok_or_else(|| Error::new(ErrorKind::Permanent, "an attachment is no longer available"))?;
        out.push((
            a.id.clone(),
            FileAttachmentUpload {
                name: a.name.clone(),
                content_type: a.content_type.clone(),
                bytes,
                is_inline: false,
                content_id: None,
            },
        ));
    }
    Ok(out)
}

/// `mail.update`: read state, flag, categories, Focused/Other.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateHandler;

impl OutboxHandler for UpdateHandler {
    fn kind(&self) -> &'static str {
        KIND_UPDATE
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let p: UpdatePayload = parse(payload)?;
            let update = MessageUpdate {
                is_read: p.is_read,
                flag: p.flag.clone(),
                categories: p.categories.clone(),
                importance: None,
                inference_classification: p
                    .focused
                    .map(|f| if f { InferenceClassification::Focused } else { InferenceClassification::Other }),
            };
            if update == MessageUpdate::default() {
                return Ok(Value::Null);
            }
            match ctx.graph.update_message(&Mailbox::Me, &p.id, &update).await {
                Ok(_) => Ok(Value::Null),
                Err(e) if is_gone(&e) => Ok(Value::Null),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn revert<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let p: UpdatePayload = parse(payload)?;
            let prev = p.previous.clone();
            ctx.store
                .write(move |tx| {
                    tx.modify_message(&p.id, |m| {
                        if p.is_read.is_some() {
                            m.is_read = prev.is_read;
                        }
                        if p.flag.is_some() {
                            m.flag_status = prev.flag.clone();
                        }
                        if p.categories.is_some() {
                            m.categories = prev.categories.clone();
                        }
                        if p.focused.is_some() {
                            m.inference_classification = if prev.focused {
                                InferenceClassification::Focused
                            } else {
                                InferenceClassification::Other
                            };
                        }
                    })
                    .map(|_| ())
                })
                .await?;
            Ok(())
        })
    }
}

/// `mail.move` (also archive and delete-to-Deleted-Items).
#[derive(Debug, Clone, Copy, Default)]
pub struct MoveHandler;

impl OutboxHandler for MoveHandler {
    fn kind(&self) -> &'static str {
        KIND_MOVE
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let p: MovePayload = parse(payload)?;
            let moved = ctx.graph.move_message(&Mailbox::Me, &p.id, &p.destination.folder_ref()).await?;
            let old = p.id.clone();
            let folder = moved.parent_folder_id.clone();
            ctx.store
                .write(move |tx| {
                    // Keep the locally known header (read state etc.); take the server's folder
                    // and, should the id have changed, the server's id.
                    if moved.id == old {
                        tx.modify_message(&old, |m| {
                            if folder.is_some() {
                                m.parent_folder_id = folder;
                            }
                        })?;
                    } else {
                        tx.delete_messages(std::slice::from_ref(&old))?;
                        tx.upsert_messages(std::slice::from_ref(&moved))?;
                    }
                    Ok(())
                })
                .await?;
            Ok(json!({ "moved": true }))
        })
    }

    fn revert<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let p: MovePayload = parse(payload)?;
            if let Some(prev) = p.previous_folder {
                ctx.store.write(move |tx| tx.move_message(&p.id, &prev).map(|_| ())).await?;
            }
            Ok(())
        })
    }
}

/// `mail.delete_permanent`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeletePermanentHandler;

impl OutboxHandler for DeletePermanentHandler {
    fn kind(&self) -> &'static str {
        KIND_DELETE_PERMANENT
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let p: DeletePayload = parse(payload)?;
            match ctx.graph.delete_message(&Mailbox::Me, &p.id).await {
                Ok(()) => Ok(Value::Null),
                Err(e) if is_gone(&e) => Ok(Value::Null),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn revert<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let p: DeletePayload = parse(payload)?;
            if let Some(m) = p.message {
                ctx.store.write(move |tx| tx.upsert_messages(std::slice::from_ref(&m))).await?;
            }
            Ok(())
        })
    }
}

/// `mail.send`: `sendMail` for a new message, otherwise update and send the server draft
/// (replies and forwards go through `createReply*` / `createForward` so Graph threads them).
#[derive(Debug, Clone, Copy, Default)]
pub struct SendHandler;

impl OutboxHandler for SendHandler {
    fn kind(&self) -> &'static str {
        KIND_SEND
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let p: SendPayload = parse(payload)?;
            let mut c = p.compose;
            if !c.has_recipients() {
                return Err(Error::new(ErrorKind::Permanent, "a message needs at least one recipient"));
            }
            // A retry after a crash continues with the server draft recorded locally.
            if let Some(local) = ctx.store.draft(p.draft_id.clone()).await?
                && let Ok(saved) = serde_json::from_value::<Compose>(local.fields)
            {
                if c.server_id.is_none() {
                    c.server_id = saved.server_id;
                    c.quote_html = saved.quote_html.or(c.quote_html);
                }
                c.uploaded = saved.uploaded;
            }
            let html = p.body_html.as_str();
            if c.mode == super::ComposeMode::New && c.server_id.is_none() {
                let mut out = c.outgoing(html);
                out.attachments = load_attachments(ctx, &c, &[]).await?.into_iter().map(|(_, a)| a).collect();
                ctx.graph.send_mail(&Mailbox::Me, &out, true).await?;
            } else {
                let server = ensure_server_draft(ctx, &p.draft_id, &mut c).await?;
                ctx.graph.update_draft(&Mailbox::Me, &server, &c.outgoing(html)).await?;
                for (id, att) in load_attachments(ctx, &c, &c.uploaded.clone()).await? {
                    ctx.graph.add_attachment(&Mailbox::Me, &server, &att).await?;
                    let done = id.clone();
                    persist(ctx, &p.draft_id, move |c| c.uploaded.push(done)).await?;
                }
                ctx.graph.send_draft(&Mailbox::Me, &server).await?;
            }
            let keys: Vec<String> = c.attachments.iter().map(|a| a.blob_key.clone()).collect();
            ctx.store.delete_draft(p.draft_id.clone()).await?;
            for k in keys {
                let _ = ctx.store.delete_blob(k).await;
            }
            Ok(json!({ "sent": true }))
        })
    }

    fn revert<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let p: SendPayload = parse(payload)?;
            // The user discarded the failed send: the draft is editable again.
            persist(ctx, &p.draft_id, |c| {
                c.sending = false;
                c.send_at = None;
                c.outbox_id = None;
            })
            .await
        })
    }
}

/// `mail.draft_sync`: mirrors a local draft into the server's Drafts folder.
#[derive(Debug, Clone, Copy, Default)]
pub struct DraftSyncHandler;

impl OutboxHandler for DraftSyncHandler {
    fn kind(&self) -> &'static str {
        KIND_DRAFT_SYNC
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let p: DraftSyncPayload = parse(payload)?;
            let Some(d): Option<Draft> = ctx.store.draft(p.draft_id.clone()).await? else { return Ok(Value::Null) };
            if d.kind != DraftKind::Mail {
                return Ok(Value::Null);
            }
            let mut c: Compose = serde_json::from_value(d.fields).unwrap_or_default();
            if c.sending || (c.server_id.is_none() && c.mode == super::ComposeMode::Forward && !c.has_recipients()) {
                return Ok(Value::Null);
            }
            let server = ensure_server_draft(ctx, &p.draft_id, &mut c).await?;
            match ctx.graph.update_draft(&Mailbox::Me, &server, &c.outgoing(d.body.as_str())).await {
                Ok(_) => Ok(json!({ "server_id": server })),
                Err(e) if is_gone(&e) => {
                    // The server draft was deleted elsewhere: recreate it next time.
                    persist(ctx, &p.draft_id, |c| {
                        c.server_id = None;
                        c.uploaded.clear();
                    })
                    .await?;
                    Err(Error::new(ErrorKind::Remote, "server draft was deleted; recreating"))
                }
                Err(e) => Err(e.into()),
            }
        })
    }
}

/// `mail.draft_delete`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DraftDeleteHandler;

impl OutboxHandler for DraftDeleteHandler {
    fn kind(&self) -> &'static str {
        KIND_DRAFT_DELETE
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let p: DraftDeletePayload = parse(payload)?;
            match ctx.graph.delete_message(&Mailbox::Me, &p.server_id).await {
                Ok(()) => Ok(Value::Null),
                Err(e) if is_gone(&e) => Ok(Value::Null),
                Err(e) => Err(e.into()),
            }
        })
    }
}

/// `mail.auto_reply`: automatic replies (`PATCH /me/mailboxSettings`).
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoReplyHandler;

impl OutboxHandler for AutoReplyHandler {
    fn kind(&self) -> &'static str {
        KIND_AUTO_REPLY
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let p: AutoReplyPayload = parse(payload)?;
            let settings = MailboxSettings { automatic_replies_setting: Some(p.setting), ..MailboxSettings::default() };
            ctx.graph.update_mailbox_settings(&Mailbox::Me, &settings).await?;
            Ok(Value::Null)
        })
    }
}
