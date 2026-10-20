//! Settings, cached blobs and URL opening.
//!
//! - `settings.get` / `settings.schema` (queries) and `settings.set` / `settings.reset`
//!   (commands). Every key is declared in [`SPECS`] with a type, a range or choice list and a
//!   default; unknown keys and badly typed values are rejected. Account-scoped keys
//!   (`mail.*`, `presence.*`) live in the account's store once it is open; device-scoped keys
//!   (notifications, appearance, agents, app) live in `settings.json` in the data folder, so
//!   the shell can read them before sign-in. In mock mode nothing touches the disk: values go
//!   to the in-memory store, or to process memory before it opens.
//! - `blob.get {id}` (query) returns cached bytes (inline mail images) as base64.
//! - `app.open_url {url}` opens an `http`, `https` or `mailto` URL in the system handler;
//!   `link.confirm {url}` only validates a link and describes it (the UI shows the dialog).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use quorum_ipc::CoreEvent;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::command::{CommandFuture, CommandSpec, always, parse};
use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

/// Store table name announced when settings change (UI queries listen on it).
pub const SETTINGS_TABLE: &str = "settings";

/// Longest URL `app.open_url` accepts.
const MAX_URL_LEN: usize = 8 * 1024;

/// Where a setting is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Per Mac: `settings.json` (readable before sign-in).
    Device,
    /// Per account: the account's store (falls back to the file before sign-in).
    Account,
}

/// A setting's type, constraint and default.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Kind {
    /// `true` / `false`.
    Bool(bool),
    /// An integer in `min..=max`.
    Int { min: i64, max: i64, default: i64 },
    /// One of `choices`.
    Choice { choices: &'static [&'static str], default: &'static str },
}

/// One declared setting.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SettingSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub section: &'static str,
    pub kind: Kind,
    pub scope: Scope,
}

const fn b(key: &'static str, label: &'static str, section: &'static str, default: bool) -> SettingSpec {
    SettingSpec { key, label, section, kind: Kind::Bool(default), scope: Scope::Device }
}

const fn i(
    key: &'static str,
    label: &'static str,
    section: &'static str,
    min: i64,
    max: i64,
    default: i64,
) -> SettingSpec {
    SettingSpec { key, label, section, kind: Kind::Int { min, max, default }, scope: Scope::Device }
}

const fn c(
    key: &'static str,
    label: &'static str,
    section: &'static str,
    choices: &'static [&'static str],
    default: &'static str,
) -> SettingSpec {
    SettingSpec { key, label, section, kind: Kind::Choice { choices, default }, scope: Scope::Device }
}

/// Marks a spec account-scoped (kept in the account's store).
const fn account(mut s: SettingSpec) -> SettingSpec {
    s.scope = Scope::Account;
    s
}

/// Every setting Quorum knows.
pub(crate) const SPECS: &[SettingSpec] = &[
    b("notifications.mail", "New mail", "notifications", true),
    b("notifications.chat", "New chat messages", "notifications", true),
    b("notifications.meetings", "Meeting reminders", "notifications", true),
    i("notifications.reminder_minutes", "Remind me before meetings (minutes)", "notifications", 0, 120, 5),
    b("notifications.quiet_when_dnd", "Quiet while Do not disturb", "notifications", true),
    account(i("mail.undo_send_seconds", "Undo send delay (seconds)", "mail", 0, 30, 5)),
    account(b("mail.remote_images", "Load remote images by default", "mail", false)),
    account(i("mail.body_retention_days", "Keep message bodies offline (days)", "mail", 1, 3650, 90)),
    account(b("presence.keep_alive", "Keep my presence available while Quorum is open", "presence", true)),
    account(i("presence.idle_minutes", "Show me as away after idle (minutes)", "presence", 1, 120, 5)),
    c("appearance.theme", "Theme", "appearance", &["system", "light", "dark"], "system"),
    c("appearance.density", "Density", "appearance", &["comfortable", "compact"], "comfortable"),
    b("agents.control_enabled", "Allow agents to control Quorum (local control channel)", "agents", false),
    i("agents.control_port", "Control channel port", "agents", 1024, 65535, 47900),
    c(
        "agents.destructive_policy",
        "Send, post, delete and share by agents",
        "agents",
        &["confirm", "deny", "allow"],
        "confirm",
    ),
    b("app.autostart", "Open Quorum at login", "app", false),
    b("app.tray", "Show Quorum in the menu bar", "app", true),
    b("app.dock_badge", "Show the unread count on the Dock icon", "app", true),
    b("app.default_mail_app", "Use Quorum for email links (mailto:)", "app", false),
];

/// The spec for `key`.
pub(crate) fn spec(key: &str) -> Option<&'static SettingSpec> {
    SPECS.iter().find(|s| s.key == key)
}

impl Kind {
    /// The default value.
    pub(crate) fn default_value(self) -> Value {
        match self {
            Self::Bool(d) => json!(d),
            Self::Int { default, .. } => json!(default),
            Self::Choice { default, .. } => json!(default),
        }
    }

    /// Validates `v`; returns the normalised value or a user-safe reason.
    pub(crate) fn check(self, v: &Value) -> std::result::Result<Value, String> {
        match self {
            Self::Bool(_) => v.as_bool().map(Value::Bool).ok_or_else(|| "expected true or false".to_owned()),
            Self::Int { min, max, .. } => match v.as_i64() {
                Some(n) if (min..=max).contains(&n) => Ok(json!(n)),
                _ => Err(format!("expected a whole number from {min} to {max}")),
            },
            Self::Choice { choices, .. } => match v.as_str() {
                Some(s) if choices.contains(&s) => Ok(json!(s)),
                _ => Err(format!("expected one of: {}", choices.join(", "))),
            },
        }
    }

    fn describe(self) -> Value {
        match self {
            Self::Bool(d) => json!({ "type": "bool", "default": d }),
            Self::Int { min, max, default } => json!({ "type": "int", "min": min, "max": max, "default": default }),
            Self::Choice { choices, default } => json!({ "type": "choice", "choices": choices, "default": default }),
        }
    }
}

/// Values of the settings file (or process memory in mock mode).
type Values = BTreeMap<String, Value>;

fn memory() -> &'static Mutex<Values> {
    static MEM: OnceLock<Mutex<Values>> = OnceLock::new();
    MEM.get_or_init(|| Mutex::new(Values::new()))
}

/// The device settings file.
pub fn settings_path() -> PathBuf {
    crate::account::data_dir().join("settings.json")
}

/// Largest settings file read, in bytes.
const MAX_FILE: u64 = 256 * 1024;

fn read_file() -> Values {
    let path = settings_path();
    let small = std::fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_FILE);
    if !small {
        return Values::new();
    }
    let Ok(text) = std::fs::read_to_string(&path) else { return Values::new() };
    let parsed: Values = serde_json::from_str(&text).unwrap_or_default();
    // Keep only known keys with valid values: the file may be hand-edited or stale.
    parsed.into_iter().filter_map(|(k, v)| spec(&k).and_then(|s| s.kind.check(&v).ok()).map(|v| (k, v))).collect()
}

fn write_file(values: &Values) -> Result<()> {
    let path = settings_path();
    let io = |e: std::io::Error| Error::new(ErrorKind::Store, "cannot save settings").with_source(e);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(values)?).map_err(io)?;
    std::fs::rename(&tmp, &path).map_err(io)
}

/// Settings outside the store: the file, or process memory in mock mode.
fn local_values(s: &Session) -> Values {
    if s.options().mock_graph { memory().lock().map(|m| m.clone()).unwrap_or_default() } else { read_file() }
}

fn store_key(key: &str) -> String {
    format!("settings.{key}")
}

/// The current value of `key` (store, then file or memory, then the default).
///
/// # Errors
/// Unknown key.
pub async fn get(s: &Session, key: &str) -> Result<Value> {
    let spec = spec(key).ok_or_else(|| Error::params(format!("no setting `{}`", clip(key))))?;
    let from_store = match s.store() {
        Ok(store) if spec.scope == Scope::Account || s.options().mock_graph => {
            store.setting::<Value>(store_key(key)).await.ok().flatten().and_then(|v| spec.kind.check(&v).ok())
        }
        _ => None,
    };
    if let Some(v) = from_store {
        return Ok(v);
    }
    Ok(local_values(s).remove(key).unwrap_or_else(|| spec.kind.default_value()))
}

/// Every setting's current value.
pub async fn all(s: &Session) -> Map<String, Value> {
    let mut out = Map::new();
    for spec in SPECS {
        if let Ok(v) = get(s, spec.key).await {
            out.insert(spec.key.to_owned(), v);
        }
    }
    out
}

/// Sets `key` to `value` after validation and announces the change.
///
/// # Errors
/// Unknown key, invalid value, or the value could not be saved.
pub async fn set(s: &Session, key: &str, value: &Value) -> Result<Value> {
    let spec = spec(key).ok_or_else(|| Error::params(format!("no setting `{}`", clip(key))))?;
    let value = spec.kind.check(value).map_err(|m| Error::params(format!("{key}: {m}")))?;
    let store = s.store().ok();
    let mock = s.options().mock_graph;
    match (store, spec.scope, mock) {
        (Some(store), _, true) | (Some(store), Scope::Account, false) => {
            store
                .set_setting(store_key(key), value.clone())
                .await
                .map_err(|e| Error::new(ErrorKind::Store, "cannot save the setting").with_source(e))?;
        }
        (None, _, true) => {
            if let Ok(mut m) = memory().lock() {
                m.insert(key.to_owned(), value.clone());
            }
        }
        (_, _, false) => {
            let mut values = read_file();
            values.insert(key.to_owned(), value.clone());
            write_file(&values)?;
        }
    }
    if key == "presence.keep_alive"
        && let (Ok(sync), Some(on)) = (s.sync(), value.as_bool())
    {
        sync.set_keepalive(on);
    }
    s.emit(CoreEvent::StoreChanged { tables: vec![SETTINGS_TABLE.to_owned()] });
    Ok(json!({ "key": key, "value": value }))
}

fn clip(s: &str) -> String {
    s.chars().take(64).filter(|c| !c.is_control()).collect()
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "settings.set",
            label: "Change a Setting",
            menu: None,
            shortcut: None,
            params_doc: r#"{"key": string, "value": bool|int|string} (keys: query settings.schema)"#,
            destructive: false,
            enabled: always,
            run: set_cmd,
        },
        CommandSpec {
            id: "settings.reset",
            label: "Reset a Setting",
            menu: None,
            shortcut: None,
            params_doc: r#"{"key": string}"#,
            destructive: false,
            enabled: always,
            run: reset_cmd,
        },
        CommandSpec {
            id: "app.open_url",
            label: "Open Link in Browser",
            menu: None,
            shortcut: None,
            params_doc: r#"{"url": string} (http, https or mailto only)"#,
            destructive: false,
            enabled: always,
            run: open_url_cmd,
        },
        CommandSpec {
            id: "link.confirm",
            label: "Check a Link",
            menu: None,
            shortcut: None,
            params_doc: r#"{"url": string} -> {"url", "scheme", "host", "allowed"} (no side effects)"#,
            destructive: false,
            enabled: always,
            run: link_confirm_cmd,
        },
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "settings.get",
            label: "Settings",
            menu: None,
            shortcut: None,
            params_doc: r#"{"key"?: string} -> {"key","value"}, or every key's value without "key""#,
            destructive: false,
            enabled: always,
            run: get_query,
        },
        CommandSpec {
            id: "settings.schema",
            label: "Setting Definitions",
            menu: None,
            shortcut: None,
            params_doc: "{} -> [{key, label, section, scope, type, default, min/max/choices}]",
            destructive: false,
            enabled: always,
            run: schema_query,
        },
        CommandSpec {
            id: "blob.get",
            label: "Cached File Bytes",
            menu: None,
            shortcut: None,
            params_doc: r#"{"id": blob key | "<message id>/<attachment id>"} -> {"id","size","content_type"?,"base64"}"#,
            destructive: false,
            enabled: always,
            run: blob_query,
        },
    ]
}

#[derive(Deserialize)]
struct KeyParams {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Value,
}

fn get_query(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: KeyParams = parse(p)?;
        match p.key {
            Some(key) => Ok(json!({ "key": key, "value": get(s, &key).await? })),
            None => Ok(Value::Object(all(s).await)),
        }
    })
}

fn schema_query(_s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        Ok(Value::Array(
            SPECS
                .iter()
                .map(|sp| {
                    let mut v = sp.kind.describe();
                    if let Some(o) = v.as_object_mut() {
                        o.insert("key".into(), json!(sp.key));
                        o.insert("label".into(), json!(sp.label));
                        o.insert("section".into(), json!(sp.section));
                        let scope = if sp.scope == Scope::Account { "account" } else { "device" };
                        o.insert("scope".into(), json!(scope));
                    }
                    v
                })
                .collect(),
        ))
    })
}

fn set_cmd(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: KeyParams = parse(p)?;
        let key = p.key.ok_or_else(|| Error::params("missing `key`"))?;
        set(s, &key, &p.value).await
    })
}

fn reset_cmd(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: KeyParams = parse(p)?;
        let key = p.key.ok_or_else(|| Error::params("missing `key`"))?;
        let spec = spec(&key).ok_or_else(|| Error::params(format!("no setting `{}`", clip(&key))))?;
        set(s, &key, &spec.kind.default_value()).await
    })
}

#[derive(Deserialize)]
struct IdParams {
    id: String,
}

/// Largest blob returned over IPC, in bytes (bigger attachments are opened as files).
const MAX_BLOB: usize = 25 * 1024 * 1024;

fn blob_query(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: IdParams = parse(p)?;
        let (bytes, content_type) = blob_bytes(s, &p.id).await?;
        if bytes.len() > MAX_BLOB {
            return Err(Error::new(ErrorKind::NotAvailable, "the file is too large to return inline"));
        }
        Ok(json!({
            "id": p.id,
            "size": bytes.len(),
            "content_type": content_type,
            "base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }))
    })
}

/// Resolves a blob key or a `<message id>/<attachment id>` token to cached bytes and the
/// attachment's declared content type (untrusted).
///
/// # Errors
/// Not signed in, malformed id, or nothing cached under it.
pub async fn blob_bytes(s: &Session, id: &str) -> Result<(Vec<u8>, Option<String>)> {
    if id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control) {
        return Err(Error::params("not a valid blob id"));
    }
    let store = s.store()?;
    let store_err = |e: quorum_store::Error| Error::new(ErrorKind::Store, "cannot read the cache").with_source(e);
    let not_found = || Error::new(ErrorKind::NotFound, "not in the local cache");
    let (key, content_type) = match id.split_once('/') {
        Some((msg, att)) => {
            let msg = quorum_model::MessageId::new(msg).map_err(|_| Error::params("not a valid message id"))?;
            let page = store.attachments(msg).await.map_err(store_err)?;
            let found = page.items.into_iter().find(|a| a.attachment.id.as_str() == att).ok_or_else(not_found)?;
            (found.blob_key.ok_or_else(not_found)?, found.attachment.content_type)
        }
        None => (id.to_owned(), None),
    };
    let bytes = store.get_blob(key).await.map_err(store_err)?.ok_or_else(not_found)?;
    Ok((bytes, content_type))
}

#[derive(Deserialize)]
struct UrlParams {
    url: String,
}

/// A link the user may open: its scheme and (for http/https) host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedUrl {
    /// The URL without surrounding spaces.
    pub url: String,
    /// `http`, `https` or `mailto` (lowercase).
    pub scheme: String,
    /// The host for http(s); the address part for mailto.
    pub host: String,
}

/// Validates a URL for opening outside Quorum: `http`, `https` or `mailto` only, no control
/// characters, bounded length.
///
/// # Errors
/// [`ErrorKind::InvalidParams`] with a user-safe reason.
pub fn check_url(url: &str) -> Result<CheckedUrl> {
    if url.chars().any(char::is_control) {
        return Err(Error::params("the link contains control characters"));
    }
    let url = url.trim_matches(' ');
    if url.is_empty() || url.len() > MAX_URL_LEN {
        return Err(Error::params("the link is empty or too long"));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(Error::params("the link contains spaces or control characters"));
    }
    let (scheme, rest) = url.split_once(':').ok_or_else(|| Error::params("the link has no scheme"))?;
    let scheme = scheme.to_ascii_lowercase();
    let host = match scheme.as_str() {
        "http" | "https" => {
            let after = rest.strip_prefix("//").ok_or_else(|| Error::params("the link has no host"))?;
            let authority = after.split(['/', '?', '#']).next().unwrap_or_default();
            if authority.contains('@') {
                return Err(Error::params("links with a user name are not opened"));
            }
            let host = authority
                .rsplit_once(':')
                .map_or(authority, |(h, port)| if port.chars().all(|c| c.is_ascii_digit()) { h } else { authority });
            if host.is_empty() {
                return Err(Error::params("the link has no host"));
            }
            host.to_ascii_lowercase()
        }
        "mailto" => rest.split('?').next().unwrap_or_default().to_owned(),
        _ => return Err(Error::params("only http, https and mailto links can be opened")),
    };
    Ok(CheckedUrl { url: url.to_owned(), scheme, host })
}

fn link_confirm_cmd(_s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: UrlParams = parse(p)?;
        Ok(match check_url(&p.url) {
            Ok(c) => json!({ "url": c.url, "scheme": c.scheme, "host": c.host, "allowed": true }),
            Err(e) => json!({ "url": clip(&p.url), "allowed": false, "reason": e.message() }),
        })
    })
}

fn open_url_cmd(_s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: UrlParams = parse(p)?;
        let checked = check_url(&p.url)?;
        open_system(&checked.url)?;
        Ok(json!({ "opened": true, "scheme": checked.scheme }))
    })
}

/// Hands a checked URL to the operating system (no shell; the URL is one argument).
fn open_system(url: &str) -> Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/open")
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
        .map(drop)
        .map_err(|e| Error::new(ErrorKind::NotAvailable, "cannot open the system browser").with_source(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spec_default_passes_its_own_check() {
        for s in SPECS {
            assert!(s.kind.check(&s.kind.default_value()).is_ok(), "{}", s.key);
        }
        let mut keys: Vec<_> = SPECS.iter().map(|s| s.key).collect();
        keys.sort_unstable();
        let n = keys.len();
        keys.dedup();
        assert_eq!(n, keys.len(), "duplicate setting keys");
    }

    #[test]
    fn scopes_follow_prefixes() {
        assert_eq!(spec("mail.undo_send_seconds").map(|s| s.scope), Some(Scope::Account));
        assert_eq!(spec("presence.keep_alive").map(|s| s.scope), Some(Scope::Account));
        assert_eq!(spec("appearance.theme").map(|s| s.scope), Some(Scope::Device));
    }

    #[test]
    fn checks_reject_wrong_types_and_ranges() {
        let undo = spec("mail.undo_send_seconds").map(|s| s.kind);
        assert!(undo.is_some_and(|k| k.check(&json!(31)).is_err()));
        assert!(undo.is_some_and(|k| k.check(&json!(-1)).is_err()));
        assert!(undo.is_some_and(|k| k.check(&json!("5")).is_err()));
        assert!(undo.is_some_and(|k| k.check(&json!(1.5)).is_err()));
        assert!(undo.is_some_and(|k| k.check(&json!(10)).is_ok()));
        let theme = spec("appearance.theme").map(|s| s.kind);
        assert!(theme.is_some_and(|k| k.check(&json!("dark")).is_ok()));
        assert!(theme.is_some_and(|k| k.check(&json!("DARK")).is_err()));
        assert!(spec("nope").is_none());
    }

    #[test]
    fn url_check_allows_only_safe_schemes() {
        assert!(check_url("https://example.com/a?b#c").is_ok_and(|c| c.host == "example.com"));
        assert!(check_url("HTTP://Example.COM:8080/").is_ok_and(|c| c.host == "example.com" && c.scheme == "http"));
        assert!(check_url("mailto:a@example.com?subject=x").is_ok_and(|c| c.host == "a@example.com"));
        for bad in [
            "",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "quorum-link:x",
            "https://user@example.com/",
            "https://exa mple.com",
            "https://example.com/\n",
            "https:example.com",
            "https://",
            "data:text/html,hi",
            "-a",
        ] {
            assert!(check_url(bad).is_err(), "{bad:?}");
        }
        assert!(check_url(&format!("https://example.com/{}", "a".repeat(MAX_URL_LEN))).is_err());
    }
}
