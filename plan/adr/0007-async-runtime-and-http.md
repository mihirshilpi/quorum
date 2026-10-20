# ADR 0007: Async runtime, HTTP, logging, time

> **Status:** accepted · 2026-10-17

| Concern | Choice | Notes |
|---|---|---|
| Runtime | `tokio` (multi-thread) | Tauri 2 already runs on tokio |
| HTTP | `reqwest` with `rustls` (platform verifier) | No OpenSSL; HTTP/2; system proxy settings honoured |
| Serialisation | `serde`, `serde_json` | Unknown fields ignored |
| Time | `jiff` | IANA and Windows time-zone names from Graph map to `jiff` zones via a table we own |
| Logging | `tracing`, `tracing-subscriber` (JSON file sink, rotating) | Redaction enforced by types |
| Errors | `thiserror` in libraries (canonical structs), `anyhow` in apps | ADR 0000 |
| Allocator | `mimalloc` in apps | M-MIMALLOC-APPS |
| IDs/crypto | `uuid`, `chacha20poly1305`, `rand` | |
| Idle detection | `user-idle` or IOKit through an existing safe crate behind a trait | No `unsafe` in our code |
