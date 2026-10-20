# ADR 0001: Desktop shell and UI toolkit

> **Status:** proposed (owner to confirm) · 2026-10-17

## Context
The app must be pure Rust, Mac first, and render HTML mail faithfully and safely, host rich-text
compose, show Office file previews, and support native notifications, a tray icon, deep links,
auto-update and accessibility (VoiceOver). Mail HTML is the forcing function: only a browser
engine renders real-world email correctly.

## Decision
**Tauri 2** for the shell (windows, tray, notifications, updater, deep links, single instance,
autostart plugins) with the system webview (WKWebView on macOS, WebView2 on Windows), and
**Leptos** (client-side rendering, compiled to `wasm32-unknown-unknown`, built with **Trunk**)
for the UI. All UI code is Rust; the only non-Rust sources we write are CSS. No handwritten
JS/TS; where a browser API is needed we call it through `web-sys`.

## Alternatives
| Option | Why not (now) |
|---|---|
| egui/eframe (CADCraft's stack) | No HTML rendering for mail bodies, weak rich-text editing, IME and screen-reader support; would still need a webview for mail and previews |
| Dioxus desktop | Strong runner-up: also webview-based, Rust runs natively with no wasm/IPC split. Rejected for now because Tauri's plugin set (updater, signing, tray, notifications, deep links) is more mature; the thin UI layer keeps a later swap cheap |
| Iced, Slint, GPUI | No embedded HTML; same mail problem as egui |
| Electron | Not Rust |

## Consequences
- Two compile targets: native core, wasm UI. `cargo xtask wasm` keeps the UI building.
- IPC is one narrow surface (`run`, `query`, `store-changed`) with types in `quorum-ipc`.
- WKWebView is the rendering engine for mail; ADR 0006 covers isolating it.

## Addendum (2026-10-18): Tauri plugins
Official Tauri 2 plugins (MIT/Apache-2.0): `tauri-plugin-notification` (native notifications),
`-single-instance`, `-window-state`, `-deep-link` (`quorum://`, `mailto:`), `-autostart` (login
item), plus tauri's `tray-icon` feature for the menu-bar presence icon. Idle time comes from
`ioreg HIDIdleTime` behind an `IdleSource` trait, so no `unsafe`.
