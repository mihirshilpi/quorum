# Control protocol

The control channel lets scripts, tests and agents drive the running Quorum window. Start it
with `quorum --control PORT` (0 picks a free port), or turn it on in **Settings ▸ Agents**
(`agents.control_enabled`, port `agents.control_port`, default 47900). It listens on
`127.0.0.1` only. At start it writes a random token to
`~/Library/Application Support/Quorum/control-token` (mode 0600; `~/.local/share/quorum/` on
Linux). Each line is a JSON request; each reply is one JSON line with the same `id`.

| Method | Params | Result |
|---|---|---|
| `auth` | `{"token": string}` | `true` (must be first) |
| `engine.execute` | `{"command": id, "params": {...}}` | command result |
| `engine.query` | `{"query": id, "params": {...}}` | query result |
| `engine.commands` | — | `[{id, label, menu, shortcut, destructive, enabled, params_doc}]` |
| `engine.queries` | — | `[{id, label, params_doc, destructive: false}]` |
| `ui.screenshot` | `{"path": string}` | `{"path"}` (needs Screen Recording permission for the app or terminal) |

Errors: `{"id":n,"error":{"kind":"...","message":"..."}}`. Kinds: `auth`, `invalid_request`,
`invalid_params`, `unknown_method`, `unknown_command`, `not_available`, `not_found`, `store`,
`remote`, `bug`, `ui`.

## Example session

```text
→ {"id":0,"method":"auth","params":{"token":"<contents of control-token>"}}
← {"id":0,"result":true}
→ {"id":1,"method":"engine.execute","params":{"command":"view.calendar"}}
← {"id":1,"result":null}
→ {"id":2,"method":"engine.query","params":{"query":"ui.state"}}
← {"id":2,"result":{"area":"calendar", ...}}
→ {"id":3,"method":"engine.query","params":{"query":"mail.list","params":{"folder":"inbox","limit":5}}}
```

## Driving the UI

UI navigation is itself a set of commands (`view.*`, `view.pane.*`, `view.list.*`,
`view.open`, `selection.set`, `palette.open`). The engine forwards them to the window as
`CoreEvent::Ui`. The query `ui.state` returns what the UI last reported is on screen, so
scripts and agents can both drive and read the whole app. Commands never open dialogs when
called programmatically.

App-level commands and queries an agent may use:

- `settings.get {"key"?}`, `settings.schema`, `settings.set {"key","value"}`, `settings.reset {"key"}`
- `app.about`, `app.queries`, `app.commands`
- `app.open_url {"url"}` (http, https, mailto) and `link.confirm {"url"}` (validates only)
- `blob.get {"id"}`: cached bytes (base64) of an inline attachment

## MCP

`quorum-cli mcp --attach PORT` bridges an MCP client to the running app over this channel;
`quorum-cli mcp` runs a headless session instead. Destructive commands (send, post, delete,
share) need `"confirm": true` after the user agrees, unless the policy
`agents.destructive_policy` (or `--allow-destructive`) says otherwise.

## Desktop integration the protocol does not cover

The shell also serves custom URI schemes to the webview: `quorum-mail:` (sanitized, script-free
mail documents), `quorum-cid:` (inline images from the cache), `quorum-remote:` (https image
proxy, 10 MiB cap, no cookies or referrer) and `quorum-hosted:` (Teams hosted content from
Graph). Link clicks in mail and chat are intercepted: the UI shows the real URL
(`link.confirm`), and it opens in the browser only through `app.open_url`.
