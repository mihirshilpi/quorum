//! Pure chat UI logic (no DOM), tested natively: time parsing and labels, message grouping,
//! composer serialisation, mention detection and the emoji sets.

/// Messages from the same sender closer than this join one group (no repeated header).
pub const GROUP_GAP_MS: i64 = 5 * 60 * 1000;

const DAY_MS: i64 = 86_400_000;

/// Parses an RFC 3339 instant (`2026-10-16T10:15:00.123Z`, `...+02:00`) to Unix milliseconds.
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') || b.get(13) != Some(&b':') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse::<i64>().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = s.get(19..)?;
    let mut ms = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        let first3: String = digits.chars().chain(std::iter::repeat('0')).take(3).collect();
        ms = first3.parse().ok()?;
        rest = frac.get(digits.len()..)?;
    }
    let offset_min = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let hh: i64 = o.get(1..3)?.parse().ok()?;
            let mm: i64 = o.get(4..6)?.parse().ok()?;
            let v = hh * 60 + mm;
            if o.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    secs.checked_mul(1000)?.checked_add(ms)
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date `(year, month, day)` of a day number.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Local day number of an instant, with the local UTC offset in minutes (east positive).
pub fn local_day(ms: i64, offset_min: i64) -> i64 {
    (ms + offset_min * 60_000).div_euclid(DAY_MS)
}

const WEEKDAYS: [&str; 7] = ["Thursday", "Friday", "Saturday", "Sunday", "Monday", "Tuesday", "Wednesday"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn weekday(day: i64) -> &'static str {
    WEEKDAYS.get(usize::try_from(day.rem_euclid(7)).unwrap_or(0)).copied().unwrap_or("")
}

fn month(m: i64) -> &'static str {
    MONTHS.get(usize::try_from(m - 1).unwrap_or(0)).copied().unwrap_or("")
}

/// `14:05` in local time.
pub fn clock(ms: i64, offset_min: i64) -> String {
    let in_day = (ms + offset_min * 60_000).rem_euclid(DAY_MS) / 60_000;
    format!("{:02}:{:02}", in_day / 60, in_day % 60)
}

/// Day separator text: `Today`, `Yesterday`, a weekday within the week, else `Mon 3 Oct` (with
/// the year when it differs).
pub fn day_label(ms: i64, now_ms: i64, offset_min: i64) -> String {
    let (d, today) = (local_day(ms, offset_min), local_day(now_ms, offset_min));
    match today - d {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => weekday(d).into(),
        _ => {
            let (y, m, day) = civil_from_days(d);
            let (ty, _, _) = civil_from_days(today);
            let wd: String = weekday(d).chars().take(3).collect();
            if y == ty { format!("{wd} {day} {}", month(m)) } else { format!("{wd} {day} {} {y}", month(m)) }
        }
    }
}

/// Compact time for the chat list: clock today, `Yesterday`, short weekday this week, else `3 Oct`.
pub fn list_label(ms: i64, now_ms: i64, offset_min: i64) -> String {
    let (d, today) = (local_day(ms, offset_min), local_day(now_ms, offset_min));
    match today - d {
        i64::MIN..=0 => clock(ms, offset_min),
        1 => "Yesterday".into(),
        2..=6 => weekday(d).chars().take(3).collect(),
        _ => {
            let (_, m, day) = civil_from_days(d);
            format!("{day} {}", month(m))
        }
    }
}

/// What grouping needs to know about a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsgMeta {
    /// Sender id (or name), `None` for system messages.
    pub from: Option<String>,
    /// Unix milliseconds.
    pub time: Option<i64>,
    /// A system/event message (always its own line).
    pub system: bool,
}

/// One entry of the rendered conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A day separator before the next message.
    Day {
        /// Local day number (stable key).
        day: i64,
        /// Label text.
        label: String,
    },
    /// Message `index` of the input.
    Message {
        /// Index into the input slice.
        index: usize,
        /// First of its group: show avatar, name and time.
        header: bool,
        /// Last of its group (rounded bottom corner, spacing).
        tail: bool,
    },
}

/// Groups messages (oldest first) by sender and time, with day separators.
pub fn group(msgs: &[MsgMeta], now_ms: i64, offset_min: i64) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(msgs.len().saturating_add(4));
    let mut last_day: Option<i64> = None;
    let mut prev: Option<&MsgMeta> = None;
    let mut last_msg_entry: Option<usize> = None;
    for (index, m) in msgs.iter().enumerate() {
        let day = m.time.map(|t| local_day(t, offset_min));
        let new_day = day.is_some() && day != last_day;
        if let (true, Some(d), Some(t)) = (new_day, day, m.time) {
            out.push(Entry::Day { day: d, label: day_label(t, now_ms, offset_min) });
            last_day = Some(d);
        }
        let continues = !new_day
            && !m.system
            && prev.is_some_and(|p| {
                !p.system
                    && p.from.is_some()
                    && p.from == m.from
                    && match (p.time, m.time) {
                        (Some(a), Some(b)) => b.saturating_sub(a) < GROUP_GAP_MS,
                        _ => true,
                    }
            });
        if !continues && let Some(Entry::Message { tail, .. }) = last_msg_entry.and_then(|i| out.get_mut(i)) {
            *tail = true;
        }
        last_msg_entry = Some(out.len());
        out.push(Entry::Message { index, header: !continues, tail: false });
        prev = Some(m);
    }
    if let Some(Entry::Message { tail, .. }) = last_msg_entry.and_then(|i| out.get_mut(i)) {
        *tail = true;
    }
    out
}

/// A piece of composer content, produced by walking the editable DOM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Text.
    Text(String),
    /// Start of a formatting element (`b`, `i`, `u`, `s`, `code`, `p`).
    Open(&'static str),
    /// End of a formatting element.
    Close(&'static str),
    /// A line break.
    Break,
    /// A mention chip.
    Mention {
        /// User id.
        user: String,
        /// Display name.
        name: String,
    },
}

/// A mention found while serialising.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MentionOut {
    /// `<at id>` index.
    pub id: u32,
    /// Shown text.
    pub text: String,
    /// User id.
    pub user: String,
}

/// Escapes text for HTML.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Serialises composer pieces to the restricted HTML Teams accepts, numbering mentions.
/// Returns `(html, mentions, has_text)`.
pub fn pieces_to_html(pieces: &[Piece]) -> (String, Vec<MentionOut>, bool) {
    let mut html = String::new();
    let mut mentions = Vec::new();
    let mut has_text = false;
    let mut open: Vec<&'static str> = Vec::new();
    for p in pieces {
        match p {
            Piece::Text(t) => {
                if !t.trim().is_empty() {
                    has_text = true;
                }
                html.push_str(&escape(&t.replace('\u{a0}', " ")));
            }
            Piece::Open(tag) => {
                if open.len() < 32 {
                    open.push(tag);
                    html.push('<');
                    html.push_str(tag);
                    html.push('>');
                }
            }
            Piece::Close(tag) => {
                if let Some(pos) = open.iter().rposition(|t| t == tag) {
                    // Close anything opened inside it too, keeping the HTML well formed.
                    while open.len() > pos {
                        if let Some(t) = open.pop() {
                            html.push_str("</");
                            html.push_str(t);
                            html.push('>');
                        }
                    }
                }
            }
            Piece::Break => html.push_str("<br>"),
            Piece::Mention { user, name } => {
                let id = u32::try_from(mentions.len()).unwrap_or(u32::MAX);
                html.push_str(&format!("<at id=\"{id}\">{}</at>", escape(name)));
                mentions.push(MentionOut { id, text: name.clone(), user: user.clone() });
                has_text = true;
            }
        }
    }
    while let Some(t) = open.pop() {
        html.push_str("</");
        html.push_str(t);
        html.push('>');
    }
    // Trailing breaks add nothing.
    while let Some(stripped) = html.strip_suffix("<br>") {
        html = stripped.to_owned();
    }
    (html, mentions, has_text)
}

/// When the text before the caret ends in an `@query` (at the start or after whitespace),
/// the query (possibly empty).
pub fn mention_query(before_caret: &str) -> Option<String> {
    let at = before_caret.rfind('@')?;
    let prefix_ok = before_caret.get(..at).is_none_or(|p| p.is_empty() || p.ends_with(char::is_whitespace));
    let q = before_caret.get(at + 1..)?;
    let ok = prefix_ok
        && q.chars().count() <= 30
        && !q.contains('\n')
        && q.split_whitespace().count() <= 2
        && !q.ends_with("  ");
    ok.then(|| q.to_owned())
}

/// Reactions offered on hover.
pub const QUICK_REACTIONS: &[&str] = &["👍", "❤️", "😆", "😮", "😢", "🙏"];

/// The emoji picker (standard Unicode emoji, grouped).
pub const EMOJI: &[(&str, &[&str])] = &[
    (
        "Smileys",
        &[
            "😀", "😃", "😄", "😁", "😆", "😅", "😂", "🙂", "😉", "😊", "😇", "😍", "🤩", "😘", "😋", "😜", "🤔", "🤨",
            "😐", "😑", "😶", "🙄", "😏", "😬", "😌", "😴", "😷", "🤒", "🤯", "🥳", "😎", "🤓", "😕", "😟", "😮", "😲",
            "😳", "🥺", "😢", "😭", "😱", "😤", "😡", "🤗", "🤭", "🫡",
        ],
    ),
    (
        "Gestures",
        &[
            "👍", "👎", "👌", "✌️", "🤞", "🤝", "👏", "🙌", "👐", "🙏", "💪", "👋", "🤙", "👉", "👈", "👆", "👇", "✋",
            "🫶", "✍️",
        ],
    ),
    ("Hearts", &["❤️", "🧡", "💛", "💚", "💙", "💜", "🖤", "🤍", "💔", "💯", "✨", "⭐", "🔥", "🎉", "🎊", "🎈"]),
    (
        "Objects",
        &[
            "☕", "🍕", "🍰", "🍻", "📅", "📌", "📎", "📝", "💡", "🔒", "🔑", "💻", "📱", "📷", "🎧", "🚀", "✅", "❌",
            "⚠️", "❓", "❗", "➕",
        ],
    ),
];

/// The initials-avatar names for a chat: up to two other members (group avatars).
pub fn group_avatar_names(members: &[(String, bool)]) -> Vec<String> {
    members.iter().filter(|(_, me)| !me).map(|(n, _)| n.clone()).take(2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_graph_times() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_time("2026-10-16T10:15:00.1234567Z"), parse_time("2026-10-16T10:15:00.123Z"));
        assert_eq!(parse_time("1970-01-01T02:00:00+02:00"), Some(0));
        assert_eq!(parse_time("1970-01-01T00:00:00-01:30"), Some(5_400_000));
        for bad in [
            "",
            "garbage",
            "2026-13-01T00:00:00Z",
            "2026-10-16T10:15:00",
            "2026-10-16T10:15:00.Z",
            "2026-10-16T25:00:00Z",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn civil_round_trips() {
        for d in [-1_000_000, -1, 0, 1, 19_000, 20_742, 2_000_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn labels() {
        let now = parse_time("2026-10-16T12:00:00Z").unwrap_or(0);
        let t = |s| parse_time(s).unwrap_or(0);
        assert_eq!(day_label(t("2026-10-16T08:00:00Z"), now, 0), "Today");
        assert_eq!(day_label(t("2026-10-15T08:00:00Z"), now, 0), "Yesterday");
        assert_eq!(day_label(t("2026-10-13T08:00:00Z"), now, 0), "Tuesday");
        assert_eq!(day_label(t("2026-10-01T08:00:00Z"), now, 0), "Thu 1 Oct");
        assert_eq!(day_label(t("2025-10-01T08:00:00Z"), now, 0), "Wed 1 Oct 2025");
        assert_eq!(clock(t("2026-10-16T08:05:00Z"), 120), "10:05");
        assert_eq!(list_label(t("2026-10-16T08:05:00Z"), now, 0), "08:05");
        assert_eq!(list_label(t("2026-10-01T08:05:00Z"), now, 0), "1 Oct");
        // Offset moves an instant across midnight.
        assert_eq!(day_label(t("2026-10-15T23:30:00Z"), now, 60), "Today");
    }

    fn m(from: &str, t: i64) -> MsgMeta {
        MsgMeta { from: Some(from.into()), time: Some(t), system: false }
    }

    #[test]
    fn groups_by_sender_gap_and_day() {
        let base = parse_time("2026-10-16T09:00:00Z").unwrap_or(0);
        let now = base + 3_600_000;
        let msgs = vec![
            m("a", base - DAY_MS),
            m("a", base),
            m("a", base + 60_000),
            m("b", base + 120_000),
            m("b", base + 120_000 + GROUP_GAP_MS),
            MsgMeta { from: None, time: Some(base + 30 * 60_000), system: true },
            m("b", base + 31 * 60_000),
        ];
        let g = group(&msgs, now, 0);
        let headers: Vec<(usize, bool, bool)> = g
            .iter()
            .filter_map(|e| match e {
                Entry::Message { index, header, tail } => Some((*index, *header, *tail)),
                Entry::Day { .. } => None,
            })
            .collect();
        assert_eq!(
            headers,
            vec![
                (0, true, true),
                (1, true, false),
                (2, false, true),
                (3, true, true),
                (4, true, true),
                (5, true, true),
                (6, true, true)
            ]
        );
        let days: Vec<&str> = g
            .iter()
            .filter_map(|e| match e {
                Entry::Day { label, .. } => Some(label.as_str()),
                Entry::Message { .. } => None,
            })
            .collect();
        assert_eq!(days, vec!["Yesterday", "Today"]);
        assert!(group(&[], now, 0).is_empty());
        let untimed = group(&[MsgMeta { from: Some("a".into()), time: None, system: false }], now, 0);
        assert_eq!(untimed, vec![Entry::Message { index: 0, header: true, tail: true }]);
    }

    #[test]
    fn serialises_composer_pieces() {
        let pieces = vec![
            Piece::Text("Hi ".into()),
            Piece::Mention { user: "u1".into(), name: "Avery <A>".into() },
            Piece::Text(" ".into()),
            Piece::Open("b"),
            Piece::Text("bold & ".into()),
            Piece::Open("i"),
            Piece::Text("both".into()),
            Piece::Close("b"),
            Piece::Break,
            Piece::Break,
        ];
        let (html, mentions, has) = pieces_to_html(&pieces);
        assert_eq!(html, "Hi <at id=\"0\">Avery &lt;A&gt;</at> <b>bold &amp; <i>both</i></b>");
        assert_eq!(mentions, vec![MentionOut { id: 0, text: "Avery <A>".into(), user: "u1".into() }]);
        assert!(has);
        let (html, _, has) = pieces_to_html(&[Piece::Text("  ".into()), Piece::Break]);
        assert!(!has);
        assert_eq!(html, "  ");
        // Unbalanced input stays well formed.
        let (html, _, _) = pieces_to_html(&[Piece::Open("u"), Piece::Close("b"), Piece::Text("x".into())]);
        assert_eq!(html, "<u>x</u>");
    }

    #[test]
    fn detects_mention_queries() {
        assert_eq!(mention_query("hello @av"), Some("av".into()));
        assert_eq!(mention_query("@"), Some(String::new()));
        assert_eq!(mention_query("mail me@example.com"), None);
        assert_eq!(mention_query("no at"), None);
        assert_eq!(mention_query("@a b c"), None);
    }

    #[test]
    fn group_avatars_skip_me() {
        let names =
            group_avatar_names(&[("Me".into(), true), ("A".into(), false), ("B".into(), false), ("C".into(), false)]);
        assert_eq!(names, vec!["A".to_owned(), "B".to_owned()]);
    }
}
