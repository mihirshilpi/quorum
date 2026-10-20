//! Mail policy: links, remote images, cid mapping, caps.

pub mod common;

use common::{MAIL_TAGS, check_safe};
use quorum_sanitize::{MAX_DEPTH, MAX_INPUT_BYTES, MailOptions, QuorumUrl, REMOTE_IMAGE_PLACEHOLDER, sanitize_mail};

#[test]
fn links_are_rewritten_with_rel() {
    let out =
        sanitize_mail("<a href=\"https://example.com/path?q=a b&x=1#f\" title=\"t\">go</a>", &MailOptions::default());
    let tags = common::tags(&out.html);
    let a = &tags[0];
    let href = &a.attrs.iter().find(|(k, _)| k == "href").unwrap().1;
    assert!(href.starts_with("quorum-link:"));
    assert_eq!(QuorumUrl::parse(href), Some(QuorumUrl::Link("https://example.com/path?q=a%20b&x=1#f".to_owned())));
    assert!(a.attrs.iter().any(|(k, v)| k == "rel" && v == "noopener noreferrer"));
    assert_eq!(out.blocked_count, 0);
}

#[test]
fn mailto_and_tel_links_are_kept() {
    let out = sanitize_mail(
        "<a href=\"mailto:someone@example.com\">m</a><a href=\"tel:+15550100\">t</a>",
        &MailOptions::default(),
    );
    assert!(out.html.contains("quorum-link:mailto%3Asomeone%40example.com"));
    assert!(out.html.contains("quorum-link:tel%3A%2B15550100"));
}

#[test]
fn relative_and_unknown_scheme_links_are_dropped() {
    let out = sanitize_mail(
        "<a href=\"/relative\">r</a><a href=\"ftp://x.test/\">f</a><a href=\"#top\">t</a>",
        &MailOptions::default(),
    );
    assert!(!out.html.contains("href"), "{}", out.html);
}

#[test]
fn remote_images_are_blocked_by_default() {
    let out = sanitize_mail(
        "<p><img src=\"https://tracker.test/pixel.gif\" width=\"1\" height=\"1\" alt=\"\"></p>",
        &MailOptions::default(),
    );
    assert!(out.had_remote_images);
    assert_eq!(out.blocked_count, 1);
    assert!(!out.html.contains("tracker.test"));
    assert!(out.html.contains(REMOTE_IMAGE_PLACEHOLDER));
    check_safe(&out.html, MAIL_TAGS).unwrap();
}

#[test]
fn remote_images_go_through_the_proxy_when_allowed() {
    let opts = MailOptions::default().with_remote_images(true);
    let out = sanitize_mail("<img src=\"https://cdn.test/logo.png\">", &opts);
    assert!(out.had_remote_images);
    assert_eq!(out.blocked_count, 0);
    assert!(out.html.contains("src=\"quorum-remote:https%3A%2F%2Fcdn.test%2Flogo.png\""));
}

#[test]
fn remote_css_backgrounds_are_blocked_by_default() {
    let out = sanitize_mail(
        "<table><tr><td style=\"background-image:url('https://tracker.test/bg.png');color:blue\">x</td></tr></table>",
        &MailOptions::default(),
    );
    assert!(out.had_remote_images);
    assert!(!out.html.contains("tracker.test"));
    assert!(out.html.contains("color:blue"));
}

#[test]
fn no_remote_images_means_no_flag() {
    let out = sanitize_mail("<p>Hello</p>", &MailOptions::default());
    assert!(!out.had_remote_images);
    assert_eq!(out.blocked_count, 0);
    assert!(!out.truncated);
}

#[test]
fn cid_images_are_mapped() {
    let opts = MailOptions::default().with_cid("<Image001.PNG@01D9ABCD>", "AAMk attachment/1");
    let out = sanitize_mail("<img src=\"cid:image001.png@01D9ABCD\" alt=\"logo\"><img src=\"cid:other@x\">", &opts);
    assert!(out.html.contains("src=\"quorum-cid:AAMk%20attachment%2F1\""), "{}", out.html);
    assert_eq!(out.blocked_count, 1, "the unmapped cid is blocked");
    assert!(!out.had_remote_images);
    assert_eq!(
        QuorumUrl::parse("quorum-cid:AAMk%20attachment%2F1"),
        Some(QuorumUrl::Cid("AAMk attachment/1".to_owned()))
    );
}

#[test]
fn cid_backgrounds_are_mapped() {
    let opts = MailOptions::default().with_cid("bg@x", "att9");
    let out = sanitize_mail("<div style=\"background:url(cid:bg@x)\">x</div>", &opts);
    assert!(out.html.contains("quorum-cid:att9"), "{}", out.html);
}

#[test]
fn raster_data_images_are_kept() {
    let png = "data:image/png;base64,iVBORw0KGgo=";
    let out = sanitize_mail(&format!("<img src=\"{png}\">"), &MailOptions::default());
    assert!(out.html.contains(png));
}

#[test]
fn real_world_table_layout_survives() {
    let html = r##"<table width="600" cellpadding="0" cellspacing="0" border="0" bgcolor="#ffffff" align="center">
        <tr><td valign="top" style="padding:10px 20px;font-family:Arial,sans-serif;font-size:14px;color:#333333">
        <font face="Calibri" color="#1f497d">Hello</font></td></tr></table>"##;
    let out = sanitize_mail(html, &MailOptions::default());
    for needle in [
        "width=\"600\"",
        "cellpadding=\"0\"",
        "bgcolor=\"#ffffff\"",
        "valign=\"top\"",
        "font-family:Arial",
        "face=\"Calibri\"",
        "padding:10px 20px",
    ] {
        assert!(out.html.contains(needle), "missing {needle} in {}", out.html);
    }
}

#[test]
fn full_documents_keep_only_body_content() {
    let html =
        "<!DOCTYPE html><html><head><title>T</title><style>p{color:red}</style></head><body><p>Body</p></body></html>";
    let out = sanitize_mail(html, &MailOptions::default());
    assert_eq!(out.html, "<p>Body</p>");
}

#[test]
fn huge_input_is_truncated_without_panicking() {
    let unit = "<p>é😀 text</p>";
    let html = unit.repeat(MAX_INPUT_BYTES / unit.len() + 10);
    let out = sanitize_mail(&html, &MailOptions::default());
    assert!(out.truncated);
    assert!(out.html.len() <= MAX_INPUT_BYTES + 1024);
    check_safe(&out.html, MAIL_TAGS).unwrap();
}

#[test]
fn deep_nesting_does_not_blow_the_stack() {
    for tag in ["div", "b", "table", "font", "blockquote", "span"] {
        let depth = 100_000;
        let html = format!("{}deep{}", format!("<{tag}>").repeat(depth), format!("</{tag}>").repeat(depth));
        let out = sanitize_mail(&html, &MailOptions::default());
        assert!(out.html.contains("deep"), "{tag}");
        let opens = common::tags(&out.html).iter().filter(|t| !t.closing).count();
        assert!(opens <= MAX_DEPTH + 8, "{tag}: {opens} open tags");
    }
}

#[test]
fn unclosed_tags_do_not_blow_the_stack() {
    let html = "<div><p><b><i><a href=https://x.test>".repeat(50_000);
    let out = sanitize_mail(&html, &MailOptions::default());
    check_safe(&out.html, MAIL_TAGS).unwrap();
}

#[test]
fn debug_does_not_leak_content() {
    let out = sanitize_mail("<p>Secret salary figures</p>", &MailOptions::default());
    let dbg = format!("{out:?}");
    assert!(!dbg.contains("Secret"), "{dbg}");
    let opts = MailOptions::default().with_cid("private-name.png@x", "token");
    let dbg = format!("{opts:?}");
    assert!(!dbg.contains("private-name") && !dbg.contains("token"), "{dbg}");
    let url = QuorumUrl::Link("mailto:someone@example.com".to_owned());
    assert!(!format!("{url:?}").contains("someone"));
}

#[test]
fn worst_case_inputs_finish() {
    // Each of these is quadratic (or worse) in a naive HTML tree builder.
    let n = MAX_INPUT_BYTES / 16;
    let mut formatting = String::new();
    for i in 0..n / 2 {
        formatting.push_str(&format!("<p><b id={i}>x</p>"));
    }
    for html in [
        "<div>".repeat(n),
        "<div/>".repeat(n),
        "<a href=https://x.test>".repeat(n),
        formatting,
        "<table><tr><td>".repeat(n),
        "<svg><g>".repeat(n),
    ] {
        let out = sanitize_mail(&html, &MailOptions::default());
        check_safe(&out.html, MAIL_TAGS).unwrap();
        assert!(out.html.len() <= html.len() * 2, "output amplified");
    }
}
