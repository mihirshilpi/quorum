//! Pure composer helpers (tested natively): base64, draft restore, people matching.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        let idx = |shift: u32| char::from(B64.get(((n >> shift) & 63) as usize).copied().unwrap_or(b'A'));
        out.push(idx(18));
        out.push(idx(12));
        out.push(if chunk.len() > 1 { idx(6) } else { '=' });
        out.push(if chunk.len() > 2 { idx(0) } else { '=' });
    }
    out
}

/// A person offered by the mention picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// User id.
    pub id: String,
    /// Display name.
    pub name: String,
}

/// Candidates whose name has a word starting with `query` (case-insensitive), at most `max`,
/// without duplicates; an empty query lists the first ones.
pub fn match_people(people: &[Candidate], query: &str, max: usize) -> Vec<Candidate> {
    let q = query.trim().to_lowercase();
    let mut seen = std::collections::HashSet::new();
    people
        .iter()
        .filter(|p| {
            q.is_empty() || {
                let n = p.name.to_lowercase();
                n.starts_with(&q) || n.split_whitespace().any(|w| w.starts_with(&q))
            }
        })
        .filter(|p| !p.id.is_empty() && seen.insert(p.id.clone()))
        .take(max)
        .cloned()
        .collect()
}

fn attr_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Turns `<at id="N">Name</at>` from a saved draft or a message being edited into the composer's
/// mention chips, using `mentions` `(id, user)` to recover the user ids. Unknown ids stay text.
pub fn at_to_chips(html: &str, mentions: &[(u32, String)]) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("<at id=\"") {
        let (before, at) = rest.split_at(pos);
        out.push_str(before);
        let after_open = at.get(8..).unwrap_or("");
        let Some(q) = after_open.find('"') else {
            out.push_str(at);
            return out;
        };
        let id: Option<u32> = after_open.get(..q).and_then(|s| s.parse().ok());
        let body = after_open.get(q..).and_then(|s| s.strip_prefix("\">")).unwrap_or("");
        let Some(end) = body.find("</at>") else {
            out.push_str(at);
            return out;
        };
        let name = body.get(..end).unwrap_or("");
        match id.and_then(|i| mentions.iter().find(|(m, _)| *m == i)) {
            Some((_, user)) => {
                let plain =
                    name.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"");
                out.push_str(&format!(
                    "<span class=\"mention\" contenteditable=\"false\" data-user=\"{}\" data-name=\"{}\">{name}</span>",
                    attr_escape(user),
                    attr_escape(&plain)
                ));
            }
            None => out.push_str(name),
        }
        rest = body.get(end + 5..).unwrap_or("");
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe]), "//4=");
    }

    #[test]
    fn matches_word_prefixes() {
        let p = |id: &str, n: &str| Candidate { id: id.into(), name: n.into() };
        let people = vec![p("1", "Morgan Sample"), p("2", "Sage Exemplar"), p("1", "Morgan Sample"), p("", "No Id")];
        assert_eq!(match_people(&people, "sa", 5).len(), 2);
        assert_eq!(match_people(&people, "ex", 5), vec![p("2", "Sage Exemplar")]);
        assert_eq!(match_people(&people, "", 5).len(), 2);
        assert!(match_people(&people, "zz", 5).is_empty());
    }

    #[test]
    fn restores_mention_chips() {
        let html = r#"Hi <at id="0">Avery &amp; Co</at> and <at id="7">Ghost</at><at id="x"#;
        let out = at_to_chips(html, &[(0, "u\"1".into())]);
        assert!(out.starts_with(r#"Hi <span class="mention" contenteditable="false" data-user="u&quot;1" data-name="Avery &amp; Co">Avery &amp; Co</span> and Ghost"#), "{out}");
        assert!(out.ends_with(r#"<at id="x"#));
    }
}
