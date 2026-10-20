//! The UI's only connection to the core: `run`, `query` and core events.
//!
//! - In the desktop app these go through Tauri: `window.__TAURI__.core.invoke` for calls and
//!   `window.__TAURI__.event.listen` on [`quorum_ipc::EVENT_CHANNEL`] for [`CoreEvent`]s.
//! - **Browser preview mode:** when `window.__TAURI__` is absent (the UI opened in a normal
//!   browser through `trunk serve`), calls are answered by the in-wasm [`crate::mock`] backend
//!   with synthetic `example.com` data, and the mock's events are delivered through the same
//!   listener list. The mode is detected once at runtime ([`backend`]).
//!
//! Every JavaScript failure becomes a [`Reply::Err`] or a logged warning; nothing here panics.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use quorum_ipc::{CoreEvent, EVENT_CHANNEL, QueryRequest, Reply, RunRequest};
use serde_json::Value;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    async fn tauri_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    /// The same `invoke`, called synchronously: after a panic no future will be polled again.
    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn tauri_invoke_now(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "event"], js_name = listen)]
    async fn tauri_listen(event: &str, handler: &JsValue) -> Result<JsValue, JsValue>;
}

/// Where calls go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The Rust core, through Tauri IPC.
    Tauri,
    /// The in-wasm mock (browser preview mode).
    Preview,
}

type Listener = Rc<dyn Fn(&CoreEvent)>;

thread_local! {
    static BACKEND: Cell<Option<Backend>> = const { Cell::new(None) };
    static LISTENERS: RefCell<Vec<Listener>> = const { RefCell::new(Vec::new()) };
    static TAURI_HANDLER: RefCell<Option<Closure<dyn FnMut(JsValue)>>> = const { RefCell::new(None) };
}

/// The backend in use, detected on first call.
pub fn backend() -> Backend {
    BACKEND.with(|b| {
        if let Some(found) = b.get() {
            return found;
        }
        let found = detect();
        b.set(Some(found));
        found
    })
}

/// Whether the UI runs in browser preview mode (mock backend).
pub fn is_preview() -> bool {
    backend() == Backend::Preview
}

fn detect() -> Backend {
    let Some(window) = web_sys::window() else { return Backend::Preview };
    match js_sys::Reflect::get(&window, &JsValue::from_str("__TAURI__")) {
        Ok(v) if v.is_object() => Backend::Tauri,
        _ => Backend::Preview,
    }
}

fn js_message(e: &JsValue) -> String {
    if let Some(s) = e.as_string() {
        return s;
    }
    js_sys::Reflect::get(e, &JsValue::from_str("message"))
        .ok()
        .and_then(|m| m.as_string())
        .unwrap_or_else(|| "IPC failed".into())
}

async fn invoke(cmd: &str, args: impl serde::Serialize) -> Reply {
    let args = match serde_wasm_bindgen::to_value(&args) {
        Ok(a) => a,
        Err(e) => return Reply::Err { kind: "bug".into(), message: e.to_string() },
    };
    match tauri_invoke(cmd, args).await {
        Ok(v) => {
            // Values may hold maps; go through serde_json so they deserialise faithfully.
            let json: Result<Value, _> = serde_wasm_bindgen::from_value(v);
            match json.map(serde_json::from_value::<Reply>) {
                Ok(Ok(reply)) => reply,
                Ok(Err(e)) => Reply::Err { kind: "bug".into(), message: e.to_string() },
                Err(e) => Reply::Err { kind: "bug".into(), message: e.to_string() },
            }
        }
        Err(e) => Reply::Err { kind: "ipc".into(), message: js_message(&e) },
    }
}

#[derive(serde::Serialize)]
struct Wrap<T> {
    req: T,
}

/// Runs a command in the core.
pub async fn run(id: &str, params: Value) -> Reply {
    match backend() {
        Backend::Tauri => invoke("run", Wrap { req: RunRequest { id: id.into(), params } }).await,
        Backend::Preview => {
            let (reply, events) = crate::mock::run(id, params);
            for ev in &events {
                dispatch(ev);
            }
            reply
        }
    }
}

/// Runs a query in the core.
pub async fn query(id: &str, params: Value) -> Reply {
    match backend() {
        Backend::Tauri => invoke("query", Wrap { req: QueryRequest { id: id.into(), params } }).await,
        Backend::Preview => crate::mock::query(id, &params),
    }
}

/// Registers a listener for every core event (desktop and preview alike).
pub fn subscribe(listener: impl Fn(&CoreEvent) + 'static) {
    LISTENERS.with(|l| match l.try_borrow_mut() {
        Ok(mut list) => list.push(Rc::new(listener)),
        Err(_) => warn("event listener registered during dispatch; ignored"),
    });
}

/// Delivers `event` to every listener. Listeners may subscribe or dispatch re-entrantly.
pub fn dispatch(event: &CoreEvent) {
    let listeners: Vec<Listener> = LISTENERS.with(|l| l.try_borrow().map(|v| v.clone()).unwrap_or_default());
    for listener in listeners {
        listener(event);
    }
}

/// Starts forwarding Tauri events on [`EVENT_CHANNEL`] to [`subscribe`]rs. No-op in preview.
pub fn start_event_bridge() {
    if backend() != Backend::Tauri {
        return;
    }
    let handler = Closure::<dyn FnMut(JsValue)>::new(move |raw: JsValue| {
        let payload = match js_sys::Reflect::get(&raw, &JsValue::from_str("payload")) {
            Ok(p) => p,
            Err(_) => return warn("core event without payload"),
        };
        let json: Result<Value, _> = serde_wasm_bindgen::from_value(payload);
        match json.map(serde_json::from_value::<CoreEvent>) {
            Ok(Ok(ev)) => dispatch(&ev),
            // A newer core may send event types this UI does not know yet: skip them.
            _ => warn("unrecognised core event ignored"),
        }
    });
    let function: JsValue = handler.as_ref().clone();
    TAURI_HANDLER.with(|h| {
        if let Ok(mut slot) = h.try_borrow_mut() {
            *slot = Some(handler);
        }
    });
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = tauri_listen(EVENT_CHANNEL, &function).await {
            warn(&format!("could not listen for core events: {}", js_message(&e)));
        }
    });
}

/// Reports a UI panic: `console.error`, and in the desktop app the core's log (`ui.panic`), so a
/// frozen window is diagnosable from the terminal. Best effort and synchronous.
pub fn report_panic(message: &str) {
    web_sys::console::error_1(&JsValue::from_str(message));
    if backend() != Backend::Tauri {
        return;
    }
    #[derive(serde::Serialize)]
    struct Args<'a> {
        message: &'a str,
    }
    if let Ok(args) = serde_wasm_bindgen::to_value(&Args { message }) {
        let _ = tauri_invoke_now("ui_panic", args);
    }
}

/// Logs a warning to the browser console (no personal data).
pub fn warn(message: &str) {
    web_sys::console::warn_1(&JsValue::from_str(message));
}
