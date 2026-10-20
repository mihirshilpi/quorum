# Setting up Quorum for your organisation

Quorum signs in to Microsoft 365 through an **app registration in your own Microsoft Entra
tenant**. Your IT admin creates it once; after that everyone in the organisation signs in with
their normal work account (MFA, Conditional Access and device policies apply as they do for
Outlook on the web).

Why your own registration rather than one shared app:

- **You approve exactly what Quorum may do.** The permissions are listed below and in the
  admin-consent screen; nothing else is possible.
- **No third party in the middle.** Quorum is a public client: no client secret, no server.
  Mail, chats and files travel only between each user's Mac and Microsoft Graph.
- **You stay in control.** Disable the app, limit it to a group of users, or apply Conditional
  Access to it like any other enterprise app.

## Option A: the setup script (about two minutes)

1. Open **[Azure Cloud Shell](https://shell.azure.com)** (Bash) signed in as someone who can
   register apps. To grant consent in the same run you need **Cloud Application Administrator**
   or **Global Administrator**; otherwise the script prints a link for an admin.
2. Run:

   ```bash
   curl -fsSLO https://raw.githubusercontent.com/mihirshilpi/quorum/main/scripts/register-quorum.sh
   less register-quorum.sh        # read it first: it only calls `az ad app/sp`
   bash register-quorum.sh        # or: bash register-quorum.sh "Quorum (Contoso)"
   ```

   On Windows with the Azure CLI installed, use `scripts/register-quorum.ps1` in PowerShell.
3. The script prints the **Directory (tenant) ID**, the **Application (client) ID** and a
   **setup link** (`quorum://setup?tenant=…&client_id=…`). Send the link to your colleagues:
   opening it starts Quorum with both IDs filled in.

The script resolves every Microsoft Graph permission by name and stops without changes if any
is unknown. It is generated from the app's own permission list (`quorum-cli setup-tenant`), so
it never drifts from what the app asks for.

## Option B: the Entra admin center

1. **Entra admin center ▸ Applications ▸ App registrations ▸ New registration**.
   - Name: `Quorum` (any name works).
   - Supported account types: **Accounts in this organizational directory only**.
   - Redirect URI: platform **Public client/native (mobile & desktop)**, value `http://localhost`.
2. Copy the **Application (client) ID** and the **Directory (tenant) ID** from *Overview*.
3. **Authentication ▸ Advanced settings ▸ Allow public client flows: Yes** (used by
   `quorum-cli login --device`; the desktop app signs in through the browser).
4. **API permissions ▸ Add a permission ▸ Microsoft Graph ▸ Delegated permissions**, add:

   ```
   openid profile email offline_access
   User.Read User.ReadBasic.All User.Read.All People.Read GroupMember.Read.All Group.ReadWrite.All
   Contacts.ReadWrite
   Mail.ReadWrite Mail.Send Mail.ReadWrite.Shared Mail.Send.Shared MailboxSettings.ReadWrite
   Calendars.ReadWrite Calendars.ReadWrite.Shared Calendars.ReadBasic Place.Read.All OnlineMeetings.ReadWrite
   Presence.Read.All Presence.ReadWrite
   Chat.ReadWrite ChatMessage.Send Chat.Create ChatMember.ReadWrite
   Team.ReadBasic.All Channel.ReadBasic.All ChannelMessage.Read.All ChannelMessage.Send
   ChannelMember.Read.All TeamMember.Read.All
   Files.ReadWrite.All Sites.ReadWrite.All
   Tasks.ReadWrite Notes.Read.All
   ```

   What each one is for: [`plan/graph-permissions.md`](../plan/graph-permissions.md). All are
   delegated: Quorum can only ever do what the signed-in person could do in Outlook or Teams.
5. **Grant admin consent for <your organisation>**, or open
   `https://login.microsoftonline.com/<tenant-id>/adminconsent?client_id=<client-id>`.
6. Send users the two IDs, or the setup link
   `quorum://setup?tenant=<tenant-id>&client_id=<client-id>`.

## For users: connect your account

1. Open the setup link from your IT admin, or start Quorum and paste the two IDs into the
   welcome screen (**Settings ▸ Account** later).
2. Click **Save and sign in**. Your browser opens Microsoft's sign-in page; sign in with your
   work account as usual. The browser hands Quorum a one-time code on `http://localhost`.
3. Quorum downloads your recent mail, calendar, chats and files once, then keeps them current
   by fetching only changes.

From a terminal:

```bash
quorum-cli login --tenant <tenant-id> --client-id <client-id>   # browser sign-in
quorum-cli login --device                                       # device code, no browser
quorum-cli whoami
quorum-cli permissions                                          # granted and missing permissions
quorum-cli consent-link                                         # admin-consent and setup links
```

**Settings ▸ Account** shows the same: who is signed in, any missing permission (Quorum turns off
only the features that need it), and buttons to copy the admin-consent and setup links.

## Troubleshooting

Quorum explains Entra ID errors in plain words and shows the `AADSTS` number. The common ones:

| Code | Meaning | Fix |
|---|---|---|
| AADSTS65001, 90094 | The organisation hasn't consented | An admin opens the admin-consent link |
| AADSTS50020 | Account from another organisation | Sign in with your work account, or check the tenant ID |
| AADSTS700016 | Client ID not found in this tenant | Check both IDs |
| AADSTS50011, 500113 | Redirect URI missing | Add `http://localhost` as a Mobile and desktop redirect |
| AADSTS7000218 | Public client flows off | Allow public client flows (device-code sign-in only) |
| AADSTS50105 | User not assigned to the app | Assign the user or group under Enterprise applications |
| AADSTS53000, 53003 | Conditional Access blocked the sign-in | Enrol the device or ask IT about a policy for Quorum |

## Conditional Access and compliance

- Sign-in happens in the system browser, so the macOS Enterprise SSO extension, MFA and
  device-compliance policies apply exactly as they do for Outlook on the web.
- Target policies at the **Quorum** enterprise application like any other app.
- Refresh tokens are stored in the macOS keychain; access tokens live only in memory. Signing
  out (**Settings ▸ Account ▸ Sign out**) removes both. Revoking sessions in Entra signs Quorum
  out at the next refresh.
- Quorum calls only documented Microsoft Graph v1.0 endpoints. Audit sign-ins under
  **Enterprise applications ▸ Quorum ▸ Sign-in logs**.

## No tenant yet?

`quorum --mock-graph` (from a checkout: `cargo xtask dev -- --mock-graph`) runs Quorum on
synthetic sample data, and nothing leaves the Mac. For a free test tenant, join the
[Microsoft 365 Developer Program](https://developer.microsoft.com/microsoft-365/dev-program)
where eligible, then run the setup script in it.
