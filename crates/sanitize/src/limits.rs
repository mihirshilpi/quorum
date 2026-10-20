//! Input size and nesting depth caps.

/// Inputs longer than this many bytes are truncated (on a char boundary) before parsing.
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// Start tags nested deeper than this are dropped before parsing (their content is kept).
pub const MAX_DEPTH: usize = 256;

/// Truncates `html` to [`MAX_INPUT_BYTES`] on a char boundary; the flag says whether it did.
pub(crate) fn truncate_input(html: &str) -> (&str, bool) {
    truncate_to(html, MAX_INPUT_BYTES)
}

pub(crate) fn truncate_to(html: &str, max: usize) -> (&str, bool) {
    if html.len() <= max {
        return (html, false);
    }
    let mut end = max;
    while end > 0 && !html.is_char_boundary(end) {
        end -= 1;
    }
    (html.get(..end).unwrap_or(""), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_char_boundary() {
        let s = "aé"; // 'é' is two bytes
        assert_eq!(truncate_to(s, 2), ("a", true));
        assert_eq!(truncate_to(s, 3), ("aé", false));
    }
}
