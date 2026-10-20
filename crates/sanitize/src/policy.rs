//! URL policy shared by the mail and chat sanitisers.
//!
//! ammonia calls the attribute filter for every attribute that survived its allow-list and its
//! URL-scheme check; the filter rewrites URLs and records what it blocked. ammonia requires the
//! callback to be `Send + Sync + 'static`, so per-call state lives behind an `Arc` with atomics.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ammonia::Url;

use crate::style;
use crate::urls::{self, CID_SCHEME, HOSTED_SCHEME, LINK_SCHEME, REMOTE_SCHEME};

/// Which allow-list and image policy applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Mail,
    Chat,
}

/// Per-call policy and counters.
#[derive(Debug)]
pub(crate) struct Policy {
    pub(crate) mode: Mode,
    pub(crate) allow_remote_images: bool,
    /// Normalised content id → caller token.
    pub(crate) cid_map: HashMap<String, String>,
    pub(crate) had_remote_images: AtomicBool,
    pub(crate) blocked: AtomicUsize,
}

/// What to do with an image URL.
pub(crate) enum ImageDecision {
    /// Use this URL instead.
    Rewrite(String),
    /// Keep the value as it is.
    Keep,
    /// Replace with this fixed value (e.g. the placeholder).
    Replace(&'static str),
    /// Remove the attribute.
    Drop,
}

/// Raster image types allowed as `data:` URLs (never SVG, which can carry script).
const DATA_IMAGE_PREFIXES: &[&str] = &[
    "data:image/png;",
    "data:image/gif;",
    "data:image/jpeg;",
    "data:image/jpg;",
    "data:image/webp;",
    "data:image/bmp;",
];

/// Normalises a Content-ID for lookup: trims whitespace and `<>`, lowercases ASCII.
pub(crate) fn normalize_cid(cid: &str) -> String {
    cid.trim().trim_start_matches('<').trim_end_matches('>').trim().to_ascii_lowercase()
}

impl Policy {
    pub(crate) fn new(mode: Mode, allow_remote_images: bool, cid_map: HashMap<String, String>) -> Arc<Self> {
        Arc::new(Self {
            mode,
            allow_remote_images,
            cid_map,
            had_remote_images: AtomicBool::new(false),
            blocked: AtomicUsize::new(0),
        })
    }

    pub(crate) fn block(&self) {
        self.blocked.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn blocked_count(&self) -> usize {
        self.blocked.load(Ordering::Relaxed)
    }

    pub(crate) fn had_remote_images(&self) -> bool {
        self.had_remote_images.load(Ordering::Relaxed)
    }

    /// The ammonia attribute filter.
    pub(crate) fn filter<'u>(&self, element: &str, attribute: &str, value: &'u str) -> Option<Cow<'u, str>> {
        match (element, attribute) {
            ("a", "href") => self.rewrite_link(value).map(Cow::Owned),
            ("img", "src") => match self.image(value) {
                ImageDecision::Rewrite(url) => Some(Cow::Owned(url)),
                ImageDecision::Keep => Some(Cow::Borrowed(value)),
                ImageDecision::Replace(fixed) => Some(Cow::Borrowed(fixed)),
                ImageDecision::Drop => None,
            },
            ("at", "id") => {
                let ok = !value.is_empty() && value.len() <= 6 && value.bytes().all(|b| b.is_ascii_digit());
                ok.then_some(Cow::Borrowed(value))
            }
            (_, "style") => {
                if self.mode == Mode::Chat {
                    return None;
                }
                style::filter_urls(value, self).map(Cow::Owned)
            }
            _ => Some(Cow::Borrowed(value)),
        }
    }

    /// Rewrites an allowed absolute link to `quorum-link:`; anything else is dropped.
    fn rewrite_link(&self, value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.starts_with('#') {
            // In-document anchors have no targets (ids are stripped), so drop them quietly.
            return None;
        }
        match Url::parse(trimmed) {
            Ok(url) if matches!(url.scheme(), "http" | "https" | "mailto" | "tel") => {
                Some(urls::encode(LINK_SCHEME, url.as_str()))
            }
            _ => {
                self.block();
                None
            }
        }
    }

    /// Decides what to do with an image URL (from `<img src>` or a CSS `url()`).
    pub(crate) fn image(&self, value: &str) -> ImageDecision {
        let trimmed = value.trim();
        let Ok(url) = Url::parse(trimmed) else {
            self.block();
            return ImageDecision::Drop;
        };
        match (self.mode, url.scheme()) {
            (_, "http" | "https") if self.mode == Mode::Chat && is_hosted_content(&url) => {
                ImageDecision::Rewrite(urls::encode(HOSTED_SCHEME, url.as_str()))
            }
            (Mode::Mail, "http" | "https") => {
                self.had_remote_images.store(true, Ordering::Relaxed);
                if self.allow_remote_images {
                    ImageDecision::Rewrite(urls::encode(REMOTE_SCHEME, url.as_str()))
                } else {
                    self.block();
                    ImageDecision::Replace(crate::mail::REMOTE_IMAGE_PLACEHOLDER)
                }
            }
            (Mode::Chat, "http" | "https") => {
                self.had_remote_images.store(true, Ordering::Relaxed);
                self.block();
                ImageDecision::Drop
            }
            (Mode::Mail, "cid") => {
                let raw = url.path();
                let cid = urls::percent_decode(raw).unwrap_or_else(|| raw.to_owned());
                match self.cid_map.get(&normalize_cid(&cid)) {
                    Some(token) => ImageDecision::Rewrite(urls::encode(CID_SCHEME, token)),
                    None => {
                        self.block();
                        ImageDecision::Drop
                    }
                }
            }
            (Mode::Mail, "data") => {
                let allowed = DATA_IMAGE_PREFIXES.iter().any(|p| {
                    trimmed.as_bytes().get(..p.len()).is_some_and(|head| head.eq_ignore_ascii_case(p.as_bytes()))
                });
                if allowed {
                    ImageDecision::Keep
                } else {
                    self.block();
                    ImageDecision::Drop
                }
            }
            _ => {
                self.block();
                ImageDecision::Drop
            }
        }
    }
}

/// Teams inline images are Graph `hostedContents` URLs.
fn is_hosted_content(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("graph.microsoft.com")
        && (url.path().starts_with("/v1.0/") || url.path().starts_with("/beta/"))
        && url.path().contains("/hostedContents/")
}
