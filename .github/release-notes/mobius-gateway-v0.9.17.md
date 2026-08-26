# möbius Gateway 0.9.17

- Replaces chat-scoped cron setup with gateway-wide structured scheduled tasks.
- Supports once, exact-interval, and five-field cron schedules with explicit IANA time zones and optional end times.
- Keeps completed tasks visible, hides execution sessions from chats, and exposes read-only run transcripts.
- Prevents preview sessions from consuming resident chat capacity.
- Bundles möbius 0.9.12.
- Requires Rust 1.98 or newer when built from source.

Gateway protocol 48 is required. Configuration 20 and chat specification 9 are unchanged. Cron state version 3 is required; migrate version 2 state before starting this release.
