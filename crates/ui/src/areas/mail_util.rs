//! Pure helpers for the Mail area (dates, sizes, base64, the mail frame document, recipient
//! parsing), unit-tested natively.

/// Minutes since the Unix epoch of an RFC 3339 UTC timestamp (`2026-10-17T09:30:00Z`, fractional
/// seconds and `+00:00` accepted). `None` for anything else.
pub fn parse_minutes(iso: &str) -> Option<i64> {
    let b = iso.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let s = iso.get(from..from.checked_add(len)?)?;
        if !s.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') || !matches!(b.get(10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let (y, m, d, h, min) = (num(0, 4)?, num(5, 2)?, num(8, 2)?, num(11, 2)?, num(14, 2)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 {
        return None;
    }
    let tail = iso.get(16..)?;
    let offset = match tail.rfind(['+', '-']) {
        Some(i) if tail.get(i..).is_some_and(|t| t.len() == 6 && t.as_bytes().get(3) == Some(&b':')) => {
            let t = tail.get(i..)?;
            let sign = if t.starts_with('-') { -1 } else { 1 };
            let oh: i64 = t.get(1..3)?.parse().ok()?;
            let om: i64 = t.get(4..6)?.parse().ok()?;
            sign * (oh * 60 + om)
        }
        _ => 0,
    };
    Some(days_from_civil(y, m, d) * 1440 + h * 60 + min - offset)
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date of days since the epoch.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn weekday(days: i64) -> &'static str {
    WEEKDAYS.get(usize::try_from(days.rem_euclid(7)).unwrap_or(0)).copied().unwrap_or("")
}

fn month(m: i64) -> &'static str {
    MONTHS.get(usize::try_from(m - 1).unwrap_or(0)).copied().unwrap_or("")
}

/// The list's short time: `09:30` today, `Tue` this week, `12 Oct` this year, else `12/10/2025`.
/// `now` and the result are in local time, `offset` minutes east of UTC.
pub fn short_time(iso: &str, now_min: i64, offset: i64) -> String {
    let Some(t) = parse_minutes(iso) else { return String::new() };
    let local = t + offset;
    let now = now_min + offset;
    let (day, today) = (local.div_euclid(1440), now.div_euclid(1440));
    let (y, m, d) = civil_from_days(day);
    let (ny, _, _) = civil_from_days(today);
    let rem = local.rem_euclid(1440);
    if day == today {
        format!("{:02}:{:02}", rem / 60, rem % 60)
    } else if today - day == 1 {
        "Yesterday".to_owned()
    } else if (0..7).contains(&(today - day)) {
        weekday(day).to_owned()
    } else if y == ny {
        format!("{d} {}", month(m))
    } else {
        format!("{d:02}/{m:02}/{y}")
    }
}

/// The reading pane's full time: `Fri 17 Oct 2026 09:30`.
pub fn long_time(iso: &str, offset: i64) -> String {
    let Some(t) = parse_minutes(iso) else { return String::new() };
    let local = t + offset;
    let day = local.div_euclid(1440);
    let (y, m, d) = civil_from_days(day);
    let rem = local.rem_euclid(1440);
    format!("{} {d} {} {y} {:02}:{:02}", weekday(day), month(m), rem / 60, rem % 60)
}

/// `482 KB`, `3.4 MB`, `12 bytes`.
pub fn size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes < KB {
        format!("{bytes} bytes")
    } else if bytes < MB {
        format!("{} KB", bytes.div_ceil(KB))
    } else {
        let tenths = bytes.saturating_mul(10) / MB;
        format!("{}.{} MB", tenths / 10, tenths % 10)
    }
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let sym = |i: u32| char::from(T.get(usize::try_from(i & 63).unwrap_or(0)).copied().unwrap_or(b'A'));
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(sym(n >> 18));
        out.push(sym(n >> 12));
        out.push(if chunk.len() > 1 { sym(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { sym(n) } else { '=' });
    }
    out
}

/// Whether `s` looks like an email address worth turning into a recipient chip.
pub fn looks_like_address(s: &str) -> bool {
    let s = s.trim();
    match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && s.len() <= 320
                && !s.chars().any(|c| c.is_whitespace() || c.is_control() || "<>,;\"".contains(c))
        }
        None => false,
    }
}

/// The document shown in the sandboxed mail frame in browser preview (`srcdoc`). The HTML is
/// already sanitised by the core; the CSP is a second fence (no scripts, no network except the
/// app's own schemes).
pub fn frame_document(body_html: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data: quorum-cid: quorum-remote:; style-src 'unsafe-inline'; font-src data:\">\
<style>html{{color-scheme:light}}body{{margin:0;padding:20px 24px;font:14px/1.5 -apple-system,BlinkMacSystemFont,'Helvetica Neue',system-ui,sans-serif;color:#1d1d1f;background:#fff;overflow-wrap:anywhere}}\
img{{max-width:100%;height:auto}}table{{max-width:100%}}a{{color:#0a64dc}}blockquote{{margin:0 0 0 8px;padding-left:12px;border-left:3px solid #d0d4dc;color:#5f5f66}}pre{{white-space:pre-wrap}}</style>\
</head><body>{body_html}</body></html>"
    )
}

/// The document of one message card in the conversation view (`srcdoc`, sanitised by the core).
///
/// `adapt_dark` colour-inverts the page for dark mode (images are inverted back), so plain mail
/// reads light-on-dark while keeping its colours' meaning; mail that paints its own backgrounds
/// is shown on a white page instead (`paper` adds the page's margins). The frame is sized to
/// its content by the parent, so the document never scrolls itself.
pub fn thread_frame_document(body_html: &str, adapt_dark: bool, paper: bool) -> String {
    let class = match (adapt_dark, paper) {
        (true, _) => " class=\"adapt\"",
        (false, true) => " class=\"paper\"",
        (false, false) => "",
    };
    format!(
        "<!doctype html><html{class}><head><meta charset=\"utf-8\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data: quorum-cid: quorum-remote: http://quorum-cid.localhost http://quorum-remote.localhost; style-src 'unsafe-inline'; font-src data:\">\
<meta name=\"referrer\" content=\"no-referrer\">\
<style>html{{color-scheme:light;overflow:hidden;background:#fff;-webkit-text-size-adjust:100%}}\
body{{margin:0;padding:2px 0 6px;font:14px/1.55 -apple-system,BlinkMacSystemFont,'Helvetica Neue',system-ui,sans-serif;color:#1f1f1f;overflow-wrap:anywhere}}\
html.paper body{{padding:14px 16px}}\
html.adapt{{filter:invert(.835) hue-rotate(180deg)}}\
html.adapt img,html.adapt video,html.adapt picture,html.adapt svg{{filter:invert(1) hue-rotate(180deg)}}\
img{{max-width:100%;height:auto}}table{{max-width:100%}}a{{color:#0a64dc}}\
body>:first-child,body>div:first-child>:first-child{{margin-top:0}}\
blockquote{{margin:0 0 0 4px;padding-left:12px;border-left:3px solid #d0d4dc;color:#5f5f66}}pre{{white-space:pre-wrap}}</style>\
</head><body>{body_html}</body></html>"
    )
}

/// A conversation's topic: the subject without any leading `RE:`/`FW:`/`Fwd:` prefixes (and
/// their common translations), as Outlook titles a conversation.
pub fn conversation_topic(subject: &str) -> String {
    const PREFIXES: &[&str] = &["re", "fw", "fwd", "aw", "wg", "sv", "vs", "tr", "rv", "antw"];
    let mut rest = subject.trim();
    for _ in 0..20 {
        let Some((head, tail)) = rest.split_once(':') else { break };
        let head = head.trim();
        let bare = head.trim_end_matches(|c: char| c.is_ascii_digit() || c == '[' || c == ']');
        if !PREFIXES.iter().any(|p| bare.eq_ignore_ascii_case(p)) {
            break;
        }
        rest = tail.trim_start();
    }
    if rest.is_empty() { "(no subject)".to_owned() } else { rest.to_owned() }
}

/// Plain text typed in the quick reply as mail HTML: one `<div>` per line, blank lines kept.
pub fn plain_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(16));
    for line in text.lines().take(10_000) {
        if line.trim().is_empty() {
            out.push_str("<div><br></div>");
            continue;
        }
        out.push_str("<div>");
        for c in line.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                c => out.push(c),
            }
        }
        out.push_str("</div>");
    }
    out
}

/// Which messages of a conversation open expanded (pure): the one selected in the list, every
/// unread one, and the newest when nothing else is. `ids` are oldest first.
pub fn initially_expanded(ids: &[(String, bool)], selected: &str) -> Vec<String> {
    let mut out: Vec<String> =
        ids.iter().filter(|(id, unread)| *unread || id == selected).map(|(id, _)| id.clone()).collect();
    if out.is_empty()
        && let Some((id, _)) = ids.last()
    {
        out.push(id.clone());
    }
    out
}

/// The `quorum-mail:` URL the desktop shell serves a message body from.
pub fn frame_url(message_id: &str, remote: bool) -> String {
    let mut out = String::from("quorum-mail://localhost/");
    for b in message_id.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if remote {
        out.push_str("?remote=1");
    }
    out
}

/// Initials-free display of a recipient: the name, else the address.
pub fn display_name(name: &str, address: &str) -> String {
    if name.trim().is_empty() { address.to_owned() } else { name.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-10-17T12:00:00Z";

    #[test]
    fn parses_rfc3339_and_rejects_garbage() {
        assert_eq!(parse_minutes("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_minutes("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_minutes("2026-10-17T12:00:00.1234567Z"), parse_minutes(NOW));
        for bad in
            ["", "yesterday", "2026-13-01T00:00:00Z", "2026-10-17", "２０２６-10-17T00:00:00Z", "2026-10-17T24:00:00Z"]
        {
            assert_eq!(parse_minutes(bad), None, "{bad}");
        }
    }

    #[test]
    fn list_times_read_like_outlook() {
        let now = parse_minutes(NOW).unwrap();
        assert_eq!(short_time("2026-10-17T09:05:00Z", now, 0), "09:05");
        assert_eq!(short_time("2026-10-17T09:05:00Z", now, 120), "11:05");
        assert_eq!(short_time("2026-10-16T09:05:00Z", now, 0), "Yesterday");
        assert_eq!(short_time("2026-10-14T09:05:00Z", now, 0), "Wed");
        assert_eq!(short_time("2026-03-02T09:05:00Z", now, 0), "2 Mar");
        assert_eq!(short_time("2025-03-02T09:05:00Z", now, 0), "02/03/2025");
        assert_eq!(short_time("nonsense", now, 0), "");
        assert_eq!(long_time(NOW, 0), "Sat 17 Oct 2026 12:00");
    }

    #[test]
    fn sizes_and_base64() {
        assert_eq!(size(12), "12 bytes");
        assert_eq!(size(2048), "2 KB");
        assert_eq!(size(3_670_016), "3.5 MB");
        assert!(!size(u64::MAX).is_empty());
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hi"), "aGk=");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn topic_drops_reply_and_forward_prefixes() {
        assert_eq!(conversation_topic("RE: FW: Re: Budget"), "Budget");
        assert_eq!(conversation_topic("Fwd: AW: Plan: v2"), "Plan: v2");
        assert_eq!(conversation_topic("Re[2]: Hello"), "Hello");
        assert_eq!(conversation_topic("Agenda: Monday"), "Agenda: Monday");
        assert_eq!(conversation_topic("RE:"), "(no subject)");
        assert!(!conversation_topic(&"re:".repeat(100)).is_empty());
    }

    #[test]
    fn quick_reply_text_becomes_escaped_lines() {
        assert_eq!(
            plain_to_html("Hi <b>\n\nThanks & bye"),
            "<div>Hi &lt;b&gt;</div><div><br></div><div>Thanks &amp; bye</div>"
        );
        assert_eq!(plain_to_html(""), "");
    }

    #[test]
    fn selected_unread_and_newest_open_expanded() {
        let ids = |v: &[(&str, bool)]| v.iter().map(|(i, u)| ((*i).to_owned(), *u)).collect::<Vec<_>>();
        assert_eq!(initially_expanded(&ids(&[("a", false), ("b", true), ("c", false)]), "a"), ["a", "b"]);
        assert_eq!(initially_expanded(&ids(&[("a", false), ("b", false)]), "zz"), ["b"]);
        assert_eq!(initially_expanded(&ids(&[("a", false), ("b", false)]), "b"), ["b"]);
        assert!(initially_expanded(&[], "a").is_empty());
    }

    #[test]
    fn addresses_and_urls() {
        assert!(looks_like_address("avery@example.com"));
        assert!(!looks_like_address("avery"));
        assert!(!looks_like_address("a b@example.com"));
        assert!(!looks_like_address("a@example."));
        assert_eq!(frame_url("AAMk/x=", false), "quorum-mail://localhost/AAMk%2Fx%3D");
        assert!(frame_url("a", true).ends_with("?remote=1"));
        let doc = thread_frame_document("<p>x</p>", true, false);
        assert!(doc.contains("<html class=\"adapt\">") && doc.contains("default-src 'none'"));
        assert!(thread_frame_document("", false, true).contains("class=\"paper\""));
        assert!(!thread_frame_document("", false, false).contains("class="));
        let doc = frame_document("<p>x</p>");
        assert!(doc.contains("default-src 'none'") && doc.contains("<p>x</p>"));
    }
}
