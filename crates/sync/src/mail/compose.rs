//! The structured fields of a mail draft (stored encrypted in `Draft::fields`; the text being
//! written is `Draft::body`, HTML).

use jiff::Timestamp;
use quorum_graph::api::mail::{OutgoingMessage, ReplyKind};
use quorum_model::{BodyType, EmailAddress, Importance, MessageBody, MessageId, Sensitive};
use serde::{Deserialize, Serialize};

/// The most recipients per field.
pub const MAX_RECIPIENTS: usize = 500;
/// The most attachments per draft.
pub const MAX_ATTACHMENTS: usize = 50;
/// The longest subject accepted, in bytes.
pub const MAX_SUBJECT_LEN: usize = 4096;

/// What a draft is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposeMode {
    /// A new message.
    #[default]
    New,
    /// A reply to the sender.
    Reply,
    /// A reply to everyone.
    ReplyAll,
    /// A forward.
    Forward,
}

impl ComposeMode {
    /// The Graph response-draft kind, for replies and forwards.
    #[must_use]
    pub fn reply_kind(self) -> Option<ReplyKind> {
        match self {
            Self::New => None,
            Self::Reply => Some(ReplyKind::Reply),
            Self::ReplyAll => Some(ReplyKind::ReplyAll),
            Self::Forward => Some(ReplyKind::Forward),
        }
    }
}

/// A file attached to a draft; the bytes are in the blob cache under `blob_key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeAttachment {
    /// Local id within the draft.
    pub id: String,
    /// File name.
    pub name: Sensitive,
    /// MIME type.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Blob cache key of the bytes.
    pub blob_key: String,
}

/// The structured fields of a mail draft.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Compose {
    /// New, reply, reply all or forward.
    pub mode: ComposeMode,
    /// The message replied to or forwarded.
    pub source: Option<MessageId>,
    /// To.
    pub to: Vec<EmailAddress>,
    /// Cc.
    pub cc: Vec<EmailAddress>,
    /// Bcc.
    pub bcc: Vec<EmailAddress>,
    /// Subject.
    pub subject: Sensitive,
    /// Importance (`None` = normal).
    pub importance: Option<Importance>,
    /// Ask for a read receipt.
    pub request_read_receipt: bool,
    /// Attached files.
    pub attachments: Vec<ComposeAttachment>,
    /// The server draft, once synced.
    pub server_id: Option<MessageId>,
    /// The quoted original Graph put in a reply/forward draft (kept below the user's text).
    pub quote_html: Option<Sensitive>,
    /// Attachment ids already added to the server draft.
    pub uploaded: Vec<String>,
    /// Queued to send (hidden from editing until sent or undone).
    pub sending: bool,
    /// When the queued send goes out (undo is possible until then).
    pub send_at: Option<Timestamp>,
    /// The outbox entry of the queued send.
    pub outbox_id: Option<i64>,
}

impl Compose {
    /// Whether the draft has at least one recipient.
    #[must_use]
    pub fn has_recipients(&self) -> bool {
        !(self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty())
    }

    /// The Graph message for this draft with `html` as the body (the quote appended), without
    /// attachments.
    #[must_use]
    pub fn outgoing(&self, html: &str) -> OutgoingMessage {
        let content = match &self.quote_html {
            Some(q) if !q.is_empty() => merge_quote(html, q.as_str()),
            _ => html.to_owned(),
        };
        OutgoingMessage {
            subject: self.subject.clone(),
            body: MessageBody { content_type: BodyType::Html, content: Sensitive::new(content) },
            to: self.to.clone(),
            cc: self.cc.clone(),
            bcc: self.bcc.clone(),
            importance: self.importance.clone(),
            request_read_receipt: self.request_read_receipt,
            ..OutgoingMessage::default()
        }
    }
}

/// Puts `user` HTML above the quoted original: just inside `<body…>` when the quote is a full
/// document, otherwise in front of it.
#[must_use]
pub fn merge_quote(user: &str, quote: &str) -> String {
    let lower = quote.to_ascii_lowercase();
    let insert_at = lower
        .find("<body")
        .and_then(|start| lower.get(start..).and_then(|rest| rest.find('>')).map(|end| start + end + 1));
    match insert_at.and_then(|at| quote.get(..at).zip(quote.get(at..))) {
        Some((head, tail)) => format!("{head}<div>{user}</div>{tail}"),
        None => format!("<div>{user}</div>{quote}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_goes_below_the_user_text() {
        assert_eq!(merge_quote("hi", "<p>old</p>"), "<div>hi</div><p>old</p>");
        assert_eq!(
            merge_quote("hi", "<html><BODY class=x><p>old</p></body></html>"),
            "<html><BODY class=x><div>hi</div><p>old</p></body></html>"
        );
        // A `<body` without `>` never panics.
        assert_eq!(merge_quote("hi", "<body"), "<div>hi</div><body");
        assert_eq!(merge_quote("é", "<bodyé"), "<div>é</div><bodyé");
    }
}
