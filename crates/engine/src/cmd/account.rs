//! Account commands: configure the tenant, sign in and out, read the account and its scopes.

use quorum_ipc::CoreEvent;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::account::{AccountConfig, REQUIRED_SCOPES};
use crate::command::{CommandFuture, CommandSpec, always, parse};
use crate::error::Error;
use crate::session::Session;

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "account.configure",
            label: "Set Tenant and Client ID",
            menu: Some("Quorum/Account/Configure…"),
            shortcut: None,
            params_doc: r#"{"tenant": string, "client_id": string}"#,
            destructive: false,
            enabled: always,
            run: configure,
        },
        CommandSpec {
            id: "account.sign_in",
            label: "Sign In",
            menu: Some("Quorum/Account/Sign In"),
            shortcut: None,
            params_doc: "{} (opens the system browser)",
            destructive: false,
            enabled: always,
            run: sign_in,
        },
        CommandSpec {
            id: "account.sign_out",
            label: "Sign Out",
            menu: Some("Quorum/Account/Sign Out"),
            shortcut: None,
            params_doc: "{}",
            destructive: false,
            enabled: always,
            run: sign_out,
        },
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "account.current",
            label: "Current Account",
            menu: None,
            shortcut: None,
            params_doc: "{}",
            destructive: false,
            enabled: always,
            run: current,
        },
        CommandSpec {
            id: "account.setup",
            label: "Organisation Setup Links",
            menu: None,
            shortcut: None,
            params_doc: r#"{} → {"tenant", "client_id", "admin_consent_url", "setup_link"} (nulls until set up)"#,
            destructive: false,
            enabled: always,
            run: setup,
        },
        CommandSpec {
            id: "account.permissions",
            label: "Granted and Missing Permissions",
            menu: None,
            shortcut: None,
            params_doc: "{}",
            destructive: false,
            enabled: always,
            run: permissions,
        },
    ]
}

#[derive(Deserialize)]
struct ConfigureParams {
    tenant: String,
    client_id: String,
}

fn configure(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ConfigureParams = parse(p)?;
        let ok = |v: &str| {
            !v.is_empty() && v.len() <= 128 && v.chars().all(|c| c.is_ascii_alphanumeric() || "-._".contains(c))
        };
        if !ok(&p.tenant) || !ok(&p.client_id) {
            return Err(Error::params("tenant and client id must be ids like 00000000-0000-0000-0000-000000000000"));
        }
        AccountConfig { tenant: p.tenant, client_id: p.client_id }.save()?;
        s.emit(CoreEvent::Account { state: "signed_out".into() });
        Ok(Value::Null)
    })
}

fn sign_in(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        s.emit(CoreEvent::Account { state: "signing_in".into() });
        let r = s.accounts().sign_in().await;
        let state = if r.is_ok() { "signed_in" } else { "signed_out" };
        s.emit(CoreEvent::Account { state: state.into() });
        r?;
        s.on_signed_in().await;
        Ok(Value::Null)
    })
}

fn sign_out(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        s.stop_runtime();
        s.accounts().sign_out().await?;
        s.emit(CoreEvent::Account { state: "signed_out".into() });
        Ok(Value::Null)
    })
}

fn current(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let state = s.accounts().state().await;
        if s.accounts().is_mock() {
            return Ok(
                json!({ "state": state, "id": "mock.account", "username": "you@contoso.example", "display_name": "Alex Example", "mock": true }),
            );
        }
        let provider = s.accounts().provider().await;
        Ok(match provider {
            Some(p) => {
                let a = p.account();
                json!({ "state": state, "id": a.id(), "username": a.username(), "display_name": a.display_name(), "tenant_id": a.tenant_id() })
            }
            None => json!({ "state": state }),
        })
    })
}

fn setup(_s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let Some(cfg) = AccountConfig::load() else {
            return Ok(json!({ "tenant": null, "client_id": null, "admin_consent_url": null, "setup_link": null }));
        };
        Ok(json!({
            "tenant": cfg.tenant,
            "client_id": cfg.client_id,
            "admin_consent_url": crate::registration::admin_consent_url(&cfg.tenant, &cfg.client_id),
            "setup_link": crate::registration::setup_link(&cfg.tenant, &cfg.client_id),
        }))
    })
}

fn permissions(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        if s.accounts().is_mock() {
            return Ok(json!({ "granted": REQUIRED_SCOPES, "missing": [] }));
        }
        let provider = s.accounts().provider().await;
        let Some(p) = provider else { return Ok(json!({ "granted": [], "missing": REQUIRED_SCOPES })) };
        Ok(json!({ "granted": p.granted_scopes().as_slice(), "missing": p.missing_scopes(REQUIRED_SCOPES) }))
    })
}
