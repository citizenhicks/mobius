# möbius for macOS 0.2.0

This release brings the integrated Chromium browser, secured Cloud connections,
and the latest workspace improvements together in one desktop app.

- Open scoped browser tabs alongside chats and files, with bounded browser controls.
- Collapse the file explorer with the folder button. Sidebar, pinned session details,
  and folders animate smoothly and respect Reduce Motion.
- Consistent refresh icons and the möbius spinner throughout the app.
- Sign in to Cloud with Apple through the browser. Cloud connections enforce
  session and age eligibility, and forget saved Cloud connections on sign-out.
- Encrypted WebSocket transport authenticates the gateway's pinned identity.

Requires Apple Silicon and macOS 26 or later. Update your gateway to **0.15.53**
and your CLI to **0.15.52** together: this release uses gateway protocol **86**.
Existing WebSocket connections require a fresh pairing code. Existing gateway
chats and files remain in place; do not reset gateway data when upgrading.

The app keeps the same bundle identifier and Sparkle signing identity as 0.1.0.
