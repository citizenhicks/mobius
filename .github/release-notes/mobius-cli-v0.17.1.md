# mobius-cli 0.17.1

Pins gateway **0.17.1** and uses protocol **100**. Read the [gateway upgrade instructions](https://github.com/citizenhicks/mobius/releases/tag/mobius-gateway-v0.17.1) before upgrading an existing installation. Protocol-99 gateways and native clients require the coordinated upgrade.

The terminal consumes the gateway transcript, caches streamed Markdown and row measurements, and shares request and selection handling across screens. In capability overlays, `q` closes, Left goes back, and Esc returns to the widget list only when there is more than one widget.

Includes the SSE UTF-8 BOM fix and consistent HTTP/WebSocket citation activity. Linux sandboxed execution requires Bubblewrap.

Release verification now reuses successful main CI for the exact source commit. macOS audio conversion compiles with the supported SDK concurrency annotations; sandbox tests use the intended Apple Git and deterministic command completion.
