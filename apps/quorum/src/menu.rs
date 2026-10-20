//! The native menu bar, built from the command registry's menu paths (`Quorum/…`, `File/…`,
//! `Edit/…`, `View/…`, `Mail/…`, …). Selecting an item sends the command to the UI as a
//! `CoreEvent::Ui`, exactly like its keyboard shortcut, so the menu bar is a keyboard- and
//! VoiceOver-accessible way to reach every command.

use std::collections::BTreeMap;

use quorum_ipc::CommandInfo;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Wry};

/// Order of the top-level menus; others follow alphabetically, then Window and Help.
const TOP_ORDER: &[&str] = &["Quorum", "File", "Edit", "View", "Mail", "Calendar", "Chat", "Teams", "Files", "People"];

/// Prefix of menu item ids that carry a command id.
pub const CMD_PREFIX: &str = "cmd:";

/// A menu node: a command item or a submenu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A command: id, label, accelerator.
    Item { id: String, label: String, accelerator: Option<String> },
    /// A submenu with its children.
    Sub { label: String, children: Vec<Node> },
}

/// Groups commands by menu path into ordered top-level menus (pure, tested).
///
/// The last path segment is the item label. Accelerators are only assigned to shortcuts that
/// have a Cmd/Ctrl modifier and belong to exactly one command (area-specific shortcuts such as
/// `Cmd+N` stay with the UI keymap, which knows the current area).
pub fn tree(commands: &[CommandInfo]) -> Vec<(String, Vec<Node>)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for c in commands {
        if let Some(s) = &c.shortcut {
            *counts.entry(s.to_ascii_lowercase()).or_default() += 1;
        }
    }
    let mut tops: Vec<(String, Vec<Node>)> = Vec::new();
    for c in commands.iter().take(2000) {
        let Some(path) = c.menu.as_deref() else { continue };
        let parts: Vec<&str> = path.split('/').map(str::trim).filter(|p| !p.is_empty()).take(4).collect();
        let Some((label, dirs)) = parts.split_last() else { continue };
        let Some((top, subs)) = dirs.split_first() else { continue };
        let accelerator = c
            .shortcut
            .as_deref()
            .filter(|s| counts.get(&s.to_ascii_lowercase()).copied() == Some(1))
            .and_then(accelerator);
        let item = Node::Item { id: c.id.clone(), label: (*label).to_owned(), accelerator };
        let idx = match tops.iter().position(|(t, _)| t == top) {
            Some(i) => i,
            None => {
                tops.push(((*top).to_owned(), Vec::new()));
                tops.len().saturating_sub(1)
            }
        };
        if let Some((_, children)) = tops.get_mut(idx) {
            insert(children, subs, item);
        }
    }
    tops.sort_by_key(|(t, _)| (TOP_ORDER.iter().position(|o| o == t).unwrap_or(TOP_ORDER.len()), t.clone()));
    tops
}

fn insert(children: &mut Vec<Node>, subs: &[&str], item: Node) {
    let Some((first, rest)) = subs.split_first() else {
        children.push(item);
        return;
    };
    let pos = children.iter().position(|n| matches!(n, Node::Sub { label, .. } if label == first));
    let pos = match pos {
        Some(p) => p,
        None => {
            children.push(Node::Sub { label: (*first).to_owned(), children: Vec::new() });
            children.len().saturating_sub(1)
        }
    };
    if let Some(Node::Sub { children, .. }) = children.get_mut(pos) {
        insert(children, rest, item);
    }
}

/// Converts a registry shortcut (`Cmd+Shift+R`) to a Tauri accelerator (`CmdOrCtrl+Shift+R`);
/// `None` without a Cmd/Ctrl modifier (single keys must keep reaching text fields).
pub fn accelerator(shortcut: &str) -> Option<String> {
    let mut has_cmd = false;
    let mut parts = Vec::new();
    for p in shortcut.split('+').map(str::trim) {
        match p.to_ascii_lowercase().as_str() {
            "cmd" | "ctrl" | "mod" | "cmdorctrl" => {
                has_cmd = true;
                parts.push("CmdOrCtrl".to_owned());
            }
            "shift" => parts.push("Shift".to_owned()),
            "alt" | "option" | "opt" => parts.push("Alt".to_owned()),
            "" => return None,
            "backspace" => parts.push("Backspace".to_owned()),
            "enter" | "return" => parts.push("Enter".to_owned()),
            "escape" | "esc" => parts.push("Escape".to_owned()),
            "," => parts.push("Comma".to_owned()),
            key if key.len() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric()) => {
                parts.push(key.to_ascii_uppercase());
            }
            key if key.starts_with('f') && key.len() <= 3 && key.get(1..).is_some_and(|n| n.parse::<u8>().is_ok()) => {
                parts.push(key.to_ascii_uppercase());
            }
            _ => return None,
        }
    }
    has_cmd.then(|| parts.join("+"))
}

fn build_nodes(app: &AppHandle, nodes: &[Node]) -> tauri::Result<Vec<Box<dyn tauri::menu::IsMenuItem<Wry>>>> {
    let mut out: Vec<Box<dyn tauri::menu::IsMenuItem<Wry>>> = Vec::new();
    for n in nodes {
        match n {
            Node::Item { id, label, accelerator } => {
                let item = MenuItem::with_id(app, format!("{CMD_PREFIX}{id}"), label, true, accelerator.as_deref())
                    .or_else(|_| MenuItem::with_id(app, format!("{CMD_PREFIX}{id}"), label, true, None::<&str>))?;
                out.push(Box::new(item));
            }
            Node::Sub { label, children } => {
                let kids = build_nodes(app, children)?;
                let refs: Vec<&dyn tauri::menu::IsMenuItem<Wry>> = kids.iter().map(|b| &**b).collect();
                out.push(Box::new(Submenu::with_items(app, label, true, &refs)?));
            }
        }
    }
    Ok(out)
}

/// Builds the menu bar from `commands`, with the platform's standard items (Edit's
/// cut/copy/paste, Hide, Quit, Window) so text fields and window management work.
///
/// # Errors
/// The platform menu could not be created.
pub fn build(app: &AppHandle, commands: &[CommandInfo]) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    let mut tops = tree(commands);
    for name in ["Quorum", "Edit", "View"] {
        if !tops.iter().any(|(t, _)| t == name) {
            tops.push((name.to_owned(), Vec::new()));
        }
    }
    tops.sort_by_key(|(t, _)| (TOP_ORDER.iter().position(|o| o == t).unwrap_or(TOP_ORDER.len()), t.clone()));
    for (top, nodes) in &tops {
        let items = build_nodes(app, nodes)?;
        let mut refs: Vec<&dyn tauri::menu::IsMenuItem<Wry>> = items.iter().map(|b| &**b).collect();
        let sep = PredefinedMenuItem::separator(app)?;
        let extra: Vec<PredefinedMenuItem<Wry>> = match top.as_str() {
            "Quorum" => vec![
                PredefinedMenuItem::services(app, None)?,
                PredefinedMenuItem::hide(app, None)?,
                PredefinedMenuItem::hide_others(app, None)?,
                PredefinedMenuItem::show_all(app, None)?,
                PredefinedMenuItem::quit(app, None)?,
            ],
            "Edit" => vec![
                PredefinedMenuItem::undo(app, None)?,
                PredefinedMenuItem::redo(app, None)?,
                PredefinedMenuItem::cut(app, None)?,
                PredefinedMenuItem::copy(app, None)?,
                PredefinedMenuItem::paste(app, None)?,
                PredefinedMenuItem::select_all(app, None)?,
            ],
            "View" => vec![PredefinedMenuItem::fullscreen(app, None)?],
            _ => Vec::new(),
        };
        if !extra.is_empty() {
            if !refs.is_empty() {
                refs.push(&sep);
            }
            refs.extend(extra.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<Wry>));
        }
        if refs.is_empty() {
            continue;
        }
        menu.append(&Submenu::with_items(app, top, true, &refs)?)?;
    }
    let window = Submenu::with_items(
        app,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            &PredefinedMenuItem::close_window(app, None)?,
        ],
    )?;
    menu.append(&window)?;
    Ok(menu)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(id: &str, menu: Option<&str>, shortcut: Option<&str>) -> CommandInfo {
        CommandInfo {
            id: id.into(),
            label: id.into(),
            menu: menu.map(Into::into),
            shortcut: shortcut.map(Into::into),
            destructive: false,
            enabled: true,
        }
    }

    #[test]
    fn tree_groups_orders_and_nests() {
        let t = tree(&[
            cmd("mail.reply", Some("Mail/Reply"), Some("Cmd+R")),
            cmd("app.about", Some("Quorum/About Quorum"), None),
            cmd("account.sign_in", Some("Quorum/Account/Sign In"), None),
            cmd("mail.compose", Some("Mail/New Message"), Some("Cmd+N")),
            cmd("chat.new", Some("Chat/New Chat"), Some("Cmd+N")),
            cmd("x.hidden", None, Some("Cmd+X")),
            cmd("bad", Some("/"), None),
            cmd("bad2", Some("Single"), None),
        ]);
        let names: Vec<&str> = t.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Quorum", "Mail", "Chat"]);
        let quorum = &t.first().map(|(_, n)| n.clone()).unwrap_or_default();
        assert!(
            matches!(quorum.get(1), Some(Node::Sub { label, children }) if label == "Account" && children.len() == 1)
        );
        let mail = t.get(1).map(|(_, n)| n.clone()).unwrap_or_default();
        assert_eq!(
            mail.first(),
            Some(&Node::Item {
                id: "mail.reply".into(),
                label: "Reply".into(),
                accelerator: Some("CmdOrCtrl+R".into())
            })
        );
        // Cmd+N is shared by two areas: no accelerator.
        assert!(matches!(mail.get(1), Some(Node::Item { accelerator: None, .. })));
    }

    #[test]
    fn accelerators_need_a_command_modifier() {
        assert_eq!(accelerator("Cmd+Shift+R"), Some("CmdOrCtrl+Shift+R".into()));
        assert_eq!(accelerator("Cmd+,"), Some("CmdOrCtrl+Comma".into()));
        assert_eq!(accelerator("Cmd+Backspace"), Some("CmdOrCtrl+Backspace".into()));
        assert_eq!(accelerator("J"), None);
        assert_eq!(accelerator("F6"), None);
        assert_eq!(accelerator("Escape"), None);
        assert_eq!(accelerator("Cmd++"), None);
        assert_eq!(accelerator("Cmd+Weird"), None);
        assert_eq!(accelerator(""), None);
    }
}
