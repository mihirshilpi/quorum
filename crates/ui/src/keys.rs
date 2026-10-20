//! Keyboard shortcuts: parsing the registry's shortcut strings, matching key events, and the
//! global keymap.
//!
//! Shortcuts are data on commands (`CommandInfo::shortcut`, e.g. `Cmd+K`, `Cmd+Shift+B`, `F6`,
//! `J`), so the keymap is built from the engine registry plus the UI commands: everything the
//! keyboard does is a command. `Cmd` is the primary modifier: ⌘ on macOS, Ctrl elsewhere.

use leptos::prelude::*;
use quorum_ipc::CommandInfo;

use crate::state::AppState;

/// A parsed key chord.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Shortcut {
    /// Primary modifier: ⌘ on macOS, Ctrl elsewhere.
    pub primary: bool,
    /// Control (macOS only; elsewhere Ctrl is the primary modifier).
    pub ctrl: bool,
    /// Alt / Option.
    pub alt: bool,
    /// Shift.
    pub shift: bool,
    /// Normalised key: lowercase letter or digit, or a name such as `escape`, `f6`, `arrowup`.
    pub key: String,
}

fn normalise_key(key: &str) -> Option<String> {
    let lower = key.trim().to_ascii_lowercase();
    let name = match lower.as_str() {
        "" => return None,
        "esc" => "escape",
        "return" => "enter",
        "del" => "delete",
        "space" | " " => "space",
        "up" => "arrowup",
        "down" => "arrowdown",
        "left" => "arrowleft",
        "right" => "arrowright",
        "pgup" => "pageup",
        "pgdn" => "pagedown",
        "plus" => "+",
        other => other,
    };
    Some(name.to_owned())
}

impl Shortcut {
    /// Parses a registry shortcut such as `Cmd+Shift+K`, `Ctrl+Alt+Delete`, `Cmd++` or `F6`.
    ///
    /// On macOS `Ctrl` stays Control; elsewhere `Cmd` and `Ctrl` both mean the primary key.
    pub fn parse(spec: &str, mac: bool) -> Option<Self> {
        let spec = spec.trim();
        let (mods, key) = if let Some(rest) = spec.strip_suffix("++") {
            (rest, "+")
        } else if spec == "+" {
            ("", "+")
        } else {
            match spec.rsplit_once('+') {
                Some((m, k)) => (m, k),
                None => ("", spec),
            }
        };
        let mut s = Self { primary: false, ctrl: false, alt: false, shift: false, key: normalise_key(key)? };
        for m in mods.split('+').map(str::trim).filter(|m| !m.is_empty()) {
            match m.to_ascii_lowercase().as_str() {
                "cmd" | "command" | "meta" | "super" | "mod" | "cmdorctrl" | "commandorcontrol" => s.primary = true,
                "ctrl" | "control" => {
                    if mac {
                        s.ctrl = true;
                    } else {
                        s.primary = true;
                    }
                }
                "alt" | "option" | "opt" => s.alt = true,
                "shift" => s.shift = true,
                _ => return None,
            }
        }
        Some(s)
    }

    /// Builds the chord of a key event. `None` for a bare modifier press.
    pub fn from_event_parts(
        key: &str,
        code: &str,
        meta: bool,
        ctrl: bool,
        alt: bool,
        shift: bool,
        mac: bool,
    ) -> Option<Self> {
        if matches!(key, "Meta" | "Control" | "Shift" | "Alt" | "AltGraph" | "OS" | "CapsLock" | "Fn") {
            return None;
        }
        // Prefer the physical code for letters and digits so Shift and Option don't change the key.
        let from_code = code
            .strip_prefix("Key")
            .filter(|k| k.len() == 1)
            .or_else(|| code.strip_prefix("Digit").filter(|k| k.len() == 1))
            .map(str::to_ascii_lowercase);
        let key = match from_code {
            Some(k) => k,
            None => normalise_key(key)?,
        };
        let (primary, ctrl) = if mac { (meta, ctrl) } else { (ctrl, false) };
        Some(Self { primary, ctrl, alt, shift, key })
    }

    /// Whether the key is a function key (`f1`..`f24`), which acts even while typing.
    pub fn is_function_key(&self) -> bool {
        self.key.len() > 1 && self.key.starts_with('f') && self.key.chars().skip(1).all(|c| c.is_ascii_digit())
    }

    /// Whether the chord has no modifier except Shift (must not fire while typing).
    pub fn is_plain(&self) -> bool {
        !(self.primary || self.ctrl || self.alt)
    }

    /// Human-readable form: `⌘⇧K` on macOS, `Ctrl+Shift+K` elsewhere.
    pub fn display(&self, mac: bool) -> String {
        let key = display_key(&self.key, mac);
        if mac {
            let mut out = String::new();
            if self.ctrl {
                out.push('⌃');
            }
            if self.alt {
                out.push('⌥');
            }
            if self.shift {
                out.push('⇧');
            }
            if self.primary {
                out.push('⌘');
            }
            out.push_str(&key);
            out
        } else {
            let mut parts: Vec<String> = Vec::new();
            if self.primary {
                parts.push("Ctrl".into());
            }
            if self.alt {
                parts.push("Alt".into());
            }
            if self.shift {
                parts.push("Shift".into());
            }
            parts.push(key);
            parts.join("+")
        }
    }
}

fn display_key(key: &str, mac: bool) -> String {
    let named = match (key, mac) {
        ("enter", true) => "↩",
        ("escape", true) => "⎋",
        ("backspace", true) => "⌫",
        ("delete", true) => "⌦",
        ("tab", true) => "⇥",
        ("escape", false) => "Esc",
        ("arrowup", _) => "↑",
        ("arrowdown", _) => "↓",
        ("arrowleft", _) => "←",
        ("arrowright", _) => "→",
        ("space", _) => "Space",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_owned();
    }
    let mut chars = key.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Formats a registry shortcut string for display; unparseable strings are shown as written.
pub fn display_shortcut(spec: &str, mac: bool) -> String {
    Shortcut::parse(spec, mac).map(|s| s.display(mac)).unwrap_or_else(|| spec.to_owned())
}

/// Picks the command a chord runs among the commands bound to it.
///
/// Context first: a command whose id starts with one of the current area's prefixes wins
/// (so `Cmd+N` composes mail in Mail and starts a chat in Chat); then an enabled command;
/// then the first one in registry order.
pub fn resolve<'a>(bound: &[&'a CommandInfo], area_prefixes: &[&str]) -> Option<&'a CommandInfo> {
    let in_context = |c: &&&CommandInfo| area_prefixes.iter().any(|p| c.id.starts_with(p));
    bound
        .iter()
        .find(|c| in_context(c) && c.enabled)
        .or_else(|| bound.iter().find(in_context))
        .or_else(|| bound.iter().find(|c| c.enabled))
        .or_else(|| bound.first())
        .copied()
}

/// Keeps only the commands a *plain* chord (no modifier but Shift) may run in the current
/// area: the area's own and global ones. Single-key shortcuts such as `E` (archive) or `R`
/// (reply) belong to their area and must not act on another area's selection. Modified
/// chords keep the cross-area fallback of [`resolve`] (`Cmd+N` from Files composes mail).
pub fn plain_in_context<'a>(bound: Vec<&'a CommandInfo>, area_prefixes: &[&str]) -> Vec<&'a CommandInfo> {
    let owned_by_an_area =
        |id: &str| crate::state::Area::ALL.iter().any(|a| a.command_prefixes().iter().any(|p| id.starts_with(p)));
    bound
        .into_iter()
        .filter(|c| area_prefixes.iter().any(|p| c.id.starts_with(p)) || !owned_by_an_area(&c.id))
        .collect()
}

/// The commands in `commands` bound to `chord`.
pub fn bound_to<'a>(commands: &'a [CommandInfo], chord: &Shortcut, mac: bool) -> Vec<&'a CommandInfo> {
    commands
        .iter()
        .filter(|c| c.shortcut.as_deref().and_then(|s| Shortcut::parse(s, mac)).as_ref() == Some(chord))
        .collect()
}

/// Installs the global keydown listener that turns chords into commands.
pub fn install(state: AppState) {
    let handle = window_event_listener(leptos::ev::keydown, move |e| on_keydown(state, &e));
    // The shell lives as long as the page; keep the listener until the root is disposed.
    on_cleanup(move || handle.remove());
}

fn on_keydown(state: AppState, e: &web_sys::KeyboardEvent) {
    if e.default_prevented() || e.is_composing() {
        return;
    }
    let mac = crate::dom::is_mac();
    let Some(chord) =
        Shortcut::from_event_parts(&e.key(), &e.code(), e.meta_key(), e.ctrl_key(), e.alt_key(), e.shift_key(), mac)
    else {
        return;
    };
    // The palette owns the keyboard while open; it handles its own keys.
    if state.palette_open.get_untracked() {
        return;
    }
    let target = e.target();
    if chord.is_plain() && !chord.is_function_key() && chord.key != "escape" && crate::dom::is_editable(target.as_ref())
    {
        // Typing in a field: only Escape, function keys and modified chords act globally.
        return;
    }
    let commands = state.all_commands();
    let prefixes = state.area.get_untracked().command_prefixes();
    let mut bound = bound_to(&commands, &chord, mac);
    if chord.is_plain() {
        bound = plain_in_context(bound, prefixes);
    }
    let Some(cmd) = resolve(&bound, prefixes) else { return };
    e.prevent_default();
    let id = cmd.id.clone();
    crate::commands::dispatch(state, &id, crate::commands::context_params(state, &id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, shortcut: &str, enabled: bool) -> CommandInfo {
        CommandInfo {
            id: id.into(),
            label: id.into(),
            menu: None,
            shortcut: Some(shortcut.into()),
            destructive: false,
            enabled,
        }
    }

    #[test]
    fn parses_common_shortcuts() {
        let k = Shortcut::parse("Cmd+K", true);
        assert_eq!(k, Some(Shortcut { primary: true, ctrl: false, alt: false, shift: false, key: "k".into() }));
        let s = Shortcut::parse("Cmd+Shift+B", true);
        assert!(s.as_ref().is_some_and(|s| s.primary && s.shift && s.key == "b"));
        assert_eq!(Shortcut::parse("F6", true).map(|s| s.key), Some("f6".into()));
        assert_eq!(Shortcut::parse("Cmd++", true).map(|s| s.key), Some("+".into()));
        assert_eq!(Shortcut::parse("Cmd+,", true).map(|s| s.key), Some(",".into()));
        assert_eq!(Shortcut::parse("Esc", false).map(|s| s.key), Some("escape".into()));
        assert_eq!(Shortcut::parse("Hyper+K", true), None);
        assert_eq!(Shortcut::parse("", true), None);
        assert_eq!(Shortcut::parse("Cmd+", true), None);
    }

    #[test]
    fn ctrl_is_primary_off_mac_and_control_on_mac() {
        let mac = Shortcut::parse("Ctrl+Tab", true);
        assert!(mac.as_ref().is_some_and(|s| s.ctrl && !s.primary));
        let other = Shortcut::parse("Ctrl+Tab", false);
        assert!(other.as_ref().is_some_and(|s| !s.ctrl && s.primary));
        assert_eq!(Shortcut::parse("Cmd+K", false), Shortcut::parse("Ctrl+K", false));
    }

    #[test]
    fn events_match_parsed_shortcuts() {
        let mac_event = Shortcut::from_event_parts("k", "KeyK", true, false, false, false, true);
        assert_eq!(mac_event, Shortcut::parse("Cmd+K", true));
        let win_event = Shortcut::from_event_parts("k", "KeyK", false, true, false, false, false);
        assert_eq!(win_event, Shortcut::parse("Cmd+K", false));
        // Shift+1 reports "!" as key; the physical code keeps it "1".
        let shifted = Shortcut::from_event_parts("!", "Digit1", true, false, false, true, true);
        assert_eq!(shifted, Shortcut::parse("Cmd+Shift+1", true));
        let f6 = Shortcut::from_event_parts("F6", "F6", false, false, false, true, true);
        assert_eq!(f6, Shortcut::parse("Shift+F6", true));
        assert_eq!(Shortcut::from_event_parts("Meta", "MetaLeft", true, false, false, false, true), None);
        let j = Shortcut::from_event_parts("j", "KeyJ", false, false, false, false, true);
        assert!(j.as_ref().is_some_and(Shortcut::is_plain));
        assert!(j.as_ref().is_some_and(|j| !j.is_function_key()));
        assert!(f6.as_ref().is_some_and(Shortcut::is_function_key));
        let f = Shortcut::parse("F", true);
        assert!(f.as_ref().is_some_and(|f| !f.is_function_key()));
    }

    #[test]
    fn displays_per_platform() {
        assert_eq!(display_shortcut("Cmd+Shift+K", true), "⇧⌘K");
        assert_eq!(display_shortcut("Cmd+Shift+K", false), "Ctrl+Shift+K");
        assert_eq!(display_shortcut("Cmd+Enter", true), "⌘↩");
        assert_eq!(display_shortcut("F6", false), "F6");
        assert_eq!(display_shortcut("Cmd+,", true), "⌘,");
        assert_eq!(display_shortcut("Nonsense+X", true), "Nonsense+X");
    }

    #[test]
    fn resolve_prefers_the_current_area() {
        let cmds = vec![
            info("mail.compose", "Cmd+N", true),
            info("chat.new", "Cmd+N", true),
            info("calendar.event.create", "Cmd+N", true),
        ];
        let chord = Shortcut::parse("Cmd+N", true).unwrap_or(Shortcut {
            primary: true,
            ctrl: false,
            alt: false,
            shift: false,
            key: "n".into(),
        });
        let bound = bound_to(&cmds, &chord, true);
        assert_eq!(bound.len(), 3);
        assert_eq!(resolve(&bound, &["chat."]).map(|c| c.id.as_str()), Some("chat.new"));
        assert_eq!(resolve(&bound, &["calendar."]).map(|c| c.id.as_str()), Some("calendar.event.create"));
        assert_eq!(resolve(&bound, &["drive."]).map(|c| c.id.as_str()), Some("mail.compose"));
        assert_eq!(resolve(&[], &["mail."]), None);
    }

    #[test]
    fn plain_keys_stay_in_their_area() {
        let cmds = [info("mail.archive", "E", true), info("view.list.next", "J", true), info("files.x", "E", true)];
        let refs: Vec<&CommandInfo> = cmds.iter().collect();
        let ids = |v: Vec<&CommandInfo>| v.into_iter().map(|c| c.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(plain_in_context(refs.clone(), &["mail."])), ["mail.archive", "view.list.next"]);
        assert_eq!(ids(plain_in_context(refs.clone(), &["calendar."])), ["view.list.next"]);
        assert_eq!(ids(plain_in_context(refs, &["drive.", "files."])), ["view.list.next", "files.x"]);
    }

    #[test]
    fn resolve_prefers_enabled_commands() {
        let cmds = [info("a.x", "Cmd+J", false), info("b.x", "Cmd+J", true)];
        let refs: Vec<&CommandInfo> = cmds.iter().collect();
        assert_eq!(resolve(&refs, &[]).map(|c| c.id.as_str()), Some("b.x"));
    }
}
