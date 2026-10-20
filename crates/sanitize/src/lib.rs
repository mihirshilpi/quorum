//! Sanitising untrusted mail and chat HTML for display.
//!
//! Implements ADR 0006. Everything here treats its input as hostile and never panics:
//!
//! * [`sanitize_mail`] cleans a mail body with an allow-list built for real-world email (tables,
//!   legacy presentational attributes, inline styles limited to safe properties). Scripts, event
//!   handlers, forms, frames, plugins, `<base>`, `<meta>` and `<style>` blocks are removed.
//!   Links are rewritten to `quorum-link:` URLs, `cid:` images to `quorum-cid:` URLs through a
//!   caller-supplied map, and remote images are blocked (replaced with a transparent
//!   placeholder) unless allowed, in which case they become `quorum-remote:` proxy URLs.
//! * [`sanitize_chat`] does the same for Teams message HTML with a much smaller allow-list.
//! * [`to_plain_text`] extracts text for previews and full-text search.
//! * [`quote_start`] / [`quote_start_text`] find where a reply's quoted history begins.
//!
//! All `quorum-*:` URLs carry the original value percent-encoded; [`QuorumUrl::parse`] decodes
//! them so the app can show the real link target or fetch the image.
//!
//! Inputs larger than [`MAX_INPUT_BYTES`] are truncated on a char boundary. Before parsing, a
//! linear token pass drops start tags nested deeper than [`MAX_DEPTH`] (keeping their text) and
//! bounds unclosed formatting elements, so hostile nesting can neither overflow the stack nor
//! make the HTML tree builder quadratic.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing
)]
#![warn(missing_docs)]

mod chat;
mod limits;
mod mail;
mod policy;
mod prepass;
mod quote;
mod style;
mod text;
mod urls;

use std::fmt;

pub use chat::sanitize_chat;
pub use limits::{MAX_DEPTH, MAX_INPUT_BYTES};
pub use mail::{MailOptions, REMOTE_IMAGE_PLACEHOLDER, sanitize_mail};
pub use quote::{quote_start, quote_start_text};
pub use text::to_plain_text;
pub use urls::{CID_SCHEME, HOSTED_SCHEME, LINK_SCHEME, QuorumUrl, REMOTE_SCHEME, WebviewPlatform, webview_urls};

/// The result of sanitising HTML.
///
/// `Debug` prints only the length of the HTML (M-PUBLIC-DEBUG): it is message content.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Sanitized {
    /// The sanitised HTML, safe to render in the sandboxed mail frame.
    pub html: String,
    /// The input referenced at least one remote (`http`/`https`) image.
    pub had_remote_images: bool,
    /// How many URLs, images or style declarations were removed or replaced by policy (remote
    /// images while blocked, unmapped `cid:` images, disallowed link schemes, unsafe CSS).
    /// Elements and attributes outside the allow-list are dropped silently and not counted.
    pub blocked_count: usize,
    /// The input was larger than [`MAX_INPUT_BYTES`] and was truncated.
    pub truncated: bool,
}

impl Sanitized {
    /// Returns the sanitised HTML.
    #[must_use]
    pub fn html(&self) -> &str {
        &self.html
    }

    /// Consumes the result and returns the HTML.
    #[must_use]
    pub fn into_html(self) -> String {
        self.html
    }
}

impl fmt::Debug for Sanitized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sanitized")
            .field("html", &format_args!("{} bytes", self.html.len()))
            .field("had_remote_images", &self.had_remote_images)
            .field("blocked_count", &self.blocked_count)
            .field("truncated", &self.truncated)
            .finish()
    }
}
