# Security policy

## Reporting a vulnerability

Please report security problems privately through GitHub's
**Security ▸ Report a vulnerability** form on this repository. Do not open a public issue.
Include what you found, how to reproduce it, and the version (`quorum-cli --version`).
We aim to acknowledge reports within a few working days and to ship a fix or mitigation before
any public disclosure.

Never include real mail, chats, tokens or tenant ids in a report; synthetic examples are enough.

## Supported versions

Quorum is in preview. Only the latest release (and `main`) receives fixes.

## How Quorum protects your account and data

- **Sign-in:** OAuth 2.0 authorization code with PKCE in the system browser (MFA, Conditional
  Access and device compliance apply), or the device-code flow from the CLI. Quorum is a public
  client registered in **your own** Entra tenant: there is no client secret and no Quorum server.
- **Tokens:** the refresh token is stored only in the operating-system keychain; access tokens
  live only in memory. Signing out deletes both.
- **Permissions:** delegated Microsoft Graph permissions only, so Quorum can never do more than the
  signed-in person could in Outlook or Teams. The full list and the reason for each is in
  [`plan/graph-permissions.md`](plan/graph-permissions.md).
- **APIs:** documented Microsoft Graph v1.0 endpoints only. No undocumented Teams or Outlook
  services, no scraping, no other app's client id.
- **Hostile content:** mail and chat HTML is sanitised in Rust before display and rendered in a
  sandboxed frame with scripts disabled; remote images are blocked until you allow them. Graph
  responses are size- and depth-limited, and parsers are fuzzed against hostile input.
- **Local data:** the cache lives in your user profile (`~/Library/Application Support/Quorum`
  on macOS) and is readable only by your account. Logs contain no message content, names or
  addresses.
- **Local automation:** the control channel and MCP server are off by default, listen only on
  `127.0.0.1`, require a per-user token, and ask for confirmation before destructive commands
  (send, post, delete, share).
- **Supply chain:** every dependency passes `cargo deny` (licences and advisories); the code
  forbids `unsafe` and panicking calls in production crates.
