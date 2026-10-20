//! Pre-filtering of inline `style` attributes.
//!
//! ammonia afterwards keeps only allow-listed properties and normalises the CSS, but it passes
//! through any CSS function and any `url()` whose scheme is allowed. This pass runs first and is
//! deliberately conservative:
//!
//! * a value containing a backslash (CSS escapes can disguise `url` or `expression`) or a
//!   comment is dropped entirely;
//! * declarations are split on `;` outside quotes and parentheses;
//! * a declaration that calls any function other than `rgb`, `rgba`, `hsl`, `hsla`, `calc` or
//!   `url` is dropped (this removes `expression()`, `image-set()`, `var()` and friends);
//! * every `url()` goes through the image policy: mapped `cid:` → `quorum-cid:`, allowed
//!   remote → `quorum-remote:`, raster `data:` kept; anything else drops the declaration.

use crate::policy::{ImageDecision, Policy};

/// CSS functions allowed in inline styles (besides `url`, handled separately).
const ALLOWED_FUNCTIONS: &[&str] = &["rgb", "rgba", "hsl", "hsla", "calc"];

/// Maximum length of a style attribute we bother to process.
const MAX_STYLE_LEN: usize = 16 * 1024;

/// Filters one `style` attribute value; `None` drops the attribute.
pub(crate) fn filter_urls(value: &str, policy: &Policy) -> Option<String> {
    if value.len() > MAX_STYLE_LEN || value.contains('\\') || value.contains("/*") {
        policy.block();
        return None;
    }
    let mut out = String::with_capacity(value.len());
    for decl in split_declarations(value) {
        let decl = decl.trim();
        if decl.is_empty() {
            continue;
        }
        match filter_declaration(decl, policy) {
            Some(kept) => {
                if !out.is_empty() {
                    out.push_str("; ");
                }
                out.push_str(&kept);
            }
            None => policy.block(),
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Splits on `;` outside quotes and parentheses.
fn split_declarations(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth: usize = 0;
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (i, c) in value.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(') => depth = depth.saturating_add(1),
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ';') if depth == 0 => {
                parts.push(value.get(start..i).unwrap_or(""));
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(value.get(start..).unwrap_or(""));
    parts
}

/// Filters one declaration; `None` drops it.
fn filter_declaration(decl: &str, policy: &Policy) -> Option<String> {
    let mut out = String::with_capacity(decl.len());
    let mut rest = decl;
    loop {
        let Some((before, name, after_paren)) = next_function(rest) else {
            out.push_str(rest);
            return Some(out);
        };
        out.push_str(before);
        if name.eq_ignore_ascii_case("url") {
            let close = find_close_paren(after_paren)?;
            let arg = after_paren.get(..close)?.trim();
            let arg = arg
                .strip_prefix('"')
                .and_then(|a| a.strip_suffix('"'))
                .or_else(|| arg.strip_prefix('\'').and_then(|a| a.strip_suffix('\'')))
                .unwrap_or(arg);
            if arg.contains(['"', '\'', '(', ')']) {
                return None;
            }
            let url = match policy.image(arg) {
                ImageDecision::Rewrite(url) => url,
                ImageDecision::Keep => arg.to_owned(),
                ImageDecision::Replace(_) | ImageDecision::Drop => return None,
            };
            out.push_str("url(\"");
            out.push_str(&url);
            out.push_str("\")");
            rest = after_paren.get(close + 1..)?;
        } else if ALLOWED_FUNCTIONS.iter().any(|f| name.eq_ignore_ascii_case(f)) {
            out.push_str(name);
            out.push('(');
            rest = after_paren;
        } else {
            return None;
        }
    }
}

/// Finds the next `ident(` outside quotes: returns (text before ident, ident, text after `(`).
fn next_function(s: &str) -> Option<(&str, &str, &str)> {
    let mut quote: Option<char> = None;
    let mut ident_start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                ident_start = None;
            }
            '(' => {
                let start = ident_start.unwrap_or(i);
                return Some((s.get(..start)?, s.get(start..i)?, s.get(i + 1..)?));
            }
            c if c.is_alphanumeric() || c == '-' || c == '_' => {
                if ident_start.is_none() {
                    ident_start = Some(i);
                }
            }
            _ => ident_start = None,
        }
    }
    None
}

/// Finds the `)` closing a `url(` argument (quotes respected).
fn find_close_paren(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, ')') => return Some(i),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::policy::Mode;

    fn policy(allow_remote: bool) -> std::sync::Arc<Policy> {
        let mut map = HashMap::new();
        map.insert("logo@x".to_owned(), "att1".to_owned());
        Policy::new(Mode::Mail, allow_remote, map)
    }

    #[test]
    fn plain_declarations_pass() {
        let p = policy(false);
        assert_eq!(
            filter_urls("color: red; background-color: rgb(1, 2, 3)", &p).as_deref(),
            Some("color: red; background-color: rgb(1, 2, 3)")
        );
    }

    #[test]
    fn expression_and_unknown_functions_are_dropped() {
        let p = policy(false);
        assert_eq!(filter_urls("width: expression(alert(1)); color: red", &p).as_deref(), Some("color: red"));
        assert_eq!(filter_urls("background: image-set('x.png' 1x)", &p), None);
        assert!(p.blocked_count() >= 2);
    }

    #[test]
    fn escapes_and_comments_drop_everything() {
        let p = policy(false);
        assert_eq!(filter_urls("background: \\75 rl(http://x)", &p), None);
        assert_eq!(filter_urls("background: u/**/rl(http://x)", &p), None);
    }

    #[test]
    fn urls_follow_image_policy() {
        let p = policy(false);
        assert_eq!(filter_urls("background: url(http://x.test/a.png); color: red", &p).as_deref(), Some("color: red"));
        assert!(p.had_remote_images());
        assert_eq!(
            filter_urls("background: url('cid:LOGO@x')", &p).as_deref(),
            Some("background: url(\"quorum-cid:att1\")")
        );
        let allowed = policy(true);
        let out = filter_urls("background:url(\"https://x.test/a.png\")", &allowed);
        assert_eq!(out.as_deref(), Some("background:url(\"quorum-remote:https%3A%2F%2Fx.test%2Fa.png\")"));
        assert_eq!(filter_urls("background:url(javascript:alert(1))", &allowed), None);
    }
}
