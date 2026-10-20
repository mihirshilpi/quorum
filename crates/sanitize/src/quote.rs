//! Finding where the quoted reply history starts in a mail body.
//!
//! Replies carry the whole earlier conversation below the new text. The conversation view
//! already shows those messages as their own cards, so the core splits each body into the new
//! part and the quoted history ("Show quoted text"). Detection works on the raw body, before
//! sanitising, because the markers clients leave (`id="divRplyFwdMsg"`, `class="gmail_quote"`)
//! are attributes the sanitiser drops. The halves are then sanitised separately; the HTML
//! parser closes whatever the cut left open.
//!
//! Recognised shapes: Outlook (web, new and classic desktop), Gmail, Apple Mail, Thunderbird,
//! and plain-text "-----Original Message-----" / "On … wrote:" + `>` lines.

/// Markers that start the quoted history; the cut is at the start of the tag containing them.
const TAG_MARKERS: &[&str] = &[
    "id=\"appendonsend\"",
    "id=\"divrplyfwdmsg\"",
    "id=divrplyfwdmsg",
    "class=\"gmail_quote",
    "class=gmail_quote",
    "class=\"moz-cite-prefix\"",
    "<blockquote type=\"cite\"",
    "-----original message-----",
];

/// How far before a bold `From:` header a separator (`<hr>` or a top border) may start.
const HEADER_LOOKBACK: usize = 800;

/// Byte offset where the quoted history starts in an HTML body, if there is one and there is
/// new text before it. Never panics; the offset is always on a char boundary.
///
/// ```
/// let html = r#"<div>Sounds good.</div><div id="appendonsend"></div><hr><div id="divRplyFwdMsg"><b>From:</b> A</div><div>Earlier</div>"#;
/// let at = quorum_sanitize::quote_start(html).unwrap();
/// assert_eq!(&html[..at], "<div>Sounds good.</div>");
/// ```
#[must_use]
pub fn quote_start(html: &str) -> Option<usize> {
    let lower = html.to_ascii_lowercase();
    let mut best: Option<usize> = None;
    let mut consider = |at: usize| {
        if best.is_none_or(|b| at < b) {
            best = Some(at);
        }
    };
    for marker in TAG_MARKERS {
        if let Some(pos) = lower.find(marker) {
            consider(if marker.starts_with('<') { pos } else { tag_start(&lower, pos) });
        }
    }
    // Classic Outlook / Outlook for Mac: a separator, then a bold "From:" header block.
    for needle in ["<b>from:</b>", "<b>from: </b>", ">from:</span></b>", ">from:</b>"] {
        let mut from = 0usize;
        while let Some(rel) = lower.get(from..).and_then(|s| s.find(needle)) {
            let pos = from.saturating_add(rel);
            let window_start = pos.saturating_sub(HEADER_LOOKBACK);
            let window = lower.get(window_start..pos).unwrap_or("");
            let sep = window.rfind("<hr").or_else(|| window.rfind("border-top:solid").map(|p| tag_start(window, p)));
            if let Some(s) = sep {
                consider(window_start.saturating_add(s));
                break;
            }
            from = pos.saturating_add(needle.len());
        }
    }
    let at = best?;
    // Only a split if something new comes first (a bare forward shows everything).
    let head = html.get(..at)?;
    (has_text(head) && html.is_char_boundary(at)).then_some(at)
}

/// Byte offset where the quoted history starts in a plain-text body: an "-----Original
/// Message-----" line, or an "On … wrote:" line followed by `>` lines, or a run of `>` lines.
#[must_use]
pub fn quote_start_text(text: &str) -> Option<usize> {
    let mut offset = 0usize;
    let mut previous: Option<(usize, &str)> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        let found = if lower.starts_with("-----original message-----") {
            Some(offset)
        } else if trimmed.starts_with('>') {
            match previous {
                Some((at, prev)) if prev.to_ascii_lowercase().ends_with("wrote:") => Some(at),
                _ => Some(offset),
            }
        } else {
            None
        };
        if let Some(at) = found {
            return text.get(..at).is_some_and(|h| !h.trim().is_empty()).then_some(at);
        }
        if !trimmed.is_empty() {
            previous = Some((offset, trimmed));
        }
        offset = offset.saturating_add(line.len());
    }
    None
}

/// The start of the tag that contains byte `pos` (the last `<` before it), else `pos`.
fn tag_start(lower: &str, pos: usize) -> usize {
    lower.get(..pos).and_then(|s| s.rfind('<')).unwrap_or(pos)
}

/// Whether an HTML fragment has visible text (outside tags, ignoring whitespace and `&nbsp;`).
fn has_text(html: &str) -> bool {
    let mut in_tag = false;
    let mut rest = html;
    while let Some(c) = rest.chars().next() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            '&' if !in_tag => {
                if rest.get(..6).is_some_and(|e| e.eq_ignore_ascii_case("&nbsp;")) {
                    rest = rest.get(6..).unwrap_or("");
                    continue;
                }
                return true;
            }
            c if !in_tag && !c.is_whitespace() && c != '\u{a0}' && c != '\u{200b}' => return true,
            _ => {}
        }
        rest = rest.get(c.len_utf8()..).unwrap_or("");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(html: &str) -> Option<&str> {
        quote_start(html).and_then(|at| html.get(..at))
    }

    #[test]
    fn outlook_web_reply() {
        let html = r#"<div style="font-size:12pt">Works for me.</div><div>Thanks,<br>Avery</div><div id="appendonsend"></div><hr style="display:inline-block;width:98%" tabindex="-1"><div id="divRplyFwdMsg" dir="ltr"><font face="Calibri"><b>From:</b> Blake &lt;b@example.com&gt;<br><b>Sent:</b> Tuesday</font></div><div>Earlier text</div>"#;
        assert_eq!(head(html), Some(r#"<div style="font-size:12pt">Works for me.</div><div>Thanks,<br>Avery</div>"#));
    }

    #[test]
    fn outlook_without_appendonsend_cuts_at_the_rule() {
        let html = r#"<p>New</p><hr style="display:inline-block"><div id="divRplyFwdMsg"><b>From:</b> x</div>old"#;
        assert_eq!(head(html), Some("<p>New</p>"));
    }

    #[test]
    fn classic_outlook_border_header() {
        let html = r#"<p class="MsoNormal">See below.</p><div style="border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0in 0in 0in"><p class="MsoNormal"><b>From:</b> Casey<br><b>Sent:</b> Monday</p></div><p>old</p>"#;
        assert_eq!(head(html), Some(r#"<p class="MsoNormal">See below.</p>"#));
    }

    #[test]
    fn gmail_apple_and_thunderbird() {
        let gmail = r#"<div dir="ltr">Yes</div><br><div class="gmail_quote"><div class="gmail_attr">On Tue, X wrote:</div><blockquote class="gmail_quote">old</blockquote></div>"#;
        assert_eq!(head(gmail), Some(r#"<div dir="ltr">Yes</div><br>"#));
        let apple =
            r#"<div>Done.</div><div><br><blockquote type="cite"><div>On 14 Oct, X wrote:</div>old</blockquote></div>"#;
        assert_eq!(head(apple), Some("<div>Done.</div><div><br>"));
        let tb = r#"<p>Ok</p><div class="moz-cite-prefix">On X wrote:</div><blockquote type="cite">old</blockquote>"#;
        assert_eq!(head(tb), Some("<p>Ok</p>"));
    }

    #[test]
    fn earliest_marker_wins_and_case_is_ignored() {
        let html = r#"<p>a</p><DIV ID="divRplyFwdMsg">h</DIV><div class="gmail_quote">g</div>"#;
        assert_eq!(head(html), Some("<p>a</p>"));
    }

    #[test]
    fn no_split_without_new_text_or_markers() {
        assert_eq!(quote_start("<p>Just a message</p><blockquote>a real quote</blockquote>"), None);
        assert_eq!(quote_start(r#"<div>&nbsp;</div><div id="divRplyFwdMsg"><b>From:</b></div>fwd"#), None);
        assert_eq!(quote_start(""), None);
        assert_eq!(quote_start("<b>From:</b> no separator"), None);
    }

    #[test]
    fn plain_text_quotes() {
        let t = "Fine by me.\n\n-----Original Message-----\nFrom: X\n";
        assert_eq!(quote_start_text(t).and_then(|a| t.get(..a)), Some("Fine by me.\n\n"));
        let t = "Sure\n\nOn Tue, X wrote:\n> old\n> older\n";
        assert_eq!(quote_start_text(t).and_then(|a| t.get(..a)), Some("Sure\n\n"));
        assert_eq!(quote_start_text("> only quote\n"), None);
        assert_eq!(quote_start_text("no quote at all"), None);
    }

    #[test]
    fn hostile_input_never_panics() {
        let nasty = [
            "<".repeat(10_000),
            "é<b>from:</b>".repeat(500),
            format!("x{}", "<hr".repeat(3000)),
            "<div id=\"appendonsend\"".to_owned(),
            "ß\u{a0}<div class=gmail_quote>".to_owned(),
            "&".to_owned(),
            "\u{200b}<blockquote type=\"cite\">".to_owned(),
        ];
        for n in &nasty {
            if let Some(at) = quote_start(n) {
                assert!(n.is_char_boundary(at));
            }
            let _ = quote_start_text(n);
        }
    }
}
