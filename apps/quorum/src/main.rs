//! Quorum desktop app: the Tauri shell around the Rust core.
//!
//! The shell owns only desktop integration: the window, custom URI schemes for mail bodies and
//! images ([`protocols`]), link interception ([`links`]), the native menu bar built from the
//! command registry ([`menu`]), the menu-bar presence icon ([`tray`]), notifications and the
//! Dock badge ([`notify`]), single instance, window state, deep links (`quorum://`, `mailto:`),
//! the login item and the optional control channel. Everything else is an engine command.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing
)]

mod activity;
mod links;
mod menu;
mod notify;
mod protocols;
mod tray;

use std::path::Path;
use std::sync::{Arc, Mutex};

use clap::Parser;
use quorum_engine::{Session, SessionOptions};
use quorum_ipc::{CoreEvent, EVENT_CHANNEL, Reply};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_deep_link::DeepLinkExt as _;
use tauri_plugin_notification::NotificationExt as _;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(name = "quorum", version)]
struct Args {
    /// Run against recorded Graph fixtures instead of the network.
    #[arg(long)]
    mock_graph: bool,
    /// Start the local control channel on this port (0 picks a free port).
    #[arg(long)]
    control: Option<u16>,
    /// Links passed by the OS or a second instance (`quorum://…`, `mailto:…`).
    #[arg(trailing_var_arg = true, hide = true)]
    links: Vec<String>,
}

/// Custom URI schemes served by [`protocols::handle`].
const SCHEMES: &[&str] = &["quorum-mail", "quorum-cid", "quorum-remote", "quorum-hosted"];

fn to_reply(r: quorum_engine::Result<Value>) -> Reply {
    match r {
        Ok(value) => Reply::Ok { value },
        Err(e) => Reply::Err { kind: e.kind().as_str().into(), message: e.message().into() },
    }
}

/// Screenshots for the control channel: captures the main window's screen rectangle.
struct WindowHooks {
    app: AppHandle,
}

impl quorum_control::UiHooks for WindowHooks {
    fn screenshot(&self, path: &Path) -> Result<(), String> {
        let win = self.app.get_webview_window("main").ok_or("no main window")?;
        let scale = win.scale_factor().map_err(|e| e.to_string())?;
        let pos = win.outer_position().map_err(|e| e.to_string())?.to_logical::<f64>(scale);
        let size = win.outer_size().map_err(|e| e.to_string())?.to_logical::<f64>(scale);
        let rect = format!("{},{},{},{}", pos.x.round(), pos.y.round(), size.width.round(), size.height.round());
        let status = std::process::Command::new("screencapture")
            .args(["-x", "-R", &rect])
            .arg(path)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() && path.exists() {
            Ok(())
        } else {
            Err("screen capture failed (grant Screen Recording permission to the terminal or app)".into())
        }
    }
}

#[expect(clippy::unreachable, reason = "expanded by #[tauri::command] for async commands")]
mod ipc {
    use super::to_reply;
    use quorum_engine::Session;
    use quorum_ipc::{QueryRequest, Reply, RunRequest};

    #[tauri::command]
    pub(crate) async fn run(state: tauri::State<'_, Session>, req: RunRequest) -> Result<Reply, ()> {
        Ok(to_reply(state.execute(&req.id, req.params).await))
    }

    /// A panic in the wasm UI, reported by its panic hook (file:line and message, no user data).
    #[tauri::command]
    pub(crate) fn ui_panic(message: String) {
        let message: String = message.chars().take(2_000).collect();
        tracing::error!(name: "ui.panic", message = %message, "the UI panicked");
    }

    #[tauri::command]
    pub(crate) async fn query(state: tauri::State<'_, Session>, req: QueryRequest) -> Result<Reply, ()> {
        Ok(to_reply(state.query(&req.id, req.params).await))
    }
}

/// Shell state kept in Tauri's state map.
struct Shell {
    remote: reqwest::Client,
    tray: Mutex<Option<tauri::tray::TrayIcon<Wry>>>,
    /// Control channel started from settings (aborted when the setting is turned off).
    control: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// The control port given on the command line (never stopped by settings).
    control_arg: Option<u16>,
    /// Window focus, for `sync.set_activity`.
    focus: Arc<activity::Focus>,
}

fn session(app: &AppHandle) -> Session {
    app.state::<Session>().inner().clone()
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Serves one custom-scheme request on the async runtime.
fn serve_scheme(
    app: &AppHandle,
    scheme: &'static str,
    req: &tauri::http::Request<Vec<u8>>,
    responder: tauri::UriSchemeResponder,
) {
    let s = session(app);
    let remote = app.state::<Shell>().remote.clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().map(str::to_owned);
    tauri::async_runtime::spawn(async move {
        let r = protocols::handle(&s, scheme, &path, query.as_deref(), &remote).await;
        responder.respond(to_http(r));
    });
}

fn to_http(r: protocols::Resp) -> tauri::http::Response<Vec<u8>> {
    let mut b = tauri::http::Response::builder().status(r.status).header("Content-Type", r.content_type);
    for (k, v) in r.headers {
        b = b.header(k, v);
    }
    b.body(r.body).unwrap_or_else(|_| {
        let mut resp = tauri::http::Response::new(Vec::new());
        *resp.status_mut() = tauri::http::StatusCode::INTERNAL_SERVER_ERROR;
        resp
    })
}

/// Routes an incoming `quorum://` or `mailto:` link to the UI.
fn open_link(app: &AppHandle, url: &str) {
    let Some(action) = links::parse_incoming(url) else {
        tracing::debug!(target: "quorum::shell", "ignored an unrecognised incoming link");
        return;
    };
    show_main(app);
    session(app).emit(CoreEvent::Ui { command: action.command, params: action.params });
}

/// Asks the UI to confirm an intercepted link (the dialog shows the real URL).
fn confirm_link(app: &AppHandle, url: String) {
    session(app).emit(CoreEvent::Ui { command: "link.confirm".into(), params: json!({ "url": url }) });
}

fn links_plugin() -> tauri::plugin::TauriPlugin<Wry> {
    tauri::plugin::Builder::<Wry, ()>::new("quorum-links")
        .on_navigation(|webview, url| match links::intercepted_link(url.as_str()) {
            Some(real) => {
                confirm_link(webview.app_handle(), real);
                false
            }
            None => true,
        })
        .build()
}

fn rebuild_menu(app: &AppHandle) {
    let commands = quorum_ipc_commands(&session(app));
    match menu::build(app, &commands) {
        Ok(m) => {
            if let Err(e) = app.set_menu(m) {
                tracing::warn!(target: "quorum::shell", error = %e, "setting the menu bar failed");
            }
        }
        Err(e) => tracing::warn!(target: "quorum::shell", error = %e, "building the menu bar failed"),
    }
}

/// Engine commands plus the UI-only commands (`view.*`, `palette.open`).
fn quorum_ipc_commands(s: &Session) -> Vec<quorum_ipc::CommandInfo> {
    let mut list = s.commands();
    for ui in quorum_ipc::UI_COMMANDS {
        if !list.iter().any(|c| c.id == ui.id) {
            list.push(ui.info());
        }
    }
    list
}

fn on_menu(app: &AppHandle, id: &str) {
    if let Some(cmd) = id.strip_prefix(menu::CMD_PREFIX) {
        show_main(app);
        let (command, params) = match cmd {
            "app.about" => ("view.settings".to_owned(), json!({ "section": "about" })),
            other => (other.to_owned(), Value::Null),
        };
        session(app).emit(CoreEvent::Ui { command, params });
    } else if let Some(p) = id.strip_prefix(tray::PRESENCE_PREFIX) {
        let s = session(app);
        let availability = p.to_owned();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = s.execute("presence.set", json!({ "availability": availability })).await {
                s.emit(CoreEvent::Toast { level: "error".into(), message: e.message().to_owned() });
            }
        });
    } else if id == tray::OPEN_ID {
        show_main(app);
    } else if id == tray::QUIT_ID {
        app.exit(0);
    }
}

fn tray_image(presence: &str) -> tauri::image::Image<'static> {
    tauri::image::Image::new_owned(tray::draw(presence), tray::SIZE, tray::SIZE)
}

fn build_tray(app: &AppHandle) -> tauri::Result<tauri::tray::TrayIcon<Wry>> {
    let menu = tray::menu(app, "unknown")?;
    tauri::tray::TrayIconBuilder::with_id("presence")
        .icon(tray_image("unknown"))
        .icon_as_template(false)
        .tooltip("Quorum")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .build(app)
}

async fn refresh_tray(app: &AppHandle) {
    let presence = notify::own_presence(&session(app)).await.unwrap_or_else(|| "unknown".into());
    let tray = app.state::<Shell>().tray.lock().ok().and_then(|t| t.clone());
    if let Some(t) = tray {
        let _ = t.set_icon(Some(tray_image(&presence)));
        let _ = t.set_tooltip(Some(format!("Quorum: {}", tray::presence_label(&presence))));
        if let Ok(m) = tray::menu(app, &presence) {
            let _ = t.set_menu(Some(m));
        }
    }
}

async fn setting_bool(s: &Session, key: &str) -> bool {
    quorum_engine::settings::get(s, key).await.ok().and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Applies device settings: login item, menu-bar icon, mailto handler, control channel.
async fn apply_settings(app: &AppHandle) {
    let s = session(app);
    let autostart = setting_bool(&s, "app.autostart").await;
    let launcher = app.autolaunch();
    if launcher.is_enabled().unwrap_or(!autostart) != autostart {
        let r = if autostart { launcher.enable() } else { launcher.disable() };
        if let Err(e) = r {
            tracing::warn!(target: "quorum::shell", error = %e, "changing the login item failed");
        }
    }
    let tray_on = setting_bool(&s, "app.tray").await;
    if let Some(t) = app.state::<Shell>().tray.lock().ok().and_then(|t| t.clone()) {
        let _ = t.set_visible(tray_on);
    }
    if !setting_bool(&s, "app.dock_badge").await
        && let Some(w) = app.get_webview_window("main")
    {
        let _ = w.set_badge_count(None);
    }
    #[cfg(any(windows, target_os = "linux"))]
    if setting_bool(&s, "app.default_mail_app").await {
        let _ = app.deep_link().register("mailto");
    }
    let shell = app.state::<Shell>();
    if shell.control_arg.is_none() {
        let enabled = setting_bool(&s, "agents.control_enabled").await;
        let port = quorum_engine::settings::get(&s, "agents.control_port")
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(47900);
        if let Ok(mut slot) = shell.control.lock() {
            match (enabled, slot.is_some()) {
                (true, false) => *slot = Some(start_control(app, port)),
                (false, true) => {
                    if let Some(h) = slot.take() {
                        h.abort();
                    }
                }
                _ => {}
            }
        }
    }
}

fn start_control(app: &AppHandle, port: u16) -> tauri::async_runtime::JoinHandle<()> {
    let s = session(app);
    let hooks: Arc<dyn quorum_control::UiHooks> = Arc::new(WindowHooks { app: app.clone() });
    tauri::async_runtime::spawn(async move {
        if let Err(e) = quorum_control::serve_with(s, port, Some(hooks)).await {
            tracing::error!(target: "quorum::control", error = %e, "control channel stopped");
        }
    })
}

/// Notifications and the Dock badge through Tauri.
struct TauriSink {
    app: AppHandle,
}

impl notify::Sink for TauriSink {
    fn show(&self, note: &notify::Note) {
        if let Err(e) = self.app.notification().builder().title(&note.title).body(&note.body).show() {
            tracing::debug!(target: "quorum::notify", error = %e, "notification not shown");
        }
    }

    fn badge(&self, count: Option<i64>) {
        if let Some(w) = self.app.get_webview_window("main") {
            let _ = w.set_badge_count(count);
        }
    }
}

/// Watches core events for the shell: forwards them to the webview and refreshes the tray,
/// menu and settings-driven integrations.
fn spawn_event_loop(app: AppHandle) {
    let mut events = session(&app).subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            let ev = match events.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    CoreEvent::StoreChanged { tables: vec!["*".into()] }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if let Err(e) = app.emit(EVENT_CHANNEL, &ev) {
                tracing::warn!(target: "quorum::shell", error = %e, "event emit failed");
            }
            match &ev {
                CoreEvent::StoreChanged { tables } => {
                    let any = |t: &str| tables.iter().any(|x| x == t || x == "*");
                    if any("presence") {
                        refresh_tray(&app).await;
                    }
                    if any(quorum_engine::settings::SETTINGS_TABLE) || any("setting") {
                        apply_settings(&app).await;
                    }
                }
                CoreEvent::Account { .. } => {
                    rebuild_menu(&app);
                    refresh_tray(&app).await;
                }
                _ => {}
            }
        }
    });
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let args = Args::parse();
    let session = Session::new(SessionOptions { mock_graph: args.mock_graph })?;
    let remote = protocols::remote_client()?;
    let control_arg = args.control;
    let launch_links = args.links.clone();
    let focus = Arc::new(activity::Focus::default());
    focus.set(true);
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            show_main(app);
            for a in argv.iter().skip(1).filter(|a| a.contains(':')) {
                open_link(app, a);
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(links_plugin())
        .manage(session.clone())
        .manage(Shell { remote, tray: Mutex::new(None), control: Mutex::new(None), control_arg, focus: focus.clone() })
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::Focused(f) if window.label() == "main" => {
                window.app_handle().state::<Shell>().focus.set(*f);
            }
            tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) => {
                let paths: Vec<String> =
                    paths.iter().take(1000).filter_map(|p| p.to_str().map(str::to_owned)).collect();
                if !paths.is_empty() {
                    crate::session(window.app_handle())
                        .emit(CoreEvent::Ui { command: "files.dropped".into(), params: json!({ "paths": paths }) });
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![ipc::run, ipc::query, ipc::ui_panic]);
    for scheme in SCHEMES {
        builder = builder.register_asynchronous_uri_scheme_protocol(*scheme, move |ctx, req, responder| {
            serve_scheme(ctx.app_handle(), scheme, &req, responder);
        });
    }
    builder
        .setup(move |app| {
            let handle = app.handle().clone();
            spawn_event_loop(handle.clone());
            let starter = session.clone();
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                starter.start().await;
                rebuild_menu(&h);
                refresh_tray(&h).await;
                apply_settings(&h).await;
            });
            rebuild_menu(&handle);
            match build_tray(&handle) {
                Ok(t) => {
                    if let Ok(mut slot) = app.state::<Shell>().tray.lock() {
                        *slot = Some(t);
                    }
                }
                Err(e) => tracing::warn!(target: "quorum::shell", error = %e, "menu-bar icon unavailable"),
            }
            app.on_menu_event(|app, event| on_menu(app, event.id().as_ref()));
            tauri::async_runtime::spawn(notify::run(session.clone(), TauriSink { app: handle.clone() }));
            tauri::async_runtime::spawn(activity::run(session.clone(), focus.clone(), activity::SystemIdle));
            if let Some(port) = control_arg {
                drop(start_control(&handle, port));
            }
            let dl = handle.clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    open_link(&dl, url.as_str());
                }
            });
            // Links that launched the app wait until the UI has had time to load and subscribe.
            let mut pending: Vec<String> = launch_links.clone();
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                pending.extend(urls.into_iter().map(|u| u.to_string()));
            }
            if !pending.is_empty() {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    for l in &pending {
                        open_link(&h, l);
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}
