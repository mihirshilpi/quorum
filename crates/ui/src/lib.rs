//! Quorum user interface (Leptos, compiled to WebAssembly and hosted in the Tauri webview).
//!
//! The UI is thin: it reads store-backed queries and acts through `run(id, params)` over IPC.
//!
//! Module map:
//! - [`ipc`]: `run` / `query` / core-event subscription; routes to [`mock`] in browser preview.
//! - [`state`]: app state (area, selections, toasts, account), `use_query` with store-changed refresh.
//! - [`commands`]: UI-only commands and the single dispatcher every UI action goes through.
//! - [`keys`]: shortcut parsing and the global keymap built from the command registry.
//! - [`palette`]: the command palette and its fuzzy matcher.
//! - [`components`]: reusable building blocks (virtual list, split pane, toasts, avatar, ...).
//! - [`areas`]: one module per app area (mail, calendar, chat, teams, files, people, ...).
//! - [`icons`]: our own SVG path data, drawn in code.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing
)]
#![warn(missing_docs)]

pub mod areas;
pub mod commands;
pub mod components;
pub mod compose_chat;
pub mod dom;
pub mod icons;
pub mod ipc;
pub mod keys;
pub mod mock;
pub mod onboarding;
pub mod palette;
mod shell;
pub mod state;

/// Mounts the app into `<body>`, after installing a panic hook that reports panics to the
/// webview console so a wasm panic is diagnosable.
pub fn start() {
    install_panic_hook();
    leptos::mount::mount_to_body(shell::App);
}

/// Logs every panic (message and location) with `console.error`, then runs the previous hook.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = panic_message(info);
        #[cfg(target_arch = "wasm32")]
        {
            ipc::report_panic(&message);
            show_crash_banner();
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = &message;
        previous(info);
    }));
}

/// After a panic the wasm UI no longer reacts, so tell the person and offer a reload (the window
/// rebuilds from the local store; drafts are autosaved). Plain DOM: a link to the page itself
/// reloads it without needing wasm to run again.
#[cfg(target_arch = "wasm32")]
fn show_crash_banner() {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
    if doc.get_element_by_id("crash-banner").is_some() {
        return;
    }
    let (Ok(banner), Ok(text), Ok(link), Some(body)) =
        (doc.create_element("div"), doc.create_element("span"), doc.create_element("a"), doc.body())
    else {
        return;
    };
    banner.set_id("crash-banner");
    banner.set_class_name("crash-banner");
    let _ = banner.set_attribute("role", "alert");
    text.set_text_content(Some("Something went wrong in this window. Your drafts are saved."));
    link.set_text_content(Some("Reload"));
    let _ = link.set_attribute("href", "");
    let _ = banner.append_child(&text);
    let _ = banner.append_child(&link);
    let _ = body.append_child(&banner);
}

/// `"quorum-ui panicked at <file:line>: <message>"` for a panic.
fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let text = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(non-string panic payload)".to_owned());
    match info.location() {
        Some(loc) => format!("quorum-ui panicked at {}:{}: {text}", loc.file(), loc.line()),
        None => format!("quorum-ui panicked: {text}"),
    }
}
