//! On-demand message bodies and attachment downloads.

use quorum_graph::ErrorKind as GraphKind;
use quorum_graph::api::mail::Mailbox;
use quorum_model::{Attachment, AttachmentId, AttachmentKind, BodyType, MessageId};

use crate::{Ctx, Error, ErrorKind, Result};

/// Inline (`cid:`) images downloaded with a body, at most.
pub const MAX_INLINE_ATTACHMENTS: usize = 20;
/// The largest inline image downloaded with a body.
pub const MAX_INLINE_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;
/// The largest attachment downloaded on demand.
pub const MAX_DOWNLOAD_BYTES: u64 = 150 * 1024 * 1024;

/// The blob cache key of an attachment's content.
#[must_use]
pub fn attachment_blob_key(message: &MessageId, attachment: &AttachmentId) -> String {
    // Hash-free and deterministic; ids are validated (no control characters), and the store
    // caps key length, so overlong ids fall back to a digest.
    let key = format!("mail-att:{}:{}", message.as_str(), attachment.as_str());
    if key.len() <= 400 { key } else { format!("mail-att:{:016x}", fnv(&key)) }
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// Fetches a message's body (and attachment metadata, and its inline images) into the store.
/// A message Graph no longer has is removed locally.
///
/// # Errors
/// Graph or store failures; [`ErrorKind::Permanent`] for a message that is gone.
pub async fn fetch_body(ctx: &Ctx, id: &MessageId) -> Result<()> {
    let full = match ctx.graph.get_message(&Mailbox::Me, id).await {
        Ok(m) => m,
        Err(e) if e.kind() == GraphKind::NotFound => {
            let gone = id.clone();
            ctx.store.write(move |tx| tx.delete_messages(std::slice::from_ref(&gone))).await?;
            return Err(Error::new(ErrorKind::Permanent, "message no longer exists"));
        }
        Err(e) => return Err(e.into()),
    };
    let html_has_cid =
        full.body.content_type == BodyType::Html && full.body.content.as_str().to_ascii_lowercase().contains("cid:");
    let mut attachments: Option<Vec<Attachment>> = None;
    if (full.message.has_attachments || html_has_cid)
        && let Ok(page) = ctx.graph.list_attachments(&Mailbox::Me, id).await
    {
        attachments = Some(page.items);
    }
    // Inline images: the list cannot select contentId, so read each inline one's metadata.
    let mut inline = Vec::new();
    if let Some(list) = attachments.as_mut() {
        for a in list.iter_mut().filter(|a| a.is_inline && a.kind == AttachmentKind::File).take(MAX_INLINE_ATTACHMENTS)
        {
            if a.content_id.is_none()
                && let Ok(meta) = ctx.graph.get_attachment(&Mailbox::Me, id, &a.id).await
            {
                a.content_id = meta.content_id;
            }
            if a.size <= MAX_INLINE_ATTACHMENT_BYTES {
                inline.push(a.id.clone());
            }
        }
    }
    let message = full.message;
    let body = full.body;
    let mid = id.clone();
    let atts = attachments.clone();
    ctx.store
        .write(move |tx| {
            tx.upsert_messages(std::slice::from_ref(&message))?;
            tx.put_message_body(&mid, &body)?;
            if let Some(a) = &atts {
                tx.put_attachments(&mid, a)?;
            }
            Ok(())
        })
        .await?;
    for att in inline {
        if let Err(e) = download_attachment(ctx, id, &att).await {
            tracing::debug!(name: "sync.mail.inline_image_failed", kind = ?e.kind(), "inline image download failed");
        }
    }
    Ok(())
}

/// Downloads an attachment's bytes into the blob cache (unless cached) and returns the blob
/// key and size.
///
/// # Errors
/// [`ErrorKind::Invalid`] for an unknown attachment or one over [`MAX_DOWNLOAD_BYTES`]; Graph
/// and store failures.
pub async fn download_attachment(ctx: &Ctx, message: &MessageId, attachment: &AttachmentId) -> Result<(String, u64)> {
    let list = ctx.store.attachments(message.clone()).await?;
    let stored = list.items.into_iter().find(|a| a.attachment.id == *attachment);
    let Some(stored) = stored else { return Err(Error::new(ErrorKind::Invalid, "no such attachment")) };
    if let Some(key) = &stored.blob_key
        && let Some(bytes) = ctx.store.get_blob(key.clone()).await?
    {
        return Ok((key.clone(), u64::try_from(bytes.len()).unwrap_or(u64::MAX)));
    }
    if stored.attachment.size > MAX_DOWNLOAD_BYTES {
        return Err(Error::new(ErrorKind::Invalid, "attachment is too large to download"));
    }
    let bytes = ctx.graph.attachment_content(&Mailbox::Me, message, attachment).await?;
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let key = attachment_blob_key(message, attachment);
    ctx.store.put_blob(key.clone(), bytes).await?;
    let (m, a, k) = (message.clone(), attachment.clone(), key.clone());
    ctx.store.write(move |tx| tx.set_attachment_blob(&m, &a, Some(&k))).await?;
    Ok((key, size))
}
