# ADR 0005: Local store

> **Status:** accepted · 2026-10-17

## Decision
**SQLite** through `rusqlite` with the `bundled` feature (compiled in, no system library, so the
crate builds out of the box, M-OOBE), WAL mode, embedded forward-only migrations (our own small runner on `PRAGMA user_version`),
**FTS5** for local search. Blocking calls run on a dedicated thread pool (`tokio::task::spawn_blocking`
behind a small `Store` handle, M-SERVICES-CLONE).
Encryption at rest: FileVault plus application-level AEAD (`chacha20poly1305`) of message
bodies, chat message bodies and blob files, with a per-account key in the Keychain. Metadata
(ids, dates, flags) stays queryable in clear inside the user's protected home folder.

## Alternatives
| Option | Why not |
|---|---|
| `redb` + `tantivy` (pure Rust) | Good libraries, but calendar range queries, joins and sorted mail lists are far simpler in SQL; two engines to keep consistent |
| SQLCipher (`bundled-sqlcipher-vendored-openssl`) | Whole-file encryption is nice, but vendors OpenSSL and complicates builds; revisit if the owner wants metadata encrypted too |
| `sqlx` | Async SQLite in sqlx still runs on a blocking thread; compile-time query checking needs a database at build time |
| `sled` | Unmaintained beta |

## Consequences
SQLite is the one C component in our own dependency graph besides the system webview and
`mimalloc`; `cargo deny` tracks it. The "pure Rust" goal applies to everything we write.
