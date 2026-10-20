//! Plain-text extraction.

use quorum_sanitize::{MAX_INPUT_BYTES, to_plain_text};

#[test]
fn strips_tags_and_decodes_entities() {
    assert_eq!(to_plain_text("<div>Caf&eacute; &lt;3 &#x1F600; <b>bold</b>&nbsp;text</div>"), "Café <3 😀 bold text");
}

#[test]
fn blocks_separate_words() {
    assert_eq!(to_plain_text("<table><tr><td>a</td><td>b</td></tr></table><p>c</p>d<br>e"), "a b c d e");
}

#[test]
fn hidden_content_is_dropped() {
    assert_eq!(to_plain_text("<head><title>T</title></head><style>.x{}</style><script>var a;</script>Body"), "Body");
}

#[test]
fn collapses_whitespace() {
    assert_eq!(to_plain_text("  a \n\n\t b  "), "a b");
    assert_eq!(to_plain_text(""), "");
    assert_eq!(to_plain_text("plain text, no tags"), "plain text, no tags");
}

#[test]
fn hostile_input_does_not_panic() {
    let deep = format!("{}x{}", "<span>".repeat(200_000), "</span>".repeat(200_000));
    assert_eq!(to_plain_text(&deep), "x");
    let huge = "a ".repeat(MAX_INPUT_BYTES);
    assert!(to_plain_text(&huge).len() <= MAX_INPUT_BYTES);
    let _ = to_plain_text("<<<>>><!--<![CDATA[\u{0}\u{FFFF}&#xD800;&#0;&");
}
