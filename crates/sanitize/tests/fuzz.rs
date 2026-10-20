//! Random hostile HTML never panics and always satisfies the safety invariants.

pub mod common;

use common::{CHAT_TAGS, MAIL_TAGS, Rng, check_safe};
use quorum_sanitize::{MailOptions, sanitize_chat, sanitize_mail, to_plain_text};

const PIECES: &[&str] = &[
    "<",
    ">",
    "</",
    "/>",
    "\"",
    "'",
    "=",
    " ",
    "\n",
    "\t",
    "\u{0}",
    "&",
    "&#",
    "&#x",
    ";",
    "&amp;",
    "&lt;",
    "<!--",
    "-->",
    "<![CDATA[",
    "]]>",
    "<!DOCTYPE html>",
    "script",
    "style",
    "img",
    "a",
    "svg",
    "math",
    "iframe",
    "table",
    "td",
    "tr",
    "div",
    "p",
    "at",
    "noscript",
    "template",
    "textarea",
    "xmp",
    "select",
    "option",
    "form",
    "base",
    "meta",
    "src",
    "href",
    "style",
    "onerror",
    "onload",
    "id",
    "class",
    "srcset",
    "xlink:href",
    "javascript:",
    "JaVa\tScRiPt:",
    "data:text/html,",
    "data:image/png;base64,AA==",
    "data:image/svg+xml,",
    "cid:",
    "cid:logo@x",
    "http://",
    "https://",
    "evil.test/x",
    "mailto:",
    "quorum-link:",
    "quorum-remote:",
    "url(",
    ")",
    "expression(",
    "\\",
    "/*",
    "*/",
    "background:",
    "color:red",
    "position:fixed",
    "alert(1)",
    "é",
    "😀",
    "\u{202e}",
    "\u{feff}",
    "%",
    "%0a",
    "{",
    "}",
    "@import",
];

fn random_html(rng: &mut Rng) -> String {
    let len = rng.below(120);
    let mut s = String::new();
    for _ in 0..len {
        s.push_str(rng.pick(PIECES));
    }
    s
}

#[test]
fn random_html_is_always_safe() {
    let mut rng = Rng::new(0x5eed_1234_abcd_ef01);
    let opts_block = MailOptions::default().with_cid("logo@x", "att1");
    let opts_allow = opts_block.clone().with_remote_images(true);
    for _ in 0..4000 {
        let html = random_html(&mut rng);
        for opts in [&opts_block, &opts_allow] {
            let out = sanitize_mail(&html, opts);
            if let Err(e) = check_safe(&out.html, MAIL_TAGS) {
                panic!("input {html:?}: {e}");
            }
        }
        let out = sanitize_chat(&html);
        if let Err(e) = check_safe(&out.html, CHAT_TAGS) {
            panic!("chat input {html:?}: {e}");
        }
        let _ = to_plain_text(&html);
    }
}

#[test]
fn sanitising_is_idempotent_for_safety() {
    let mut rng = Rng::new(42);
    for _ in 0..500 {
        let html = random_html(&mut rng);
        let once = sanitize_mail(&html, &MailOptions::default());
        let twice = sanitize_mail(&once.html, &MailOptions::default());
        check_safe(&twice.html, MAIL_TAGS).unwrap();
    }
}
