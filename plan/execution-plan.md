# Quorum execution plan

> **Status:** plan · **Written:** 2026-10-17 · Read with [architecture.md](architecture.md) and [STATUS.md](STATUS.md).

Each milestone ends with something the owner can run and use. A milestone is done when its exit
criteria pass in `--mock-graph` mode in CI **and** a live smoke test against the test tenant.
Tasks are sized to one feature arc (one commit that builds and passes `cargo xtask ci`).

## 1. Milestones

### M0 Foundation
Goal: an empty app that builds, runs and can be driven.
- Cargo workspace (edition 2024, resolver 3, MSRV, shared lints, `unsafe_code = "forbid"`,
  never-crash clippy denies), `rustfmt.toml`, `clippy.toml`, `deny.toml`.
- `xtask`: `ci` (fmt, clippy -D warnings, tests, layers, assets, wasm, deny), `layers`, `assets`, `wasm`.
- Tauri 2 shell + Leptos UI (Trunk build) showing a placeholder sidebar (Mail, Calendar, Chat,
  Teams, Files, People) and a command palette wired to the engine's registry.
- `quorum-engine` with `CommandSpec`, registry, `guard.rs` (panic guard), `app.about`,
  `app.quit`, `palette.open`; `quorum-control` with `engine.execute`, `engine.commands`,
  `ui.screenshot` (window capture).
- `quorum-cli commands`, `run`.
- About window with Contributors: `contributors/people.toml` template, `cargo xtask contributors` (generate stats, `--add-me`, `--check` in CI), `docs/contributors.md`.
- GitHub Actions on macOS: `cargo xtask ci`.
- Exit: `cargo run -p quorum -- --control 0` opens a window; a control-channel `engine.execute`
  of `palette.open` opens the palette; CI green.

### M1 Sign-in and the Graph client
- Entra app registration documented in `docs/setup-tenant.md` (redirect `http://localhost`,
  public client, the scope list, admin-consent URL for the IT admin).
- `quorum-auth`: PKCE loopback flow, Keychain token cache, silent refresh, claims challenge,
  sign-out.
- `quorum-graph`: transport trait, reqwest transport, paging, `$batch`, throttling, errors,
  fixture transport (`test-util`), `/me`, `/me/photo`.
- Commands: `account.sign_in`, `account.sign_out`, `account.whoami`, `account.permissions`.
- `quorum-cli login`, `whoami`, `permissions`.
- Exit: sign in to the test tenant from the app and the CLI; profile + photo shown; missing
  scopes listed; hostile Graph JSON tests pass.

### M2 Local store and sync framework
- `quorum-store`: SQLite, migrations, delta-state table, outbox, change feed to the UI,
  encryption of bodies/blobs, FTS5.
- `quorum-sync`: supervisor, per-resource task trait (`initial`, `delta`, `apply`), adaptive
  cadence (focus, background, idle, on battery, offline), backoff, outbox replay.
- `--mock-graph` app mode backed by `fixtures/graph/`.
- Exit: kill the network mid-sync and restart; the app opens with cached data and resumes
  from the stored delta link; an outbox write survives a restart.

### M3 Mail
- Folder tree, message list (virtualised), conversation view, reading pane (ADR 0006),
  attachments, compose/reply/forward with rich text editor and signatures, drafts sync,
  send/undo send, flags, categories, move/archive/delete, mark read, focused inbox, search
  (local + server), automatic replies, rules list/edit, shared mailboxes.
- Commands: `mail.*` (about 40).
- Exit: daily-driver mail for one week on the owner's account without opening Outlook.

### M4 Calendar (merged)
- Calendar sources panel (own, groups, shared with me, opened people's calendars, group
  calendars), merged day/week/month/agenda views, event details, create/edit/recurrence,
  RSVP/propose time, Teams meeting toggle, reminders, time zones.
- Scheduling assistant: `getSchedule` free/busy grid for any people, `findMeetingTimes`,
  room finder (`/places`).
- Exit: open a colleague's shared calendar and a non-shared colleague's free/busy side by side
  with one's own in a single week view; schedule a meeting with a room and a Teams link.

### M5 Presence
- Read presence for every visible person (batched, cached, `$batch`).
- Set presence and status message; tray menu; keep-alive session (`setPresence`, 4-minute
  refresh, idle → Away, sleep/quit → `clearPresence`); optional calendar-driven Busy.
- Exit: with Teams closed on every device, the owner shows Available while active in Quorum
  and Away after the idle threshold, as seen from a second account.

### M6 Teams chat
- Chat list with unread, message view, send (rich text, mentions, emoji), replies/quotes,
  reactions, edit/delete, inline images, file attachments via OneDrive, new chat, members,
  mark read, meeting chats linked from calendar events.
- Exit: hold a group chat with a colleague entirely from Quorum, including an image and a file.

### M7 Teams and channels
- Joined teams, channels (standard, private, shared), channel posts and replies with delta,
  post and reply, mentions of channel/team, channel files tab.
- Exit: read and reply in a channel thread; channel files open in the Files view.

### M8 OneDrive and SharePoint
- My files, Recent, Shared with me, followed sites, site libraries, lists and list items,
  browse/search, upload (resumable), download, rename/move/copy/delete, share links,
  permissions, versions, preview, open in desktop app / browser, drag and drop to mail and chat.
- Exit: find a SharePoint document by search, share a link to it in a chat, attach it to mail.

### M9 Live updates tier 2 (optional, owner decision D4)
- `quorum-relay` service (Rust, axum): subscription lifecycle, validation, lifecycle
  notifications, rich-notification decryption, authenticated WebSocket fan-out.
- Deployment template (Bicep) for the tenant's Azure; app setting to point at the relay.
- Exit: a new chat message appears in under 2 seconds; with the relay down the app silently
  falls back to tier 1.

### M10 One workspace
- Universal search across all areas, people cards and org chart, unified activity feed,
  notification rules, keyboard map, settings UI, onboarding, accessibility pass (VoiceOver,
  keyboard-only), dark mode.
- Exit: the owner uses Quorum as the only Microsoft 365 client for two weeks.

### M11 Ship
- Signed and notarised macOS build (Developer ID), Tauri updater with signed releases,
  crash-free telemetry (local logs only unless the owner opts in), multiple accounts, Windows
  build, localisation scaffold.
- Exit: `.dmg` installs on a clean Mac; update from N-1 to N works.

### M12 Stretch
To Do / Planner tasks, contacts, OneNote read, Bookings, Viva Insights-free features, Linux.

## 2. Dependencies between milestones

M0 → M1 → M2 → (M3, M4, M5, M6, M8 in any order) → M7 after M6 → M9 any time after M6 →
M10 → M11. M3 and M4 first because they replace Outlook; M5 is small and high-value.

## 3. Known Graph limits that shape the product

- **No push to a desktop client** without infrastructure (tier 2 above).
- **Chats have no delta query**; messages are fetched per chat whose preview changed.
- **No Teams activity feed read API**; Quorum builds its own from mentions, replies and invites.
- **No calls or meeting media**; join/call deep-links to Teams.
- **No typing indicators or read receipts per message** in Graph (only mark-chat-read).
- **Presence propagation is delayed** by up to a few minutes in Graph.
- **Throttling** is per app per mailbox and per tenant; the cadence table is a budget.
- Some Teams features are beta-only; each one used is listed in graph-permissions.md and gated.

## 4. Risks

| Risk | Mitigation |
|---|---|
| Conditional Access requires a compliant device / broker | System-browser sign-in passes the macOS Enterprise SSO extension; test early in M1 against the real tenant |
| Admin consent delayed | M0–M2 run fully in `--mock-graph`; consent URL ready in M1 |
| Rich-text compose in a webview without handwritten JS | `web-sys` contenteditable + our own command layer; fallback to Markdown-style editor if needed |
| Throttling on large tenants | Budgets, `$batch`, backoff; telemetry counters for 429s |
| HTML mail rendering security | ADR 0006: sanitise in Rust + sandboxed iframe + CSP; fuzz tests |
| Trademark | Product name and icon contain no Microsoft marks; "for Microsoft 365" wording only as allowed (D1) |

## 5. Owner decisions

| # | Decision | Default until decided |
|---|---|---|
| D1 | Product name and bundle id | "Quorum", `dev.quorum.app` (placeholder) |
| D2 | Single-tenant (your company only) or multi-tenant app registration | Single-tenant |
| D3 | Open source licence or private | Private repo, MIT OR Apache-2.0 headers ready |
| D4 | Build the tier-2 push relay in the tenant's Azure | Not yet; tier 1 only |
| D5 | Apple Developer ID for signing and notarisation | Unsigned dev builds |
| D6 | Any beta Graph endpoint in a release build | Off in release, on in dev |

## 6. Autonomous operation protocol

1. Start from `plan/STATUS.md`: the current milestone and next task. Work the next task; don't
   skip ahead unless blocked.
2. One feature arc per commit: code, tests (including a hostile-input test where input is
   external), docs updated, `cargo xtask ci` green, then commit and push.
3. Update `STATUS.md` (done, next, blockers) and the affected rows of `ROADMAP.md` and
   `docs/feature-parity.md` with every arc.
4. Decide technical questions yourself and record non-obvious ones as a new ADR. Stop and ask
   only for: the owner decisions above, any new Graph permission not on the list, anything that
   sends mail/messages or changes data in the live tenant outside the test mailbox, spending
   money, and anything touching signing keys.
5. When blocked on the tenant, keep going in `--mock-graph` and record the blocker.
6. Never weaken a quality gate or a never-crash lint to land a feature.
