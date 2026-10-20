# Quorum architecture

> **Status:** plan (no code yet) · **Written:** 2026-10-17 · **Working name:** Quorum (owner decision D1 in [execution-plan.md](execution-plan.md#5-owner-decisions))

Quorum is a pure-Rust desktop app that puts a Microsoft 365 work account in one window with one
sign-in: Outlook mail, one merged calendar (including other people's shared calendars and
free/busy), Teams chats and channels, Teams presence (read anyone's, set your own and keep it
alive), and OneDrive/SharePoint files. Everything goes through documented Microsoft Graph APIs.
macOS first; the stack also targets Windows and Linux.

This document is the target shape. Library choices and their rejected alternatives are in
[adr/](adr/); the Graph scopes are in [graph-permissions.md](graph-permissions.md); the
milestones in [execution-plan.md](execution-plan.md).

## 1. Principles

1. **Local-first.** The UI reads only from the local store. Network work happens in the sync
   engine and lands in the store; the UI is notified of changes. The app opens instantly, works
   offline for reading, and queues writes (send, flag, move, RSVP) for when it is back online.
2. **Everything is a command.** Every user-visible action is a `CommandSpec` in the engine. Menus,
   shortcuts, the command palette, the CLI, the local control channel and the MCP server all
   reach the same commands. Programmatic calls never open dialogs.
3. **Never crash, never lose a draft.** Hostile input (Graph JSON, mail HTML, attachments, file
   names, notification payloads) is validated. Drafts are saved locally on every keystroke pause.
4. **Documented APIs only.** Microsoft Graph v1.0 by default; beta only behind a named feature
   flag listed in [graph-permissions.md](graph-permissions.md#beta-endpoints). No undocumented
   Teams/Outlook endpoints (no Trouter, chatsvc, OWA scraping).
5. **The UI is thin and swappable.** Nothing below the UI layer knows about Tauri, Leptos or the
   webview.
6. **Follow the [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/)**,
   with the deviations recorded in [adr/0000-rust-guidelines.md](adr/0000-rust-guidelines.md).

## 2. Process model

```
┌──────────────────────────── Quorum.app (one process) ─────────────────────────────┐
│                                                                                   │
│  WKWebView (Tauri window)                Rust core (Tauri backend, tokio)         │
│  ┌──────────────────────┐   IPC: invoke   ┌──────────────────────────────────┐    │
│  │ quorum-ui (Leptos,   │ ───────────────▶│ quorum-engine: Session, command  │    │
│  │ compiled to wasm32)  │ ◀────────────── │ registry, undo of local actions   │    │
│  │ panels, palette,     │  events: store  └──────┬───────────────┬───────────┘    │
│  │ sandboxed mail iframe│  changed, toasts       │ reads/writes  │ jobs           │
│  └──────────────────────┘                 ┌──────▼──────┐ ┌──────▼────────────┐   │
│                                           │quorum-store │ │ quorum-sync       │   │
│  Control channel (127.0.0.1, token)  ───▶ │SQLite cache,│◀│ delta loops,      │   │
│  MCP (stdio via quorum-cli)          ───▶ │FTS5, outbox │ │ outbox, presence  │   │
│                                           └─────────────┘ │ keep-alive, subs  │   │
│                                                           └──────┬────────────┘   │
│                                           ┌──────────────┐ ┌─────▼────────────┐   │
│                                           │ quorum-auth  │▶│ quorum-graph     │   │
│                                           │ PKCE, Keychain│ │ typed Graph REST │   │
│                                           └──────────────┘ └─────┬────────────┘   │
└──────────────────────────────────────────────────────────────────┼────────────────┘
                                                                   ▼
                                 graph.microsoft.com (v1.0)  ·  login.microsoftonline.com
                                 optional: Quorum relay for push (ADR 0004, tier 2)
```

The Rust core runs natively inside the Tauri process; the Leptos UI runs as wasm in the webview
and talks to the core only through Tauri commands (`invoke("run", {id, params})`) and events.
There is one IPC surface: `run(command_id, json_params)`, `query(query_id, json_params)` and a
`store-changed` event stream. The UI never calls Graph.

## 3. Workspace and layering

Flat `crates/` folder (M-CRATES-FLAT-FOLDER), every crate a workspace member with its version in
`[workspace.dependencies]` (M-CRATES-IN-WORKSPACE), edition 2024, MSRV pinned (M-MSRV).
Folders drop the prefix: `crates/engine` holds the `quorum-engine` package.

| Layer | Crate | What it does | May depend on |
|---|---|---|---|
| L0 | `quorum-model` | Domain types: strongly typed ids (`MessageId`, `ChatId`, `DriveItemId`, `UserId` newtypes with fallible constructors, M-STRONG-TYPES-GUARD), `Message`, `Event`, `Calendar`, `Chat`, `ChatMessage`, `Channel`, `Presence`, `DriveItem`, `Person`; time ranges; no I/O | serde, time |
| L0 | `quorum-sanitize` | Mail and chat HTML sanitising (allow-list), plain-text fallback, link rewriting, `cid:` mapping, size and depth caps | ammonia, html5ever |
| L1 | `quorum-graph` | Graph REST client: request types per endpoint, JSON models, paging (`@odata.nextLink`), delta (`@odata.deltaLink`), `$batch` (≤20 per batch), throttling (429/503 + `Retry-After`, per-mailbox concurrency ≤4), large-file upload sessions, typed errors. Transport is a trait so tests run against recorded fixtures (M-MOCKABLE-SYSCALLS) | model, reqwest (rustls), serde_json |
| L1 | `quorum-auth` | Entra ID sign-in: authorization code + PKCE via system browser and loopback redirect; refresh tokens; token cache in the OS keychain; account switching; redacting `Debug` for secrets (M-PUBLIC-DEBUG) | oauth2, keyring, model |
| L2 | `quorum-store` | Local cache: SQLite (WAL) with migrations, per-resource tables, delta tokens, FTS5 search index, outbox of pending writes, draft autosave, attachment blob cache with size quota; change feed for the UI | model, rusqlite (bundled) |
| L3 | `quorum-sync` | Sync engine: one supervised task per resource (mail folders, calendar views, chats, channels, drives, presence); adaptive polling cadence; outbox replay with conflict handling; presence keep-alive; optional push relay client | graph, auth, store |
| L4 | `quorum-engine` | `Session`, command registry (`cmd/*`), queries, settings, undo for local actions, notification rules, the panic guard (`guard.rs`) | store, sync, sanitize |
| L5 | `quorum-ui` | Leptos front end compiled to `wasm32-unknown-unknown`: shell, mail, calendar, chat, files, people, palette; icons drawn in code; theme tokens | leptos, quorum-ipc |
| L0 | `quorum-ipc` | Shared IPC wire types (serde) between UI and core; the only crate both sides depend on | serde, model |
| L5 | `quorum-mcp` | MCP server (JSON-RPC over stdio) exposing commands and queries to agents | engine |
| L5 | `quorum-control` | Local JSON-lines control channel (127.0.0.1, bearer token in a 0600 file) for scripted driving and tests | engine |
| app | `quorum` | Tauri 2 desktop shell: windows, tray (presence), notifications, deep links, updater, single instance, login item | engine, control, ui assets |
| app | `quorum-cli` | Headless: `login`, `whoami`, `sync`, `run <command> <json>`, `query`, `commands`, `mcp`, `permissions` | engine, mcp |
| tool | `xtask` | `ci`, `layers`, `assets`, `wasm`, `deny`, `fixtures`, `bundle` | — |

`cargo xtask layers` enforces: a crate may depend only on crates in lower layers. Nothing below L5 depends on `tauri`, `leptos`, `wry`, `web-sys` or `wasm-bindgen`.
`quorum-ui` must always build for wasm32 (`cargo xtask wasm`).

Libraries use canonical error structs (M-ERRORS-CANONICAL-STRUCTS): each crate has one `Error`
with a `kind()` (e.g. `graph::ErrorKind::{Throttled, Unauthorized, NotFound, Conflict, Network,
Malformed}`) and `From` impls (M-FROM-ERROR). The two apps may use `anyhow` (M-APP-ERROR).

## 4. Authentication (ADR [0002](adr/0002-auth.md))

- One multi-tenant or single-tenant **public client** app registration in Entra ID (D2). Redirect
  URI `http://localhost` (loopback, any port, per RFC 8252). No client secret.
- Sign-in opens the system browser (so the macOS Enterprise SSO plug-in, passkeys and
  Conditional Access work as they do in the browser), authorization code + PKCE, `state` and
  `nonce` checked.
- Scope: because the IT admin grants tenant-wide consent, the app requests
  `https://graph.microsoft.com/.default offline_access openid profile` and receives every
  consented scope at once. The `permissions` CLI command compares the granted `scp` claim with
  the list in [graph-permissions.md](graph-permissions.md) and the UI disables features whose
  scopes are missing instead of failing.
- Tokens: access token in memory only; refresh token in the macOS Keychain (`keyring`), one item
  per account. Silent refresh 5 minutes before expiry; on `invalid_grant` or a Conditional Access
  claims challenge (`WWW-Authenticate: ... claims=`), the app pauses sync and asks the user to
  sign in again with the claims parameter.
- Multiple accounts are a later milestone (M11); the model carries an `AccountId` everywhere from
  day one so it is not a rewrite.

## 5. Graph client

- Base `https://graph.microsoft.com/v1.0`; national clouds via a setting (endpoint + authority).
- Every request carries `client-request-id`; responses log `request-id` for support (structured,
  M-LOG-STRUCTURED; never bodies, subjects, names or tokens).
- **Throttling:** a token-bucket per resource family (mail, calendar, Teams, files) and per
  mailbox; honour `Retry-After`; exponential backoff with jitter on 5xx; never retry non-idempotent
  POSTs without an idempotency guard (outbox entries carry a local id stored in an extended
  property / `transactionId` where Graph supports it, e.g. events).
- **Batching:** `$batch` for fan-out reads (presence of many users, per-chat last messages,
  calendar views of several shared calendars).
- **Hostile input:** response size cap, JSON depth cap, string length caps on display fields,
  `get()` not `[i]`, checked arithmetic on sizes and offsets, unknown enum values map to
  `Unknown(String)` instead of failing the whole page.

## 6. Local store (ADR [0005](adr/0005-local-store.md))

- One SQLite database per account in `~/Library/Application Support/Quorum/<account>/`,
  WAL mode, `PRAGMA foreign_keys=ON`, versioned migrations embedded in the binary.
- Tables: `mail_folder`, `message` (headers + preview), `message_body` (sanitised HTML, lazily
  fetched), `attachment` (metadata) + blob files, `calendar`, `event`, `event_instance`
  (expanded for the visible range), `chat`, `chat_member`, `chat_message`, `team`, `channel`,
  `channel_message`, `drive`, `drive_item`, `person`, `presence`, `delta_state`, `outbox`,
  `draft`, `setting`.
- FTS5 virtual tables for mail (subject, from, preview, body text), chat messages and file names;
  server search (`/search/query`) supplements results for items not cached.
- Encryption at rest: relies on FileVault plus a per-account key in the Keychain used to encrypt
  bodies and blobs (`chacha20poly1305`); see ADR 0005 for why not SQLCipher.
- Retention: mail bodies for the last N days (setting, default 90), attachments by quota
  (default 2 GB), chats last 30 days hydrated, older on demand.

## 7. Sync engine and live updates (ADR [0004](adr/0004-live-updates.md))

Graph cannot push to a desktop app directly: change notifications need a public HTTPS endpoint
or an Azure Event Hub. So live updates come in two tiers.

**Tier 1 (always on, no infrastructure):** adaptive delta polling.

| Resource | Mechanism | Cadence (focused / background / idle) |
|---|---|---|
| Mail, per folder | `/me/mailFolders/{id}/messages/delta` | Inbox 15 s / 60 s / 5 min; other folders on open + 5 min |
| Mail folders | `/me/mailFolders/delta` | 10 min |
| Calendars (own + shared) | `/me/calendarView/delta` for a rolling window (−30 d … +180 d), `/users/{id}/calendarView` for shared | 60 s / 3 min / 10 min |
| Chats list | `/me/chats?$expand=lastMessagePreview&$orderby=lastMessagePreview/createdDateTime desc` | 5 s / 30 s / 2 min |
| Chat messages | `/chats/{id}/messages?$orderby=lastModifiedDateTime desc&$filter=lastModifiedDateTime gt …` for chats whose preview changed; open chat polled faster | open chat 3 s |
| Channel messages | `/teams/{id}/channels/{id}/messages/delta` | open channel 5 s / others 2 min |
| Presence of visible people | `POST /communications/getPresencesByUserId` (batched) | 30 s / 2 min |
| OneDrive / SharePoint | `/drives/{id}/root/delta` | 2 min / 10 min |

Cadences are budgets, not promises: the throttling layer slows everything down when Graph says so.

**Tier 2 (optional, needs the tenant's Azure):** Graph change notifications for mail, events,
chats, channel messages and presence, delivered to a small **Quorum relay** (Rust, `axum`) the
tenant deploys (Azure Container Apps or Functions). The relay validates subscriptions, decrypts
rich notifications, and pushes "resource X changed" hints to the signed-in user's app over an
authenticated WebSocket. The app treats a hint as "poll this resource now"; tier 1 stays the
source of truth, so a lost hint costs only latency. Presence subscriptions expire within an hour
and are renewed by the relay. Owner decision D4.

**Download once, then only changes.** Like Outlook's cached mode or Apple Mail, each item is
downloaded once and then kept current from deltas:
1. First sync fetches the newest headers and previews first (Inbox, then other folders), so the
   app is usable within seconds, then backfills older headers in the background, newest to oldest,
   at a throttled pace. Bodies are fetched when a message is opened, plus prefetched for the
   most recent N days (setting, default 30).
2. After that, every resource polls its stored `@odata.deltaLink`, which returns only items
   added, changed or deleted since the last call. Nothing is re-downloaded.
3. Attachments download on demand only and are cached by quota.
4. If Graph expires a delta token (HTTP 410 / `syncStateNotFound`), that one folder resyncs and
   its changes are merged by item id; bodies already cached are kept.

**Outbox:** writes go to the `outbox` table first, are applied optimistically in the store, and
replayed in order per resource; failures surface as a toast with Retry/Discard, never silently.

## 8. Feature areas

### Mail
Folders and favourites, focused/other, conversation view (by `conversationId`), read pane with
sanitised HTML in a sandboxed iframe (ADR [0006](adr/0006-mail-rendering.md)), remote images off
by default with per-sender allow, attachments (download, preview, save to OneDrive), compose /
reply / reply-all / forward with rich text, signatures, drafts synced to the server, send later,
flags, categories, pin, move, archive, delete, undo send (delayed outbox), rules
(`messageRules`), automatic replies (`mailboxSettings`), shared and delegated mailboxes
(`Mail.ReadWrite.Shared`), search (local FTS + `/search/query`), mentions, read receipts flag.

### Calendar (one merged view)
All own calendars and calendar groups, plus any calendar shared with the user, plus calendars of
people the user "opens" (`/users/{id}/calendar` where shared, otherwise free/busy only via
`getSchedule`). Each source is a toggleable, colour-coded layer in one day/week/month/agenda view.
Scheduling assistant: pick attendees and rooms, `getSchedule` for free/busy grid,
`findMeetingTimes` for suggestions, rooms via `/places`. Create, edit, recurring events, RSVP,
propose new time, Teams meeting links (`isOnlineMeeting`, `onlineMeetingProvider=teamsForBusiness`),
join button that opens the Teams join URL, reminders as native notifications, time zones.

### Teams chat and channels
Chat list (1:1, group, meeting chats) with unread state, message view with replies, reactions,
mentions, edits/deletes, inline images (`hostedContents`), file attachments (uploaded to the
user's OneDrive "Microsoft Teams Chat Files" folder and referenced, as Teams does), new chat,
add members, mark read (`markChatReadForUser`). Joined teams and channels, channel posts and
replies, channel files tab (the channel's SharePoint folder). Calls and meeting media are out of
scope: Graph offers no client media stack; "Call" and "Join" deep-link to Teams.

### Presence
Read: avatar dots everywhere a person appears, batched. Set: the user picks a status (Available,
Busy, Do not disturb, Be right back, Away, Offline) via `setUserPreferredPresence`, plus a status
message (`setStatusMessage`). **Keep-alive:** while the app runs and the user is active (OS idle
time below a threshold), the app holds an application presence session with `setPresence`
(`sessionId` = our client id, `expirationDuration` = PT1H) refreshed every 4 minutes, so the user
shows Available even without the Teams client open; on idle it reports Away, on sleep/quit it
calls `clearPresence`. Calendar-aware: Busy / In a meeting during accepted events, optional.
Tray icon shows and sets presence.

### OneDrive and SharePoint
My files, Recent, Shared with me, followed sites, site document libraries (each a drive), lists
and list items (read and edit), browse/search, upload (resumable upload sessions for large
files), download, rename, move, copy, delete to recycle bin, share links and permissions,
version history, preview via the item `preview` endpoint, "Open in desktop app" via the Office
URI schemes, Open in browser. No local sync folder (the OneDrive client does that).

### Across everything
Universal search (mail, chat, files, people, events) with the Microsoft Search API plus local FTS;
people cards (profile, photo, presence, org chart via `manager`/`directReports`, shared files,
recent mail and chats with them); unified activity feed built locally from mail, mentions,
replies and invitations (Graph has no read API for the Teams activity feed); command palette;
native notifications with Do-Not-Disturb aware rules; keyboard-first navigation.

### Later (stretch)
Microsoft To Do and Planner tasks, contacts, OneNote (read), Loop links, Viva/Bookings are
explicitly later; see the execution plan.

## 9. Security and privacy

- The control channel and MCP can send mail and messages as the user. The control channel binds
  to 127.0.0.1 only and requires a random bearer token stored in a 0600 file; it is off unless
  started with `--control`. MCP write commands (send, delete, post) are marked `destructive` in
  their `CommandSpec` and need a confirmation in the UI unless the user disables that per tool.
- Mail HTML is untrusted: sanitised in Rust, rendered in a sandboxed iframe with a strict CSP,
  no scripts, links open in the system browser after showing the real target.
- Logs never contain tokens, message bodies, subjects, names or email addresses; types holding
  them have redacting `Debug` with leak tests (M-PUBLIC-DEBUG).
- Tenant data and screenshots of it are never committed; tests use synthetic fixtures.

## 10. Testing

- Unit tests per crate; integration tests in `tests/` against the public API (M-INTEGRATION-TESTS).
- `quorum-graph` ships a `test-util` feature (M-TEST-UTIL) with a fixture transport replaying
  recorded-then-scrubbed JSON in `fixtures/graph/`; `cargo xtask fixtures check` refuses fixtures
  with real-looking addresses or tenant ids.
- A `--mock-graph` mode runs the whole app against the fixture server, so agents and CI can drive
  every feature without a tenant.
- Property and hostile-input tests: `hostile_graph_json_never_panics`,
  `hostile_mail_html_is_sanitised`, `hostile_params_never_panic` over every command.
- Live smoke tests against a developer tenant (Microsoft 365 Developer Program or the owner's
  test tenant), opt-in via `QUORUM_LIVE_TENANT=1`, never in CI by default.
