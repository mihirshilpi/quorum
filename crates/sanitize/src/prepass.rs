//! A linear-time token pass that runs before any tree building.
//!
//! html5ever's tree builder does work proportional to the depth of the open-element stack for
//! many start tags, so hostile nesting (`<div>` × 1 000 000) costs quadratic time, and its list
//! of active formatting elements can grow without bound (`<p><b id=1></p><p><b id=2></p>…`).
//! This pass tokenises the input once (html5ever's tokenizer is linear) and re-emits it with:
//!
//! * open elements tracked approximately (with the common implied end tags) and start tags
//!   beyond [`MAX_DEPTH`] dropped, their content kept;
//! * at most [`MAX_OPEN_FORMATTING`] open formatting elements, and formatting elements that
//!   would be closed implicitly get explicit end tags (so the tree builder cannot re-open them
//!   in every following paragraph);
//! * raw-text elements (`script`, `style`, `title`, `textarea`, `xmp`, `iframe`, `noembed`,
//!   `noframes`) removed with their content, everything after `<plaintext>` removed;
//! * comments, doctypes and NUL characters removed; tag and attribute names restricted to
//!   `[A-Za-z0-9_:.-]`.
//!
//! The output is not trusted: it is always cleaned by ammonia afterwards. The same tokens also
//! drive [`plain_text`].

use std::cell::RefCell;
use std::collections::HashMap;

use html5ever::TokenizerResult;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer};

use crate::limits::MAX_DEPTH;

/// The most unclosed formatting elements (`b`, `font`, `a`, …) passed to the tree builder.
const MAX_OPEN_FORMATTING: usize = 64;

/// How far down the open-element stack implied end tags are searched.
const IMPLIED_SCAN_LIMIT: usize = 64;

/// Elements whose content is raw text and is dropped together with the element.
fn raw_kind(tag: &str) -> Option<RawKind> {
    match tag {
        "script" => Some(RawKind::ScriptData),
        "style" | "xmp" | "iframe" | "noembed" | "noframes" => Some(RawKind::Rawtext),
        "title" | "textarea" => Some(RawKind::Rcdata),
        _ => None,
    }
}

fn is_void(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "basefont"
            | "bgsound"
            | "br"
            | "col"
            | "embed"
            | "frame"
            | "hr"
            | "image"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_formatting(tag: &str) -> bool {
    matches!(
        tag,
        "a" | "b" | "big" | "code" | "em" | "font" | "i" | "nobr" | "s" | "small" | "strike" | "strong" | "tt" | "u"
    )
}

/// Start tags that close an open `<p>` (HTML "close a p element" cases, abridged).
fn closes_p(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "center"
            | "details"
            | "dir"
            | "div"
            | "dl"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "main"
            | "menu"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "summary"
            | "table"
            | "ul"
    )
}

/// Elements that separate words in plain text.
fn breaks_words(tag: &str) -> bool {
    closes_p(tag) || matches!(tag, "br" | "dd" | "dt" | "li" | "td" | "th" | "tr" | "caption")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Html,
    Text,
}

struct State {
    mode: Mode,
    out: String,
    stack: Vec<String>,
    open_formatting: usize,
    dropped: HashMap<String, usize>,
    /// Inside a raw-text element whose content is dropped.
    skip_until: Option<String>,
    /// After `<plaintext>`: everything is dropped.
    stopped: bool,
    /// Text mode: a word separator is due before the next character.
    pending_space: bool,
}

impl State {
    fn new(mode: Mode, capacity: usize) -> Self {
        Self {
            mode,
            out: String::with_capacity(capacity),
            stack: Vec::new(),
            open_formatting: 0,
            dropped: HashMap::new(),
            skip_until: None,
            stopped: false,
            pending_space: false,
        }
    }

    /// Pops the nearest open `target` if found before any of `boundaries`.
    /// Pops open elements above `len`. Formatting elements popped this way get an explicit
    /// end tag, so the tree builder never keeps them in its list of active formatting elements
    /// (which it would otherwise re-open in every following block: unbounded amplification).
    fn pop_to(&mut self, len: usize) {
        while self.stack.len() > len {
            let Some(open) = self.stack.pop() else {
                break;
            };
            if is_formatting(&open) {
                self.open_formatting = self.open_formatting.saturating_sub(1);
                self.out.push_str("</");
                self.out.push_str(&open);
                self.out.push('>');
            }
        }
    }

    /// Pops the nearest open `target` if found before any of `boundaries` (searching only the
    /// top [`IMPLIED_SCAN_LIMIT`] entries, which keeps the pass linear).
    fn close_implied(&mut self, targets: &[&str], boundaries: &[&str]) {
        let mut found = None;
        for (i, open) in self.stack.iter().enumerate().rev().take(IMPLIED_SCAN_LIMIT) {
            if targets.contains(&open.as_str()) {
                found = Some(i);
                break;
            }
            if boundaries.contains(&open.as_str()) {
                break;
            }
        }
        if let Some(i) = found {
            self.pop_to(i + 1);
            self.stack.truncate(i);
        }
    }

    fn apply_implied_end_tags(&mut self, tag: &str) {
        const P_SCOPE: &[&str] = &["table", "td", "th", "caption", "button", "object", "marquee", "template"];
        if closes_p(tag) {
            self.close_implied(&["p"], P_SCOPE);
        }
        match tag {
            "li" => self.close_implied(&["li"], &["ul", "ol", "table", "td", "th"]),
            "dt" | "dd" => self.close_implied(&["dt", "dd"], &["dl", "table", "td", "th"]),
            "tr" => self.close_implied(&["tr"], &["table"]),
            "td" | "th" => self.close_implied(&["td", "th"], &["tr", "table"]),
            "tbody" | "thead" | "tfoot" => self.close_implied(&["tbody", "thead", "tfoot"], &["table"]),
            "option" => self.close_implied(&["option"], &["select"]),
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: &Tag) -> TokenSinkResult<()> {
        let name = &*tag.name;
        if name == "plaintext" {
            self.stopped = true;
            return TokenSinkResult::Plaintext;
        }
        if let Some(kind) = raw_kind(name) {
            self.skip_until = Some(name.to_owned());
            return TokenSinkResult::RawData(kind);
        }
        if self.mode == Mode::Text {
            if breaks_words(name) {
                self.pending_space = true;
            }
            return TokenSinkResult::Continue;
        }
        if matches!(name, "html" | "head" | "body") || !valid_name(name) {
            return TokenSinkResult::Continue;
        }
        if is_void(name) {
            self.emit_start(tag);
            return TokenSinkResult::Continue;
        }
        self.apply_implied_end_tags(name);
        let formatting = is_formatting(name);
        if (formatting && self.open_formatting >= MAX_OPEN_FORMATTING) || self.stack.len() >= MAX_DEPTH {
            match self.dropped.get_mut(name) {
                Some(count) => *count = count.saturating_add(1),
                None => {
                    self.dropped.insert(name.to_owned(), 1);
                }
            }
            return TokenSinkResult::Continue;
        }
        if formatting {
            self.open_formatting += 1;
        }
        self.stack.push(name.to_owned());
        self.emit_start(tag);
        TokenSinkResult::Continue
    }

    fn end_tag(&mut self, tag: &Tag) {
        let name = &*tag.name;
        if self.mode == Mode::Text {
            if breaks_words(name) {
                self.pending_space = true;
            }
            return;
        }
        if matches!(name, "html" | "head" | "body") || !valid_name(name) {
            return;
        }
        if let Some(count) = self.dropped.get_mut(name)
            && *count > 0
        {
            *count -= 1;
            return;
        }
        if let Some(i) = self.stack.iter().rposition(|open| open == name) {
            self.pop_to(i + 1);
            self.stack.truncate(i);
            if is_formatting(name) {
                self.open_formatting = self.open_formatting.saturating_sub(1);
            }
        }
        self.out.push_str("</");
        self.out.push_str(name);
        self.out.push('>');
    }

    fn emit_start(&mut self, tag: &Tag) {
        self.out.push('<');
        self.out.push_str(&tag.name);
        for attr in &tag.attrs {
            let attr_name = &*attr.name.local;
            if !valid_name(attr_name) {
                continue;
            }
            self.out.push(' ');
            self.out.push_str(attr_name);
            self.out.push_str("=\"");
            for c in attr.value.chars() {
                match c {
                    '&' => self.out.push_str("&amp;"),
                    '"' => self.out.push_str("&quot;"),
                    '\0' => {}
                    c => self.out.push(c),
                }
            }
            self.out.push('"');
        }
        self.out.push('>');
    }

    fn text(&mut self, text: &StrTendril) {
        match self.mode {
            Mode::Html => {
                for c in text.chars() {
                    match c {
                        '&' => self.out.push_str("&amp;"),
                        '<' => self.out.push_str("&lt;"),
                        '>' => self.out.push_str("&gt;"),
                        '\0' => {}
                        c => self.out.push(c),
                    }
                }
            }
            Mode::Text => {
                for c in text.chars() {
                    if c.is_whitespace() {
                        self.pending_space = true;
                    } else if !c.is_control() {
                        if self.pending_space && !self.out.is_empty() {
                            self.out.push(' ');
                        }
                        self.pending_space = false;
                        self.out.push(c);
                    }
                }
            }
        }
    }
}

struct Sink(RefCell<State>);

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<()> {
        let mut state = self.0.borrow_mut();
        if state.stopped {
            return TokenSinkResult::Continue;
        }
        if let Some(raw) = state.skip_until.clone() {
            // In a raw-text state the tokenizer only ends it with the matching end tag.
            if let Token::TagToken(tag) = &token
                && tag.kind == TagKind::EndTag
                && *tag.name == *raw
            {
                state.skip_until = None;
                if state.mode == Mode::Text {
                    state.pending_space = true;
                }
            }
            return TokenSinkResult::Continue;
        }
        match token {
            Token::TagToken(tag) => match tag.kind {
                TagKind::StartTag => state.start_tag(&tag),
                TagKind::EndTag => {
                    state.end_tag(&tag);
                    TokenSinkResult::Continue
                }
            },
            Token::CharacterTokens(text) => {
                state.text(&text);
                TokenSinkResult::Continue
            }
            _ => TokenSinkResult::Continue,
        }
    }
}

fn run(html: &str, mode: Mode) -> String {
    let sink = Sink(RefCell::new(State::new(mode, html.len())));
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(html));
    let tokenizer = Tokenizer::new(sink, Default::default());
    // The sink never returns `Script`, so one feed consumes everything; loop defensively but
    // boundedly in case a future html5ever pauses.
    for _ in 0..1024 {
        if matches!(tokenizer.feed(&input), TokenizerResult::Done) {
            break;
        }
    }
    tokenizer.end();
    let state = tokenizer.sink.0.into_inner();
    state.out
}

/// Re-emits `html` with bounded nesting and raw-text elements removed (see module docs).
pub(crate) fn limit_nesting(html: &str) -> String {
    run(html, Mode::Html)
}

/// Extracts text with whitespace collapsed, skipping raw-text elements.
pub(crate) fn plain_text(html: &str) -> String {
    run(html, Mode::Text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_nesting_is_capped() {
        let html = format!("{}x{}", "<div>".repeat(1000), "</div>".repeat(1000));
        let out = limit_nesting(&html);
        assert_eq!(out.matches("<div>").count(), MAX_DEPTH);
        assert_eq!(out.matches("</div>").count(), MAX_DEPTH);
        assert!(out.contains('x'));
    }

    #[test]
    fn unclosed_paragraphs_do_not_count_as_nesting() {
        let html = "<p>para".repeat(1000);
        assert_eq!(limit_nesting(&html).matches("<p>").count(), 1000);
        let html = "<ul>".to_owned() + &"<li>item".repeat(1000) + "</ul>";
        assert_eq!(limit_nesting(&html).matches("<li>").count(), 1000);
    }

    #[test]
    fn implicitly_closed_formatting_gets_end_tags() {
        assert_eq!(limit_nesting("<p><b>x</p><p>y"), "<p><b>x</b></p><p>y");
        assert_eq!(limit_nesting("<b><i>x</b>y"), "<b><i>x</i></b>y");
    }

    #[test]
    fn open_formatting_is_capped() {
        let html = "<b>".repeat(1000);
        assert_eq!(limit_nesting(&html).matches("<b>").count(), MAX_OPEN_FORMATTING);
    }

    #[test]
    fn raw_text_elements_are_removed() {
        assert_eq!(limit_nesting("a<script>x<b>y</b></script>b<style>p{}</style>c<title>t</title>d"), "abcd");
        assert_eq!(limit_nesting("a<plaintext><b>x"), "a");
    }

    #[test]
    fn markup_is_re_escaped() {
        assert_eq!(
            limit_nesting("<a title='\"&amp;'>&lt;b&gt;</a><!-- c --><!DOCTYPE x>"),
            "<a title=\"&quot;&amp;\">&lt;b&gt;</a>"
        );
    }
}
