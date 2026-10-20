//! Command specifications and the registry.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

/// A boxed command future.
pub type CommandFuture<'a> = Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;

/// The function a command runs: JSON params in, JSON value out.
pub type RunFn = for<'a> fn(&'a Session, Value) -> CommandFuture<'a>;

/// Whether a command can run in the current session.
pub type EnabledFn = fn(&Session) -> bool;

/// One user-visible action.
///
/// Menus, shortcuts, the palette, the CLI, the control channel and MCP all reach commands
/// through [`Session::execute`]. Programmatic calls never open dialogs.
#[derive(Clone, Copy)]
pub struct CommandSpec {
    /// Dotted id, e.g. `mail.reply`.
    pub id: &'static str,
    /// Label for menus and the palette.
    pub label: &'static str,
    /// Menu path, e.g. `Mail/Reply`.
    pub menu: Option<&'static str>,
    /// Shortcut, e.g. `Cmd+R`.
    pub shortcut: Option<&'static str>,
    /// Parameter documentation (JSON shape, defaults).
    pub params_doc: &'static str,
    /// Sends, posts, deletes or shares; MCP callers must confirm.
    pub destructive: bool,
    /// Whether the command can run now.
    pub enabled: EnabledFn,
    /// The implementation.
    pub run: RunFn,
}

impl std::fmt::Debug for CommandSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandSpec").field("id", &self.id).finish_non_exhaustive()
    }
}

/// Always enabled.
pub fn always(_: &Session) -> bool {
    true
}

/// All registered commands, by id.
#[derive(Debug, Default)]
pub struct Registry {
    commands: BTreeMap<&'static str, CommandSpec>,
}

impl Registry {
    /// Builds the registry with every built-in command.
    pub fn builtin() -> Self {
        let mut r = Self::default();
        for spec in crate::cmd::all() {
            r.insert(spec);
        }
        r
    }

    /// Builds the registry of built-in queries (read-only specs; `destructive` is always false).
    pub fn builtin_queries() -> Self {
        let mut r = Self::default();
        for spec in crate::cmd::queries() {
            r.insert(spec);
        }
        r
    }

    /// Adds or replaces a command.
    pub fn insert(&mut self, spec: CommandSpec) {
        self.commands.insert(spec.id, spec);
    }

    /// Looks up a command.
    pub fn get(&self, id: &str) -> Result<&CommandSpec> {
        self.commands.get(id).ok_or_else(|| Error::new(ErrorKind::UnknownCommand, format!("no command `{id}`")))
    }

    /// Iterates commands in id order.
    pub fn iter(&self) -> impl Iterator<Item = &CommandSpec> {
        self.commands.values()
    }

    /// Number of commands.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// Deserialises params, treating `null` as an empty object so `#[serde(default)]` applies.
pub fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T> {
    let params = if params.is_null() { Value::Object(serde_json::Map::new()) } else { params };
    Ok(serde_json::from_value(params)?)
}
