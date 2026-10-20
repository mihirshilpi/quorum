# Graph fixtures

Synthetic Microsoft Graph responses replayed by `quorum_graph::FixtureTransport`
(feature `test-util`) and, later, by `--mock-graph`.

**Only synthetic data.** Addresses use `example.com` / `contoso.example`, ids are fake GUIDs such
as `00000000-0000-0000-0000-000000000001`, names are obviously made up. Never commit recorded
tenant data unless it has been scrubbed to this standard.

## Layout

| File | What it is |
|---|---|
| `routes.json` | Manifest mapping requests to response files (see below) |
| `me.json` | `GET /me` |
| `messages-page1.json` | `GET /me/messages`, first page, with `@odata.nextLink` (`$skiptoken=page2`) |
| `messages-page2.json` | Second (last) page, no `nextLink` |
| `messages-delta-page1.json` | `GET /me/mailFolders/inbox/messages/delta`, with `nextLink` (`$skiptoken=delta-page2`) and an `@removed` item |
| `messages-delta-page2.json` | Last delta page, with `@odata.deltaLink` (`$deltatoken=synthetic-token-1`) |
| `delta-expired.json` | 410 body with code `syncStateNotFound` |
| `throttled.json` | 429 body (`TooManyRequests`) |
| `batch-partial-throttle.json` | `$batch` response: 200, 429 with `Retry-After`, 424 dependent, 404 |
| `mail/` | Mail wrappers (`tests/api_mail.rs`): folders (+ delta with `@removed`), inbox pages (`$skiptoken=inbox-page2`), message with HTML body, per-folder message delta (`mail-delta-2` → `mail-token-1`), attachments of every `@odata.type` plus an inline one, an Outlook upload session, mailbox settings, rules, master categories, Focused Inbox overrides, and `messages-hostile.json` (missing fields, wrong types, unknown enums) |
| `calendar/` | Calendar wrappers (`tests/api_calendar.rs`): calendars (own, shared, unknown colour), calendar groups, calendar view pages (recurring series master, Teams meeting), event with body, calendar-view delta (`cal-delta-2` → `cal-token-1`), `getSchedule` (hostile item, unknown digits, per-schedule error), `findMeetingTimes` (plus an empty answer), rooms and room lists, and `events-hostile.json` |
| `people/` | People, directory, contacts and search wrappers (`tests/api_people.rs`): `/me`, manager, user search, direct reports (with a non-user), group members (with a nested group and `nextLink`), `/me/people`, contacts, and `/search/query` responses (`search-mixed.json` with every entity type, `search-hostile.json`) |
| `chat/` | Teams chat wrappers (`tests/api_chat.rs`): chat pages, members, messages (and hostile ones), a sent message, pinned messages |
| `teams/` | Teams and channel wrappers (`tests/api_teams.rs`): joined teams, channels, members, replies, the channel files folder, channel message delta pages |
| `presence/` | Presence wrappers (`tests/api_presence.rs`): own presence and `getPresencesByUserId` |
| `files/` | OneDrive and SharePoint wrappers (`tests/api_files.rs`): drives, items, children pages, delta pages, permissions, versions, thumbnails, recent, followed sites, lists, list items, columns, site pages |

Each response file is the body exactly as Graph would send it.

## `routes.json`

```json
{
  "routes": [
    {
      "method": "GET",
      "path": "/me/messages",
      "query": { "$skiptoken": "page2" },
      "responses": [
        { "status": 429, "headers": { "Retry-After": "1" }, "file": "throttled.json" },
        { "status": 200, "file": "messages-page2.json" }
      ]
    }
  ]
}
```

- `path` is relative to the version (`/v1.0` or `/beta` is stripped from the request path; the
  host is ignored).
- `query` (optional) is a subset of decoded query parameters that must be present. When several
  routes match, the one with the most `query` entries wins, then the first listed.
- `responses` is a script: each call takes the next entry, and the last one repeats. An entry has
  `status` (default 200), optional `headers`, and either `file` (relative to this folder) or an
  inline JSON `body`.
- Requests that match nothing get a 404 with error code `fixtureNotFound`.

In code, tests can also add routes directly:
`transport.add(Route::new(Method::Get, "/me").respond(FixtureResponse::file(200, "me.json")?))`.
