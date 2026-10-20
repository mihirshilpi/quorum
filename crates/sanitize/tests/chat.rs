//! Chat policy: small allow-list, mentions, hosted content.

pub mod common;

use common::{CHAT_TAGS, check_safe};
use quorum_sanitize::{QuorumUrl, sanitize_chat};

#[test]
fn basic_formatting_survives() {
    let html = "<div><p><b>bold</b> <i>it</i> <u>u</u> <s>s</s></p><ul><li>one</li></ul><pre><code>x()</code></pre><blockquote>q</blockquote></div>";
    let out = sanitize_chat(html);
    assert_eq!(out.html, html);
}

#[test]
fn mentions_survive_with_numeric_ids() {
    let out = sanitize_chat("<p>Hi <at id=\"0\">Alex</at> and <at id=\"x1\">Bob</at></p>");
    assert_eq!(out.html, "<p>Hi <at id=\"0\">Alex</at> and <at>Bob</at></p>");
}

#[test]
fn styles_and_tables_are_not_allowed() {
    let out = sanitize_chat("<p style=\"color:red\">x</p><table><tr><td>y</td></tr></table><h1>z</h1>");
    assert_eq!(out.html, "<p>x</p>yz");
}

#[test]
fn hosted_content_images_are_rewritten() {
    let src = "https://graph.microsoft.com/v1.0/chats/19:abc@thread.v2/messages/1/hostedContents/aWQ=/$value";
    let out = sanitize_chat(&format!("<p><img src=\"{src}\" width=\"200\" alt=\"image\"></p>"));
    let tags = common::tags(&out.html);
    let img = tags.iter().find(|t| t.name == "img").unwrap();
    let got = &img.attrs.iter().find(|(k, _)| k == "src").unwrap().1;
    assert_eq!(QuorumUrl::parse(got), Some(QuorumUrl::Hosted(src.to_owned())));
    assert!(!out.had_remote_images);
    check_safe(&out.html, CHAT_TAGS).unwrap();
}

#[test]
fn other_images_lose_src() {
    let out = sanitize_chat(
        "<img src=\"https://statics.teams.cdn.office.net/emoji/smile.png\" alt=\"smile\"><img src=\"http://graph.microsoft.com/v1.0/x/hostedContents/y\">",
    );
    assert!(out.had_remote_images);
    assert_eq!(out.blocked_count, 2);
    assert!(!out.html.contains("src="));
    assert!(out.html.contains("alt=\"smile\""));
}

#[test]
fn chat_links_are_rewritten() {
    let out = sanitize_chat("<a href=\"https://contoso.test/doc\">doc</a>");
    assert_eq!(
        out.html,
        "<a href=\"quorum-link:https%3A%2F%2Fcontoso.test%2Fdoc\" rel=\"noopener noreferrer\">doc</a>"
    );
}
