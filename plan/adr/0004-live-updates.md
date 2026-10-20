# ADR 0004: Live updates

> **Status:** accepted for tier 1; tier 2 proposed (owner decision D4) · 2026-10-17

## Context
Graph change notifications are delivered to a public HTTPS webhook or to Azure Event Hubs, never
to a desktop client directly. Chats have no delta query. Presence subscriptions last at most an
hour. The undocumented Teams real-time channel (Trouter) is off-limits.

## Decision
- **Tier 1, always:** adaptive delta/poll loops per resource (architecture §7), driven by window
  focus, the open view, OS idle time, power state and throttling feedback.
- **Tier 2, optional:** a tenant-hosted `quorum-relay` (Rust, axum) receives webhooks, validates
  and renews subscriptions created with the user's delegated token, decrypts rich notifications
  with a Key Vault certificate, and pushes change hints to the user's app over a WebSocket
  authenticated with an Entra token for the relay's own API scope. Hints only trigger an
  immediate tier-1 fetch; correctness never depends on them.

## Alternatives
| Option | Why not |
|---|---|
| Event Hubs read directly by every desktop client | Every client would need hub credentials and would see notifications for all users; privacy and key-distribution problem |
| Public tunnel per client (ngrok-style) | Fragile, a security exposure, not acceptable in a corporate tenant |
| Teams Trouter websocket | Undocumented, against the documented-APIs rule |
| Polling only, forever | Acceptable; tier 2 exists only to cut latency and request volume |

## Consequences
Tier 1 latency is seconds for the focused chat and inbox, minutes elsewhere. Tier 2 needs the
tenant's Azure subscription, a Key Vault certificate and an exposed API on the app registration.
