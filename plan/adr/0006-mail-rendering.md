# ADR 0006: Rendering untrusted mail and chat HTML

> **Status:** accepted · 2026-10-17

## Decision
1. **Sanitise in Rust** (`quorum-sanitize`, built on `ammonia`/`html5ever`): allow-list of tags
   and attributes, no scripts, no event handlers, no forms, no `<object>/<embed>/<iframe>`,
   CSS limited to inline styles with `url()` stripped unless it is a mapped `cid:` image; caps on
   document size and nesting depth.
2. **Isolate:** the sanitised body is shown in an `<iframe sandbox>` (no `allow-scripts`, no
   `allow-same-origin`) served from a custom `quorum-mail://` URI scheme with
   `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; img-src quorum-cid: data:`
   plus `quorum-remote:` only after the user allows remote images for that sender.
3. **Remote images off by default** (tracking pixels). Allowed images are fetched by the Rust core
   through a proxy scheme without cookies or referrer.
4. **Links** are intercepted; the real target is shown and opened in the system browser.
5. Chat HTML (Teams messages) goes through the same sanitiser with a smaller allow-list.

## Alternatives
Rendering in the main webview with the UI (rejected: one bug exposes the IPC bridge);
converting all mail to plain text (rejected: unusable for real mail).

## Consequences
Fuzz and hostile-input tests on the sanitiser are part of `cargo xtask ci`.

## Addendum (2026-10-18): conversation view frames
The conversation view shows each message in its own frame sized to its content, which the UI
can only measure from a same-origin document. Those frames are `srcdoc` documents with
`sandbox="allow-same-origin"` and **no** `allow-scripts` (nor forms, popups or top navigation):
scripts cannot run in them at all, so same origin only lets the UI read the frame's height and
forward key presses; it gives the mail nothing. The core still sanitises every body, the
document carries the same CSP, and `quorum-mail://` remains for anything opened on its own.
The core also splits each reply's quoted history (`quorum_sanitize::quote_start`) before
sanitising, so the view shows new text first.
