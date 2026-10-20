//! Teams chat and channel message sanitising.

use std::collections::{HashMap, HashSet};

use ammonia::{Builder, UrlRelative};

use crate::Sanitized;
use crate::limits::truncate_input;
use crate::policy::{Mode, Policy};
use crate::prepass;

/// Tags allowed in Teams message HTML (`at` is Teams' @mention element).
const CHAT_TAGS: &[&str] = &[
    "p",
    "br",
    "b",
    "strong",
    "i",
    "em",
    "u",
    "s",
    "strike",
    "del",
    "a",
    "ul",
    "ol",
    "li",
    "code",
    "pre",
    "blockquote",
    "img",
    "at",
    "div",
    "span",
];

/// Tags removed together with their content.
const CHAT_CLEAN_CONTENT_TAGS: &[&str] = &[
    "script",
    "style",
    "title",
    "template",
    "iframe",
    "frame",
    "frameset",
    "object",
    "embed",
    "applet",
    "noembed",
    "noframes",
    "svg",
    "math",
    "select",
    "textarea",
    "button",
    "head",
    "xmp",
    "plaintext",
    "noscript",
];

/// Per-tag attributes.
const CHAT_TAG_ATTRIBUTES: &[(&str, &[&str])] =
    &[("a", &["href", "title"]), ("img", &["src", "alt", "width", "height"]), ("at", &["id"])];

const CHAT_URL_SCHEMES: &[&str] = &["http", "https", "mailto", "tel"];

/// Sanitises Teams chat or channel message HTML.
///
/// Allows only basic formatting, lists, code, quotes, links (rewritten to `quorum-link:`),
/// `<at id="N">` mentions (numeric ids only) and inline images that are Graph
/// `hostedContents` URLs (rewritten to `quorum-hosted:` so the core fetches them with the
/// user's token). Other images lose their `src` and count as blocked remote images.
///
/// ```
/// let out = quorum_sanitize::sanitize_chat(r#"<p>Hi <at id="0">Alex</at><img src=x onerror=alert(1)></p>"#);
/// assert!(out.html.contains(r#"<at id="0">Alex</at>"#));
/// assert!(!out.html.contains("onerror"));
/// ```
#[must_use]
pub fn sanitize_chat(html: &str) -> Sanitized {
    let (input, truncated) = truncate_input(html);
    let input = prepass::limit_nesting(input);

    let policy = Policy::new(Mode::Chat, false, HashMap::new());
    let tag_attributes: HashMap<&str, HashSet<&str>> =
        CHAT_TAG_ATTRIBUTES.iter().map(|(tag, attrs)| (*tag, attrs.iter().copied().collect())).collect();

    let mut builder = Builder::empty();
    builder
        .tags(CHAT_TAGS.iter().copied().collect())
        .clean_content_tags(CHAT_CLEAN_CONTENT_TAGS.iter().copied().collect())
        .generic_attributes(HashSet::new())
        .tag_attributes(tag_attributes)
        .url_schemes(CHAT_URL_SCHEMES.iter().copied().collect())
        .url_relative(UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer"))
        .strip_comments(true);
    let filter_policy = policy.clone();
    builder.attribute_filter(move |element, attribute, value| filter_policy.filter(element, attribute, value));

    let clean = builder.clean(&input).to_string();
    drop(builder);
    Sanitized {
        html: clean,
        had_remote_images: policy.had_remote_images(),
        blocked_count: policy.blocked_count(),
        truncated,
    }
}
