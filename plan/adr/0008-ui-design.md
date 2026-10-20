# ADR 0008: UI design direction

> **Status:** accepted (owner, 2026-10-18)

## Decision
- **Layout: familiar Office arrangement** (Outlook on the web and Teams): narrow left app rail
  (Mail, Calendar, Chat, Teams, Files, People), folder/navigation column, list pane, reading
  pane; top bar with central universal search and the account/presence avatar; a simplified
  command bar above the list instead of a ribbon. Reference is Outlook on the web.
- **Apple cues for polish:** system font (SF), generous spacing, translucent rail/sidebar
  materials, soft shadows, 8–10 px radii, 120–180 ms ease-out transitions, light/dark and
  reduced-motion respected. Improve on Outlook/Teams wherever it is cheap to do so (one merged
  calendar, unified search, people cards, fewer clicks).
- **Smooth:** virtualised lists, optimistic updates, no layout shift, 60 fps scrolling.
- **Keyboard-complete:** every action reachable by shortcut or the command palette (Cmd+K),
  j/k list navigation, F6 pane cycling, visible focus.
- **AI-controllable:** every action is an engine command, including UI-only ones
  (`view.*`, `palette.*`, `pane.*`, `selection.*`), so MCP and the control channel can drive the
  whole app; MCP also exposes queries for reading what is on screen (`ui.state`).
- Own visuals only: no Microsoft logos, Fluent icons or Segoe fonts.
