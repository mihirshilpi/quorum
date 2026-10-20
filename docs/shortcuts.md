# Keyboard shortcuts

Generated from the command registry (`quorum-cli commands`). Shortcuts are data on commands, so
menus, the palette and the keymap all show the same keys. `Cmd` is ⌘ on macOS and Ctrl elsewhere.

## How keys are resolved

- A chord bound to several commands runs the one in the **current area** first (`Cmd+N` is New mail in Mail,
  New event in Calendar, New chat in Chat, New post in Teams). Area-scoped duplicates across different
  areas are intentional.
- **Plain keys** (no modifier except Shift, e.g. `E`, `R`, `Backspace`) only run commands of the current area
  or global commands; they never act on another area's selection. Modified chords fall back to another area's
  command when the current area has none (e.g. `Cmd+N` in Files composes mail).
- While typing in a field only `Escape`, function keys and modified chords act.
- Lists also take ↑/↓, Home/End, PageUp/PageDown and Enter (not listed below). In a mail conversation ↑/↓ move between
  messages and Enter expands or collapses one; ⌘↩ sends the quick reply. Shortcuts keep working while a message body has focus.

Audit (2026-10-18): no two commands in the same area, and no two global commands, share a key.

| Area | Shortcut | Command | Action |
|---|---|---|---|
| Global | `Cmd+,` | `view.settings` | Settings |
| Global | `Cmd+1` | `view.mail` | Go to Mail |
| Global | `Cmd+2` | `view.calendar` | Go to Calendar |
| Global | `Cmd+3` | `view.chat` | Go to Chat |
| Global | `Cmd+4` | `view.teams` | Go to Teams |
| Global | `Cmd+5` | `view.files` | Go to Files |
| Global | `Cmd+6` | `view.people` | Go to People |
| Global | `Cmd+E` | `view.search` | Search |
| Global | `Cmd+K` | `palette.open` | Command Palette |
| Global | `Cmd+Shift+B` | `view.sidebar.toggle` | Toggle Folder Pane |
| Global | `Escape` | `view.escape` | Close or Go Back |
| Global | `F6` | `view.pane.next` | Focus Next Pane |
| Global | `J` | `view.list.next` | Next Item |
| Global | `K` | `view.list.previous` | Previous Item |
| Global | `N` | `view.thread.next` | Next Message in Conversation |
| Global | `P` | `view.thread.previous` | Previous Message in Conversation |
| Global | `Shift+C` | `view.thread.collapse_all` | Collapse Conversation |
| Global | `Shift+E` | `view.thread.expand_all` | Expand Conversation |
| Global | `Shift+F6` | `view.pane.previous` | Focus Previous Pane |
| Mail | `Backspace` | `mail.delete` | Delete |
| Mail | `C` | `mail.categorize` | Categorize |
| Mail | `Cmd+Enter` | `mail.send` | Send |
| Mail | `Cmd+N` | `mail.compose` | New Message |
| Mail | `Cmd+S` | `mail.save_draft` | Save Draft |
| Mail | `Cmd+Shift+G` | `mail.flag` | Flag |
| Mail | `E` | `mail.archive` | Archive |
| Mail | `Q` | `mail.mark_read` | Mark as Read |
| Mail | `R` | `mail.reply` | Reply |
| Mail | `Shift+F` | `mail.forward` | Forward |
| Mail | `Shift+R` | `mail.reply_all` | Reply All |
| Mail | `U` | `mail.mark_unread` | Mark as Unread |
| Mail | `V` | `mail.move` | Move to Folder |
| Calendar | `Cmd+Alt+1` | `calendar.view.day` | Day View |
| Calendar | `Cmd+Alt+2` | `calendar.view.work_week` | Work Week View |
| Calendar | `Cmd+Alt+3` | `calendar.view.week` | Week View |
| Calendar | `Cmd+Alt+4` | `calendar.view.month` | Month View |
| Calendar | `Cmd+Alt+5` | `calendar.view.agenda` | Agenda View |
| Calendar | `Cmd+Backspace` | `calendar.delete_event` | Delete Event |
| Calendar | `Cmd+N` | `calendar.new_event` | New Event |
| Calendar | `Cmd+Shift+J` | `calendar.join` | Join Online Meeting |
| Chat | `Cmd+N` | `chat.new` | New Chat |
| Chat | `Cmd+Shift+R` | `chat.mark_read` | Mark Chat Read |
| Chat | `Cmd+Shift+U` | `chat.mark_unread` | Mark Chat Unread |
| Teams | `Cmd+N` | `channel.post` | New Channel Post |
| Files | `Cmd+Backspace` | `files.delete` | Delete |
| Files | `Cmd+Shift+C` | `files.copy_link` | Copy Link |
| Files | `Cmd+Shift+D` | `files.download` | Download |
| Files | `Cmd+Shift+M` | `files.move` | Move To |
| Files | `Cmd+Shift+N` | `files.new_folder` | New Folder |
| Files | `Cmd+U` | `files.upload` | Upload |
| Search | `Cmd+F` | `search.open` | Search Everything |
