//! Mail body sanitising.

use std::collections::{HashMap, HashSet};
use std::fmt;

use ammonia::{Builder, UrlRelative};

use crate::Sanitized;
use crate::limits::truncate_input;
use crate::policy::{Mode, Policy, normalize_cid};
use crate::prepass;

/// A 1×1 transparent GIF that replaces blocked remote images (keeps layout from collapsing).
pub const REMOTE_IMAGE_PLACEHOLDER: &str =
    "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

/// Options for [`sanitize_mail`].
///
/// ```
/// use quorum_sanitize::MailOptions;
/// let opts = MailOptions::default().with_cid("image001.png@01D9", "attachment-42");
/// assert!(!opts.allow_remote_images);
/// ```
#[derive(Clone, Default)]
pub struct MailOptions {
    /// Load remote images through the `quorum-remote:` proxy instead of blocking them.
    pub allow_remote_images: bool,
    /// Content-ID (without `cid:`; `<>` and ASCII case are ignored) → token placed in the
    /// `quorum-cid:` URL, typically the attachment id. Unmapped `cid:` images are removed.
    pub cid_map: HashMap<String, String>,
}

impl MailOptions {
    /// Maps a Content-ID to the token used in its `quorum-cid:` URL.
    #[must_use]
    pub fn with_cid(mut self, content_id: impl Into<String>, token: impl Into<String>) -> Self {
        self.cid_map.insert(content_id.into(), token.into());
        self
    }

    /// Sets whether remote images are loaded through the proxy.
    #[must_use]
    pub fn with_remote_images(mut self, allow: bool) -> Self {
        self.allow_remote_images = allow;
        self
    }
}

impl fmt::Debug for MailOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailOptions")
            .field("allow_remote_images", &self.allow_remote_images)
            .field("cid_map", &format_args!("{} entries", self.cid_map.len()))
            .finish()
    }
}

/// Tags allowed in mail.
const MAIL_TAGS: &[&str] = &[
    "a",
    "abbr",
    "acronym",
    "address",
    "article",
    "aside",
    "b",
    "bdi",
    "bdo",
    "big",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "font",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "main",
    "mark",
    "nav",
    "ol",
    "p",
    "pre",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "section",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "wbr",
];

/// Tags removed together with their content.
const MAIL_CLEAN_CONTENT_TAGS: &[&str] = &[
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

/// Attributes allowed on every allowed tag.
const MAIL_GENERIC_ATTRIBUTES: &[&str] = &["style", "title", "dir", "lang", "align", "valign"];

/// Per-tag attributes.
const MAIL_TAG_ATTRIBUTES: &[(&str, &[&str])] = &[
    ("a", &["href"]),
    ("img", &["src", "alt", "width", "height", "border", "hspace", "vspace"]),
    ("table", &["border", "cellpadding", "cellspacing", "width", "height", "bgcolor", "summary"]),
    ("tr", &["bgcolor", "height"]),
    ("td", &["colspan", "rowspan", "bgcolor", "nowrap", "width", "height", "abbr", "scope"]),
    ("th", &["colspan", "rowspan", "bgcolor", "nowrap", "width", "height", "abbr", "scope"]),
    ("col", &["span", "width"]),
    ("colgroup", &["span", "width"]),
    ("font", &["color", "face", "size"]),
    ("ol", &["start", "type", "reversed"]),
    ("ul", &["type"]),
    ("li", &["value", "type"]),
    ("hr", &["size", "width", "noshade", "color"]),
    ("time", &["datetime"]),
    ("details", &["open"]),
    ("bdo", &["dir"]),
];

/// CSS properties allowed in inline styles (no positioning, no `content`, no behaviours).
const MAIL_STYLE_PROPERTIES: &[&str] = &[
    "background",
    "background-color",
    "background-image",
    "background-position",
    "background-repeat",
    "background-size",
    "border",
    "border-bottom",
    "border-bottom-color",
    "border-bottom-left-radius",
    "border-bottom-right-radius",
    "border-bottom-style",
    "border-bottom-width",
    "border-collapse",
    "border-color",
    "border-left",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "border-radius",
    "border-right",
    "border-right-color",
    "border-right-style",
    "border-right-width",
    "border-spacing",
    "border-style",
    "border-top",
    "border-top-color",
    "border-top-left-radius",
    "border-top-right-radius",
    "border-top-style",
    "border-top-width",
    "border-width",
    "caption-side",
    "clear",
    "color",
    "direction",
    "display",
    "empty-cells",
    "float",
    "font",
    "font-family",
    "font-feature-settings",
    "font-kerning",
    "font-size",
    "font-stretch",
    "font-style",
    "font-variant",
    "font-weight",
    "height",
    "letter-spacing",
    "line-height",
    "list-style",
    "list-style-position",
    "list-style-type",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-height",
    "max-width",
    "min-height",
    "min-width",
    "mso-line-height-rule",
    "opacity",
    "outline",
    "overflow",
    "overflow-wrap",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "table-layout",
    "text-align",
    "text-decoration",
    "text-decoration-color",
    "text-decoration-line",
    "text-decoration-style",
    "text-indent",
    "text-overflow",
    "text-shadow",
    "text-transform",
    "unicode-bidi",
    "vertical-align",
    "visibility",
    "white-space",
    "width",
    "word-break",
    "word-spacing",
    "word-wrap",
];

/// URL schemes ammonia lets through to our attribute filter (which decides the final URL).
const MAIL_URL_SCHEMES: &[&str] = &["http", "https", "mailto", "tel", "cid", "data", "quorum-cid", "quorum-remote"];

/// Sanitises an untrusted mail body for display in the sandboxed mail frame.
///
/// Never panics and never fails: hostile input yields (possibly empty) safe HTML. See the
/// crate docs for the policy. Use [`Sanitized::had_remote_images`] to offer "load images".
///
/// ```
/// use quorum_sanitize::{MailOptions, sanitize_mail};
/// let out = sanitize_mail(
///     r#"<p onclick="x()">Hi <a href="https://example.com">there</a><script>bad()</script></p>"#,
///     &MailOptions::default(),
/// );
/// assert!(!out.html.contains("script") && !out.html.contains("onclick"));
/// assert!(out.html.contains(r#"href="quorum-link:https%3A%2F%2Fexample.com%2F""#));
/// ```
#[must_use]
pub fn sanitize_mail(html: &str, opts: &MailOptions) -> Sanitized {
    let (input, truncated) = truncate_input(html);
    let input = prepass::limit_nesting(input);

    let cid_map = opts.cid_map.iter().map(|(cid, token)| (normalize_cid(cid), token.clone())).collect();
    let policy = Policy::new(Mode::Mail, opts.allow_remote_images, cid_map);

    let tag_attributes: HashMap<&str, HashSet<&str>> =
        MAIL_TAG_ATTRIBUTES.iter().map(|(tag, attrs)| (*tag, attrs.iter().copied().collect())).collect();

    let mut builder = Builder::empty();
    builder
        .tags(MAIL_TAGS.iter().copied().collect())
        .clean_content_tags(MAIL_CLEAN_CONTENT_TAGS.iter().copied().collect())
        .generic_attributes(MAIL_GENERIC_ATTRIBUTES.iter().copied().collect())
        .tag_attributes(tag_attributes)
        .url_schemes(MAIL_URL_SCHEMES.iter().copied().collect())
        .url_relative(UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer"))
        .strip_comments(true)
        .filter_style_properties(MAIL_STYLE_PROPERTIES.iter().copied().collect());
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
