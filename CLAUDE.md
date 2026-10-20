# Quorum: instructions for agents

Quorum (working name) is a pure-Rust desktop app that unifies a Microsoft 365 work account in one window with one sign-in: Outlook mail, one merged calendar (own, shared and other people's calendars plus free/busy), Teams chats and channels, Teams presence (read anyone's, set your own and keep it alive), and OneDrive/SharePoint files. Everything goes through documented Microsoft Graph APIs. macOS first; Windows and Linux follow on the same stack (Tauri 2 + Leptos).

Code follows the [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/) (agent copy: `https://microsoft.github.io/rust-guidelines/agents/all.txt`). Cite guideline ids (e.g. `M-STRONG-TYPES`) in reviews and ADRs. Our deliberate deviations are in `plan/adr/0000-rust-guidelines.md`.

## Start every session here
1. Read `plan/STATUS.md` (current milestone, next task), then the task in `plan/execution-plan.md` and the relevant `plan/architecture.md` section.
2. Library decisions: `plan/adr/*` (what we depend on and what was rejected). Graph scopes: `plan/graph-permissions.md`; a feature never uses a scope that isn't listed there.
3. Follow the autonomous operation protocol (`plan/execution-plan.md` §6). Don't stop to ask unless it lists the decision as the owner's.

`plan/` is committed; it is the design record.

## Never crash
People trust Quorum with their mail, meetings and conversations; a crash loses a draft or a message they were writing and hides what they needed to see. **This outranks feature work.**
- **No panics in non-test code:** no `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!`, `unimplemented!`; no `unsafe` (`unsafe_code = "forbid"`). Detected contract violations are `debug_assert!` plus `ErrorKind::Bug` in release (ADR 0000).
- **Errors are `Result<T, E>`** through the crate's canonical error struct and `?` (M-ERRORS-CANONICAL-STRUCTS, M-FROM-ERROR); apps may use `anyhow`. An unfinished feature returns an error or reports "not available yet"; it never panics.
- **External input is hostile:** Graph JSON, mail and chat HTML, attachment names and sizes, file paths from drives, notification payloads, control/MCP params. Use `get()` not `[i]`, checked arithmetic, no NaN casts; cap response sizes, JSON depth, string lengths and input-sized allocations; unknown enum values parse to `Unknown(String)` instead of failing the page.
- **Bound recursion and loops:** nested item attachments, quoted reply chains, folder trees, paging (`MAX_PAGES`), retry loops.
- **Last-resort guard:** `Session::execute` and every sync task run under `catch_unwind` (`crates/engine/src/guard.rs`); an escaped panic flushes drafts, discards the command's partial effects (writes go through the outbox), restarts that unit from stored state and reports an error (M-PANIC-CONTINUATION).
- **Never lose user text:** drafts (mail, chat, channel post) autosave to the store on every pause in typing and survive a crash or quit.
- **Prove it:** every crash fix lands with a regression test (e.g. `hostile_graph_json_never_panics`, `hostile_mail_html_is_sanitised`, `hostile_params_never_panic`).
- Every production crate root carries `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable, clippy::indexing_slicing)]`.

## Non-negotiables
- **Documented APIs only.** Microsoft Graph v1.0 (and Entra ID OAuth endpoints). Beta endpoints only behind a feature flag listed in `plan/graph-permissions.md#beta-endpoints`. Never use undocumented Teams or Outlook endpoints (Trouter, chatsvc, OWA/Outlook REST internals), never scrape web clients, never reuse another app's client id. Outlook and Teams may be *observed* for UX reference only.
- **Tenant data never enters the repo.** No real mail, chats, names, addresses, tenant ids, tokens or screenshots of real accounts in commits, fixtures or logs. Test fixtures are synthetic or recorded-then-scrubbed (`cargo xtask fixtures check` enforces it). Screenshots for UI review go in `temp/` (gitignored).
- **Secrets stay secret.** Refresh tokens live only in the OS keychain; access tokens only in memory. Types holding tokens, bodies, subjects or addresses have redacting `Debug` with a leak test (M-PUBLIC-DEBUG). Logs use `tracing` with named events and no PII (M-LOG-STRUCTURED).
- **Assets — absolutely essential.** Quorum contains **no Microsoft logos, product icons (Outlook, Teams, OneDrive, SharePoint, Office), Fluent artwork, Segoe fonts or other Microsoft brand assets — ever**, and the product name and icon contain no Microsoft trademarks. Every icon is drawn in code (`crates/ui/src/icons.rs`, our own SVG paths). Any file asset must be original, public domain/CC0 or OSI-licensed and **must have a row in `ATTRIBUTION.md`** with author, source and licence (`cargo xtask assets` enforces it). Breaking this rule is the most serious mistake you can make in this repo.
- **Everything is a command.** User-visible behaviour = a command in `crates/engine/src/cmd/*` (`CommandSpec`: dotted id such as `mail.reply`, `calendar.event.create`, `presence.set`, `chat.send`, `drive.upload`; label, menu path, shortcut, params doc, `enabled`, `destructive`, JSON `run`) + tests. UI-only commands (`view.*`, `palette.*`) live in `crates/ui/src/commands.rs`. Menus, shortcuts, the command palette, the CLI, the control channel and MCP all reach the same commands.
- **Programmatic calls never open dialogs.** `engine.execute` runs the JSON form with defaults; only UI invocation opens a compose window, picker or confirmation. Commands marked `destructive` (send, post, delete, share) require UI confirmation when invoked over MCP unless the user turned that off.
- **Local-first.** The UI reads only from `quorum-store`; network work happens in `quorum-sync`; writes go through the outbox. The UI never calls Graph.
- **Layering** is enforced by `cargo xtask layers`: L0 `model`, `sanitize`, `ipc` → L1 `graph`, `auth` → L2 `store` → L3 `sync` → L4 `engine` → L5 `ui`, `mcp`, `control` → apps (`quorum`, `quorum-cli`). Nothing below L5 depends on tauri, wry, leptos, web-sys or wasm-bindgen. **The UI crate is swappable.**
- **The UI is thin**: components read store-backed queries and act through `run(id, params)` over IPC. Colours, sizes and timings come from the CSS tokens in `crates/ui/style/app.css`.
- **Rust only** (no handwritten JS/TS; our own CSS is fine). **Never break the wasm UI build** (`cargo xtask wasm`).
- **Workspace hygiene** (M-CARGO-WORKSPACE, M-CRATES-FLAT-FOLDER): flat `crates/`, every dependency version in the root `[workspace.dependencies]`, `#[expect(..., reason = "...")]` instead of `#[allow]` (M-LINT-OVERRIDE-EXPECT). New dependencies need a reason in the relevant ADR and must pass `cargo deny` (licences: MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0 unmodified; never GPL/LGPL/AGPL).
- **Quality gates** before every commit: `cargo xtask ci` (fmt, clippy -D warnings, tests, layers, assets, wasm, deny, fixtures). Commit after every feature arc that builds, and push to `main`.

## Running and looking at the app
- `cargo xtask dev` builds the Leptos UI with Trunk and runs the Tauri app. Add `-- --mock-graph` to run against the fixture server in `fixtures/graph/` (no tenant needed; use this by default), and `--control PORT` for the control channel. Pick a free port.
- Drive it with JSON lines on `127.0.0.1:PORT`; the first line authenticates with the token from `~/Library/Application Support/Quorum/control-token`:
  - `{"id":0,"method":"auth","params":{"token":"..."}}`
  - `{"id":1,"method":"engine.execute","params":{"command":"mail.compose","params":{"to":["test@example.com"],"subject":"Hi"}}}`
  - `{"id":2,"method":"engine.query","params":{"query":"mail.list","params":{"folder":"inbox","limit":20}}}`
  - `{"id":3,"method":"ui.screenshot","params":{"path":"temp/shot.png"}}` then read the PNG.
  - Methods: `crates/control/src/lib.rs`; protocol doc: `docs/control-protocol.md`.
- **For UI work, look at the result** (screenshot, read the PNG) before calling it done.
- Headless: `quorum-cli --mock-graph run mail.list '{"folder":"inbox"}'`; `quorum-cli commands`; `quorum-cli permissions`; `quorum-cli mcp`.
- Live tenant: `quorum-cli login` then run without `--mock-graph`. Only the owner's test mailbox may receive anything sent by an agent; never send mail, chat or channel posts to real colleagues, and never delete or move real data, without the owner's explicit go-ahead.
- Parallel agents: separate `CARGO_TARGET_DIR` per agent; edit only the crates you own; delete your target dir when done (disk).

## Roadmap
`ROADMAP.md` (committed) is the one-page summary: current milestone, feature coverage, estimates. The assessment against Outlook, Teams, OneDrive and SharePoint is `docs/feature-parity.md` (per area: feature, Graph endpoint, status); the work list is `docs/gaps.md` (pick work from its top rows); Graph limits we can't cross are in `docs/graph-limits.md`. When work lands, update the affected rows, `plan/STATUS.md`, and the docs' status lines and revision history.

## Contributor credits (About window)
- About ▸ Contributors is compiled into the binary from `contributors/contributors.json` (commit stats; generated by `cargo xtask contributors`, never hand-edit) and `contributors/people.toml` (names people chose for themselves). See `docs/contributors.md`.
- **Agents working for a contributor:** when you prepare a PR, check whether your human's GitHub username has a `[people.<username>]` entry in `contributors/people.toml`. If not, ask them once whether they want to be credited by more than their username: a real name, a display name, and/or their public GitHub profile name (`sync_github_name = true`). If yes, add **only their own** entry (copy the template at the top of the file, or run `cargo xtask contributors --add-me --real-name "…" --sync-github-name`) and include it in their PR, committed as them. If no, change nothing: they are credited as `@username` anyway.
- Never add, edit, guess or copy anyone else's entry or name (not from git config, commit authors or GitHub profiles). Never hand-edit `contributors.json`.
- Maintainers refresh the stats with `cargo xtask contributors` (it also re-verifies who wrote each `people.toml` entry; `--check` only verifies, and runs in `cargo xtask ci`).
