# Privacy

Quorum is a desktop app. **There is no Quorum service**: nobody who builds or distributes Quorum
can see your mail, calendar, chats, files or account.

- **What leaves your computer:** only requests to Microsoft (`login.microsoftonline.com` for
  sign-in, `graph.microsoft.com` for your data), made with your own account through your
  organisation's own app registration. Remote images in mail are fetched only when you allow
  them.
- **What is stored locally:** a cache of your recent mail, events, chats and file listings, your
  drafts and settings, in your user profile (`~/Library/Application Support/Quorum` on macOS).
  The refresh token is in the OS keychain. Signing out removes the token; to remove the cache
  too, quit Quorum and delete that folder.
- **Telemetry:** none. Quorum sends no analytics, crash reports or usage data anywhere.
- **Your organisation:** sign-ins appear in your tenant's Entra sign-in logs like any other app,
  and your admin can disable or restrict Quorum at any time.
