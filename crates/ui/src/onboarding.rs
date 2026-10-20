//! App-level overlays: first-run onboarding, the link confirmation dialog, and applying the
//! appearance settings (`data-theme`, `data-density` on `<html>`).
//!
//! - **Onboarding** shows while `account.current` reports `not_configured` or `signed_out`:
//!   it explains Quorum in plain words, shows the admin's key steps (from
//!   `docs/setup-tenant.md`), takes the tenant and client id (`account.configure`) and signs in
//!   (`account.sign_in`), and explains the sample-data mode (`--mock-graph`). In browser
//!   preview add `?onboarding` to the URL to see it.
//! - **Link confirmation**: the shell intercepts link clicks in mail and chat and sends
//!   `CoreEvent::Ui {command: "link.confirm", params: {url}}`; the dialog shows the real URL
//!   (host emphasised) and opens it with `app.open_url` only after the user confirms.

use leptos::prelude::*;
use leptos::task::spawn_local;
use quorum_ipc::{CoreEvent, Reply};
use serde_json::{Map, Value, json};
use wasm_bindgen::JsValue;

use crate::areas::settings::{TenantForm, run_or_toast, select_section};
use crate::components::Icon;
use crate::icons::IconName;
use crate::state::{AppState, app_state, use_query};

/// Longest URL shown in the dialog.
const MAX_SHOWN_URL: usize = 2048;

/// Splits a URL for display: (scheme + "//", host, rest). Pure, for tests.
pub fn split_url(url: &str) -> (String, String, String) {
    // Drop control and bidi-override characters: they can disguise the real host.
    let url: String = url
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(MAX_SHOWN_URL)
        .collect();
    if let Some(rest) = url.strip_prefix("mailto:") {
        return ("mailto:".into(), rest.split('?').next().unwrap_or_default().into(), String::new());
    }
    let Some((scheme, after)) = url.split_once("://") else { return (String::new(), url, String::new()) };
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let (host, rest) = after.split_at(end);
    (format!("{scheme}://"), host.to_owned(), rest.to_owned())
}

/// Whether the URL's query string asks for the onboarding screen (preview only).
fn wants_onboarding() -> bool {
    let Some(window) = web_sys::window() else { return false };
    js_sys::Reflect::get(&window, &JsValue::from_str("location"))
        .and_then(|l| js_sys::Reflect::get(&l, &JsValue::from_str("search")))
        .ok()
        .and_then(|s| s.as_string())
        .is_some_and(|s| s.contains("onboarding"))
}

fn set_root_attr(name: &str, value: &str) {
    if let Some(root) = crate::dom::document().and_then(|d| d.document_element()) {
        let _ = root.set_attribute(name, value);
    }
}

/// Mount once in the shell, after the main region.
#[component]
pub fn Overlays() -> impl IntoView {
    let state = app_state();
    let link = RwSignal::new(None::<String>);
    let dismissed = RwSignal::new(false);
    // Kept apart from `state.account`: `load_account` re-reads the stored state, which stays
    // `signed_in` while sync is paused for sign-in.
    let reauth = RwSignal::new(false);
    let force = state.preview && wants_onboarding();
    // `quorum://setup?tenant=..&client_id=..` from the admin pre-fills the welcome screen.
    let prefill = RwSignal::new(None::<(String, String)>);
    crate::ipc::subscribe(move |ev| {
        if let CoreEvent::Account { state } = ev {
            reauth.try_set(state == "needs_reauth");
            if state == "signed_in" {
                prefill.try_set(None);
            }
        }
        if let CoreEvent::Ui { command, params } = ev {
            match command.as_str() {
                "link.confirm" => {
                    if let Some(url) = params.get("url").and_then(Value::as_str) {
                        link.try_set(Some(url.to_owned()));
                    }
                }
                "setup.prefill" => {
                    let id = |k: &str| params.get(k).and_then(Value::as_str).map(str::to_owned);
                    if let (Some(t), Some(c)) = (id("tenant"), id("client_id")) {
                        prefill.try_set(Some((t, c)));
                        dismissed.try_set(false);
                    }
                }
                "view.settings" => {
                    if let Some(s) = params.get("section").and_then(Value::as_str) {
                        select_section(s);
                    }
                }
                _ => {}
            }
        }
    });
    // Appearance: apply the theme and density settings to <html>.
    let values = use_query::<Map<String, Value>>("settings.get", || json!({}), &["settings", "setting"]);
    let last_density = StoredValue::new(String::new());
    Effect::new(move |_| {
        let Some(v) = values.data() else { return };
        let theme = v.get("appearance.theme").and_then(Value::as_str).unwrap_or("system").to_owned();
        let density = v.get("appearance.density").and_then(Value::as_str).unwrap_or("comfortable").to_owned();
        set_root_attr("data-theme", &theme);
        set_root_attr("data-density", &density);
        if last_density.try_get_value().is_some_and(|d| d != density) {
            // Virtual lists read the row height on resize.
            if let Some(w) = web_sys::window()
                && let Ok(ev) = web_sys::Event::new("resize")
            {
                let _ = w.dispatch_event(&ev);
            }
        }
        last_density.set_value(density);
    });
    let account = use_query::<Value>("account.current", || json!({}), &["account"]);
    // Re-read the account whenever the core reports a sign-in change.
    Effect::new(move |_| {
        let _ = state.account.with(|a| a.state.clone());
        account.refetch();
    });
    let needs_reauth = move || reauth.get();
    let show_onboarding = move || {
        if dismissed.get() {
            return false;
        }
        force
            || prefill.with(Option::is_some)
            || account.data().is_some_and(|a| {
                matches!(a.get("state").and_then(Value::as_str), Some("not_configured" | "signed_out"))
            })
    };
    view! {
        {move || show_onboarding().then(|| view! { <Onboarding dismissed=dismissed prefill=prefill /> })}
        {move || link.get().map(|url| view! { <LinkDialog url=url link=link /> })}
        {move || needs_reauth().then(|| view! { <ReauthBanner reauth=reauth /> })}
    }
}

/// Non-blocking banner shown while sync is paused for sign-in (`needs_reauth`).
#[component]
fn ReauthBanner(reauth: RwSignal<bool>) -> impl IntoView {
    let state = app_state();
    let busy = RwSignal::new(false);
    let sign_in = move |_| {
        busy.set(true);
        spawn_local(async move {
            if run_or_toast(state, "account.sign_in", json!({})).await.is_some() {
                // account.sign_in restarts the runtime; resuming is a harmless safety net.
                let _ = crate::ipc::run("sync.resume", json!({})).await;
                reauth.try_set(false);
            }
            busy.try_set(false);
        });
    };
    view! {
        <div class="reauth-banner" role="status" aria-live="polite">
            <span>"Sign in again to keep Quorum up to date."</span>
            <button class="button primary" on:click=sign_in disabled=move || busy.get()>"Sign in"</button>
        </div>
    }
}

#[component]
fn Onboarding(dismissed: RwSignal<bool>, prefill: RwSignal<Option<(String, String)>>) -> impl IntoView {
    let state = app_state();
    let from_link = prefill.get_untracked();
    let linked = from_link.is_some();
    let signing_in = RwSignal::new(false);
    let sign_in = move |_| {
        signing_in.set(true);
        spawn_local(async move {
            let _ = run_or_toast(state, "account.sign_in", json!({})).await;
            signing_in.try_set(false);
        });
    };
    request_animation_frame(|| {
        let _ = crate::dom::focus_selector(".onboarding input");
    });
    view! {
        <div class="onboarding" role="dialog" aria-modal="true" aria-labelledby="onboarding-title">
            <div class="onboarding-card">
                <header class="onboarding-head">
                    <span class="onboarding-mark"><Icon name=IconName::Mail /></span>
                    <h1 id="onboarding-title">"Welcome to Quorum"</h1>
                    <p class="onboarding-lead">
                        "Your work mail, calendar, Teams chats and files in one window, with one sign-in. "
                        "Quorum keeps a private copy on this Mac so it is fast and works offline, and it talks only to Microsoft's official APIs."
                    </p>
                </header>
                <div class="onboarding-grid">
                    <section class="onboarding-step" aria-labelledby="ob-connect">
                        <h2 id="ob-connect">"Connect your organisation"</h2>
                        <p class="settings-note">
                            {if linked {
                                "Your organisation's IDs are filled in from the setup link. Check them, then sign in with your work account in your browser."
                            } else {
                                "Your IT admin sets Quorum up once for your organisation and gives you a setup link, or two IDs to paste here. Then sign in with your work account in your browser."
                            }}
                        </p>
                        <TenantForm submit="Save and sign in" sign_in=true initial=from_link.unwrap_or_default() />
                        <button class="button link-button" on:click=sign_in disabled=move || signing_in.get()>
                            "Already set up? Sign in"
                        </button>
                    </section>
                    <section class="onboarding-step" aria-labelledby="ob-admin">
                        <h2 id="ob-admin">"For the IT admin (about two minutes)"</h2>
                        <ol class="onboarding-list">
                            <li>"Open Azure Cloud Shell (shell.azure.com) signed in as an admin of your organisation."</li>
                            <li>"Run the setup script from the Quorum repository: " <code>"scripts/register-quorum.sh"</code> "."</li>
                            <li>"It registers Quorum, grants consent and prints a setup link. Send that link to your colleagues."</li>
                        </ol>
                        <p class="settings-note">
                            "Prefer the portal? docs/setup-tenant.md walks through it. Quorum has no client secret, uses delegated permissions only, and your data goes only between this Mac and Microsoft."
                        </p>
                    </section>
                    <section class="onboarding-step" aria-labelledby="ob-sample">
                        <h2 id="ob-sample">"Try it with sample data"</h2>
                        <p class="settings-note">
                            "No tenant yet? Start Quorum with made-up mail, meetings and chats. Nothing leaves your Mac."
                        </p>
                        <pre class="code-block">"quorum --mock-graph"</pre>
                        <p class="settings-note">"From a source checkout: " <code>"cargo xtask dev -- --mock-graph"</code></p>
                    </section>
                </div>
                <footer class="onboarding-foot">
                    <button class="button" on:click=move |_| {
                        prefill.set(None);
                        dismissed.set(true);
                    }>"Not now"</button>
                </footer>
            </div>
        </div>
    }
}

#[component]
fn LinkDialog(url: String, link: RwSignal<Option<String>>) -> impl IntoView {
    let state: AppState = app_state();
    let (scheme, host, rest) = split_url(&url);
    let close = move || link.set(None);
    let open = {
        let url = url.clone();
        move |_| {
            let url = url.clone();
            link.set(None);
            spawn_local(async move {
                if let Reply::Err { message, .. } = crate::ipc::run("app.open_url", json!({ "url": url })).await {
                    state.toast(crate::state::ToastLevel::Error, message);
                }
            });
        }
    };
    request_animation_frame(|| {
        let _ = crate::dom::focus_selector(".link-dialog .button.primary");
    });
    view! {
        <div class="dialog-backdrop" on:click=move |_| close()>
            <div
                class="dialog link-dialog"
                role="alertdialog"
                aria-modal="true"
                aria-labelledby="link-dialog-title"
                aria-describedby="link-dialog-url"
                on:click=|e| e.stop_propagation()
                on:keydown=move |e: web_sys::KeyboardEvent| {
                    if e.key() == "Escape" {
                        e.prevent_default();
                        e.stop_propagation();
                        close();
                    }
                }
            >
                <h2 id="link-dialog-title">"Open this link?"</h2>
                <p class="settings-note">"It opens in your browser. Check the address is the one you expect."</p>
                <p class="link-url" id="link-dialog-url">
                    <span class="link-scheme">{scheme}</span>
                    <strong class="link-host">{host}</strong>
                    <span class="link-rest">{rest}</span>
                </p>
                <div class="dialog-actions">
                    <button class="button" on:click=move |_| close()>"Cancel"</button>
                    <button class="button primary" on:click=open>"Open"</button>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_for_display() {
        assert_eq!(split_url("https://example.com/a?b#c"), ("https://".into(), "example.com".into(), "/a?b#c".into()));
        assert_eq!(split_url("https://example.com"), ("https://".into(), "example.com".into(), String::new()));
        assert_eq!(split_url("mailto:a@example.com?x"), ("mailto:".into(), "a@example.com".into(), String::new()));
        assert_eq!(split_url("weird").1, "weird");
        assert_eq!(split_url("https://ex\u{202e}ample.com\n/x").1, "example.com");
        let long = format!("https://example.com/{}", "a".repeat(10_000));
        assert!(split_url(&long).2.len() < MAX_SHOWN_URL);
    }
}
