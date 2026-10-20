# Microsoft Graph permissions

> **Status:** plan · **Written:** 2026-10-17 · All permissions are **delegated** (the app acts as the signed-in user). No application permissions, no client secret.

The IT admin grants tenant-wide consent once for the whole list
(`https://login.microsoftonline.com/{tenant}/adminconsent?client_id={client-id}`), and the app
signs in with the `.default` scope so it receives exactly what was consented. The app checks the
token's `scp` claim at sign-in (`account.permissions`) and disables any feature whose scopes are
missing rather than failing.

Least privilege still applies: a scope is on this list only because a named feature needs it.
Adding a scope is an owner decision (execution-plan §6).

"Admin consent" says whether Microsoft requires an admin to consent; with tenant-wide consent it
does not matter for us, but it tells the admin which ones users could not have consented to
themselves. Rows marked *verify* are checked against the Entra portal during M1.

## Sign-in

| Scope | Admin consent | Why |
|---|---|---|
| `openid`, `profile`, `email` | No | Sign-in, account picker |
| `offline_access` | No | Refresh tokens so the app stays signed in |
| `User.Read` | No | The signed-in user's profile and photo |

## People and directory

| Scope | Admin consent | Why |
|---|---|---|
| `User.ReadBasic.All` | No | Names, photos and email of colleagues (people cards, pickers, presence) |
| `User.Read.All` | Yes | Full profiles, `manager`/`directReports` for the org chart |
| `People.Read` | No | Relevance-ranked people for pickers and search |
| `GroupMember.Read.All` | Yes | Expand distribution lists and Microsoft 365 group members |
| `Group.ReadWrite.All` | Yes | Microsoft 365 group calendars, conversations and files (M4, M8); narrow to `Group.Read.All` if group writes are dropped |
| `Contacts.ReadWrite` | No | Personal contacts (M12) |

## Mail (M3)

| Scope | Admin consent | Why |
|---|---|---|
| `Mail.ReadWrite` | No | Read, move, flag, delete, drafts in the user's mailbox |
| `Mail.Send` | No | Send mail |
| `Mail.ReadWrite.Shared` | No | Shared and delegated mailboxes the user has rights to |
| `Mail.Send.Shared` | No | Send as / on behalf of a shared mailbox |
| `MailboxSettings.ReadWrite` | No | Automatic replies, time zone, working hours, signatures-adjacent settings, categories |

## Calendar (M4)

| Scope | Admin consent | Why |
|---|---|---|
| `Calendars.ReadWrite` | No | Own calendars and events, RSVP, `findMeetingTimes` |
| `Calendars.ReadWrite.Shared` | No | Calendars other people shared with the user or delegated to them, including editing where granted |
| `Calendars.ReadBasic` | No | `getSchedule` free/busy for anyone in the org (covered by `Calendars.ReadWrite`; listed so the admin sees the intent) |
| `Place.Read.All` | Yes | Room and room-list finder (`/places`) |
| `OnlineMeetings.ReadWrite` | No | Create Teams meeting links and read join info |

## Presence (M5)

| Scope | Admin consent | Why |
|---|---|---|
| `Presence.Read.All` | No | Read colleagues' presence (`getPresencesByUserId`) |
| `Presence.ReadWrite` | No | `setPresence` keep-alive session, `setUserPreferredPresence`, `clearPresence`, `setStatusMessage` |

## Teams chat (M6)

| Scope | Admin consent | Why |
|---|---|---|
| `Chat.ReadWrite` | No | List chats, read/edit/delete own messages, mark read, reactions |
| `ChatMessage.Send` | No | Send chat messages |
| `Chat.Create` | No | Start new 1:1 and group chats |
| `ChatMember.ReadWrite` | *verify* | Add and remove chat members |

## Teams and channels (M7)

| Scope | Admin consent | Why |
|---|---|---|
| `Team.ReadBasic.All` | No | Joined teams |
| `Channel.ReadBasic.All` | No | Channels in those teams |
| `ChannelMessage.Read.All` | Yes | Read channel posts and replies |
| `ChannelMessage.Send` | No | Post and reply in channels |
| `ChannelMember.Read.All` | *verify* | Members of private and shared channels (mentions, people cards) |
| `TeamMember.Read.All` | *verify* | Team rosters for @team mentions and people cards |

## Files: OneDrive and SharePoint (M8)

| Scope | Admin consent | Why |
|---|---|---|
| `Files.ReadWrite.All` | No | The user's OneDrive and every file they can reach, upload sessions, share links, chat file attachments |
| `Sites.ReadWrite.All` | No | SharePoint sites, document libraries, lists and list items, followed sites |

`Sites.FullControl.All` / `Sites.Manage.All` are deliberately **not** requested: Quorum does not
create sites or change site permissions.

## Search and tasks

| Scope | Admin consent | Why |
|---|---|---|
| (covered above) | — | `/search/query` uses the scopes of the entity types searched (mail, events, files, chat messages) |
| `Tasks.ReadWrite` | No | To Do and Planner tasks (M12) |
| `Notes.Read.All` | No | OneNote read (M12) |

## Total for the admin

Admin-required scopes in this list: `User.Read.All`, `GroupMember.Read.All`,
`Group.ReadWrite.All`, `Place.Read.All`, `ChannelMessage.Read.All`, plus any *verify* rows that
turn out to require it. Everything else a user could consent to alone.

## Tier-2 relay (M9, only if D4 is approved)

The relay creates subscriptions **with the user's delegated token** passed from the app (the same
scopes as above); it needs no Graph permissions of its own. It does need an Entra app ID URI and
an exposed API scope (`api://<client-id>/relay.connect`) so the app can authenticate to it, and a
certificate for rich-notification encryption stored in the tenant's Key Vault.

## Beta endpoints

None are planned for M0–M8. Any beta endpoint used later is added here with the feature flag
that gates it (D6).

| Endpoint | Feature | Flag |
|---|---|---|
| — | — | — |
