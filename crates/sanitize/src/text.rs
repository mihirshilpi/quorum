//! Plain-text extraction for previews and full-text search.

use crate::limits::truncate_input;
use crate::prepass;

/// Converts untrusted HTML to plain text: tags stripped, entities decoded, whitespace
/// collapsed to single spaces, trimmed.
///
/// Script, style, title and textarea content is dropped; block elements and line breaks
/// separate words. Input over [`crate::MAX_INPUT_BYTES`] is truncated first. Runs in linear
/// time and never panics.
///
/// ```
/// assert_eq!(
///     quorum_sanitize::to_plain_text("<p>Fish &amp; chips</p><p>at  <b>noon</b></p><script>x</script>"),
///     "Fish & chips at noon"
/// );
/// ```
#[must_use]
pub fn to_plain_text(html: &str) -> String {
    let (input, _) = truncate_input(html);
    prepass::plain_text(input)
}
