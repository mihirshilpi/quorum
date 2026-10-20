# ADR 0003: Microsoft Graph client

> **Status:** accepted · 2026-10-17

## Decision
Write our own typed Graph client in `quorum-graph` on **reqwest** (rustls, HTTP/2, gzip/brotli)
and **tokio**, with `serde` models only for the endpoints and fields we use. Requests are plain
values (method, path, query, body) built without I/O (M-IMPL-IO spirit), executed by a
`Transport` trait: `reqwest` in production, a fixture replayer under `test-util`.
Built in: paging, delta links, `$batch` (≤20), throttling with `Retry-After`, per-mailbox
concurrency limits, upload sessions, `client-request-id`, typed errors with Graph's error code.

## Alternatives
| Option | Why not |
|---|---|
| `graph-rs-sdk` (community) | Broad but generated and loosely typed; we would wrap it anyway to add throttling, fixtures, redaction and our error kinds; a large API surface we don't control |
| Kiota-generated Rust | No supported Rust generator |
| `msgraph-sdk` style codegen from the OpenAPI spec ourselves | The spec is enormous; generating everything adds compile time for endpoints we never call. We may generate models for selected schemas later |

## Consequences
- We own maintenance of the models; unknown fields are ignored and unknown enum values map to
  `Unknown(String)` so Graph additions never break parsing.
- No third-party Graph types leak into other crates (M-DONT-LEAK-TYPES).
