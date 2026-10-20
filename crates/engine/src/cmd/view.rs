//! UI-driving commands, so agents and the keyboard can operate everything the mouse can.
//!
//! These commands do not change data. They ask the UI to navigate (`CoreEvent::Ui`), and the
//! UI reports what is on screen back with `ui.report` so agents can read it with `ui.state`.

use quorum_ipc::CoreEvent;
use serde_json::{Value, json};

use crate::command::{CommandFuture, CommandSpec, always};
use crate::error::Error;
use crate::session::Session;

/// Largest UI state report accepted, in bytes of JSON.
const MAX_UI_STATE: usize = 256 * 1024;

pub(crate) fn commands() -> Vec<CommandSpec> {
    let mut v: Vec<CommandSpec> = quorum_ipc::UI_COMMANDS
        .iter()
        .map(|u| CommandSpec {
            id: u.id,
            label: u.label,
            menu: u.menu,
            shortcut: u.shortcut,
            params_doc: "{} (forwarded to the UI)",
            destructive: false,
            enabled: always,
            run: forward,
        })
        .collect();
    v.push(CommandSpec {
        id: "selection.set",
        label: "Select Items",
        menu: None,
        shortcut: None,
        params_doc: r#"{"ids": [string]}"#,
        destructive: false,
        enabled: always,
        run: forward,
    });
    v.push(CommandSpec {
        id: "ui.report",
        label: "Report UI State",
        menu: None,
        shortcut: None,
        params_doc: r#"{"area": string, "selection": [string], "focus": string, ...} (sent by the UI)"#,
        destructive: false,
        enabled: always,
        run: report,
    });
    v
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "ui.state",
        label: "What Is On Screen",
        menu: None,
        shortcut: None,
        params_doc: "{}",
        destructive: false,
        enabled: always,
        run: state,
    }]
}

/// Forwards the command to the UI as a [`CoreEvent::Ui`].
fn forward<'a>(s: &'a Session, p: Value) -> CommandFuture<'a> {
    Box::pin(async move {
        // The id is recovered from the registry by the caller; we carry it via the event.
        let command = s.current_command().unwrap_or_default();
        let params = if p.is_null() { json!({}) } else { p };
        if !params.is_object() {
            return Err(Error::params("parameters must be an object"));
        }
        s.emit(CoreEvent::Ui { command, params });
        Ok(Value::Null)
    })
}

fn report(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        if !p.is_object() {
            return Err(Error::params("ui state must be an object"));
        }
        if p.to_string().len() > MAX_UI_STATE {
            return Err(Error::params("ui state is too large"));
        }
        super::sync::follow_focus(s, &p);
        s.set_ui_state(p);
        Ok(Value::Null)
    })
}

fn state(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move { Ok(s.ui_state()) })
}
