//! Wire types shared by the Quorum UI and core over Tauri IPC.
//!
//! The UI talks to the core through exactly three surfaces:
//! - [`RunRequest`]: run a command by id with JSON params (`invoke("run", ...)`).
//! - [`QueryRequest`]: read store-backed data by query id (`invoke("query", ...)`).
//! - [`CoreEvent`]: events pushed from the core (`store-changed`, toasts, sign-in state).
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

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Name of the Tauri event channel carrying [`CoreEvent`]s.
pub const EVENT_CHANNEL: &str = "quorum://event";

/// Run a command by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    /// Dotted command id, e.g. `mail.reply`.
    pub id: String,
    /// JSON parameters; `null` means defaults.
    #[serde(default)]
    pub params: Value,
}

/// Read data by query id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    /// Dotted query id, e.g. `mail.list`.
    pub id: String,
    /// JSON parameters; `null` means defaults.
    #[serde(default)]
    pub params: Value,
}

/// Result of a command or query as seen by the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    /// The call succeeded.
    Ok {
        /// Result value.
        value: Value,
    },
    /// The call failed; `message` is safe to show to the user.
    Err {
        /// Machine-readable kind, e.g. `not_found`.
        kind: String,
        /// Human-readable message without personal data.
        message: String,
    },
}

/// Summary of one registered command, for menus and the palette.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandInfo {
    /// Dotted command id.
    pub id: String,
    /// Label shown in menus and the palette.
    pub label: String,
    /// Menu path such as `Mail/Reply`, if the command appears in a menu.
    pub menu: Option<String>,
    /// Keyboard shortcut such as `Cmd+R`.
    pub shortcut: Option<String>,
    /// Whether the command sends, posts, deletes or shares.
    pub destructive: bool,
    /// Whether the command can run right now.
    pub enabled: bool,
}

/// Event pushed from the core to the UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent {
    /// Rows changed in the store; queries over `tables` should refresh.
    StoreChanged {
        /// Changed table names.
        tables: Vec<String>,
    },
    /// Show a transient message.
    Toast {
        /// `info`, `success`, `warning` or `error`.
        level: String,
        /// Text to show.
        message: String,
    },
    /// The UI should perform a UI-only command (e.g. open the palette).
    Ui {
        /// UI command id, e.g. `palette.open`.
        command: String,
        /// Parameters.
        params: Value,
    },
    /// Account sign-in state changed.
    Account {
        /// `signed_out`, `signing_in`, `signed_in` or `needs_reauth`.
        state: String,
    },
}

/// A UI-only command: handled entirely inside the UI (switching areas, focusing panes, the
/// palette). Listed here, in the crate both sides share, so the engine can register a
/// forwarding command for each one that emits [`CoreEvent::Ui`]; that makes every UI action
/// reachable from MCP, the control channel and the CLI as well as from the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiCommandSpec {
    /// Dotted command id, e.g. `view.mail`.
    pub id: &'static str,
    /// Label shown in menus and the palette.
    pub label: &'static str,
    /// Menu path, if the command appears in a menu.
    pub menu: Option<&'static str>,
    /// Keyboard shortcut such as `Cmd+1`.
    pub shortcut: Option<&'static str>,
}

impl UiCommandSpec {
    /// Describes the command like an engine command (UI commands are never destructive).
    pub fn info(&self) -> CommandInfo {
        CommandInfo {
            id: self.id.to_owned(),
            label: self.label.to_owned(),
            menu: self.menu.map(str::to_owned),
            shortcut: self.shortcut.map(str::to_owned),
            destructive: false,
            enabled: true,
        }
    }
}

const fn ui(
    id: &'static str,
    label: &'static str,
    menu: &'static str,
    shortcut: Option<&'static str>,
) -> UiCommandSpec {
    UiCommandSpec { id, label, menu: Some(menu), shortcut }
}

/// Every UI-only command. The UI implements them (`crates/ui/src/commands.rs`); the engine may
/// forward them as [`CoreEvent::Ui`] with the caller's params.
pub const UI_COMMANDS: &[UiCommandSpec] = &[
    ui("palette.open", "Command Palette", "View/Command Palette", Some("Cmd+K")),
    ui("view.mail", "Go to Mail", "View/Mail", Some("Cmd+1")),
    ui("view.calendar", "Go to Calendar", "View/Calendar", Some("Cmd+2")),
    ui("view.chat", "Go to Chat", "View/Chat", Some("Cmd+3")),
    ui("view.teams", "Go to Teams", "View/Teams", Some("Cmd+4")),
    ui("view.files", "Go to Files", "View/Files", Some("Cmd+5")),
    ui("view.people", "Go to People", "View/People", Some("Cmd+6")),
    ui("view.search", "Search", "View/Search", Some("Cmd+E")),
    ui("view.settings", "Settings", "Quorum/Settings", Some("Cmd+,")),
    ui("view.pane.next", "Focus Next Pane", "View/Next Pane", Some("F6")),
    ui("view.pane.previous", "Focus Previous Pane", "View/Previous Pane", Some("Shift+F6")),
    ui("view.list.next", "Next Item", "View/Next Item", Some("J")),
    ui("view.list.previous", "Previous Item", "View/Previous Item", Some("K")),
    ui("view.sidebar.toggle", "Toggle Folder Pane", "View/Toggle Folder Pane", Some("Cmd+Shift+B")),
    ui("view.escape", "Close or Go Back", "View/Close", Some("Escape")),
    // The open mail conversation (no-ops elsewhere).
    ui("view.thread.next", "Next Message in Conversation", "View/Next Message in Conversation", Some("N")),
    ui("view.thread.previous", "Previous Message in Conversation", "View/Previous Message in Conversation", Some("P")),
    ui("view.thread.expand_all", "Expand Conversation", "View/Expand Conversation", Some("Shift+E")),
    ui("view.thread.collapse_all", "Collapse Conversation", "View/Collapse Conversation", Some("Shift+C")),
    ui("view.thread.quoted", "Show or Hide Quoted Text", "View/Show Quoted Text", None),
    ui("view.thread.order", "Newest or Oldest on Top", "View/Conversation Order", None),
    ui("view.thread.reply", "Quick Reply", "View/Quick Reply", None),
    // Params: {"area": "chat", "id": "<item id>", "nav"?: "<folder/team id>", "team"?: ...}.
    // Emitted by engine commands such as `chat.open`; handled only in the UI, never re-run.
    ui("view.open", "Open Item", "View/Open Item", None),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_command_ids_are_unique_and_dotted() {
        let mut ids: Vec<&str> = UI_COMMANDS.iter().map(|c| c.id).collect();
        assert!(ids.iter().all(|id| id.contains('.')));
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len());
    }

    #[test]
    fn ui_command_info_is_never_destructive() {
        assert!(UI_COMMANDS.iter().all(|c| !c.info().destructive && c.info().enabled));
    }
}
