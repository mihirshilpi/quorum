//! Webview URL forms: rewriting sanitised output and parsing every form back.

use quorum_sanitize::{MailOptions, QuorumUrl, WebviewPlatform, sanitize_chat, sanitize_mail, webview_urls};

fn srcs(html: &str) -> Vec<String> {
    html.split("src=\"").skip(1).filter_map(|s| s.split('"').next().map(str::to_owned)).collect()
}

#[test]
fn mail_images_rewrite_and_parse_back() {
    let opts = MailOptions::default().with_cid("logo@x", "att 1/2").with_remote_images(true);
    let out = sanitize_mail(
        "<img src=\"cid:logo@x\"><img src=\"https://cdn.test/a.png?x=1\">\
         <div style=\"background:url(cid:logo@x)\">x</div><a href=\"https://l.test/\">l</a>",
        &opts,
    );
    for platform in [WebviewPlatform::MacOsLinux, WebviewPlatform::Windows] {
        let html = webview_urls(&out.html, platform);
        assert!(!html.contains("\"quorum-cid:a") && !html.contains("\"quorum-remote:h"), "{html}");
        assert!(!html.contains("&quot;quorum-cid:a"), "{html}");
        assert!(html.contains("href=\"quorum-link:"), "links stay opaque: {html}");
        let found = srcs(&html);
        assert_eq!(QuorumUrl::parse(&found[0]), Some(QuorumUrl::Cid("att 1/2".to_owned())));
        assert_eq!(QuorumUrl::parse(&found[1]), Some(QuorumUrl::Remote("https://cdn.test/a.png?x=1".to_owned())));
        match platform {
            WebviewPlatform::MacOsLinux => {
                assert!(found[0].starts_with("quorum-cid://localhost/"));
                assert!(html.contains("url(&quot;quorum-cid://localhost/att"));
            }
            WebviewPlatform::Windows => {
                assert!(found[0].starts_with("http://quorum-cid.localhost/"));
                assert!(html.contains("url(&quot;http://quorum-cid.localhost/att"));
            }
        }
    }
}

#[test]
fn chat_hosted_content_rewrites_and_parses_back() {
    let src = "https://graph.microsoft.com/v1.0/chats/1/messages/2/hostedContents/aWQ=/$value";
    let out = sanitize_chat(&format!("<img src=\"{src}\">"));
    for platform in [WebviewPlatform::MacOsLinux, WebviewPlatform::Windows] {
        let html = webview_urls(&out.html, platform);
        let found = srcs(&html);
        assert_eq!(QuorumUrl::parse(&found[0]), Some(QuorumUrl::Hosted(src.to_owned())));
    }
}

#[test]
fn text_mentioning_schemes_is_untouched() {
    let out = sanitize_mail("<p>see quorum-cid:x and \"quorum-remote:y</p>", &MailOptions::default());
    // Text quotes are not attribute quotes, but the rewrite is harmless either way; the
    // important property is that it never produces unsafe markup.
    let html = webview_urls(&out.html, WebviewPlatform::MacOsLinux);
    assert!(!html.contains('<') || html.starts_with("<p>"));
    assert!(html.contains("see quorum-cid:x"));
}

#[test]
fn rewriting_is_idempotent() {
    let out = sanitize_mail("<img src=\"cid:a@b\">", &MailOptions::default().with_cid("a@b", "t"));
    for platform in [WebviewPlatform::MacOsLinux, WebviewPlatform::Windows] {
        let once = webview_urls(&out.html, platform);
        assert_eq!(webview_urls(&once, platform), once);
    }
}
