# ADR 0000: Adopting the Microsoft Pragmatic Rust Guidelines

> **Status:** accepted · 2026-10-17

## Decision
Quorum follows the [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/)
(agent copy: `https://microsoft.github.io/rust-guidelines/agents/all.txt`). Reviews cite guideline
ids (e.g. `M-STRONG-TYPES`). The ones that shape this codebase most:

| Guideline | How it applies here |
|---|---|
| M-CARGO-WORKSPACE, M-CRATES-FLAT-FOLDER, M-CRATES-IN-WORKSPACE, M-LATEST-EDITION, M-MSRV | One workspace, flat `crates/`, edition 2024, MSRV in `[workspace.package]` |
| M-SMALLER-CRATES | Graph client, auth, store, sync, sanitize are separate crates |
| M-STATIC-VERIFICATION | `cargo xtask ci`: rustfmt, clippy `-D warnings`, `cargo deny` (advisories + licences), `cargo hack --each-feature` on library crates |
| M-LINT-OVERRIDE-EXPECT | `#[expect(lint, reason = "...")]`, never bare `#[allow]` |
| M-ERRORS-CANONICAL-STRUCTS, M-FROM-ERROR, M-APP-ERROR | Library crates: one `Error` struct with `kind()`, backtrace, source; apps may use `anyhow` |
| M-STRONG-TYPES, M-STRONG-TYPES-GUARD | Graph ids, email addresses, time ranges are newtypes with fallible constructors |
| M-PUBLIC-DEBUG, M-LOG-STRUCTURED | Tokens, bodies, subjects, addresses have redacting `Debug`, tested for leaks; `tracing` with named events, no PII |
| M-LOG-NOT-PRINT | No `println!`/`dbg!` outside the CLI's user output |
| M-MOCKABLE-SYSCALLS, M-TEST-UTIL | HTTP transport, clock, keychain, idle-time and file system behind traits; fakes under the `test-util` feature |
| M-INTEGRATION-TESTS | Public-API tests in each crate's `tests/` |
| M-SERVICES-CLONE | `GraphClient`, `Store`, `Session` are cheap `Clone` (`Arc<Inner>`) |
| M-DI-HIERARCHY | Concrete types, then generics; `dyn Trait` only at the transport/keychain seams |
| M-SHORT-NAMES, M-WEASEL-WORDS | `graph::Client`, not `GraphServiceManager` |
| M-CANONICAL-DOCS, M-MODULE-DOCS, M-FIRST-DOC-SENTENCE | `#![warn(missing_docs)]` on library crates; Errors/Panics sections |
| M-DESIGN-FOR-AI, M-TAUTOLOGICAL-TESTS | Agent-friendly APIs and docs; tests assert behaviour, not constants |
| M-MIMALLOC-APPS | `mimalloc` as global allocator in `quorum` and `quorum-cli` |
| M-HOTPATH, M-YIELD-POINTS | Benchmarks for store queries, FTS and HTML sanitising; long sync loops yield |
| M-ASYNC-FN, M-TYPES-SEND | `async fn`; public futures are `Send` (tokio multi-thread) |

## Deliberate deviations
1. **Panics (M-PANIC-ON-BUG, M-PANIC-IS-STOP).** The guidelines say detected bugs should panic and
   panics end the program. Quorum's never-crash rule is stricter: production code contains no
   `unwrap`/`expect`/`panic!`/`unreachable!`/`todo!`/`unimplemented!`. A detected contract
   violation is `debug_assert!` (panics in tests and debug builds, where we want it) plus a
   returned `ErrorKind::Bug` in release, which is logged and reported. Reason: a mail/chat client
   that exits loses drafts and misses messages; a degraded feature is better.
2. **Last-resort guard (M-PANIC-CONTINUATION).** We do use `catch_unwind`, exactly as that guideline
   allows: around each command execution and each sync task, followed by a *controlled restart* of
   that unit (the sync task is respawned from its stored delta state; the command's partial effects
   are discarded because writes go through the outbox). Drafts are flushed before reporting.
