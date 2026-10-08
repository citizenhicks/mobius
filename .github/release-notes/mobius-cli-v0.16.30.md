CLI and bundled gateway advance together to 0.16.30.

- Streaming Markdown reuses finished blocks and parses the changing tail, with full rendering for syntax requiring whole-document context and on completion.
- Terminal redraws are bounded to 60 frames per second; chat history no longer blocks the initial view.
- Workspace sources load when references are requested, and turn summaries fetch diff totals instead of full patches.
- Startup first attempts an authenticated connection to the matching gateway before invoking the existing launcher.
- The bundled gateway batches adjacent streamed deltas, enables NODELAY, and independently configures text, image and live providers.

Protocol 92 and config version 28 are unchanged from 0.16.28. Older state still requires the [offline upgrade](https://github.com/citizenhicks/mobius/blob/mobius-cli-v0.16.30/scripts/README-portable-upgrade.md). No new migration is needed from 0.16.28 or 0.16.29.
